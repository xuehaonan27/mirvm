//! What a guest body is made of: the builtin vocabulary, the parameter and return conventions, the
//! call roles, and the statement, rvalue and terminator block that a function is.

use super::*;

/// Engine primitives: runtime extern boundaries that std declares itself. A native build synthesizes
/// shims for them at link time, and the engine takes over at the same boundary.
/// Lower emits a preceding `Stmt::Trap` at these sites so that a missing implementation fails loudly
/// instead of silently.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Builtin {
    /// `__rust_alloc(size, align) -> ptr`
    RustAlloc,
    /// `__rust_dealloc(ptr, size, align)`
    RustDealloc,
    /// `__rust_realloc(ptr, old_size, align, new_size) -> ptr`
    RustRealloc,
    /// `__rust_alloc_zeroed(size, align) -> ptr`
    RustAllocZeroed,
    /// `__rust_no_alloc_shim_is_unstable_v2()`: allocation sentinel, no-op
    NoAllocShim,
    /// `_Unwind_RaiseException(exc) -> !`: the unwind primitive. The host unwinder carries MIRVM's own
    /// exception and keeps the guest's exception pointer inside it, while the panic_unwind structures
    /// stay in guest heap and under the guest standard library's control.
    UnwindRaise,
    /// `catch_unwind(try_fn, data, catch_fn) -> i32` intrinsic (rust_try):
    /// raw unwinder catch + exception category / owning Engine classification + indirect call dispatch.
    /// Only guest panics from the current Engine are handed to `catch_fn`.
    CatchUnwind,
    /// Minimal os:: pass-through used by the panic chain, marshalling raw addresses without copying.
    HostGetenv,
    /// `write(fd, buf, len) -> isize`
    HostWrite,
    /// `strlen(s) -> usize`
    HostStrlen,
    /// `abort() -> !` (libc abort semantics; core::intrinsics::abort also flows here)
    HostAbort,
    /// `fork()`: allowed only while the guest is single-threaded, where the child is a full process
    /// copy and the interpreter state is naturally consistent; with more than one guest thread it is
    /// rejected loudly, because forking a multithreaded process is unsafe on native too. This is what
    /// makes Command::pre_exec and single-threaded daemonize work.
    HostFork,
    /// `atexit(fn)` / `__cxa_atexit(fn, arg, dso)` / `on_exit(fn, arg)`: register a guest exit
    /// callback. glibc does not export `atexit` for guest dlsym, so this is a builtin: the engine keeps
    /// a LIFO registry and, on the first registration, hooks a native trampoline through its own
    /// linked libc `atexit`. At process teardown the guest callbacks run interpreted, in LIFO order.
    /// Returns 0.
    HostAtexit,
    HostCxaAtexit,
    HostOnExit,
    /// `syscall(nr, ...) -> long` variadic passthrough (dispatched by actual argument count)
    HostSyscall,
    /// `signal(signum, handler)`: guest handler is registered through a stable kernel signal stub into
    /// the owning Engine inbox, then executed at a normal VM safepoint.
    HostSignal,
    /// `raise(signum)`: synchronous signal delivery for the current guest thread. Unlike asynchronous
    /// kernel delivery, the handler must finish before `raise` returns to preserve POSIX nesting order.
    HostRaise,
    /// `sigaction(signum, act, oldact)`: process-level registry keeps the guest-visible handler/mask/
    /// flags, and restores the previous disposition when the Engine shuts down.
    HostSigaction,
    /// A host boundary that must not be passed through. Reaching it fails explicitly rather than
    /// faking success. This covers boundaries that need their own async-signal-safe implementations
    /// and unwinder APIs that need guest frame/context translation, since the host interpreter state
    /// must not become visible to the guest.
    Unsupported(StaticStr),
    /// `_Unwind_DeleteException`: calls the cleanup callback stored in the exception object, per the
    /// Itanium ABI.
    UnwindDeleteException,
    /// `_Unwind_Backtrace(trace_fn, arg)` calls the guest trace_fn once per frame. The answers come
    /// from Ctx's shadow-frame stack, with the IP being a synthetic function token rather than a host
    /// address.
    UnwindBacktrace,
    /// `_Unwind_GetIP(ctx)` / `_Unwind_GetIPInfo(ctx, &ip_before)`: read the IP out of the synthetic
    /// context.
    UnwindGetIp,
    UnwindGetIpInfo,
    /// `_Unwind_GetCFA(ctx)`: read the frame stack position out of the synthetic context.
    UnwindGetCfa,
    /// `_Unwind_FindEnclosingFunction(ip)`: a synthetic IP is already the function entry, so this
    /// returns `ip` unchanged.
    UnwindFindEnclosing,
    /// A processor hint that changes neither the guest abstract machine nor RAM state (for example
    /// `pause` or `vzeroupper`). Since the interpreter does not persist host vector register state,
    /// ignoring it at runtime is correct.
    CpuHintNop,
    /// `core::intrinsics::breakpoint()`: executes a real int3, so the observable SIGTRAP behavior
    /// matches native (the process terminates by default when it is not being traced).
    Breakpoint,
    /// `llvm.x86.addcarry.64(carry, a, b) -> (carry, result)`, preserving the field order of LLVM's
    /// unadjusted intrinsic pair.
    AddCarry64,
    /// `llvm.x86.subborrow.64(borrow, a, b) -> (borrow, result)`.
    SubBorrow64,
    /// `llvm.x86.xgetbv(xcr) -> u64`: reads the real host extended control register.
    Xgetbv,
    /// x86 vector hardware intrinsics with no portable `simd_*` equivalent. Their arguments and return
    /// vectors still travel through the frozen bytecode's indirect place ABI; the executor helpers call
    /// the real host instructions.
    X86Pshufb128,
    X86Pshufb256,
    X86Sha256Msg1,
    X86Sha256Msg2,
    X86Sha256Rnds2,
    /// `llvm.x86.sse2.psad.bw(a, b)` (`_mm_sad_epu8`): the sum of absolute differences over each
    /// 8-byte group, placed as a u64 in qword lane 0 and 1 with the remaining bits cleared.
    X86PsadBw128,
    /// `llvm.x86.avx2.psad.bw(a, b)` (`_mm256_sad_epu8`): the same operation within each 128-bit lane,
    /// giving four u64 results.
    X86PsadBw256,
    /// `llvm.x86.pclmulqdq(a, b, imm8)` (`_mm_clmulepi64_si128`): bit 0 of imm8 selects which qword of
    /// `a` and bit 4 which qword of `b` feeds the 64x64 carryless multiply that produces 128 bits; the
    /// hardware ignores the remaining imm8 bits.
    X86Pclmulqdq,
    /// The AES-NI single-round family, 128-bit: `llvm.x86.aesni.aesenc(a, round_key)` and the enc-last,
    /// dec and dec-last forms.
    X86AesEnc,
    X86AesEncLast,
    X86AesDec,
    X86AesDecLast,
    /// `llvm.x86.aesni.aesimc(a)`: InvMixColumns, the round-key transformation used when decrypting.
    X86AesImc,
    /// `llvm.x86.aesni.aeskeygenassist(a, imm8)`: SubWord and RotWord, xored with RCON taken from
    /// imm8.
    X86AesKeygenAssist,
    /// `llvm.x86.sse42.crc32.32.8/16/32` and `.64.64` (`_mm_crc32_u8/16/32/64`), all carrying CRC32C
    /// hardware semantics: reflected polynomial 0x82F63B78, or 0xC96C5795D7870F42 for the 64-bit form,
    /// with no initial or final inversion (the wrapper performs any inversion). Uses the scalar
    /// channel.
    X86Crc32U8,
    X86Crc32U16,
    X86Crc32U32,
    X86Crc32U64,
    /// `llvm.x86.avx2.permd(a, idx)` (`_mm256_permutevar8x32_epi32`): a cross-lane dword permute,
    /// `dst.dword[i] = a.dword[idx.dword[i] & 7]`.
    X86Permd256,
    /// `llvm.x86.avx2.gather.q.pd.256(src, base, vindex, mask, scale)`: a per-lane conditional gather.
    /// A mask lane whose sign bit is set reads the f64 at `base + vindex * scale`; an unset lane copies
    /// the `src` lane instead. The unset lanes never touch memory, which is what suppresses faults.
    X86GatherQPd256,
    /// `llvm.x86.avx2.gather.d.pd.256`: as above, but `vindex` holds four i32 values that are
    /// sign-extended to 64 bits for the address arithmetic.
    X86GatherDPd256,
    /// `llvm.x86.avx512.vpmadd52l/h.uq.128/256/512(a, b, c)`: a 52-bit unsigned multiply-add where
    /// `dst.qword[i] = a[i] + (b[i][51:0] * c[i][51:0])`, taking bits 51:0 of the product for the `l`
    /// forms and bits 103:52 for the `h` forms. The addition wraps at 64 bits.
    X86Pmadd52Lo128,
    X86Pmadd52Hi128,
    X86Pmadd52Lo256,
    X86Pmadd52Hi256,
    X86Pmadd52Lo512,
    X86Pmadd52Hi512,
    /// `llvm.x86.ssse3.pmadd.ub.sw.128` / `llvm.x86.avx2.pmadd.ub.sw`
    /// (`_mm(256)_maddubs_epi16`): each unsigned byte of `a` times the signed byte of `b`, with the sum
    /// of adjacent products saturating to i16.
    X86PmaddUbSw128,
    X86PmaddUbSw256,
    /// `llvm.x86.sse2.pmadd.wd` / `llvm.x86.avx2.pmadd.wd` (`_mm(256)_madd_epi16`): the sum of
    /// adjacent i16 pair products, placed in an i32. The hardware defines the overflow case: a pair of
    /// i16::MIN products sums and wraps to i32::MIN.
    X86PmaddWd128,
    X86PmaddWd256,
    /// `llvm.x86.sse3.ldu.dq(p)` (`_mm_lddqu_si128`): an unaligned 16-byte pure load, bit-identical in
    /// semantics to loadu.
    X86Lddqu128,
    /// `llvm.x86.avx.ldu.dq.256(p)` (`_mm256_lddqu_si256`): the same operation over 32 bytes.
    X86Lddqu256,
    /// `llvm.x86.vcvtps2ph.128(a, rounding)` (`_mm_cvtps_ph`): f32x4 to f16x4, packed into the low 64
    /// bits with the high 64 cleared. In `rounding`, imm[2]=0 selects the rounding mode from imm[1:0]
    /// (0=RNE, 1=floor, 2=ceil, 3=trunc) and imm[2]=1 selects MXCSR.RC, where the engine always uses
    /// the default RNE. The software model is bit-identical to hardware, including the NaN case (quiet
    /// bit forced, payload shifted right 13 bits), overflow, subnormals and all four rounding modes.
    X86Cvtps2ph128,
    /// `llvm.x86.vcvtph2ps.128(a)` (`_mm_cvtph_ps`): the low 64 bits as f16x8 to f32x4, an exact
    /// expansion (quiet bit forced and payload shifted left 13 bits for NaN; subnormals exactly
    /// normalized).
    /// NOTE: recent stdarch implements `_mm_cvtph_ps` portably via simd_shuffle/simd_cast, taking the
    /// f16 lane path rather than this symbol; the symbol stays for older emit surfaces and direct
    /// calls.
    X86Cvtph2ps128,
    /// `llvm.x86.vcvtps2ph.256(a, rounding)` (`_mm256_cvtps_ph`): f32x8 to f16x8, returning 128 bits.
    /// Rounding works as in the .128 form.
    X86Cvtps2ph256,
    /// `llvm.x86.vcvtph2ps.256(a)` (`_mm256_cvtph_ps`): f16x8 to f32x8, an exact expansion.
    X86Cvtph2ps256,
    /// `llvm.x86.sse.max.ps(a, b)` and `.min` (`_mm_max_ps` / `_mm_min_ps`): `a > b ? a : b` and
    /// `a < b ? a : b`. An unordered comparison and a comparison of equal zeros both select the second
    /// source, and NaN passes through bit-identically, which is what makes this agree with Rust's
    /// scalar comparison.
    X86MaxPs128,
    X86MinPs128,
    /// `llvm.x86.avx.max.ps.256` / `.min`: the same semantics as the .128 form, per lane over f32x8.
    X86MaxPs256,
    X86MinPs256,
    /// `llvm.x86.sse.cmp.ps(a, b, imm8)` / `llvm.x86.avx.cmp.ps.256`: the full 32-predicate table
    /// (EQ/LT/LE/UNORD/NEQ/NLT/NLE/ORD and EQ_UQ/NGE/NGT/FALSE/NEQ_OQ/GE/GT/TRUE, in signalling and
    /// quiet variants; the two variants differ only in exception flags, never in value bits). A true
    /// lane becomes all 1s.
    X86CmpPs128,
    X86CmpPs256,
    /// `llvm.x86.sse2.cmp.pd` / `llvm.x86.avx.cmp.pd.256`: the same predicate table as cmp.ps, over f64
    /// lanes with a 64-bit mask.
    X86CmpPd128,
    X86CmpPd256,
    /// `llvm.x86.sse2.max.pd` / `min.pd` / `llvm.x86.avx.max.pd.256` / `min.pd.256`: the same max/min
    /// semantics as the ps forms, on f64 lanes.
    X86MaxPd128,
    X86MinPd128,
    X86MaxPd256,
    X86MinPd256,
    /// `llvm.x86.sse2.max.sd` / `min.sd`: scalar f64 max/min.
    X86MaxSd,
    X86MinSd,
    /// `llvm.x86.sse41.round.ps(a, imm8)` / `llvm.x86.avx.round.ps.256`: imm[1:0] selects the rounding
    /// mode (0=RNE, 1=floor, 2=ceil, 3=trunc), bit 2 makes it come from MXCSR (RNE in the engine) and
    /// bit 3 only suppresses exception flags. NaN keeps its payload with the quiet bit forced. This is
    /// handled by an explicit arm rather than libm or roundss because their NaN bit behaviour varies
    /// with the host build target.
    X86RoundPs128,
    X86RoundPs256,
    /// `llvm.x86.sse2.cvtps2dq(a)` (`_mm_cvtps_epi32`): f32 to i32 rounded by MXCSR.RC, which is RNE
    /// in the engine. NaN, out-of-range values and infinities all become 0x80000000, the indefinite
    /// integer.
    X86CvtPs2dq128,
    /// `llvm.x86.sse2.cvttps2dq(a)` (`_mm_cvttps_epi32`): as above but with truncating rounding.
    X86CvttPs2dq128,
    /// `llvm.x86.avx.cvt.ps2dq.256` / `.cvtt.ps2dq.256`: f32x8 versions of the two symbols above.
    X86CvtPs2dq256,
    X86CvttPs2dq256,
    /// `llvm.x86.sse41.blendvps(a, b, mask)` / `llvm.x86.avx.blendv.ps.256`: a mask lane whose sign bit
    /// is set picks `b`, a cleared one picks `a`. Pure bit selection, no arithmetic.
    X86BlendvPs128,
    X86BlendvPs256,
    /// `llvm.x86.sse2.psll.d(a, count)` (`_mm_sll_epi32`): a v4i32 logical left shift. The count is a
    /// single value in the low 64 bits of the vector operand; a count above 31 yields all zeros. The
    /// hardware reads only those low 64 bits and ignores the rest of the count vector.
    X86PsllD128,
    /// `llvm.x86.sse2.psrl.d(a, count)` (`_mm_srl_epi32`): a v4i32 logical right shift under the same
    /// count rule.
    X86PsrlD128,
    /// The form `HostSyscall` is rewritten to at the cold-create boundary of a capture-capable Module.
    /// The generic builtin helper recognises it, which keeps session checks out of the ordinary
    /// syscall and JIT paths.
    /// It is last in the enum so the postcard variant numbers of every existing variant stay
    /// unchanged.
    HostSyscallTrace,
}

/// Right-hand operand of Bin128: a 128-bit place, or a scalar of at most 64 bits (the shift amount for
/// Shl/Shr).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Bin128Rhs {
    Wide(PlaceExpr),
    Scalar(Operand),
}

/// Where an argument lands inside the callee frame. The engine calling convention flattens actuals
/// into a sequence of u64 slots.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum ParamAbi {
    /// ZST: occupies no argument slot
    Zst,
    /// Scalar: 1 slot
    Scalar(Slot),
    /// Scalar pair: 2 slots, the low and high frame-local slots whose offsets come from the frozen pair
    /// layout.
    Pair(Slot, Slot),
    /// Large aggregate: 1 slot = source real address; prologue memcpy `size` bytes to frame `off`
    Indirect { off: u32, size: u32 },
}

/// Return channel of the engine calling convention.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum RetAbi {
    Zst,
    /// Scalar: the interpreter returns the low slot.
    Scalar(Slot),
    /// Scalar pair: returns (lo, hi)
    Pair(Slot, Slot),
    /// Large aggregate: the caller prepends a hidden first argument holding the destination real
    /// address, and the callee's Return memcpys `size` bytes from the _0 slot to the slot that pointer
    /// names. The hidden pointer slot is appended at the frame tail (`sret_off`).
    Indirect {
        ret_off: u32,
        size: u32,
        sret_off: u32,
    },
}

/// Return landing point of a Call (caller side).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RetDest {
    /// Ignored (ZST or no destination)
    Ignore,
    Scalar(ScalarPlace),
    /// Two half landing points, whose places and frozen offsets and widths come from the pair layout.
    Pair(ScalarPlace, ScalarPlace),
    /// Large aggregate: the caller computes the destination real address and prepends it as the hidden
    /// first argument of the call.
    Indirect(PlaceExpr),
}

/// `SwitchInt` discriminant. Ordinary integers use a scalar operand; i128/u128 stay in a place and are
/// read as full 128 bits at runtime, not truncated to u64 first.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum SwitchDiscr {
    Scalar(Operand),
    Wide(PlaceExpr),
}

/// Role of a direct guest call in the startup chain. Almost every call is `Normal`; the one
/// `catch_unwind` call that wraps user `main` is marked `MainPanicBoundary` during lowering, so the
/// Engine can still keep a structured result after guest std has consumed the exception.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CallRole {
    #[default]
    Normal,
    MainPanicBoundary,
}

/// Role of an engine primitive call in the startup chain. Only the `std::intrinsics::catch_unwind`
/// call site that passes lowering's structural validation is marked `MainPanicCatcher`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BuiltinCallRole {
    #[default]
    Normal,
    MainPanicCatcher,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Terminator {
    Goto(Bb),
    SwitchInt {
        discr: SwitchDiscr,
        targets: Vec<(u128, Bb)>,
        otherwise: Bb,
    },
    Call {
        callee: FuncId,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
        #[serde(default)]
        role: CallRole,
    },
    /// Engine primitive call. It targets no guest function and therefore adds no call-graph edge.
    CallBuiltin {
        builtin: Builtin,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
        #[serde(default)]
        role: BuiltinCallRole,
    },
    /// Foreign pass-through: dlsym plus a direct libffi call using the frozen signature. Because guest
    /// pointers are host pointers, no marshalling is needed.
    CallForeign {
        sym: Box<str>,
        sig: ForeignSig,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
    },
    /// Indirect call, used for fn-ptrs and dyn virtual dispatch. The callee evaluates to a function
    /// entry real address, which is reverse-looked up through the instance's fn-address table.
    /// `--vm-stats` reachability cannot follow this edge.
    /// `null_ok`: vtable slot 0 of a dyn virtual drop may be null for types without Drop, and a null
    /// target is then a no-op.
    /// `native_sig` is the frozen signature at an `extern "C"` fn-ptr call site. A reverse-lookup miss
    /// means the guest holds native real code, such as a runtime dlsym result, and the call goes
    /// directly through libffi with this signature. None means the ABI is Rust or unclassifiable, and a
    /// miss diagnoses and exits.
    CallIndirect {
        callee: Operand,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
        null_ok: bool,
        native_sig: Option<ForeignSig>,
    },
    /// Inline asm site. `stub` indexes the instance's asm-stub address table, the real address of a `fn(*mut u8)`
    /// slot-buffer wrapper that the load phase produced by assembling and dlopening it.
    /// Execution stack-allocates a `buf_size` buffer, writes a slot per `ins`, calls the real address,
    /// then reads a destination per `outs`.
    /// Unwinding out of an inline asm site is unreachable: lower rejects MAY_UNWIND asm.
    InlineAsm {
        stub: AsmStubId,
        buf_size: u32,
        /// Buffer slot offset and input value channel. A scalar writes 8 bytes low in its slot; a
        /// vector byte channel copies the full width.
        ins: Vec<(u32, AsmIoVal)>,
        /// Buffer slot offset and output destination channel. A scalar reads 8 bytes low from its slot;
        /// a vector byte channel copies the full width.
        outs: Vec<(u32, AsmIoDst)>,
        target: Bb,
    },
    Return,
    Unreachable,
    /// Tail of a cleanup chain (MIR's UnwindResume), reaching only while a frame guard's Drop runs.
    /// Returning from there lets the host unwinder continue on its own, because interpreted frames use
    /// the same single native stack and need no VM-side coordination.
    Resume,
    /// MIR's UnwindTerminate: abort when reached.
    TerminateAbort,
    /// Placeholder standing in for a construct lower does not support.
    /// Lowering stays total over the collected set: an unknown construct never aborts lowering, it
    /// becomes a Trap in place, and only executed paths have to be trap-free. The diagnostic string
    /// names the construct that is still missing.
    Trap(Box<str>),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FuncBody {
    pub frame_size: u32,
    pub frame_align: u32,
    /// Return channel (_0).
    pub ret: RetAbi,
    /// Argument landing positions for _1 through _argc, in flattened argument order.
    pub params: Vec<ParamAbi>,
    /// Frame-local slot of the hidden trailing `&Location` argument of a `#[track_caller]` function.
    /// It is an ABI phantom parameter with no entry in `params`.
    pub caller_loc_off: Option<u32>,
    pub blocks: Vec<Block>,
    /// Symbol name, used for diagnostics.
    pub name: Box<str>,
}
