//! engine_builtins built-in symbol table (moved wholesale from lower/mod.rs M7): unwind/
//! atexit/signal/backtrace/x86 full family registration (llvm.x86.* → ir::Builtin mapping
//! data—tied to rustc types, documented in the lower domain, not in arch/).

use super::*;

/// Symbols are mangled (`mangle_internal_symbol`). Special = default allocator (engine-managed).
/// Non-special entries are generated and resolved by rustc inside the final crate, not rewritten at the std external-declaration boundary;
/// the four ordinary `__rust_*` entries for a custom #[global_allocator] are routed uniformly at the Module level.
pub(super) fn engine_builtins(tcx: TyCtxt<'_>) -> FxHashMap<Symbol, ir::Builtin> {
    use rustc_ast::expand::allocator::{self, SpecialAllocatorMethod as S};
    use rustc_symbol_mangling::mangle_internal_symbol;

    let mut out = FxHashMap::default();
    if let Some(kind) = tcx.allocator_kind(()) {
        for method in rustc_codegen_ssa::base::allocator_shim_contents(tcx, kind) {
            let Some(special) = method.special else {
                continue;
            };
            let b = match special {
                S::Alloc => ir::Builtin::RustAlloc,
                S::Dealloc => ir::Builtin::RustDealloc,
                S::Realloc => ir::Builtin::RustRealloc,
                S::AllocZeroed => ir::Builtin::RustAllocZeroed,
            };
            let sym = mangle_internal_symbol(tcx, &allocator::global_fn_name(method.name));
            out.insert(Symbol::intern(&sym), b);
        }
    }
    let sentinel =
        mangle_internal_symbol(tcx, rustc_ast::expand::allocator::NO_ALLOC_SHIM_IS_UNSTABLE);
    out.insert(Symbol::intern(&sentinel), ir::Builtin::NoAllocShim);
    // unwind primitive (M4.2): panic_unwind is still interpreted; the engine takes over at the platform unwinder symbol layer
    out.insert(
        Symbol::intern("_Unwind_RaiseException"),
        ir::Builtin::UnwindRaise,
    );
    // fast-path passthrough (frequent in panic chains; remaining foreign symbols use the generic dlsym+libffi path)
    out.insert(Symbol::intern("getenv"), ir::Builtin::HostGetenv);
    out.insert(Symbol::intern("write"), ir::Builtin::HostWrite);
    out.insert(Symbol::intern("strlen"), ir::Builtin::HostStrlen);
    out.insert(Symbol::intern("abort"), ir::Builtin::HostAbort);
    // fork (D8f): the builtin guards the guest thread count then passes through to os::process::fork (P7 os layer).
    out.insert(Symbol::intern("fork"), ir::Builtin::HostFork);
    // atexit family (D8g): glibc does not export `atexit` for guest dlsym → builtin takes over.
    out.insert(Symbol::intern("atexit"), ir::Builtin::HostAtexit);
    out.insert(Symbol::intern("__cxa_atexit"), ir::Builtin::HostCxaAtexit);
    out.insert(Symbol::intern("on_exit"), ir::Builtin::HostOnExit);
    out.insert(Symbol::intern("syscall"), ir::Builtin::HostSyscall);
    // signal/sigaction handlers are hidden inside integers/structs and cannot be thunked by the
    // generic FFI fn-ptr parameter path; moreover, the signal trampoline must be async-signal-safe,
    // which ordinary libffi closures are not. Explicitly Trap until a dedicated implementation exists.
    // Remaining old StubZero items go through dlsym+libffi; explicit fn-ptr arguments for
    // atexit/dl_iterate_phdr can be handled by the M4.4 thunk factory.
    out.insert(Symbol::intern("signal"), ir::Builtin::HostSignal);
    out.insert(Symbol::intern("raise"), ir::Builtin::HostRaise);
    out.insert(Symbol::intern("sigaction"), ir::Builtin::HostSigaction);
    // The host unwinder retrieves IPs from the libffi/interpreter native stack and cannot represent
    // guest frozen function entries. Callback thunks only resolve the call direction, not stack frames;
    // therefore, until guest-frame/IP mapping is done, explicitly reject rather than return a
    // seemingly-successful host backtrace.
    // `_Unwind_RaiseException` / `_Unwind_DeleteException` have guest-specific semantics above;
    // remaining libgcc context/stack APIs, if passed through, would see only host interpreter frames.
    // Deny the whole group explicitly to avoid reintroducing silently-wrong values via GetIPInfo/CFA/LSDA.
    // Still-denied unwinder context/state APIs: passthrough would see only host interpreter frames,
    // with no guest semantics. Deny the whole group explicitly to avoid silently-wrong values via CFA/LSDA/SetGR.
    for name in [
        "_Unwind_Find_FDE",
        "_Unwind_ForcedUnwind",
        "_Unwind_GetDataRelBase",
        "_Unwind_GetGR",
        "_Unwind_GetLanguageSpecificData",
        "_Unwind_GetRegionStart",
        "_Unwind_GetTextRelBase",
        "_Unwind_Resume",
        "_Unwind_Resume_or_Rethrow",
        "_Unwind_SetGR",
        "_Unwind_SetIP",
    ] {
        out.insert(
            Symbol::intern(name),
            ir::Builtin::Unsupported(ir::StaticStr(name.into())),
        );
    }
    // backtrace shadow frames (D8e): these four are answered honestly from the Ctx shadow frame stack (IP = synthetic fn token).
    out.insert(
        Symbol::intern("_Unwind_Backtrace"),
        ir::Builtin::UnwindBacktrace,
    );
    out.insert(Symbol::intern("_Unwind_GetIP"), ir::Builtin::UnwindGetIp);
    out.insert(
        Symbol::intern("_Unwind_GetIPInfo"),
        ir::Builtin::UnwindGetIpInfo,
    );
    out.insert(
        Symbol::intern("_Unwind_FindEnclosingFunction"),
        ir::Builtin::UnwindFindEnclosing,
    );
    out.insert(Symbol::intern("_Unwind_GetCFA"), ir::Builtin::UnwindGetCfa);
    out.insert(
        Symbol::intern("_Unwind_DeleteException"),
        ir::Builtin::UnwindDeleteException,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.pause"),
        ir::Builtin::CpuHintNop,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.vzeroupper"),
        ir::Builtin::CpuHintNop,
    );
    out.insert(
        Symbol::intern("llvm.x86.addcarry.64"),
        ir::Builtin::AddCarry64,
    );
    out.insert(
        Symbol::intern("llvm.x86.subborrow.64"),
        ir::Builtin::SubBorrow64,
    );
    out.insert(Symbol::intern("llvm.x86.xgetbv"), ir::Builtin::Xgetbv);
    out.insert(
        Symbol::intern("llvm.x86.ssse3.pshuf.b.128"),
        ir::Builtin::X86Pshufb128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.pshuf.b"),
        ir::Builtin::X86Pshufb256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256msg1"),
        ir::Builtin::X86Sha256Msg1,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256msg2"),
        ir::Builtin::X86Sha256Msg2,
    );
    out.insert(
        Symbol::intern("llvm.x86.sha256rnds2"),
        ir::Builtin::X86Sha256Rnds2,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.psad.bw"),
        ir::Builtin::X86PsadBw128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.psad.bw"),
        ir::Builtin::X86PsadBw256,
    );
    out.insert(
        Symbol::intern("llvm.x86.pclmulqdq"),
        ir::Builtin::X86Pclmulqdq,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesenc"),
        ir::Builtin::X86AesEnc,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesenclast"),
        ir::Builtin::X86AesEncLast,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesdec"),
        ir::Builtin::X86AesDec,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesdeclast"),
        ir::Builtin::X86AesDecLast,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aesimc"),
        ir::Builtin::X86AesImc,
    );
    out.insert(
        Symbol::intern("llvm.x86.aesni.aeskeygenassist"),
        ir::Builtin::X86AesKeygenAssist,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.32.8"),
        ir::Builtin::X86Crc32U8,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.32.16"),
        ir::Builtin::X86Crc32U16,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.32.32"),
        ir::Builtin::X86Crc32U32,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse42.crc32.64.64"),
        ir::Builtin::X86Crc32U64,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.permd"),
        ir::Builtin::X86Permd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.gather.q.pd.256"),
        ir::Builtin::X86GatherQPd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.gather.d.pd.256"),
        ir::Builtin::X86GatherDPd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52l.uq.128"),
        ir::Builtin::X86Pmadd52Lo128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52h.uq.128"),
        ir::Builtin::X86Pmadd52Hi128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52l.uq.256"),
        ir::Builtin::X86Pmadd52Lo256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52h.uq.256"),
        ir::Builtin::X86Pmadd52Hi256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52l.uq.512"),
        ir::Builtin::X86Pmadd52Lo512,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx512.vpmadd52h.uq.512"),
        ir::Builtin::X86Pmadd52Hi512,
    );
    out.insert(
        Symbol::intern("llvm.x86.ssse3.pmadd.ub.sw.128"),
        ir::Builtin::X86PmaddUbSw128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.pmadd.ub.sw"),
        ir::Builtin::X86PmaddUbSw256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.pmadd.wd"),
        ir::Builtin::X86PmaddWd128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx2.pmadd.wd"),
        ir::Builtin::X86PmaddWd256,
    );
    // Family ⑨ LDDQU (c_tantivy bitpacking/termdict column-value read dispatch): ldu.dq pure load
    out.insert(
        Symbol::intern("llvm.x86.sse3.ldu.dq"),
        ir::Builtin::X86Lddqu128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.ldu.dq.256"),
        ir::Builtin::X86Lddqu256,
    );
    // Family ⑧ F16C (f16c path after half 2.x runtime detection)
    out.insert(
        Symbol::intern("llvm.x86.vcvtps2ph.128"),
        ir::Builtin::X86Cvtps2ph128,
    );
    out.insert(
        Symbol::intern("llvm.x86.vcvtph2ps.128"),
        ir::Builtin::X86Cvtph2ps128,
    );
    out.insert(
        Symbol::intern("llvm.x86.vcvtps2ph.256"),
        ir::Builtin::X86Cvtps2ph256,
    );
    out.insert(
        Symbol::intern("llvm.x86.vcvtph2ps.256"),
        ir::Builtin::X86Cvtph2ps256,
    );
    // Family ⑨ packed-f32 (tiny-skia simd default path; rcp/rsqrt intentionally not registered—
    // hardware approximations are not reproducibly portable, kept as loud trap, see m9 report)
    out.insert(
        Symbol::intern("llvm.x86.sse.max.ps"),
        ir::Builtin::X86MaxPs128,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse.min.ps"),
        ir::Builtin::X86MinPs128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.max.ps.256"),
        ir::Builtin::X86MaxPs256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.min.ps.256"),
        ir::Builtin::X86MinPs256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse.cmp.ps"),
        ir::Builtin::X86CmpPs128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.cmp.ps.256"),
        ir::Builtin::X86CmpPs256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.cmp.pd"),
        ir::Builtin::X86CmpPd128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.cmp.pd.256"),
        ir::Builtin::X86CmpPd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.max.pd"),
        ir::Builtin::X86MaxPd128,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.min.pd"),
        ir::Builtin::X86MinPd128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.max.pd.256"),
        ir::Builtin::X86MaxPd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.min.pd.256"),
        ir::Builtin::X86MinPd256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.max.sd"),
        ir::Builtin::X86MaxSd,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.min.sd"),
        ir::Builtin::X86MinSd,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse41.round.ps"),
        ir::Builtin::X86RoundPs128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.round.ps.256"),
        ir::Builtin::X86RoundPs256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.cvtps2dq"),
        ir::Builtin::X86CvtPs2dq128,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.cvttps2dq"),
        ir::Builtin::X86CvttPs2dq128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.cvt.ps2dq.256"),
        ir::Builtin::X86CvtPs2dq256,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.cvtt.ps2dq.256"),
        ir::Builtin::X86CvttPs2dq256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse41.blendvps"),
        ir::Builtin::X86BlendvPs128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.blendv.ps.256"),
        ir::Builtin::X86BlendvPs256,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.psll.d"),
        ir::Builtin::X86PsllD128,
    );
    out.insert(
        Symbol::intern("llvm.x86.sse2.psrl.d"),
        ir::Builtin::X86PsrlD128,
    );
    out
}
