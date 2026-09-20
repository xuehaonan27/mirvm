//! Instruction families and module container.
//!
//! Owns the typed instructions (`Rvalue`, `Stmt`, `Terminator`), the functions and blocks they
//! form, and the `Module` tables that carry a linked artifact: frozen-area fixups, entry stubs,
//! TLS slots, and the eager/lazy function table. The operand, width, and operation vocabulary
//! these are written in stays in the parent module.

use super::{
    AsmIoDst, AsmIoVal, AsmSite, AsmStubId, Bb, FuncId, LinkAddr, LoadMap, Operand, PlaceExpr,
    ScalarPlace, Slot, Stmt, UnwindAction,
};

/// Owned name of an unsupported builtin. Deserialization cannot produce a `&'static str`, so the name
/// is owned and released together with the Module.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StaticStr(pub Box<str>);

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

/// Category of one libffi argument or return value, frozen at lower time from the function signature's
/// layout.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FfiKind {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Ptr,
    Void,
    /// Pass-by-value aggregate, handed off in guest memory at its real address on both sides.
    /// Outbound: the libffi avalue points straight at guest memory, and libffi does the eightbyte
    /// register marshalling itself. Inbound: the closure's avalue points at the bytes and the marshaller
    /// maps them per the callee's ParamAbi, either passing the address for Indirect or reading values in
    /// declared field order for Scalar and Pair.
    Agg(FfiAgg),
}

/// Frozen layout of a pass-by-value aggregate, expanded from the rustc layout: fields in declared
/// order, with padding implied by their offsets.
/// An alignment of at most 8 is the construction boundary, because the result buffer is allocated with
/// 8-byte alignment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FfiAgg {
    pub size: u32,
    pub align: u32,
    pub fields: Vec<FfiField>,
}

/// One aggregate field: its offset and leaf. Nesting is recursive; ZST members are omitted, and padding
/// is implied by the surrounding size and offsets.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FfiField {
    pub off: u32,
    pub leaf: FfiLeaf,
}

/// An aggregate leaf: either a scalar or a nested aggregate, which is how a ScalarPair `{ptr, len}` or
/// an inner struct of the same shape appears.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FfiLeaf {
    Scalar(FfiKind),
    Agg(FfiAgg),
}

/// Right-hand operand of Bin128: a 128-bit place, or a scalar of at most 64 bits (the shift amount for
/// Shl/Shr).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Bin128Rhs {
    Wide(PlaceExpr),
    Scalar(Operand),
}

/// Guest TLS slot id: a dense index over `#[thread_local]` statics.
pub type TlsId = u32;

/// Guest TLS slot description, frozen at lower time. `template` is the real address of the initial
/// bytes in the frozen area, relocations included. A thread's first access heap-allocates `size` bytes
/// and copies the template.
/// NOTE: the destructor does not run yet; the instance is freed with the Ctx.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct TlsSlot {
    pub template: LinkAddr,
    pub size: u64,
    pub align: u32,
}

/// Frozen foreign signature. A variadic function freezes its trailing arguments from the call site's
/// actual arguments; `fixed` is how many leading parameters are fixed.
/// The Eq/Hash impls exist because this type keys the thunk cache that maps a function entry address
/// plus signature to the real thunk code address.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ForeignSig {
    pub args: Vec<FfiKind>,
    pub ret: FfiKind,
    /// Some(n) = variadic function, first n are fixed parameters (libffi prep_cif_var)
    pub fixed: Option<usize>,
    /// Positions holding an fn-ptr-typed argument, with the frozen signature of that fn ptr itself.
    /// At runtime the argument at such a position is a fn entry address: a reverse-lookup hit in
    /// `fn_addrs` is swapped for the thunk's real code, while NULL and already-native real code pass
    /// through untouched.
    /// An inner signature's `thunk_args` is always empty, so thunks do not nest.
    pub thunk_args: Vec<(usize, ForeignSig)>,
    /// The ABI's unwind attribute: false for an ordinary C boundary, true for a C-unwind boundary that
    /// lets exceptions through. Direct foreign calls, callbacks and native fn-ptrs all share this
    /// field.
    #[serde(default)]
    pub unwind: bool,
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
    /// entry real address, which is reverse-looked up through `Module.fn_addrs` to a FuncId.
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
    /// Inline asm site. `stub` indexes `Module.asm_stub_addrs`, the real address of a `fn(*mut u8)`
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

/// The function table has two ownership modes. Lower and images hold decoded bodies in an ordinary
/// `Vec`; a `.mirvm` package holds only the immutable byte snapshot taken at load time, the per-function
/// slice bounds, and on-demand publish slots, decoding bodies lazily in a background worker. Readers
/// see one interface either way, through `len`/`get`/`index`/`iter`.
pub struct FuncTable {
    storage: FuncStorage,
}

enum FuncStorage {
    Eager(Vec<FuncBody>),
    Lazy(LazyFuncs),
}

struct LazyFuncs {
    state: std::sync::Arc<DecodeState>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FuncBlob {
    pub start: usize,
    pub end: usize,
    pub expected_hash: u128,
}

struct DecodeState {
    map: std::sync::Arc<[u8]>,
    blobs: Box<[FuncBlob]>,
    cells: Box<[std::sync::OnceLock<Result<FuncBody, String>>]>,
    queue: std::sync::Mutex<DecodeQueue>,
    ready: std::sync::Condvar,
    done: std::sync::Condvar,
    access: std::sync::Mutex<(Vec<u32>, std::collections::BTreeSet<u32>)>,
    heat_path: std::path::PathBuf,
}

#[derive(Default)]
pub(super) struct DecodeQueue {
    pub(super) demand: std::collections::VecDeque<usize>,
    pub(super) predicted: std::collections::VecDeque<usize>,
    pub(super) queued: std::collections::BTreeSet<usize>,
    pub(super) stop: bool,
}

impl DecodeQueue {
    pub(super) fn request_demand(&mut self, index: usize) {
        if self.queued.insert(index) {
            self.demand.push_back(index);
        } else if let Some(at) = self.predicted.iter().position(|item| *item == index) {
            self.predicted.remove(at);
            self.demand.push_back(index);
        }
    }

    pub(super) fn pop_next(&mut self) -> Option<usize> {
        let index = self
            .demand
            .pop_front()
            .or_else(|| self.predicted.pop_front())?;
        self.queued.remove(&index);
        Some(index)
    }
}

impl Default for FuncTable {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl From<Vec<FuncBody>> for FuncTable {
    fn from(funcs: Vec<FuncBody>) -> Self {
        Self {
            storage: FuncStorage::Eager(funcs),
        }
    }
}

impl FromIterator<FuncBody> for FuncTable {
    fn from_iter<T: IntoIterator<Item = FuncBody>>(iter: T) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl FuncTable {
    pub(crate) fn from_bytes(
        map: std::sync::Arc<[u8]>,
        blobs: Vec<FuncBlob>,
        heat_path: std::path::PathBuf,
    ) -> Self {
        let predicted = read_heat_order(&heat_path, blobs.len());
        let state = std::sync::Arc::new(DecodeState {
            map,
            cells: (0..blobs.len())
                .map(|_| std::sync::OnceLock::new())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            blobs: blobs.into_boxed_slice(),
            queue: std::sync::Mutex::new(DecodeQueue {
                predicted: predicted.iter().copied().collect(),
                queued: predicted.into_iter().collect(),
                ..DecodeQueue::default()
            }),
            ready: std::sync::Condvar::new(),
            done: std::sync::Condvar::new(),
            access: std::sync::Mutex::new((Vec::new(), std::collections::BTreeSet::new())),
            heat_path,
        });
        let worker_state = std::sync::Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("mirvm-decode".into())
            .spawn(move || decode_worker(worker_state))
            .ok();
        if worker.is_some() {
            state.ready.notify_one();
        }
        Self {
            storage: FuncStorage::Lazy(LazyFuncs { state, worker }),
        }
    }

    pub fn len(&self) -> usize {
        match &self.storage {
            FuncStorage::Eager(funcs) => funcs.len(),
            FuncStorage::Lazy(lazy) => lazy.state.blobs.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&FuncBody> {
        if index >= self.len() {
            return None;
        }
        Some(match &self.storage {
            FuncStorage::Eager(funcs) => &funcs[index],
            FuncStorage::Lazy(lazy) => lazy.get(index),
        })
    }

    pub fn iter(&self) -> FuncIter<'_> {
        FuncIter {
            funcs: self,
            next: 0,
        }
    }

    fn iter_mut(&mut self) -> std::slice::IterMut<'_, FuncBody> {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        funcs.iter_mut()
    }

    pub fn push(&mut self, body: FuncBody) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        funcs.push(body);
    }

    pub fn drain_into(&mut self, out: &mut Vec<FuncBody>) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        out.append(funcs);
    }

    pub(crate) fn flush_heat_order(&self) {
        if let FuncStorage::Lazy(lazy) = &self.storage {
            write_heat_order(&lazy.state);
        }
    }

    fn make_eager(&mut self) {
        if matches!(self.storage, FuncStorage::Eager(_)) {
            return;
        }
        let funcs = self.iter().cloned().collect();
        self.storage = FuncStorage::Eager(funcs);
    }
}

impl LazyFuncs {
    fn get(&self, index: usize) -> &FuncBody {
        record_access(&self.state, index as u32);
        if self.worker.is_none() {
            decode_one(&self.state, index);
        } else if self.state.cells[index].get().is_none() {
            let mut queue = self.state.queue.lock().unwrap();
            if self.state.cells[index].get().is_none() {
                queue.request_demand(index);
                self.state.ready.notify_one();
                while self.state.cells[index].get().is_none() {
                    queue = self.state.done.wait(queue).unwrap();
                }
            }
        }
        match self.state.cells[index]
            .get()
            .expect("function decode slot not published")
        {
            Ok(body) => body,
            Err(error) => panic!("verified function failed during lazy decode: {error}"),
        }
    }
}

impl Drop for LazyFuncs {
    fn drop(&mut self) {
        write_heat_order(&self.state);
        {
            let mut queue = self.state.queue.lock().unwrap();
            queue.stop = true;
            self.state.ready.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn decode_worker(state: std::sync::Arc<DecodeState>) {
    loop {
        let index = {
            let mut queue = state.queue.lock().unwrap();
            loop {
                if queue.stop {
                    return;
                }
                if let Some(index) = queue.pop_next() {
                    break index;
                }
                queue = state.ready.wait(queue).unwrap();
            }
        };
        decode_one(&state, index);
        state.done.notify_all();
    }
}

fn decode_one(state: &DecodeState, index: usize) {
    if state.cells[index].get().is_some() {
        return;
    }
    let blob = state.blobs[index];
    let bytes = &state.map[blob.start..blob.end];
    let decoded = if func_blob_hash(bytes) != blob.expected_hash {
        Err(format!(
            "function {index} changed after package verification"
        ))
    } else {
        postcard::from_bytes(bytes)
            .map_err(|error| format!("function {index} decode failed: {error}"))
    };
    let _ = state.cells[index].set(decoded);
}

fn func_blob_hash(data: &[u8]) -> u128 {
    let fnv = |prefix: &[u8]| {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in prefix.iter().chain(data) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    };
    ((fnv(&[]) as u128) << 64) | u128::from(fnv(b"\x01mirvmar"))
}

fn record_access(state: &DecodeState, id: u32) {
    let mut access = state.access.lock().unwrap();
    if access.1.insert(id) {
        access.0.push(id);
    }
}

fn read_heat_order(path: &std::path::Path, count: usize) -> Vec<usize> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    text.split_ascii_whitespace()
        .filter_map(|value| value.parse::<usize>().ok())
        .filter(|id| *id < count && seen.insert(*id))
        .collect()
}

fn write_heat_order(state: &DecodeState) {
    let access = state.access.lock().unwrap();
    if access.0.is_empty() {
        return;
    }
    let Some(dir) = state.heat_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let body = access
        .0
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Best effort: a heat file that cannot be published only costs the next run its learned
    // order, so the failure is swallowed rather than reported.
    let _ = crate::store::publish_bytes(&state.heat_path, body.as_bytes());
}

pub struct FuncIter<'a> {
    funcs: &'a FuncTable,
    next: usize,
}

impl<'a> Iterator for FuncIter<'a> {
    type Item = &'a FuncBody;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.funcs.get(self.next)?;
        self.next += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.funcs.len().saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for FuncIter<'_> {}

impl<'a> IntoIterator for &'a FuncTable {
    type Item = &'a FuncBody;
    type IntoIter = FuncIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::ops::Index<usize> for FuncTable {
    type Output = FuncBody;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("FuncId outside function table")
    }
}

impl std::fmt::Debug for FuncTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl serde::Serialize for FuncTable {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> serde::Deserialize<'de> for FuncTable {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Vec<FuncBody> as serde::Deserialize>::deserialize(deserializer).map(Self::from)
    }
}

/// Startup plan for the guest's main, matching what cg_ssa's `create_entry_fn` produces:
/// `lang_start(main fn-ptr, argc, argv, sigpipe) -> isize`, where the result is the process exit
/// code.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct EntryPlan {
    pub lang_start: FuncId,
    /// Real entry address of user `main`, passed as lang_start's first argument and dispatched through
    /// a CallIndirect.
    pub main_addr: LinkAddr,
    pub argc: u64,
    /// Real address of the argv C-string pointer table in the frozen area.
    pub argv_ptr: u64,
    pub sigpipe: u8,
}

/// One entry of the GOT symbol table: a name plus whether the symbol is weak. A weak symbol that
/// fails to resolve writes 0 rather than aborting, which is the NULL semantics of an absent weak extern.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GotSym {
    pub name: Box<str>,
    pub weak: bool,
}

/// A fixup point applied at startup: the load phase writes
/// `*addr = resolve(foreign_syms[sym]) + addend`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct GotFixup {
    pub addr: LinkAddr,
    pub sym: u32,
    pub addend: u64,
}

/// One object pointer inside the frozen bytes. `at` is the 8-byte cell to write, `target` is the link
/// address it should point to (addend already folded); at instantiation both ends are translated via
/// LoadMap before writing.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct FrozenReloc {
    pub at: LinkAddr,
    pub target: FrozenRelocTarget,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum FrozenRelocTarget {
    Frozen(LinkAddr),
    Entry(LinkAddr),
}

/// Recipe for an executable entry. The artifact stores only the guest function's logical address, its
/// FuncId and its frozen C ABI signature; each Engine materializes its own libffi closure at startup.
/// Real fn-ptrs are never cached or packaged, because they differ per process.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntryStubSite {
    /// Logical identity shared by all fn-ptr references to this fn in the artifact; each Engine maps it
    /// to a unique closure.
    pub link_addr: LinkAddr,
    pub func: FuncId,
    pub sig: ForeignSig,
}

/// Hidden ELF symbol used by native bridges that call back into a guest entry.
/// The symbol identifies the artifact address only; each Engine writes its own
/// runtime closure address into the corresponding slot while instantiating.
pub(crate) fn native_entry_slot_name(link_addr: LinkAddr) -> String {
    format!("__mirvm_p1_target_{:016x}", link_addr.0)
}

/// FuncIds of the four `__rust_*` shims that a custom `#[global_allocator]` produces. For a crate with
/// that attribute, the HIR expander generates four local forwarding functions, each calling one method
/// of the user's GlobalAlloc.
/// Allocation is program-level semantics: `CallBuiltin(Rust*)` arms baked into a base or dependency
/// image and the shim in the delta module must reach the same allocator, since freeing a pointer on a
/// different heap corrupts allocator metadata. The interpreter therefore routes all of them through
/// this field, whichever session baked the bytecode.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AllocShims {
    pub alloc: FuncId,
    pub dealloc: FuncId,
    pub realloc: FuncId,
    pub alloc_zeroed: FuncId,
}

/// How to reclaim guest resources for an uncaught guest panic.
///
/// `cleanup` is std's object function `std::panicking::catch_unwind::cleanup`: it receives the
/// panic_unwind raw exception pointer, extracts the `Box<dyn Any + Send>`, and decrements the guest
/// panic count. `drop_payload` is the drop glue for that Box, which runs the user payload's Drop and
/// frees through the guest's own global allocator. The engine moves two opaque machine words and never
/// reads std's private exception, Box or vtable layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuestPanicCleanup {
    pub cleanup: FuncId,
    pub drop_payload: FuncId,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Module {
    pub funcs: FuncTable,
    /// Symbol names in FuncId order. Even when function bodies are decoded lazily, backtrace can build
    /// a standard ELF symbol table from this without decoding every body.
    pub function_names: Vec<Box<str>>,
    /// FuncId to address in this process's symbol ELF. Generated at load time; never cached or
    /// packaged.
    #[serde(skip)]
    pub backtrace_ips: Vec<u64>,
    /// Keeps the in-memory ELF file and dlopen object alive for the Engine lifetime.
    #[serde(skip)]
    pub backtrace_image: Option<super::super::backtrace::SymbolImage>,
    /// Exported `no_mangle` symbol name to FuncId, used by `--vm-call` lookup.
    pub exports: std::collections::HashMap<Box<str>, FuncId>,
    /// Frozen area holding statics, the constant pool and fn entries. Lower materializes it, and it is
    /// read-only after publication except for `static mut` cells.
    pub frozen: Option<super::super::frozen::FrozenArena>,
    /// Link address to runtime address mapping for this Module instance. Built when a package or image
    /// is instantiated; not part of the artifact.
    #[serde(skip)]
    pub load_map: LoadMap,
    /// fn-ptr entry real address to FuncId, the reverse lookup indirect call dispatch uses.
    pub fn_addrs: std::collections::HashMap<u64, FuncId>,
    /// Artifact-address form of `fn_addrs`, rebuilt whenever instance or entry-closure addresses
    /// change.
    pub link_fn_addrs: std::collections::HashMap<LinkAddr, FuncId>,
    /// Entry-closure addresses this Engine has materialized and native code can call directly.
    #[serde(skip)]
    pub executable_entry_addrs: std::collections::HashSet<u64>,
    /// Candidate paths of shared libraries named by `-l` directives. They are optional: a candidate that
    /// does not exist is skipped in favour of the next.
    pub native_libs: Vec<Box<str>>,
    /// Shared libraries already materialized by the loading phase that must dlopen successfully before
    /// executing foreign code, for instance the `.so` products of static archives. Loading one of these
    /// must not degrade into an ordinary dlsym miss.
    pub required_native_libs: Vec<Box<str>>,
    /// Expected content identity of each required native library, positionally matched with
    /// `required_native_libs`. A zero entry marks a raw in-process Module whose caller supplied no
    /// artifact hash; a Package always carries and verifies this list.
    #[serde(skip)]
    pub required_native_hashes: Vec<u128>,
    /// Per-Engine shared library images the Engine produced itself. They have completed dependency
    /// resolution and ELF relocation, but the Engine keeps managing them so it can run init and fini at
    /// the right time. Never cached or packaged.
    #[serde(skip)]
    pub native_images: Vec<super::super::native_instance::NativeImage>,
    /// This package's self-loaded machine-code image. It is visible only to this Module's foreign symbol
    /// resolution, which keeps global_asm names from colliding across Engines. Not serialized; rebuilt
    /// from the MC section when the package loads.
    #[serde(skip)]
    pub mc_images: Vec<super::super::mcload::McImage>,
    /// Guest TLS slot table, indexed by TlsId. The per-thread instances live in `Ctx.tls`.
    pub tls: Vec<TlsSlot>,
    /// Real addresses of the asm-stub wrappers, indexed by AsmStubId. The load phase produced each one by
    /// assembling it with cc, then dlopening and dlsym'ing it. Execution only reads a u64 and calls it
    /// directly, which keeps it pure.
    /// NOTE: not part of snapshot semantics; a warm load rematerializes these idempotently from
    /// `asm_sites` and overwrites whatever the snapshot held.
    pub asm_stub_addrs: Vec<u64>,
    /// asm-stub materialization recipe: the symbol name plus the full wrapper GAS text, ordered by
    /// AsmStubId, which is also the bit order. A warm load reruns `asm::materialize` over it, so a content
    /// hash hit in the `.so` cache costs only dlopen and dlsym, while a cleared cache re-runs cc.
    /// Symbol names are decoupled from bit order because split lowering only knows the final bit order at
    /// the end; it therefore uses class-prefixed names (`mirvm_asm_xi{j}` / `mirvm_asm_xd{k}`) and
    /// non-split lowering keeps the positional `mirvm_asm_{id}`.
    pub asm_sites: Vec<AsmSite>,
    /// GOT symbol table. The slots themselves are ordinary 8-byte cells in the frozen area, and the
    /// bytecode bakes their addresses rather than their values, so startup can re-resolve each name and
    /// rewrite the cells. That is what keeps a module position-independent across ASLR. Each image has its
    /// own table, merged by name during absorb.
    pub foreign_syms: Vec<GotSym>,
    /// Fixup points applied at startup: `*(addr) = resolve(foreign_syms[sym]) + addend`. `addr` is a
    /// frozen-domain LinkAddr of this module; LoadMap turns it into the real slot after instantiation.
    pub got_fixups: Vec<GotFixup>,
    /// Object pointer relocations inside/between frozen domains, excluding foreign GOT fixup points.
    pub frozen_relocs: Vec<FrozenReloc>,
    /// Entry recipes for guest functions in this domain that have their address taken and can be exported
    /// with the C ABI. Each Engine builds its own closures and LinkAddr mappings from them.
    pub entry_stub_sites: Vec<EntryStubSite>,
    /// Code-area handle lower used to allocate stable logical addresses. The mapping is released when the
    /// Engine starts, since the runtime executes per-Engine libffi closures instead. Not part of the
    /// artifact.
    #[serde(skip)]
    pub entry_stubs: super::super::codearena::StubArena,
    /// Entry logical-address domains and recipes of the absorbed image and base, as
    /// (link-address domain base, recipes, lower-time address allocation handle).
    #[serde(skip)]
    pub image_entry_stubs: Vec<(
        usize,
        Vec<EntryStubSite>,
        super::super::codearena::StubArena,
    )>,
    /// Custom `__rust_*` shims of a `#[global_allocator]`; see the `AllocShims` note. A Global allocator is
    /// always registered on the delta side, and the interpreter's `CallBuiltin(Rust*)` arms route through
    /// it.
    pub custom_alloc_shims: Option<AllocShims>,
    /// Cleanup plan the Engine top level executes after catching an uncaught guest panic. Hand-built test
    /// Modules and non-executable image stack layers may leave it None, but every executable lower product
    /// must have one, and the run entry refuses None rather than leaking the payload.
    pub guest_panic_cleanup: Option<GuestPanicCleanup>,
    /// Startup chain of `main`; None when running in `--vm-call` mode.
    pub entry: Option<EntryPlan>,
    /// Frozen areas of the base image and every absorbed dependency image. Absorbing mounts them with the
    /// same lifetime as this module, because the delta bytecode embeds absolute addresses in those domains
    /// and they must stay mapped until guest exit.
    /// NOTE: not part of snapshot semantics; image files have their own lifecycles, and delta entries refer
    /// to them only through the key chain.
    #[serde(skip)]
    pub image_frozens: Vec<super::super::frozen::FrozenArena>,
}

impl Module {
    /// Translate a link-time address to this Module instance's runtime address. A freshly lowered
    /// module resolves to identity until a dynamic load mapping is attached.
    pub fn resolve_link_addr(&self, addr: LinkAddr) -> u64 {
        self.load_map
            .resolve_or_identity(addr)
            .unwrap_or_else(|| panic!("unmapped artifact address {:#x}", addr.0))
    }

    pub fn try_resolve_link_addr(&self, addr: LinkAddr) -> Result<u64, String> {
        self.load_map
            .resolve_or_identity(addr)
            .ok_or_else(|| format!("unmapped artifact address {:#x}", addr.0))
    }

    pub fn is_executable_entry(&self, addr: u64) -> bool {
        self.executable_entry_addrs.contains(&addr)
    }

    pub fn rebuild_load_map(&mut self) {
        let mut map = LoadMap::default();
        if let Some(frozen) = &self.frozen {
            map.add_frozen(frozen);
        }
        for frozen in &self.image_frozens {
            map.add_frozen(frozen);
        }
        self.load_map = map;
    }

    pub fn apply_frozen_relocs(&self) -> Result<(), String> {
        for (index, reloc) in self.frozen_relocs.iter().enumerate() {
            let at = self
                .load_map
                .resolve(reloc.at)
                .ok_or_else(|| format!("frozen relocation {index} write address is unmapped"))?;
            let target_link = match reloc.target {
                FrozenRelocTarget::Frozen(addr) | FrozenRelocTarget::Entry(addr) => addr,
            };
            let target = self.load_map.resolve(target_link).ok_or_else(|| {
                format!(
                    "frozen relocation {index} target address {:#x} ({:?}) is unmapped",
                    target_link.0, reloc.target
                )
            })?;
            unsafe { (at as *mut u64).write_unaligned(target) };
        }
        Ok(())
    }

    pub fn rebuild_fn_addrs(&mut self) {
        if self.link_fn_addrs.is_empty() {
            self.link_fn_addrs = self
                .fn_addrs
                .iter()
                .map(|(&addr, &func)| (LinkAddr(addr), func))
                .collect();
        }
        self.fn_addrs = self
            .link_fn_addrs
            .iter()
            .map(|(&addr, &func)| (self.resolve_link_addr(addr), func))
            .collect();
    }

    pub fn ensure_function_names(&mut self) {
        if self.function_names.len() != self.funcs.len() {
            self.function_names = self.funcs.iter().map(|body| body.name.clone()).collect();
        }
    }

    /// Switch the plain syscall builtin for the trace-capable one.
    /// The serialized Module always stores the plain form; the rewrite happens only once a session has
    /// armed capture and before `Shared` publishes the Module for execution, because the builtin must
    /// match the code domain that publication freezes.
    pub(crate) fn rewrite_host_syscalls_for_capture(&mut self) {
        for body in self.funcs.iter_mut() {
            for block in &mut body.blocks {
                let Terminator::CallBuiltin { builtin, .. } = &mut block.term else {
                    continue;
                };
                if matches!(builtin, Builtin::HostSyscall) {
                    *builtin = Builtin::HostSyscallTrace;
                }
            }
        }
    }
    /// Append argv to the frozen area as a NUL-terminated C string table and fill the entry plan's
    /// `argc` and `argv_ptr`.
    /// argv is a runtime input, so it must not enter the cache snapshot. Cold and warm paths both append
    /// and backfill after the snapshot on every run, which keeps the two paths from drifting apart.
    pub fn finalize_entry_argv(&mut self, argv: &[String]) -> Result<(), String> {
        let Some(entry) = self.entry.as_mut() else {
            return Ok(());
        };
        let frozen = self
            .frozen
            .as_mut()
            .ok_or("executable module has no frozen memory for argv")?;
        let mut ptrs: Vec<u64> = Vec::with_capacity(argv.len());
        for a in argv {
            let bytes = a.as_bytes();
            let p = frozen.alloc(bytes.len() as u64 + 1, 1);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                *((p + bytes.len() as u64) as *mut u8) = 0;
            }
            ptrs.push(p);
        }
        let table = frozen.alloc((ptrs.len() as u64 + 1) * 8, 8);
        for (i, &p) in ptrs.iter().enumerate() {
            unsafe { *((table + i as u64 * 8) as *mut u64) = p };
        }
        // The trailing NULL is guaranteed because the frozen allocator zeroes new memory.
        entry.argc = argv.len() as u64;
        entry.argv_ptr = table;
        Ok(())
    }

    /// Merge an image's GOT into this module: symbols are deduplicated by name and the fixups' symbol
    /// indices are remapped to the merged table. A fixup address is a frozen-domain address from the
    /// image, which uses a fixed base, so it still names the same cell after the merge and is taken
    /// over unchanged.
    pub fn absorb_got(&mut self, syms: Vec<GotSym>, mut fixups: Vec<GotFixup>) {
        if fixups.is_empty() {
            return;
        }
        let mut remap: Vec<u32> = Vec::with_capacity(syms.len());
        for s in syms {
            let idx = match self.foreign_syms.iter().position(|e| e.name == s.name) {
                Some(i) => {
                    // Merging also merges weak/strong: one strong definition makes the symbol strong.
                    if !s.weak {
                        self.foreign_syms[i].weak = false;
                    }
                    i as u32
                }
                None => {
                    self.foreign_syms.push(s);
                    (self.foreign_syms.len() - 1) as u32
                }
            };
            remap.push(idx);
        }
        for f in &mut fixups {
            f.sym = remap[f.sym as usize];
        }
        self.got_fixups.append(&mut fixups);
    }
}
