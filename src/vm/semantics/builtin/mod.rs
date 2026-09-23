//! The builtin vocabulary's execution: one guest-visible operation per arm, shared by the
//! interpreter and the JIT so the two backends cannot drift.
//!
//! The scalar-pair lane comes first, because LLVM's `addcarry`/`subborrow` return the
//! `ScalarPair` `(flag, result)` rather than one scalar and so never reach the routing match.
//! Everything else is routed by that match, whose arms are the vocabulary's families and nothing
//! else -- allocation, host services, the guest unwinder, and this CPU's own instructions, each
//! with its bodies in its own file. A variant added to [`Builtin`] without an arm is a compile
//! error rather than a silent zero.
//!
//! The `edge` protocol is owned here: the cleanup edge of the builtin's unwind action is
//! installed before anything can unwind, and cleared on every return.

use std::cell::Cell;

use crate::vm::ctx::Ctx;
use crate::vm::instance::Instance;
use crate::vm::ir::{Bb, Builtin, BuiltinCallRole, FuncBody, Module, UnwindAction};
use crate::vm::unwind::engine_abort;

mod alloc;
mod host;
mod unwind;
mod x86_64;

/// Executes one builtin call and returns `(lo, hi)` for the call site's uniform write-back:
/// a plain scalar lane returns `(r, 0)`, the addcarry/subborrow pair lane returns
/// `(flag, result)`, and an x86 vector lane has already stored its sret bytes and returns
/// `(0, 0)`.
///
/// `av` holds the call site's already-flattened arguments (a builtin has no leading sret slot)
/// and `ret_dst` is the true destination address of a `RetDest::Indirect` result (evaluated at
/// the call site; the sret landing spot for an x86 vector lane).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_builtin(
    ctx: *mut Ctx,
    body: &FuncBody,
    edge: &Cell<Option<Bb>>,
    builtin: &Builtin,
    av: &[u64],
    ret_dst: Option<u64>,
    unwind: &UnwindAction,
    role: BuiltinCallRole,
) -> (u64, u64) {
    // Every host builtin in both backends funnels through here, so this is the
    // one ordinary boundary a `fork` child is guaranteed to reach. The rebuild
    // must not run earlier: `after_fork_child` is kernel-side and may only
    // store, while this runs with the allocator and thread machinery available.
    crate::telemetry::capture::rebuild_on_boundary();
    // Reserved in the signature for call-site symmetry; the body reads the module from ctx.
    let _ = body;
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let instance: &Instance = unsafe { &(*(*ctx).shared).instance };
    let a = |i: usize| av[i];
    // RaiseException starts its unwind through this edge.
    edge.set(unwind.cleanup_edge());
    // LLVM's addcarry/subborrow return the ScalarPair `(flag, result)`, while every other
    // builtin returns a single scalar. This dedicated pair lane returns `(flag, result)` =
    // `(lo, hi)`, keeping field order consistent with the frozen ABI; the call site does the
    // uniform Pair write-back.
    let carry_result = match builtin {
        Builtin::AddCarry64 => {
            let carry_in = u64::from(a(0) != 0);
            let (partial, carry1) = a(1).overflowing_add(a(2));
            let (result, carry2) = partial.overflowing_add(carry_in);
            Some((carry1 || carry2, result))
        }
        Builtin::SubBorrow64 => {
            let borrow_in = u64::from(a(0) != 0);
            let (partial, borrow1) = a(1).overflowing_sub(a(2));
            let (result, borrow2) = partial.overflowing_sub(borrow_in);
            Some((borrow1 || borrow2, result))
        }
        _ => None,
    };
    if let Some((flag, result)) = carry_result {
        edge.set(None);
        return (u64::from(flag), result);
    }
    let r = match builtin {
        // This CPU's own instruction vocabulary, split by where the result has to live.
        Builtin::X86Pshufb128
        | Builtin::X86Pshufb256
        | Builtin::X86Sha256Msg1
        | Builtin::X86Sha256Msg2
        | Builtin::X86Sha256Rnds2
        | Builtin::X86PsadBw128
        | Builtin::X86PsadBw256
        | Builtin::X86Pclmulqdq
        | Builtin::X86AesEnc
        | Builtin::X86AesEncLast
        | Builtin::X86AesDec
        | Builtin::X86AesDecLast
        | Builtin::X86AesImc
        | Builtin::X86AesKeygenAssist
        | Builtin::X86Permd256
        | Builtin::X86GatherQPd256
        | Builtin::X86GatherDPd256
        | Builtin::X86Pmadd52Lo128
        | Builtin::X86Pmadd52Hi128
        | Builtin::X86Pmadd52Lo256
        | Builtin::X86Pmadd52Hi256
        | Builtin::X86Pmadd52Lo512
        | Builtin::X86Pmadd52Hi512
        | Builtin::X86PmaddUbSw128
        | Builtin::X86PmaddUbSw256
        | Builtin::X86PmaddWd128
        | Builtin::X86PmaddWd256
        | Builtin::X86Cvtps2ph128
        | Builtin::X86Cvtph2ps128
        | Builtin::X86Cvtps2ph256
        | Builtin::X86Cvtph2ps256
        | Builtin::X86MaxPs128
        | Builtin::X86MinPs128
        | Builtin::X86MaxPs256
        | Builtin::X86MinPs256
        | Builtin::X86CmpPs128
        | Builtin::X86CmpPs256
        | Builtin::X86CmpPd128
        | Builtin::X86CmpPd256
        | Builtin::X86MaxPd128
        | Builtin::X86MinPd128
        | Builtin::X86MaxPd256
        | Builtin::X86MinPd256
        | Builtin::X86MaxSd
        | Builtin::X86MinSd
        | Builtin::X86RoundPs128
        | Builtin::X86RoundPs256
        | Builtin::X86CvtPs2dq128
        | Builtin::X86CvttPs2dq128
        | Builtin::X86CvtPs2dq256
        | Builtin::X86CvttPs2dq256
        | Builtin::X86BlendvPs128
        | Builtin::X86BlendvPs256
        | Builtin::X86Lddqu128
        | Builtin::X86Lddqu256
        | Builtin::X86PsllD128
        | Builtin::X86PsrlD128 => x86_64::exec_indirect_vector(builtin, av, ret_dst),
        Builtin::Xgetbv
        | Builtin::X86Crc32U8
        | Builtin::X86Crc32U16
        | Builtin::X86Crc32U32
        | Builtin::X86Crc32U64
        | Builtin::CpuHintNop
        | Builtin::Breakpoint => x86_64::exec_scalar(builtin, av),

        // The managed heap, and the guest shim that takes it over when one is registered.
        Builtin::NoAllocShim
        | Builtin::RustAlloc
        | Builtin::RustAllocZeroed
        | Builtin::RustRealloc
        | Builtin::RustDealloc => alloc::exec(builtin, ctx, module, av, unwind),

        // The C library services the engine provides itself.
        Builtin::HostGetenv
        | Builtin::HostWrite
        | Builtin::HostStrlen
        | Builtin::HostAbort
        | Builtin::HostFork
        | Builtin::HostAtexit
        | Builtin::HostCxaAtexit
        | Builtin::HostOnExit
        | Builtin::HostSignal
        | Builtin::HostRaise
        | Builtin::HostSigaction
        | Builtin::HostSyscall
        | Builtin::HostSyscallTrace => host::exec(ctx, builtin, av),

        // The guest unwinder.
        Builtin::UnwindRaise
        | Builtin::UnwindDeleteException
        | Builtin::UnwindBacktrace
        | Builtin::UnwindGetIp
        | Builtin::UnwindGetIpInfo
        | Builtin::UnwindGetCfa
        | Builtin::UnwindFindEnclosing
        | Builtin::CatchUnwind => unwind::exec(ctx, instance, builtin, av, role),

        // Handled by the pair lane above, which returns the ScalarPair form.
        Builtin::AddCarry64 | Builtin::SubBorrow64 => {
            unreachable!("addcarry/subborrow are handled by the pair lane")
        }
        // Emitted by the load phase for something this engine does not implement.
        Builtin::Unsupported(name) => engine_abort(&format!("unsupported builtin `{}`", name.0)),
    };
    edge.set(None);
    (r, 0)
}
