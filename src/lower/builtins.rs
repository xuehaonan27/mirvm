//! engine_builtins 内建符号大表（自 lower/mod.rs M7 整搬）：unwind/
//! atexit/signal/backtrace/x86 全族注册（llvm.x86.* → ir::Builtin 映射
//! 数据——rustc 类型耦合，记档留 lower 域，不进 arch/）。

use super::*;

/// 符号是 mangled 的——`mangle_internal_symbol`）。Special = 默认分配器（引擎接管）；
/// 非 special（自定义 #[global_allocator] 的 __rust_* → 用户 __rg_* 转发、
/// __rust_alloc_error_handler）暂不注册 → 走 ③ Trap 诊断。
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
    // unwind 原语（M4.2）：panic_unwind 照常解释，引擎在平台 unwinder 符号层接管
    out.insert(
        Symbol::intern("_Unwind_RaiseException"),
        ir::Builtin::UnwindRaise,
    );
    // 快路径直通（panic 链高频；其余 foreign 走通用 dlsym+libffi 道）
    out.insert(Symbol::intern("getenv"), ir::Builtin::HostGetenv);
    out.insert(Symbol::intern("write"), ir::Builtin::HostWrite);
    out.insert(Symbol::intern("strlen"), ir::Builtin::HostStrlen);
    out.insert(Symbol::intern("abort"), ir::Builtin::HostAbort);
    // fork（D8f）：builtin 守卫 guest 线程数后经 os::process::fork 直通（P7 os 层）。
    out.insert(Symbol::intern("fork"), ir::Builtin::HostFork);
    // atexit 家族（D8g）：glibc 不导出 `atexit` 供 guest dlsym → builtin 接管。
    out.insert(Symbol::intern("atexit"), ir::Builtin::HostAtexit);
    out.insert(Symbol::intern("__cxa_atexit"), ir::Builtin::HostCxaAtexit);
    out.insert(Symbol::intern("on_exit"), ir::Builtin::HostOnExit);
    out.insert(Symbol::intern("syscall"), ir::Builtin::HostSyscall);
    // signal/sigaction 的 handler 藏在整数/结构体中，不能由通用 FFI fn-ptr 参数
    // thunk 化；而且 signal trampoline 必须异步信号安全，普通 libffi closure 不满足。
    // 明确 Trap，直到有专用实现。其余旧 StubZero 项改走 dlsym+libffi；
    // atexit/dl_iterate_phdr 的显式 fn-ptr 参数可由 M4.4 thunk 工厂处理。
    out.insert(Symbol::intern("signal"), ir::Builtin::HostSignal);
    out.insert(Symbol::intern("sigaction"), ir::Builtin::HostSigaction);
    // 宿主 unwinder 从 libffi/解释器的 native stack 取回 IP，无法代表
    // guest 的冻结函数条目。回调 thunk 只解决调用方向，不会翻译栈帧；
    // 所以在 guest-frame/IP 映射完成前必须明确拒绝，不能返回貌似成功
    // 的宿主 backtrace。
    // `_Unwind_RaiseException` / `_Unwind_DeleteException` 上面有 guest 专用语义；
    // 其余 libgcc context/stack API 若直通，看到的只会是宿主解释器帧。
    // 整组显式 deny，避免从 GetIPInfo/CFA/LSDA 等旁路重新引入静默错值。
    // 仍拒绝的 unwinder context/state API：直通看到的只是宿主解释器帧，无 guest
    // 语义。整组显式 deny，避免从 CFA/LSDA/SetGR 等旁路重新引入静默错值。
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
            ir::Builtin::Unsupported(ir::StaticStr(name)),
        );
    }
    // backtrace 影子帧（D8e）：这四个由 Ctx 影子帧栈诚实回答（IP=合成 fn token）。
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
    // GetCFA：backtrace 用作帧的 sp 身份（去重/相等）。合成帧无真 CFA，返回该帧
    // synth IP 作唯一 sp 替身（每帧不同即满足身份用途）。
    out.insert(Symbol::intern("_Unwind_GetCFA"), ir::Builtin::UnwindGetIp);
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
    // 族⑨ LDDQU（c_tantivy bitpacking/termdict 列值读取派发点）：ldu.dq 纯 load
    out.insert(
        Symbol::intern("llvm.x86.sse3.ldu.dq"),
        ir::Builtin::X86Lddqu128,
    );
    out.insert(
        Symbol::intern("llvm.x86.avx.ldu.dq.256"),
        ir::Builtin::X86Lddqu256,
    );
    // 族⑧ F16C（half 2.x 运行期探测后的 f16c 通道）
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
    // 族⑨ packed-f32（tiny-skia simd 默认路径；rcp/rsqrt 有意不注册——
    // 硬件近似不可便携复现，保持响亮 trap，见 m9 报告）
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

