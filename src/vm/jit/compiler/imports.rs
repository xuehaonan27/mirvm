//! Everything the generated code is allowed to call: the libcall symbols a fresh `JITBuilder` is
//! given and the imported helper signatures the translator refers to by `ClifFuncId`.

use super::*;

impl<'a> Compiler<'a> {
    /// Build a compiler for an explicit code domain. The plain domain is what every
    /// production path uses; the trace domain's semantics are exercised by tests
    /// before it is wired to activation entry.
    pub(super) fn with_domain(shared: &'a Shared, domain: CodeDomain) -> Self {
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
        for (n, p) in math_symbols() {
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
        // shared semantics::simd body.
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
}
