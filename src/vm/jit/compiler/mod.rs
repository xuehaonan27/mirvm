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
    /// `mirvm_c2i`: compiled code reaches an uncompiled guest function through it and
    /// lands back in the interpreter. `ctx` restoration is the boundary TLS attach,
    /// shared with the thunk factory and idempotent.
    c2i: ClifFuncId,
    call_main_catch: ClifFuncId,
    unreachable: ClifFuncId,
    /// Host memmove/memset channel used by Copy and frame zeroing.
    memmove: ClifFuncId,
    memset: ClifFuncId,
    /// MemCmp (the `compare_bytes` intrinsic).
    memcmp: ClifFuncId,
    /// Divide-by-zero diagnostic exit; same message and exit code as interp's
    /// `engine_abort`.
    div_zero: ClifFuncId,
    /// Volatile read/write, sharing the interpreter's opaque-byte carrier.
    volatile_load: ClifFuncId,
    volatile_store: ClifFuncId,
    /// CallIndirect/TlsRef helpers; same bodies as `helpers.rs`.
    call_indirect: ClifFuncId,
    tls_ref: ClifFuncId,
    call_foreign: ClifFuncId,
    /// CallBuiltin and the allocation fast path; same `exec_builtin` bodies as
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
    /// Trap placeholder helper; same message and exit code as interp's `engine_abort`.
    trap: ClifFuncId,
    /// Unified SIMD/wide statement helper and the SIMD rvalue helper; thin shells that
    /// re-match and call interp's shared `simd_exec` body.
    simd_stmt: ClifFuncId,
    simd_rv: ClifFuncId,
    /// Checks stack headroom before a compiled body allocates its frame.
    stack_guard: ClifFuncId,
    /// Deferred async-signal delivery at compiled block boundaries.
    poll_signals: ClifFuncId,
    /// The trace domain's syscall site helper. The caller passes the recorder it read
    /// from the pinned register.
    host_syscall_trace: ClifFuncId,
    /// (clif id, unwind info, LSDA bytes of a try_call function) accumulated for this
    /// batch; all of it is registered together after finalize.
    pending_unwind: Vec<(ClifFuncId, UnwindInfo, Option<Vec<u8>>)>,
    #[cfg(test)]
    fail_after_symbol: Option<JitSymbolRole>,
}

struct PendingJitSymbol {
    id: ClifFuncId,
    func: u32,
    role: JitSymbolRole,
    size: u64,
}

/// DW.ref indirection cell for the personality CIE: one cell shared by every FDE in
/// the table.
static PERS_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ===== LSDA generation (layout copied line for line from the ABI; do not invent) =====

mod lsda;
pub(crate) use lsda::*;

mod imports;
mod symbols;
mod trace;
mod translate;

/// Start the compiler service (called by run_vm_engine after Shared is finalized;
/// not started when --jit off).
pub fn start(shared: &std::sync::Arc<Shared>) {
    if !shared.jit.enabled {
        return;
    }
    shared.jit.stopping.store(false, Ordering::Release);
    let (tx, rx): (Sender<u32>, Receiver<u32>) = std::sync::mpsc::channel();
    *shared.jit.queue.lock().unwrap() = Some(tx);
    // A failed compilation or a dead worker just stays interpreted; no semantic
    // path depends on the JIT.
    let worker_shared = std::sync::Arc::clone(shared);
    // The worker compiles for the Engine's frozen domain, so the code it
    // publishes lands in the slot set dispatch will read for that domain.
    let worker_domain = shared.domain;
    let worker = std::thread::Builder::new()
        .name("mirvm-jit".into())
        .spawn(move || worker(worker_shared, rx, worker_domain));
    *shared.jit.worker.lock().unwrap() = worker.ok();
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
}

pub(super) fn worker(shared: std::sync::Arc<Shared>, rx: Receiver<u32>, domain: CodeDomain) {
    let dbg = crate::options::get().jit_debug;
    let mut c = Compiler::with_domain(&shared, domain);
    while let Ok(func) = rx.recv() {
        if shared.jit.stopping.load(Ordering::Acquire) {
            break;
        }
        if dbg {
            eprintln!(
                "mirvm-jit-debug: received compilation request for f{func} ({})",
                shared.module.funcs[func as usize].name
            );
        }
        c.compile(func);
        if dbg {
            let s = shared.jit.slots[func as usize].load(Ordering::Acquire);
            let ok = s != 0 && s != FAIL_SENTINEL;
            if ok {
                let addr = shared.jit.slots[func as usize].load(Ordering::Acquire);
                let fast = shared.jit.slots_fast[func as usize].load(Ordering::Acquire);
                eprintln!(
                    "mirvm-jit-debug: f{func} release={ok} @{addr:#x} fast@{fast:#x}({})",
                    shared.module.funcs[func as usize].name
                );
            } else {
                eprintln!("mirvm-jit-debug: f{func} release={ok}");
            }
        }
    }
    // Published JIT code may still be live on a sleeping stack in the process-wide
    // thread pool. Teardown has to join the compile thread rather than race libc
    // cleanup, and it must not destruct the JITModule either, since that would unmap
    // published code. The address space is reclaimed in one go at process exit.
    std::mem::forget(c);
}

// ===== Runtime helpers (compiled code calls back into the engine through imported symbols) =====

#[cfg(test)]
mod tests;
