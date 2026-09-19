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

use super::admit::{CalleeAbi, admit, callee_abi};
use super::frame::analyze_frame;
use super::helpers::_Unwind_Resume;
use super::helpers::*;
use super::translate::Translator;
use super::*;

mod isa;
pub(crate) use isa::*;

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

fn worker(shared: std::sync::Arc<Shared>, rx: Receiver<u32>, domain: CodeDomain) {
    let dbg = std::env::var_os("MIRVM_JIT_DEBUG").is_some();
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

impl<'a> Compiler<'a> {
    /// Build a compiler for an explicit code domain. The plain domain is what every
    /// production path uses; the trace domain's semantics are exercised by tests
    /// before it is wired to activation entry.
    fn with_domain(shared: &'a Shared, domain: CodeDomain) -> Self {
        // MIRVM_JIT_STATS=1 turns on helper call-frequency counters; process-wide, once.
        stat_init();
        let isa = domain_isa(domain);
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("mirvm_c2i", mirvm_c2i as *const u8);
        jb.symbol("mirvm_call_main_catch", mirvm_call_main_catch as *const u8);
        jb.symbol("mirvm_jit_unreachable", mirvm_jit_unreachable as *const u8);
        jb.symbol("mirvm_jit_div_zero", mirvm_jit_div_zero as *const u8);
        jb.symbol("mirvm_volatile_load", mirvm_volatile_load as *const u8);
        jb.symbol("mirvm_volatile_store", mirvm_volatile_store as *const u8);
        jb.symbol("mirvm_call_indirect", mirvm_call_indirect as *const u8);
        jb.symbol(
            "mirvm_jit_terminate_abort",
            mirvm_jit_terminate_abort as *const u8,
        );
        jb.symbol("mirvm_call_terminate", mirvm_call_terminate as *const u8);
        jb.symbol(
            "mirvm_exception_is_engine_fault",
            mirvm_exception_is_engine_fault as *const u8,
        );
        jb.symbol("_Unwind_Resume", _Unwind_Resume as *const u8);
        jb.symbol("mirvm_jit_trap", mirvm_jit_trap as *const u8);
        jb.symbol("mirvm_simd_stmt", mirvm_simd_stmt as *const u8);
        jb.symbol("mirvm_simd_rv", mirvm_simd_rv as *const u8);
        jb.symbol("mirvm_tls_ref", mirvm_tls_ref as *const u8);
        jb.symbol("mirvm_call_foreign", mirvm_call_foreign as *const u8);
        jb.symbol("mirvm_call_builtin", mirvm_call_builtin as *const u8);
        jb.symbol("mirvm_alloc", mirvm_alloc as *const u8);
        jb.symbol("mirvm_jit_stack_guard", mirvm_jit_stack_guard as *const u8);
        jb.symbol("mirvm_poll_signals", mirvm_poll_signals as *const u8);
        jb.symbol(
            "mirvm_host_syscall_trace",
            mirvm_host_syscall_trace as *const u8,
        );
        // Wide-float (f128/f16) helper symbols.
        jb.symbol("mirvm_bin128_ovf", mirvm_bin128_ovf as *const u8);
        jb.symbol("mirvm_bin128_divrem", mirvm_bin128_divrem as *const u8);
        jb.symbol("mirvm_f128_bin", mirvm_f128_bin as *const u8);
        jb.symbol("mirvm_f128_cmp", mirvm_f128_cmp as *const u8);
        jb.symbol("mirvm_f128_un", mirvm_f128_un as *const u8);
        jb.symbol("mirvm_f128_math", mirvm_f128_math as *const u8);
        jb.symbol(
            "mirvm_f128_from_scalar",
            mirvm_f128_from_scalar as *const u8,
        );
        jb.symbol("mirvm_f128_to_scalar", mirvm_f128_to_scalar as *const u8);
        jb.symbol("mirvm_f128_from_wide", mirvm_f128_from_wide as *const u8);
        jb.symbol("mirvm_f128_to_wide", mirvm_f128_to_wide as *const u8);
        jb.symbol("mirvm_float_to_wide", mirvm_float_to_wide as *const u8);
        jb.symbol("mirvm_wide_to_f16", mirvm_wide_to_f16 as *const u8);
        jb.symbol("mirvm_wide_to_f32", mirvm_wide_to_f32 as *const u8);
        jb.symbol("mirvm_wide_to_f64", mirvm_wide_to_f64 as *const u8);
        jb.symbol("mirvm_f16_bin", mirvm_f16_bin as *const u8);
        jb.symbol("mirvm_f16_cmp", mirvm_f16_cmp as *const u8);
        jb.symbol("mirvm_f16_neg", mirvm_f16_neg as *const u8);
        jb.symbol("mirvm_f16_cast", mirvm_f16_cast as *const u8);
        jb.symbol("mirvm_f16_to_int", mirvm_f16_to_int as *const u8);
        jb.symbol("mirvm_f16_from_int", mirvm_f16_from_int as *const u8);
        jb.symbol("mirvm_f16_math_un", mirvm_f16_math_un as *const u8);
        jb.symbol("mirvm_f16_math_bin", mirvm_f16_math_bin as *const u8);
        jb.symbol("mirvm_f16_fma", mirvm_f16_fma as *const u8);
        jb.symbol("memmove", crate::os::process::memmove_addr());
        jb.symbol("memset", crate::os::process::memset_addr());
        jb.symbol("memcmp", crate::os::process::memcmp_addr());
        for (n, p) in libm_syms() {
            jb.symbol(n, p as *const u8);
        }
        let mut module = JITModule::new(jb);

        let mut sig_c2i = module.make_signature();
        for _ in 0..4 {
            sig_c2i.params.push(AbiParam::new(types::I64));
        }
        let c2i = module
            .declare_function("mirvm_c2i", Linkage::Import, &sig_c2i)
            .unwrap();
        let call_main_catch = module
            .declare_function("mirvm_call_main_catch", Linkage::Import, &sig_c2i)
            .unwrap();
        let mut sig_unr = module.make_signature();
        sig_unr.params.push(AbiParam::new(types::I64));
        let unreachable = module
            .declare_function("mirvm_jit_unreachable", Linkage::Import, &sig_unr)
            .unwrap();
        // memmove(d, s, n) -> d; memset(d, c, n) -> d (the Copy/frame-zeroing channel).
        let mut sig_mm = module.make_signature();
        for _ in 0..3 {
            sig_mm.params.push(AbiParam::new(types::I64));
        }
        sig_mm.returns.push(AbiParam::new(types::I64));
        let memmove = module
            .declare_function("memmove", Linkage::Import, &sig_mm)
            .unwrap();
        let memset = module
            .declare_function("memset", Linkage::Import, &sig_mm)
            .unwrap();
        // memcmp(s1, s2, n) -> c_int, so the return must be declared i32: an I64
        // return makes Cranelift feed a sextend.i64 to the verifier and reject the
        // function.
        let mut sig_memcmp = module.make_signature();
        for _ in 0..3 {
            sig_memcmp.params.push(AbiParam::new(types::I64));
        }
        sig_memcmp.returns.push(AbiParam::new(types::I32));
        let memcmp = module
            .declare_function("memcmp", Linkage::Import, &sig_memcmp)
            .unwrap();
        let div_zero = module
            .declare_function("mirvm_jit_div_zero", Linkage::Import, &sig_unr)
            .unwrap();
        let volatile_load = module
            .declare_function("mirvm_volatile_load", Linkage::Import, &sig_mm)
            .unwrap();
        let volatile_store = module
            .declare_function("mirvm_volatile_store", Linkage::Import, &sig_mm)
            .unwrap();
        // Call helpers; the bodies in helpers.rs mirror interp's dispatch and lazy
        // materialization.
        let mut sig_ci = module.make_signature();
        for _ in 0..8 {
            sig_ci.params.push(AbiParam::new(types::I64));
        }
        let call_indirect = module
            .declare_function("mirvm_call_indirect", Linkage::Import, &sig_ci)
            .unwrap();
        let mut sig_tls = module.make_signature();
        sig_tls.params.push(AbiParam::new(types::I64));
        sig_tls.returns.push(AbiParam::new(types::I64));
        let tls_ref = module
            .declare_function("mirvm_tls_ref", Linkage::Import, &sig_tls)
            .unwrap();
        // mirvm_call_foreign takes eight params: sp/sl/sg/ap/nv/ret_dst/fv plus the
        // terminate flag. This declaration must match the helper exactly: a mismatch
        // makes the verifier reject every function containing a CallForeign.
        let mut sig_cf = module.make_signature();
        for _ in 0..8 {
            sig_cf.params.push(AbiParam::new(types::I64));
        }
        sig_cf.returns.push(AbiParam::new(types::I64));
        let call_foreign = module
            .declare_function("mirvm_call_foreign", Linkage::Import, &sig_cf)
            .unwrap();
        // CallBuiltin helper: builtin pointer + av array + n + ret_dst + caller +
        // (lo,hi) out pointers + terminate flag + the call duty. The allocation fast
        // path takes six params and returns u64 directly.
        let mut sig_cb = module.make_signature();
        for _ in 0..8 {
            sig_cb.params.push(AbiParam::new(types::I64));
        }
        let call_builtin = module
            .declare_function("mirvm_call_builtin", Linkage::Import, &sig_cb)
            .unwrap();
        let mut sig_alloc = module.make_signature();
        for _ in 0..6 {
            sig_alloc.params.push(AbiParam::new(types::I64));
        }
        sig_alloc.returns.push(AbiParam::new(types::I64));
        let alloc = module
            .declare_function("mirvm_alloc", Linkage::Import, &sig_alloc)
            .unwrap();
        // TerminateAbort pure helper, the Terminate boundary called directly, and
        // _Unwind_Resume (the Resume terminator reaches it through exception_slot).
        let sig_ta = module.make_signature();
        let terminate_abort = module
            .declare_function("mirvm_jit_terminate_abort", Linkage::Import, &sig_ta)
            .unwrap();
        let mut sig_ct = module.make_signature();
        for _ in 0..4 {
            sig_ct.params.push(AbiParam::new(types::I64));
        }
        let call_terminate = module
            .declare_function("mirvm_call_terminate", Linkage::Import, &sig_ct)
            .unwrap();
        let mut sig_ur = module.make_signature();
        sig_ur.params.push(AbiParam::new(types::I64));
        let unwind_resume = module
            .declare_function("_Unwind_Resume", Linkage::Import, &sig_ur)
            .unwrap();
        let mut sig_efi = module.make_signature();
        sig_efi.params.push(AbiParam::new(types::I64));
        sig_efi.returns.push(AbiParam::new(types::I64));
        let exception_is_engine_fault = module
            .declare_function("mirvm_exception_is_engine_fault", Linkage::Import, &sig_efi)
            .unwrap();
        // Trap helper: reason pointer/length + func (u64::MAX marks the statement form).
        let mut sig_tr = module.make_signature();
        for _ in 0..3 {
            sig_tr.params.push(AbiParam::new(types::I64));
        }
        let trap = module
            .declare_function("mirvm_jit_trap", Linkage::Import, &sig_tr)
            .unwrap();
        // Unified SIMD/wide statement helper (7 params, 1 return) and SIMD rvalue
        // helper (2 params, 1 return): thin shells that re-match and call interp's
        // shared simd_exec body.
        let mut sig_ss = module.make_signature();
        for _ in 0..7 {
            sig_ss.params.push(AbiParam::new(types::I64));
        }
        sig_ss.returns.push(AbiParam::new(types::I64));
        let simd_stmt = module
            .declare_function("mirvm_simd_stmt", Linkage::Import, &sig_ss)
            .unwrap();
        let mut sig_sr = module.make_signature();
        for _ in 0..2 {
            sig_sr.params.push(AbiParam::new(types::I64));
        }
        sig_sr.returns.push(AbiParam::new(types::I64));
        let simd_rv = module
            .declare_function("mirvm_simd_rv", Linkage::Import, &sig_sr)
            .unwrap();
        let mut sig_sg = module.make_signature();
        sig_sg.params.push(AbiParam::new(types::I64));
        sig_sg.params.push(AbiParam::new(types::I64));
        let stack_guard = module
            .declare_function("mirvm_jit_stack_guard", Linkage::Import, &sig_sg)
            .unwrap();
        let poll_signals = module
            .declare_function(
                "mirvm_poll_signals",
                Linkage::Import,
                &module.make_signature(),
            )
            .unwrap();
        // Trace domain syscall site helper: producer / nr / args / n, plus an out
        // param returning the producer actually used, to a result. Only trace bodies
        // call it; the plain domain never emits this import.
        let mut sig_hst = module.make_signature();
        for _ in 0..5 {
            sig_hst.params.push(AbiParam::new(types::I64));
        }
        sig_hst.returns.push(AbiParam::new(types::I64));
        let host_syscall_trace = module
            .declare_function("mirvm_host_syscall_trace", Linkage::Import, &sig_hst)
            .unwrap();

        let mut compiler = Compiler {
            shared,
            domain,
            module,
            fbc: FunctionBuilderContext::new(),
            c2i,
            call_main_catch,
            unreachable,
            memmove,
            memset,
            memcmp,
            div_zero,
            volatile_load,
            volatile_store,
            call_indirect,
            tls_ref,
            call_foreign,
            call_builtin,
            alloc,
            terminate_abort,
            call_terminate,
            unwind_resume,
            exception_is_engine_fault,
            trap,
            simd_stmt,
            simd_rv,
            stack_guard,
            poll_signals,
            host_syscall_trace,
            pending_unwind: Vec::new(),
            #[cfg(test)]
            fail_after_symbol: None,
        };
        // The trace domain can only publish bodies once its boundary trampoline
        // exists: a trace body reads the recorder from the pinned register, so
        // entering one without the pin would read garbage. Installing it before
        // the first compile keeps that ordering true by construction.
        if domain == CodeDomain::Trace {
            compiler.install_trace_enter();
        }
        compiler
    }

    /// Define and publish the trace domain's boundary entry.
    ///
    /// Trace bodies address the recorder through the pinned register, which is
    /// not callee-saved under that convention, so `r15` must be installed on the
    /// way in and restored on the way out -- including when the call unwinds, or
    /// ABI-conforming Rust would get a clobbered callee-saved register back.
    /// This is the one place that happens; internal trace calls inherit the pin.
    ///
    /// If the definition fails nothing is published, and [`Compiler::compile`]
    /// refuses to produce a trace body, so the interpreter (which records through
    /// TLS) stays the fallback exactly as it does for any other compile failure.
    fn install_trace_enter(&mut self) {
        let Some(id) = self.define_trace_enter() else {
            return;
        };
        if self.module.finalize_definitions().is_err() {
            return;
        }
        self.register_pending_eh_frames();
        let addr = self.module.get_finalized_function(id) as u64;
        debug_assert_ne!(addr, 0, "a finalized trace entry has an address");
        self.shared.jit.trace_enter.store(addr, Ordering::Release);
    }

    fn define_trace_enter(&mut self) -> Option<ClifFuncId> {
        use cranelift_codegen::ir::{
            BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
        };
        let mut sig = self.module.make_signature();
        for _ in 0..4 {
            sig.params.push(AbiParam::new(types::I64));
        }
        let id = self
            .module
            .declare_function("mirvm_trace_enter", Linkage::Local, &sig)
            .ok()?;
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig.clone();
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let ps = b.block_params(entry).to_vec();
            let (producer, body, args, ret) = (ps[0], ps[1], ps[2], ps[3]);

            let saved = b.ins().get_pinned_reg(types::I64);
            b.ins().set_pinned_reg(producer);

            let pad = b.create_block();
            b.append_block_param(pad, types::I64);
            let ok = b.create_block();
            let normal = BlockCall::new(
                ok,
                std::iter::empty::<BlockArg>(),
                &mut b.func.dfg.value_lists,
            );
            let pad_call = b.func.dfg.block_call(pad, &[BlockArg::TryCallExn(0)]);
            let mut body_sig = self.module.make_signature();
            body_sig.params.push(AbiParam::new(types::I64));
            body_sig.params.push(AbiParam::new(types::I64));
            let sigref = b.func.import_signature(body_sig);
            let et = b.func.dfg.exception_tables.push(ExceptionTableData::new(
                sigref,
                normal,
                [ExceptionTableItem::Tag(
                    ExceptionTag::with_number(0).unwrap(),
                    pad_call,
                )],
            ));
            b.ins().try_call_indirect(body, &[args, ret], et);

            b.switch_to_block(ok);
            b.ins().set_pinned_reg(saved);
            b.ins().return_(&[]);

            b.switch_to_block(pad);
            let exn = b.block_params(pad)[0];
            b.ins().set_pinned_reg(saved);
            let resume = self.module.declare_func_in_func(self.unwind_resume, b.func);
            b.ins().call(resume, &[exn]);
            b.ins().trap(TrapCode::user(1).unwrap());

            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: trace boundary define failed: {e:#?}");
            }
            return None;
        }
        let ui = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())?;
        let lsda = build_lsda(&collect_call_sites(&cctx));
        self.pending_unwind.push((id, ui, Some(lsda)));
        self.module.clear_context(&mut cctx);
        Some(id)
    }

    fn fast_sig(&mut self, abi: CalleeAbi) -> Signature {
        let mut sig = self.module.make_signature();
        for _ in 0..abi.nparams {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..abi.nrets {
            sig.returns.push(AbiParam::new(types::I64));
        }
        sig
    }

    /// Compile one function that crossed the threshold. A rejected or failed request
    /// stays interpreted, silently.
    fn compile(&mut self, func: u32) {
        let jit = &self.shared.jit;
        if self.domain == CodeDomain::Trace && jit.trace_enter.load(Ordering::Acquire) == 0 {
            // Without the boundary pin a trace body has no recorder to read, so
            // it must not be built at all; interpretation keeps recording through
            // TLS and stays correct.
            return;
        }
        if jit.slots_for(self.domain).slots[func as usize].load(Ordering::Acquire) != 0 {
            return; // already compiled in this domain
        }
        let Some(body) = self.shared.module.funcs.get(func as usize) else {
            return;
        };
        if !admit(self.shared, body) {
            // Staying interpreted is the intended outcome for a non-admitted function,
            // not a failure; strict mode only records the set. Gated by MIRVM_JIT_DEBUG.
            if jit.sync && std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!(
                    "mirvm-jit-strict: f{func} not admitted ({})",
                    self.shared.module.funcs[func as usize].name
                );
            }
            return;
        }
        let abi = callee_abi(body).expect("admit already checked the shape");

        // Pre-warm the fast slots of PLT-visible callees: an uncompiled one gets a c2i
        // trampoline (fast shape, so its call sites keep a constant shape). Callees that
        // do not fit the fast shape are excluded; their call sites go straight to c2i
        // (cold path).
        let mut callees: Vec<(u32, CalleeAbi)> = Vec::new();
        for blk in &body.blocks {
            if let Terminator::Call {
                callee, args, ret, ..
            } = &blk.term
                && *callee != func
                && !callees.iter().any(|(c, _)| c == callee)
                && let Some(cabi) = callee_abi(&self.shared.module.funcs[*callee as usize])
                && cabi.nparams == args.len() + usize::from(matches!(ret, RetDest::Indirect(_)))
            {
                callees.push((*callee, cabi));
            }
        }
        for (c, cabi) in callees {
            if jit.slots_for(self.domain).slots_fast[c as usize].load(Ordering::Acquire) == 0
                && let Some((tramp, ranges)) = self.define_c2i_trampoline(c, cabi)
            {
                jit.publish_c2i_entry_for(self.domain, c, tramp as u64, ranges);
            }
        }

        // Silent-failure discipline: a compile failure keeps the function interpreted
        // and never writes a panic to stderr, because the differential oracle compares
        // stderr byte-for-byte and thread ids would pollute it. MIRVM_JIT_SYNC is the
        // exception: an admitted function that fails records the FAIL sentinel loudly.
        // TODO: report these errors through the log system once one exists.
        let Some((fast_id, fast_symbol)) = self.define_fast(func, body, abi) else {
            self.strict_fail(func);
            return;
        };
        let mut symbols = vec![fast_symbol];
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::FastBody) {
            return;
        }
        let Some((guarded_id, guarded_symbol)) = self.define_guarded_fast(func, body, abi, fast_id)
        else {
            self.strict_fail(func);
            return;
        };
        symbols.push(guarded_symbol);
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::Guarded) {
            return;
        }
        let Some((packed_id, packed_symbol)) = self.define_packed(func, body, abi, guarded_id)
        else {
            self.strict_fail(func);
            return;
        };
        symbols.push(packed_symbol);
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::Packed) {
            return;
        }
        if self.module.finalize_definitions().is_err() {
            self.strict_fail(func);
            return;
        }
        self.register_pending_eh_frames();
        let ranges = self.finalized_symbol_ranges(symbols);

        let fast = self.module.get_finalized_function(guarded_id) as u64;
        let packed = self.module.get_finalized_function(packed_id) as u64;
        // Publish order: fast first (self-recursion and other compiled callers reach
        // it), then packed (only the interpreter can enter compiled code through it).
        // Every memory range is complete before these two Release stores; the perf map
        // is written only by an explicit stop.
        jit.publish_compiled_entries_for(self.domain, func, fast, packed, ranges);
    }

    /// Published fast entry. Keeping the check in a separate slot-free
    /// function is important: putting it in `define_fast` would run only after
    /// Cranelift's prologue had already moved the native stack pointer.
    fn define_guarded_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("g{func}"), Linkage::Local, &sig)
            .ok()?;
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let guard = self.module.declare_func_in_func(self.stack_guard, b.func);
            let fv = b.ins().iconst(types::I64, func as i64);
            let explicit = u64::from(body.frame_size)
                .saturating_add(u64::from(body.frame_align.saturating_sub(16)));
            let frame = b.ins().iconst(types::I64, explicit as i64);
            b.ins().call(guard, &[fv, frame]);
            let target = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(target, &params);
            let results = b.inst_results(call).to_vec();
            b.ins().return_(&results);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: guarded entry define failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::Guarded,
                size,
            },
        ))
    }

    /// Strict verification mode (MIRVM_JIT_SYNC): an admitted function that fails to
    /// compile records the FAIL sentinel loudly, and the SYNC waiter aborts on it. The
    /// non-strict path never reaches here, so the silent-failure discipline holds.
    fn strict_fail(&self, func: u32) {
        let jit = &self.shared.jit;
        if jit.sync {
            eprintln!(
                "mirvm-jit-strict: f{func} ({}) meets compilation threshold but failed to be compiled",
                self.shared.module.funcs[func as usize].name
            );
            jit.slots_for(self.domain).slots[func as usize].store(FAIL_SENTINEL, Ordering::Release);
        }
    }

    /// c2i trampoline: fast signature, packs the arguments into a stack array, calls
    /// `mirvm_c2i` back into the interpreter. Any compile failure returns `None`; the
    /// caller then skips pre-warming this slot and stays interpreted, silently.
    fn define_c2i_trampoline(
        &mut self,
        target: u32,
        abi: CalleeAbi,
    ) -> Option<(*const u8, Vec<JitSymbolRange>)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("t{target}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let args_ss = b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                (abi.nparams.max(1) * 8) as u32,
                3,
            ));
            for (i, p) in params.iter().enumerate() {
                b.ins().stack_store(*p, args_ss, (i * 8) as i32);
            }
            let ret_ss =
                b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
            let fref = self.module.declare_func_in_func(self.c2i, b.func);
            let fv = b.ins().iconst(types::I64, target as i64);
            let ap = b.ins().stack_addr(types::I64, args_ss, 0);
            let nv = b.ins().iconst(types::I64, abi.nparams as i64);
            let rp = b.ins().stack_addr(types::I64, ret_ss, 0);
            b.ins().call(fref, &[fv, ap, nv, rp]);
            // mirvm_c2i always writes both the (lo, hi) slots (helpers.rs); read back
            // according to this callee's return shape.
            match abi.nrets {
                0 => {
                    b.ins().return_(&[]);
                }
                1 => {
                    let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                    b.ins().return_(&[lo]);
                }
                _ => {
                    let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = b.ins().stack_load(types::I64, ret_ss, 8);
                    b.ins().return_(&[lo, hi]);
                }
            }
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        let symbol = PendingJitSymbol {
            id,
            func: target,
            role: JitSymbolRole::C2i,
            size,
        };
        self.module.clear_context(&mut cctx);
        if self.module.finalize_definitions().is_err() {
            return None;
        }
        self.register_pending_eh_frames();
        let entry = self.module.get_finalized_function(id);
        let ranges = self.finalized_symbol_ranges(vec![symbol]);
        Some((entry, ranges))
    }

    /// Fast body: bytecode blocks -> CLIF, slots -> SSA variables under the "I64
    /// zero-extended to width" invariant. Any compile failure returns `None` and keeps
    /// the function interpreted: a panic must never pollute the stderr differential.
    fn define_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("f{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        // The Translator sets `has_try_call` while building; it is read outside the
        // builder to decide whether an LSDA is needed.
        let has_try_call;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let frame_offs = analyze_frame(body);
            let frame_ss = if !frame_offs.needs_frame() {
                None
            } else {
                Some(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    // A forced 0-byte frame materializes as 1 byte; the offset is still
                    // counted as 0, so nothing changes semantically.
                    // frame_align > 16: cranelift's x86_64 stack base only guarantees
                    // 16-byte alignment and there is no dynamic realignment, so the slot
                    // carries (align - 16) extra bytes and the translator aligns the
                    // entry in code as (addr + align - 1) & -align. A `__m256d` local
                    // reaching the 32-byte precondition of `mem::zeroed` is the case
                    // that motivates this.
                    if body.frame_align > 16 {
                        body.frame_size + (body.frame_align - 16)
                    } else {
                        body.frame_size.max(1)
                    },
                    body.frame_align.trailing_zeros() as u8,
                )))
            };
            let mut tr = Translator {
                shared: self.shared,
                domain: self.domain,
                module: &mut self.module,
                b: &mut b,
                vars: std::collections::HashMap::new(),
                frame_offs,
                frame_ss,
                unreachable: self.unreachable,
                c2i: self.c2i,
                call_main_catch: self.call_main_catch,
                memmove: self.memmove,
                memset: self.memset,
                memcmp: self.memcmp,
                div_zero: self.div_zero,
                volatile_load: self.volatile_load,
                volatile_store: self.volatile_store,
                call_indirect: self.call_indirect,
                tls_ref: self.tls_ref,
                call_foreign: self.call_foreign,
                call_builtin: self.call_builtin,
                alloc: self.alloc,
                terminate_abort: self.terminate_abort,
                call_terminate: self.call_terminate,
                unwind_resume: self.unwind_resume,
                exception_is_engine_fault: self.exception_is_engine_fault,
                trap: self.trap,
                simd_stmt: self.simd_stmt,
                simd_rv: self.simd_rv,
                poll_signals: self.poll_signals,
                host_syscall_trace: self.host_syscall_trace,
                exception_var: None,
                has_try_call: false,
                frame_base_var: None,
            };
            tr.build(func, body);
            has_try_call = tr.has_try_call;
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            if std::env::var_os("MIRVM_JIT_DEBUG_DUMP").is_some() {
                eprintln!(
                    "mirvm-jit-debug: CLIF dump of failed function f{func}:\n{}",
                    cctx.func.display()
                );
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            // A function with a try_call gets an LSDA, and it must list every call site,
            // handler-less ones included; build_lsda explains why.
            let lsda = if has_try_call {
                Some(build_lsda(&collect_call_sites(&cctx)))
            } else {
                None
            };
            self.pending_unwind.push((id, ui, lsda));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::FastBody,
                size,
            },
        ))
    }

    /// Packed entry `(args: *const u64, ret: *mut u64)`: one interp i2c hop.
    fn define_packed(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let _ = body;
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function(&format!("p{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let ps = b.block_params(entry).to_vec();
            let (argp, retp) = (ps[0], ps[1]);
            let mut args: Vec<Value> = Vec::with_capacity(abi.nparams);
            for i in 0..abi.nparams {
                args.push(
                    b.ins()
                        .load(types::I64, MemFlagsData::trusted(), argp, (i * 8) as i32),
                );
            }
            let fref = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(fref, &args);
            // Both (lo, hi) slots are always written, so shapes with sret/nrets = 0
            // store zero.
            let r0 = if abi.nrets >= 1 {
                b.inst_results(call)[0]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            let r1 = if abi.nrets >= 2 {
                b.inst_results(call)[1]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            b.ins().store(MemFlagsData::trusted(), r0, retp, 0);
            b.ins().store(MemFlagsData::trusted(), r1, retp, 8);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::Packed,
                size,
            },
        ))
    }

    /// FrameTable -> eh_frame bytes -> one whole-section registration. The registration
    /// entry keeps the bytes alive for the process lifetime, because the unwinder reads
    /// the shared CIE and each function's FDE out of them later.
    fn register_pending_eh_frames(&mut self) {
        if self.pending_unwind.is_empty() {
            return;
        }
        use gimli::RunTimeEndian;
        use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
        unsafe extern "C" {
            fn rust_eh_personality();
        }
        // Two CIEs: functions without a try_call use the plain CIE; the others use a
        // personality CIE whose personality is rust_eh_personality reached indirectly
        // through a DW.ref (lsda_encoding = absptr; embedding an absptr directly does
        // not work) with fde.lsda attached.
        PERS_REF.store(rust_eh_personality as *const u8 as u64, Ordering::SeqCst);
        let isa = self.module.isa();
        let mut table = FrameTable::default();
        let cie_plain = table.add_cie(isa.create_systemv_cie().expect("systemv cie"));
        let mut cie_pers = isa.create_systemv_cie().expect("systemv cie");
        cie_pers.lsda_encoding = Some(gimli::DW_EH_PE_absptr);
        cie_pers.personality = Some((
            gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_absptr.0),
            Address::Constant(&PERS_REF as *const std::sync::atomic::AtomicU64 as u64),
        ));
        let cie_pers_id = table.add_cie(cie_pers);
        for (id, ui, lsda) in self.pending_unwind.drain(..) {
            if let UnwindInfo::SystemV(info) = ui {
                let addr = self.module.get_finalized_function(id) as u64;
                match lsda {
                    Some(bytes) => {
                        let lsda_addr = bytes.as_ptr() as u64;
                        std::mem::forget(bytes); // the FDE's lsda pointer must outlive this call
                        let mut fde = info.to_fde(Address::Constant(addr));
                        fde.lsda = Some(Address::Constant(lsda_addr));
                        table.add_fde(cie_pers_id, fde);
                    }
                    None => {
                        table.add_fde(cie_plain, info.to_fde(Address::Constant(addr)));
                    }
                }
            }
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        super::register_eh_frame_section(eh.0.into_vec());
    }

    fn finalized_symbol_ranges(&self, symbols: Vec<PendingJitSymbol>) -> Vec<JitSymbolRange> {
        symbols
            .into_iter()
            .map(|pending| {
                let guest_name = &self.shared.module.funcs[pending.func as usize].name;
                let start = self.module.get_finalized_function(pending.id) as u64;
                JitSymbolRange::new(
                    self.shared.id,
                    pending.func,
                    pending.role,
                    start,
                    pending.size,
                    guest_name,
                )
            })
            .collect()
    }
}

/// DW.ref indirection cell for the personality CIE: one cell shared by every FDE in
/// the table.
static PERS_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ===== LSDA generation (layout copied line for line from the ABI; do not invent) =====

mod lsda;
pub(crate) use lsda::*;

#[cfg(test)]
mod tests;
