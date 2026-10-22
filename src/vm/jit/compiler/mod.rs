//! Bytecode -> Cranelift translator (scalar subset) + compiler service thread.
//!
//! Input = frozen `ir::FuncBody`: the JIT consumes bytecode, not MIR, and `tcx` does not enter
//! the execution phase.
//! Semantic contract = **bit-identical to the interpreter** (the JIT-on/off differential is the
//! first oracle): all values preserve the "I64 zero-extended to width" slot invariant; operations
//! mirror interp's int_bin/int_cmp/int_ovf identities; results are masked back by width band.
//! Non-aliased scalar slots use SSA variables; addressable values retain frame storage.
//! OSR continuations borrow the interpreter frame and preserve every live address.
//!
//! Calls (two entries + PLT):
//! - **fast**: pure guest signature (n x I64 -> 0/1 x I64). Compiled code calls via
//!   `slots_fast[callee]` memory indirection (load + call_indirect, call site has constant
//!   shape); slots of not-yet-compiled callees first receive a **c2i trampoline** (fast
//!   shape, internally packs arguments and calls `mirvm_c2i` back to the interpreter).
//! - **packed**: `extern "C-unwind" fn(*const u64, *mut u64)` -- one interp i2c hop
//!   (call_guest reads `slots[f]`).
//!
//! Publish order = fast first, then packed (Release); call_guest Acquire read => any thread
//! entering compiled code must see its callee trampoline/entry (happens-before chain).
//!
//! unwind (CFI-only): create_unwind_info -> gimli FrameTable -> whole `.eh_frame` section
//! registered at once. Cleanup edges also carry LSDA landing-pad descriptions.
//!
//! Single worker thread holds the JITModule (code memory lives for the process lifetime;
//! cranelift-jit has no per-function release).
//!
//! This file is the service thread and the `Compiler` it drives; the translator is split by what
//! it is doing: [`imports`] declares what generated code may call, [`translate`] defines one body,
//! [`trace`] pins the recorder for a trace body, and [`symbols`] publishes what came out.

use super::admit::{CalleeAbi, admit, callee_abi};
use super::frame::analyze_frame;
use super::helpers::_Unwind_Resume;
use super::helpers::*;
use super::translate::Translator;
use super::*;

mod isa;
mod loader;
pub(crate) use isa::*;

/// Compiler for one code domain: owns the Cranelift module, the ids of every imported
/// helper, and the unwind records accumulated for the batch being defined.
struct Compiler<'a> {
    shared: &'a Shared,
    /// Which domain's ISA this compiler builds with and which slot set it publishes
    /// into. Plain publishes into `Jit::slots`/`slots_fast`, Trace into its own pair.
    domain: CodeDomain,
    module: JITModule,
    fbc: FunctionBuilderContext,
    /// Cleared scratch storage retained between definitions. A failed definition drops its context
    /// instead, so the next one starts fresh without retaining partial IR or analysis state.
    context: Option<cranelift_codegen::Context>,
    /// `mirvm_c2i`: compiled code reaches an uncompiled guest function through it and
    /// lands back in the interpreter. `ctx` restoration is the boundary TLS attach,
    /// shared with the thunk factory and idempotent.
    c2i: ClifFuncId,
    /// Defined trampolines that no successful caller has published yet.
    pending_callees: std::collections::HashMap<u32, PendingJitSymbol>,
    /// Unrelocated code for each flattened signature, private to this module's ISA and imports.
    c2i_templates: std::collections::HashMap<(usize, usize), c2i::Template>,
    call_main_catch: ClifFuncId,
    unreachable: ClifFuncId,
    /// Host memmove/memset channel used by Copy and frame zeroing.
    memmove: ClifFuncId,
    memset: ClifFuncId,
    /// MemCmp (the `compare_bytes` intrinsic).
    memcmp: ClifFuncId,
    /// Divide-by-zero diagnostic exit; same message and exit code as
    /// `unwind::engine_abort`.
    div_zero: ClifFuncId,
    /// Volatile read/write, sharing the interpreter's opaque-byte carrier.
    volatile_load: ClifFuncId,
    volatile_store: ClifFuncId,
    /// CallIndirect/TlsRef helpers; same bodies as `helpers.rs`.
    call_indirect: ClifFuncId,
    tls_ref: ClifFuncId,
    call_foreign: ClifFuncId,
    /// CallBuiltin and the allocation fast path; same `semantics::builtin` bodies as
    /// `helpers.rs`.
    call_builtin: ClifFuncId,
    alloc: ClifFuncId,
    /// TerminateAbort pure helper, the Terminate boundary called directly, and the
    /// `_Unwind_Resume` import.
    terminate_abort: ClifFuncId,
    call_terminate: ClifFuncId,
    unwind_resume: ClifFuncId,
    /// Lets a JIT cleanup pad tell an EngineFault from any other exception pointer it
    /// catches.
    exception_is_engine_fault: ClifFuncId,
    /// Trap placeholder helper; same message and exit code as `unwind::engine_abort`.
    trap: ClifFuncId,
    /// Unified SIMD/wide statement helper and the SIMD rvalue helper; thin shells that
    /// re-match and call the shared `semantics::simd` body.
    simd_stmt: ClifFuncId,
    simd_rv: ClifFuncId,
    /// Checks stack headroom before a compiled body allocates its frame. The trace domain calls it;
    /// the plain domain inlines the comparison and calls `stack_overflow` instead.
    stack_guard: ClifFuncId,
    /// The inline check's slow path: report the overflow and abort.
    stack_overflow: ClifFuncId,
    /// Deferred async-signal delivery at compiled block boundaries.
    poll_signals: ClifFuncId,
    /// The trace domain's syscall site helper. The caller passes the recorder it read
    /// from the pinned register.
    host_syscall_trace: ClifFuncId,
    /// (clif id, unwind info, LSDA bytes of a try_call function) accumulated for this
    /// batch; all of it is registered together after finalize.
    pending_unwind: Vec<(ClifFuncId, UnwindInfo, Option<Vec<u8>>)>,
    /// The values this compiler's sites baked, by site name ([`reloc`]).
    values: std::sync::Arc<reloc::Values>,
    /// The sites of the body being compiled, in emission order.
    sites: reloc::Sites,
    /// Which optimization level this compiler builds at; ordinary entries include it in the jit-key.
    tier: Tier,
    /// A separate module keeps continuation signatures and relocation names out of ordinary code.
    osr: bool,
    /// The functions this compiler published. Compilation is once per tier, so a second request for a
    /// tier that already answered is a no-op while the *other* tier's answer is an upgrade.
    published: std::collections::HashSet<u32>,
    /// The artifact of the function being compiled, one symbol at a time, while a reload is on.
    artifacts: Vec<artifact::Symbol>,
    /// The body's canonical reference numbering, while an entry for it is being built.
    ordinals: Option<artifact::Ordinals>,
    /// Whether this compiler links what it just compiled back from its artifact and publishes that,
    /// instead of publishing the module's own code (`MIRVM_JIT_RELOAD`).
    reload: bool,
    /// Whether this compiler publishes completed entries into the JIT code store (`cache/jit`).
    jit_cache: bool,
    /// The key every entry this compiler writes is filed under: the codegen inputs that are not the
    /// fragment.
    jit_key: artifact::JitKey,
    /// Entries captured but not yet published, as one pack per batch.
    jit_staging: crate::store::jit::Session,
    #[cfg(test)]
    fail_after_symbol: Option<JitSymbolRole>,
    /// The last entry captured, for tests that link one back or compare two sessions' artifacts.
    #[cfg(test)]
    last_entry: Option<artifact::Entry>,
}

struct PendingJitSymbol {
    id: ClifFuncId,
    func: u32,
    role: JitSymbolRole,
    size: u64,
}

// ===== LSDA generation (layout copied line for line from the ABI; do not invent) =====

mod lsda;
pub(crate) use lsda::*;

mod c2i;
mod imports;
mod osr;
mod symbols;
mod trace;
mod translate;

impl Compiler<'_> {
    fn take_context(&mut self) -> cranelift_codegen::Context {
        self.context
            .take()
            .unwrap_or_else(|| self.module.make_context())
    }

    fn recycle_context(&mut self, mut context: cranelift_codegen::Context) {
        self.module.clear_context(&mut context);
        self.context = Some(context);
    }
}

/// Start the compiler service (called by run_vm_engine after Shared is finalized;
/// not started when --jit off).
pub fn start(shared: &std::sync::Arc<Shared>) {
    if !shared.jit.enabled {
        return;
    }
    super::install_if_asked();
    shared.jit.stopping.store(false, Ordering::Release);
    let (tx, rx): (Sender<Request>, Receiver<Request>) = std::sync::mpsc::channel();
    *shared.jit.queue.lock().unwrap() = Some(tx);
    // A failed compilation or a dead worker just stays interpreted; no semantic
    // path depends on the JIT.
    let worker_shared = std::sync::Arc::clone(shared);
    // The worker compiles for the Engine's frozen domain, so the code it
    // publishes lands in the slot set dispatch will read for that domain.
    let worker_domain = shared.domain;
    // What a previous run of this program found hot, and where this run leaves its own order.
    let worker_heat = shared.heat.get().cloned().unwrap_or_default();
    let worker = std::thread::Builder::new()
        .name("mirvm-jit".into())
        .spawn(move || worker(worker_shared, rx, worker_domain, worker_heat));
    match worker {
        Ok(handle) => *shared.jit.worker.lock().unwrap() = Some(handle),
        Err(error) => {
            // The queue is already in place, so every later request lands in a channel nothing
            // reads. Staying interpreted is the semantics, but that state must be visible when
            // someone asks why this run compiled nothing.
            crate::diag_debug!(
                Jit,
                "the compile worker did not start ({error}); every function stays interpreted"
            );
            *shared.jit.worker.lock().unwrap() = None;
        }
    }
}

/// Stop this Engine's compiler service. Already-published machine code stays valid;
/// requests not yet published fall back to the interpreter. Each Engine joins its own
/// worker, so no process-global pointer can let the last starter reap an earlier one.
pub fn stop(shared: &Shared) {
    shared.jit.stopping.store(true, Ordering::Release);
    shared.jit.queue.lock().unwrap().take();
    if let Some(worker) = shared.jit.worker.lock().unwrap().take() {
        let _ = worker.join();
    }
    if !shared.jit.enabled
        && let Some(work) = &shared.jit.work
    {
        work.report(0);
    }
}

pub(super) fn worker(
    shared: std::sync::Arc<Shared>,
    rx: Receiver<Request>,
    domain: CodeDomain,
    heat: crate::vm::jit::Heat,
) {
    let dbg = crate::options::jit_debug();
    helpers::stat_init();
    let mut baseline = None;
    let mut optimized = None;
    let mut osr = None;
    let mut loader = None;
    let mut observed = Vec::new();
    shared.jit.set_hot(&heat.order);
    let mut history = heat.order.iter().peekable();
    let cache = domain == CodeDomain::Plain && !crate::options::no_jit_cache();
    let mut served = 0usize;
    loop {
        if shared.jit.stopping.load(Ordering::Acquire) {
            break;
        }
        // Current demand gets first service; historical work is bounded both ways so neither
        // a long heat list nor a steady request stream can starve the other.
        let ready = if served < 8 { rx.try_recv().ok() } else { None };
        let request = if let Some(request) = ready {
            request
        } else {
            let batch: Vec<_> = if cache {
                history
                    .by_ref()
                    .take(8)
                    .filter(|&&func| {
                        !shared.jit.work.as_ref().is_some_and(|work| work.enforce)
                            || !shared
                                .module
                                .funcs
                                .execution(func as usize)
                                .is_some_and(|body| body.shared.is_some())
                    })
                    .map(|&func| (func, Tier::Optimized))
                    .collect()
            } else {
                Vec::new()
            };
            served = 0;
            if !batch.is_empty() {
                let loaded = loader
                    .get_or_insert_with(|| loader::Loader::new(&shared, domain))
                    .load(&batch, true);
                helpers::cache_prelinked(loaded.len() as u64);
                observed.extend(loaded);
                continue;
            }
            if cache && history.peek().is_some() {
                continue;
            }
            let Ok(request) = rx.recv() else { break };
            request
        };
        served += 1;
        let func = request.func;
        if shared.jit.stopping.load(Ordering::Acquire) {
            break;
        }
        if request.osr {
            if !shared.jit.begin_osr(domain, func) {
                helpers::stat(helpers::S_OSR_CANCEL);
                if dbg {
                    crate::diag_debug!(
                        Jit,
                        "OSR f{func}: canceled before compilation; no active demand"
                    );
                }
                continue;
            }
            if dbg {
                crate::diag_debug!(
                    Jit,
                    "OSR f{func}: starting with {} waiting after {} us",
                    shared.jit.slots_for(domain).osr[func as usize].waiting(),
                    request
                        .queued
                        .map_or(0, |queued| queued.elapsed().as_micros())
                );
            }
            osr.get_or_insert_with(|| Compiler::for_osr(&shared, domain))
                .compile_osr(func);
            continue;
        }
        let started = shared.jit.work.as_ref().map(|_| std::time::Instant::now());
        let units = shared.jit.work.as_ref().and_then(|_| {
            let body = shared.module.funcs.execution(func as usize)?;
            body.shared.as_ref()?;
            Some(
                body.code()
                    .iter()
                    .map(|block| block.stmts.len() as u64 + 1)
                    .sum::<u64>()
                    .max(1),
            )
        });
        if let (Some(work), Some(units)) = (&shared.jit.work, units)
            && !work.admit(func, units)
        {
            continue;
        }
        if dbg {
            crate::diag_debug!(
                Jit,
                "received compilation request for f{func} ({})",
                shared.module.funcs.header(func as usize).unwrap().name()
            );
        }
        let specialize_ns = units
            .map(|_| {
                let started = std::time::Instant::now();
                let _ = shared.module.funcs.get(func as usize);
                started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
            })
            .unwrap_or(0);
        let loaded = cache && {
            let loader = loader.get_or_insert_with(|| loader::Loader::new(&shared, domain));
            if !loader.has(func, request.tier) {
                loader.load(&[(func, request.tier)], true);
            }
            loader.has(func, request.tier)
        };
        if !loaded && !shared.jit.stopping.load(Ordering::Acquire) {
            let compiler = match request.tier {
                Tier::Baseline => &mut baseline,
                Tier::Optimized => &mut optimized,
            }
            .get_or_insert_with(|| Compiler::with_tier(&shared, domain, request.tier));
            compiler.compile(func);
            if compiler.published.contains(&func)
                && let Some(loader) = &mut loader
            {
                loader.record(func, request.tier);
            }
        }
        if let (Some(work), Some(units), Some(started)) = (&shared.jit.work, units, started) {
            let queue_ns = request
                .queued
                .map(|queued| {
                    started
                        .duration_since(queued)
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64
                })
                .unwrap_or(0);
            let worker_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            let entry = shared.jit.slots_for(domain).slots[func as usize].load(Ordering::Acquire);
            work.transition(
                func,
                units,
                queue_ns,
                specialize_ns,
                worker_ns,
                entry != 0 && entry != FAIL_SENTINEL,
            );
        }
        observed.push(func);
        // The other compiler is not consulted here: a function is compiled once per tier, and the
        // upgrade request a hot function raises is what replaces a baseline entry with an optimized one.
        if dbg {
            let s = shared.jit.slots[func as usize].load(Ordering::Acquire);
            let ok = s != 0 && s != FAIL_SENTINEL;
            if ok {
                let addr = shared.jit.slots[func as usize].load(Ordering::Acquire);
                let fast = shared.jit.slots_fast[func as usize].load(Ordering::Acquire);
                crate::diag_debug!(
                    Jit,
                    "f{func} release={ok} @{addr:#x} fast@{fast:#x}({})",
                    shared.module.funcs.header(func as usize).unwrap().name()
                );
            } else {
                crate::diag_debug!(Jit, "f{func} release={ok}");
            }
            if let Some(sites) = shared.jit.recorded_sites(domain, func) {
                crate::diag_debug!(Jit, "f{func} {sites} recorded sites");
            }
        }
    }
    // Retain successful prelinks, followed by newly requested functions in arrival order.
    heat.write(&observed);
    // Whatever a batch still holds is worth a file: the next session can only reuse what is in the
    // store, and a worker that exits with entries staged would lose them.
    let publishing = shared.jit.work.as_ref().map(|_| std::time::Instant::now());
    for c in [&mut baseline, &mut optimized].into_iter().flatten() {
        if c.jit_cache {
            let session = std::mem::take(&mut c.jit_staging);
            if let Err(error) = session.publish()
                && dbg
            {
                crate::diag_debug!(Jit, "cannot publish JIT entries: {error}");
            }
        }
    }
    if let (Some(work), Some(publishing)) = (&shared.jit.work, publishing) {
        work.report(publishing.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
    }
    // Published code can remain on sleeping guest stacks. Retain code arenas, but release
    // compiler scratch and the independently owned store index when the service exits.
    for mut compiler in [baseline, optimized, osr].into_iter().flatten() {
        compiler.context = None;
        compiler.c2i_templates = Default::default();
        std::mem::forget(compiler);
    }
}

// ===== Runtime helpers (compiled code calls back into the engine through imported symbols) =====

#[cfg(test)]
mod tests;
