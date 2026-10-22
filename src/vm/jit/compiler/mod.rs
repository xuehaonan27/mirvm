//! Bytecode -> Cranelift translator (scalar subset) + compiler service thread.
//!
//! Input = frozen `ir::FuncBody`: the JIT consumes bytecode, not MIR, and `tcx` does not enter
//! the execution phase.
//! Semantic contract = **bit-identical to the interpreter** (the JIT-on/off differential is the
//! first oracle): all values preserve the "I64 zero-extended to width" slot invariant; operations
//! mirror interp's int_bin/int_cmp/int_ovf identities; results are masked back by width band.
//! Frame locals are all promoted to Cranelift SSA variables (admission excludes address-of /
//! memory operands => no stack frame memory); entry uniformly defs 0 (valid MIR has no
//! read-before-write paths, which makes this deterministic).
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
//! registered at once. Admission already excludes cleanup edges (unwind-transparent: panic
//! only passes through, does not land).
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
    /// The store's index, loaded on the first lookup: a session reuses what was published before it
    /// started, never what it just wrote itself.
    jit_index: Option<crate::store::jit::Index>,
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
    // One compiler per tier: each has its own ISA, its own module and so its own jit-key, which is what
    // lets one fragment have an entry at each level. Both publish into the same slots, so dispatch reads
    // whichever was published last.
    let mut baseline = Compiler::with_tier(&shared, domain, Tier::Baseline);
    let mut optimized = Compiler::with_tier(&shared, domain, Tier::Optimized);
    let mut osr = None;
    // Before a single request is served: link what the store holds for the functions the last run found
    // hot. A stored optimized entry wins, because that is the tier the function is about to be asked
    // for; otherwise the baseline entry is linked and the upgrade threshold will replace it later, and
    // either way the first call finds a published entry instead of enqueueing a compile.
    let mut observed: Vec<u32> = Vec::new();
    // The set the tier policy reads: a function in this order is asked for optimized the first time
    // this session compiles it.
    shared.jit.set_hot(&heat.order);
    if baseline.jit_cache {
        let mut prelinked = 0u64;
        for func in &heat.order {
            if shared.jit.work.as_ref().is_some_and(|work| work.enforce)
                && shared
                    .module
                    .funcs
                    .execution(*func as usize)
                    .is_some_and(|body| body.shared.is_some())
            {
                // A previous run's heat is not credit for work this Engine has performed.
                continue;
            }
            let Some(body) = shared.module.funcs.get(*func as usize) else {
                continue;
            };
            if optimized.load_cached(*func, body) || baseline.load_cached(*func, body) {
                prelinked += 1;
                // A linked body no longer reaches the interpretation counter. Retain it in
                // the next order so later requests cannot erase the reusable hot set.
                observed.push(*func);
            }
        }
        if prelinked != 0 {
            helpers::cache_prelinked(prelinked);
            if dbg {
                crate::diag_debug!(Jit, "prelinked {prelinked} entries from the heat order");
            }
        }
    }
    while let Ok(request) = rx.recv() {
        let func = request.func;
        let c = match request.tier {
            Tier::Baseline => &mut baseline,
            Tier::Optimized => &mut optimized,
        };
        if shared.jit.stopping.load(Ordering::Acquire) {
            break;
        }
        if request.osr {
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
        c.compile(func);
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
    for c in [&mut baseline, &mut optimized] {
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
    // Scratch belongs to the worker, not to the published code's process-long lifetime.
    for compiler in [&mut baseline, &mut optimized] {
        compiler.context = None;
        compiler.c2i_templates = Default::default();
    }
    // Published JIT code may still be live on a sleeping stack in the process-wide
    // thread pool. Teardown has to join the compile thread rather than race libc
    // cleanup, and it must not destruct the JITModules either, since that would unmap
    // published code. The address space is reclaimed in one go at process exit.
    std::mem::forget(baseline);
    std::mem::forget(optimized);
    if let Some(mut compiler) = osr {
        compiler.context = None;
        compiler.c2i_templates = Default::default();
        std::mem::forget(compiler);
    }
}

// ===== Runtime helpers (compiled code calls back into the engine through imported symbols) =====

#[cfg(test)]
mod tests;
