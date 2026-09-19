//! Call and incoming FFI: `call_fn_addr`, `cleanup_edge`, `call_guarding_terminate`,
//! `run_cleanup`, `ret_abi_of`, `call_guest_ffi`, `exec_builtin` and `interp_frame`
//! (model A: host recursion with a real stack byte guard).
//!
//! `call_guest` -- the reader anchor of the publish protocol -- stays in `mod.rs` because it
//! is inseparable from the JIT/compiler worker's writer side.

use super::*;
use super::{
    runblocks::run_blocks,
    services::{
        AtexitKind, atexit_register, func_synth_ip, resolve_signal_handler, unwind_backtrace,
    },
};
use crate::vm::ir;

pub(super) fn call_fn_addr(ctx: *mut Ctx, addr: u64, args: &[u64], caller: &str) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let Some(&fid) = module.fn_addrs.get(&addr) else {
        engine_abort(&format!(
            "indirect call target {addr:#x} is not a known fn entry (caller {caller})"
        ));
    };
    call_guest(ctx, fid, args)
}

/// The cleanup target block of an unwind edge, if it has one.
#[inline]
pub(super) fn cleanup_edge(u: &UnwindAction) -> Option<Bb> {
    match u {
        UnwindAction::Cleanup(b) => Some(*b),
        _ => None,
    }
}

/// Call wrapper for a `Terminate` unwind edge: a panic reaching it aborts (double panic or an
/// `extern "C"` ABI boundary).
#[inline]
pub(super) fn call_guarding_terminate<R>(unwind: &UnwindAction, f: impl FnOnce() -> R) -> R {
    if let UnwindAction::Terminate = unwind {
        crate::vm::unwind::guard_terminate(f)
    } else {
        f()
    }
}

/// Runs a cleanup chain in `guard.drop` (the host-Rust form of a landing pad): from the
/// cleanup block to `Resume`. A `Call` inside the chain may re-enter mixed execution; a panic
/// inside the chain aborts on a `Terminate` edge, and on a `Continue` edge escapes through
/// `Drop` as a host double-panic abort (matching native).
pub(super) fn run_cleanup(ctx: *mut Ctx, func: u32, base: usize, entry: Bb) {
    // Cleanup blocks contain no nested cleanup (MIR invariant), so a dummy edge is enough.
    let edge = Cell::new(None);
    match run_blocks(ctx, func, base, &edge, entry) {
        Exit::Resume => {} // return to the guard; the host unwinder continues on its own
        Exit::Ret(..) => engine_abort("cleanup chain ended in Return (MIR invariant broken)"),
    }
}

#[inline]
pub(crate) fn ret_abi_of(ctx: *mut Ctx, func: u32) -> RetAbi {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    module.funcs[func as usize].ret
}

/// C1 inbound FFI marshalling: expands the C-side arguments that `marshal_args` produced
/// (scalars as-is, aggregates as the true address of their bytes) into ABI argument slots
/// according to the callee's `ParamAbi`, then calls `call_guest`. Shared by the thunk factory
/// and the P1 entry trampoline. `ret_addr` is libffi's result buffer for a by-value aggregate
/// return; it becomes the hidden first argument slot only when the callee returns
/// `RetAbi::Indirect` (sret passed through), while small forms are re-packed into `FfiAgg`
/// from `(lo, hi)` by the caller.
pub(crate) fn call_guest_ffi(
    ctx: *mut Ctx,
    func: u32,
    kinds: &[FfiKind],
    vals: &[u64],
    ret_addr: Option<u64>,
) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];
    let mut av: Vec<u64> = Vec::with_capacity(vals.len() + body.params.len() + 1);
    if let RetAbi::Indirect { .. } = body.ret {
        av.push(ret_addr.expect(
            "C1: callee returns an aggregate by value (RetAbi::Indirect) but has no result address",
        ));
    }
    let mut ki = 0usize;
    for p in &body.params {
        match p {
            ParamAbi::Zst => {}
            ParamAbi::Scalar(_) => match kinds.get(ki) {
                Some(FfiKind::Agg(agg)) => {
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 0) });
                    ki += 1;
                }
                Some(_) => {
                    av.push(vals[ki]);
                    ki += 1;
                }
                None => engine_abort(&format!(
                    "C1 marshalling is missing an argument (callee fn {} params {:?})",
                    body.name, body.params
                )),
            },
            ParamAbi::Pair(_, _) => match kinds.get(ki) {
                Some(FfiKind::Agg(agg)) => {
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 0) });
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 1) });
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 marshalling mismatch: a `Pair` callee parameter met a non-scalar C argument (fn {} params {:?} kinds {:?})",
                    body.name, body.params, kinds
                )),
            },
            ParamAbi::Indirect { .. } => match kinds.get(ki) {
                Some(FfiKind::Agg(_)) => {
                    av.push(vals[ki]);
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 marshalling mismatch: a by-address callee parameter met a non-scalar C argument (fn {} params {:?} kinds {:?})",
                    body.name, body.params, kinds
                )),
            },
        }
    }
    if ki != vals.len() {
        engine_abort(&format!(
            "C1 marshalling slot count mismatch: callee fn {} consumes {ki}, marshal supplies {}",
            body.name,
            vals.len()
        ));
    }
    call_guest(ctx, func, &av)
}

/// Reads field `idx` of `agg`, in declaration order, at its declared width for a scalar leaf.
/// A top-level nested leaf is structurally exclusive with the Pair/Scalar parameter forms by
/// the same rustc layout derivation, so encountering one breaks an engine invariant.
pub(super) unsafe fn agg_leaf_at(addr: u64, agg: &FfiAgg, idx: usize) -> u64 {
    let Some(f) = agg.fields.get(idx) else {
        engine_abort("C1 marshalling: a Pair parameter met a single-field aggregate");
    };
    let FfiLeaf::Scalar(k) = &f.leaf else {
        engine_abort("C1 marshalling: a top-level nested leaf met a Pair parameter");
    };
    let p = addr.wrapping_add(f.off as u64) as *const u8;
    unsafe {
        match k {
            FfiKind::I8 | FfiKind::U8 => p.read() as u64,
            FfiKind::I16 | FfiKind::U16 => (p as *const u16).read_unaligned() as u64,
            FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => (p as *const u32).read_unaligned() as u64,
            FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                (p as *const u64).read_unaligned()
            }
            FfiKind::Void | FfiKind::Agg(_) => engine_abort("C1 marshalling: illegal leaf kind"),
        }
    }
}

/// Model A: a guest call is host recursion.
///
/// Calling convention v2: arguments are flattened into `&[u64]` (a pair takes 2 slots, an
/// indirect argument passes its address) and the result is `(lo, hi)`.
///
/// Guest stack overflow protection is a real stack byte guard: the address of a local
/// approximates the host SP, and dropping below the safety floor frozen in the Ctx (thread
/// stack low end plus margin) exits with a diagnostic. A fixed frame limit would be a poor
/// proxy for the real bound -- a native 8 MiB main stack holds on the order of 100k shallow
/// frames -- so the guard adapts to the thread's actual stack instead. Native semantics are
/// SIGSEGV with "has overflowed its stack"; this guard is the diagnostic stand-in and only
/// approximates native (the overflow depth is unspecified).
pub(crate) fn interp_frame(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    // The prologue can raise an EngineFault too (stack guard, argument ABI, exhausted operand
    // region), so staged guards are armed from the first Ctx mutation and undo only the steps
    // already completed.
    let mut guard = FrameGuard {
        ctx,
        depth_active: false,
        base: None,
        shadow_active: false,
        unwind_edge: Cell::new(None),
    };
    let Some(depth) = (unsafe { (*ctx).depth.checked_add(1) }) else {
        engine_abort(&format!(
            "guest interpretation depth counter overflow (fn {})",
            body.name
        ));
    };
    unsafe { (*ctx).depth = depth };
    guard.depth_active = true;
    let sp_approx = &depth as *const u32 as usize;
    if unsafe { (*ctx).stack_floor } > sp_approx {
        engine_abort(&format!(
            "guest stack overflow (the host execution stack reached the safety margin; interpretation depth {depth}; fn {})",
            body.name
        ));
    }

    let base = region_reserve(ctx, body.frame_size, body.frame_align);
    guard.base = Some(base);
    // Prologue: consume the argument slots per ParamAbi. The count is checked up front so a
    // mismatch names the function and the expectation rather than panicking out of bounds.
    let needed: usize = matches!(body.ret, RetAbi::Indirect { .. }) as usize
        + body
            .params
            .iter()
            .map(|p| match p {
                ParamAbi::Zst => 0,
                ParamAbi::Scalar(_) | ParamAbi::Indirect { .. } => 1,
                ParamAbi::Pair(..) => 2,
            })
            .sum::<usize>()
        + body.caller_loc_off.is_some() as usize;
    if args.len() < needed {
        engine_abort(&format!(
            "ABI mismatch: fn `{}` expects {needed} arguments (params {:?} ret {:?} loc {:?}) but receives {}",
            body.name,
            body.params,
            body.ret,
            body.caller_loc_off,
            args.len()
        ));
    }
    let mut ai = 0usize;
    // Indirect return: the hidden first argument is the true destination address, stored in
    // the sret slot.
    if let RetAbi::Indirect { sret_off, .. } = body.ret {
        slot_write(
            ctx,
            base,
            Slot {
                off: sret_off,
                width: Width::W64,
            },
            args[ai],
        );
        ai += 1;
    }
    for p in &body.params {
        match p {
            ParamAbi::Zst => {}
            ParamAbi::Scalar(s) => {
                slot_write(ctx, base, *s, args[ai]);
                ai += 1;
            }
            ParamAbi::Pair(lo, hi) => {
                slot_write(ctx, base, *lo, args[ai]);
                slot_write(ctx, base, *hi, args[ai + 1]);
                ai += 2;
            }
            ParamAbi::Indirect { off, size } => {
                let src = args[ai];
                ai += 1;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src as *const u8,
                        (base as u64 + *off as u64) as *mut u8,
                        *size as usize,
                    )
                };
            }
        }
    }
    // #[track_caller]: the hidden trailing &Location argument
    if let Some(off) = body.caller_loc_off {
        let Some(&loc) = args.get(ai) else {
            engine_abort(&format!(
                "ABI mismatch: track_caller fn `{}` expects the location trailing argument (got {} slots)",
                body.name,
                args.len()
            ));
        };
        slot_write(
            ctx,
            base,
            Slot {
                off,
                width: Width::W64,
            },
            loc,
        );
    }

    // The frame guard always restores the operand region; the surrounding raw catch picks the
    // cleanup from the actual exception identity.
    // Push the shadow frame with a synthetic IP for this FuncId: an opaque, non-executable
    // token used only as a backtrace IP, never dereferenced as code.
    let shadow_marker = 0u8;
    unsafe {
        (*ctx).shadow.push(crate::vm::ctx::ShadowFrame {
            ip: func_synth_ip(ctx, func),
            cfa: &shadow_marker as *const u8 as u64,
        })
    };
    guard.shadow_active = true;
    match crate::vm::unwind::catch_raw(|| run_blocks(ctx, func, base, &guard.unwind_edge, 0)) {
        Ok(Exit::Ret(lo, hi)) => (lo, hi), // guard drop restores the region
        Ok(Exit::Resume) => engine_abort(&format!(
            "Resume reached on the normal execution path (fn {})",
            body.name
        )),
        Err(exception) => {
            if !exception.is_engine_fault()
                && let Some(cleanup) = guard.unwind_edge.get()
            {
                run_cleanup(ctx, func, base, cleanup);
            }
            exception.resume_or_rethrow()
        }
    }
}

/// Semantics of `CallBuiltin`, shared verbatim by the interpreter's thin arm and the JIT
/// `mirvm_call_builtin`/`mirvm_alloc` helpers so the two backends cannot drift.
///
/// `av` holds the call site's already-flattened arguments (a builtin has no leading sret
/// slot) and `ret_dst` is the true destination address of a `RetDest::Indirect` result
/// (evaluated at the call site; the sret landing spot for an x86 vector lane). Returns
/// `(lo, hi)`: a plain scalar lane returns `(r, 0)`, the addcarry/subborrow pair lane returns
/// `(flag, result)`, and an x86 vector lane has already stored its sret bytes and returns
/// `(0, 0)`. The call site performs the uniform write-back (nothing for Ignore/Indirect, `lo`
/// for Scalar, `(lo, hi)` for Pair) and `lower` only emits matching forms. The `edge`
/// protocol is owned by this body.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_builtin(
    ctx: *mut Ctx,
    body: &ir::FuncBody,
    edge: &Cell<Option<Bb>>,
    builtin: &ir::Builtin,
    av: &[u64],
    ret_dst: Option<u64>,
    unwind: &ir::UnwindAction,
    role: ir::BuiltinCallRole,
) -> (u64, u64) {
    use crate::vm::ir::Builtin;
    // Every host builtin in both backends funnels through here, so this is the
    // one ordinary boundary a `fork` child is guaranteed to reach. The rebuild
    // must not run earlier: `after_fork_child` is kernel-side and may only
    // store, while this runs with the allocator and thread machinery available.
    crate::telemetry::capture::rebuild_on_boundary();
    // Reserved in the signature for call-site symmetry; the body reads the module from ctx.
    let _ = body;
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let a = |i: usize| av[i];
    edge.set(cleanup_edge(unwind)); // RaiseException starts its unwind through this edge
    // x86 vector intrinsics take indirect vector addresses and write their result to the sret
    // place. Each helper carries its own target_feature, so ordinary guest CPUID dispatch
    // decides whether it is reached.
    let vector_done = match builtin {
        Builtin::X86Pshufb128 => {
            let Some(dst) = ret_dst else {
                engine_abort("pshufb128 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::pshufb128(dst, a(0) as *const u8, a(1) as *const u8) };
            true
        }
        Builtin::X86Pshufb256 => {
            let Some(dst) = ret_dst else {
                engine_abort("pshufb256 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::pshufb256(dst, a(0) as *const u8, a(1) as *const u8) };
            true
        }
        Builtin::X86Sha256Msg1 | Builtin::X86Sha256Msg2 => {
            let Some(dst) = ret_dst else {
                engine_abort("sha256msg return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                if matches!(builtin, Builtin::X86Sha256Msg1) {
                    crate::arch::x86_64::sha256msg1(dst, a(0) as *const u8, a(1) as *const u8);
                } else {
                    crate::arch::x86_64::sha256msg2(dst, a(0) as *const u8, a(1) as *const u8);
                }
            }
            true
        }
        Builtin::X86Sha256Rnds2 => {
            let Some(dst) = ret_dst else {
                engine_abort("sha256rnds2 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                crate::arch::x86_64::sha256rnds2(
                    dst,
                    a(0) as *const u8,
                    a(1) as *const u8,
                    a(2) as *const u8,
                );
            }
            true
        }
        Builtin::X86PsadBw128 | Builtin::X86PsadBw256 => {
            let Some(dst) = ret_dst else {
                engine_abort("psad.bw return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                if matches!(builtin, Builtin::X86PsadBw128) {
                    crate::arch::x86_64::psad_bw128(dst, a(0) as *const u8, a(1) as *const u8);
                } else {
                    crate::arch::x86_64::psad_bw256(dst, a(0) as *const u8, a(1) as *const u8);
                }
            }
            true
        }
        Builtin::X86Pclmulqdq => {
            let Some(dst) = ret_dst else {
                engine_abort("pclmulqdq return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe {
                crate::arch::x86_64::pclmulqdq(dst, a(0) as *const u8, a(1) as *const u8, a(2))
            };
            true
        }
        Builtin::X86AesEnc
        | Builtin::X86AesEncLast
        | Builtin::X86AesDec
        | Builtin::X86AesDecLast => {
            let Some(dst) = ret_dst else {
                engine_abort("aesni return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, k) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86AesEnc => crate::arch::x86_64::aesenc(dst, x, k),
                    Builtin::X86AesEncLast => crate::arch::x86_64::aesenclast(dst, x, k),
                    Builtin::X86AesDec => crate::arch::x86_64::aesdec(dst, x, k),
                    _ => crate::arch::x86_64::aesdeclast(dst, x, k),
                }
            }
            true
        }
        Builtin::X86AesImc => {
            let Some(dst) = ret_dst else {
                engine_abort("aesimc return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::aesimc(dst, a(0) as *const u8) };
            true
        }
        Builtin::X86AesKeygenAssist => {
            let Some(dst) = ret_dst else {
                engine_abort("aeskeygenassist return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::aeskeygenassist(dst, a(0) as *const u8, a(1)) };
            true
        }
        Builtin::X86Permd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("permd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            unsafe { crate::arch::x86_64::permd256(dst, a(0) as *const u8, a(1) as *const u8) };
            true
        }
        Builtin::X86PmaddUbSw128
        | Builtin::X86PmaddUbSw256
        | Builtin::X86PmaddWd128
        | Builtin::X86PmaddWd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("pmadd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86PmaddUbSw128 => crate::arch::x86_64::pmaddubsw128(dst, x, y),
                    Builtin::X86PmaddUbSw256 => crate::arch::x86_64::pmaddubsw256(dst, x, y),
                    Builtin::X86PmaddWd128 => crate::arch::x86_64::pmaddwd128(dst, x, y),
                    _ => crate::arch::x86_64::pmaddwd256(dst, x, y),
                }
            }
            true
        }
        Builtin::X86GatherQPd256 | Builtin::X86GatherDPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("gather.pd.256 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            // (src vec, base scalar pointer, vindex vec, mask vec, scale imm)
            unsafe {
                if matches!(builtin, Builtin::X86GatherQPd256) {
                    crate::arch::x86_64::gather_q_pd_256(
                        dst,
                        a(0) as *const u8,
                        a(1),
                        a(2) as *const u8,
                        a(3) as *const u8,
                        a(4),
                    );
                } else {
                    crate::arch::x86_64::gather_d_pd_256(
                        dst,
                        a(0) as *const u8,
                        a(1),
                        a(2) as *const u8,
                        a(3) as *const u8,
                        a(4),
                    );
                }
            }
            true
        }
        Builtin::X86Pmadd52Lo128
        | Builtin::X86Pmadd52Hi128
        | Builtin::X86Pmadd52Lo256
        | Builtin::X86Pmadd52Hi256
        | Builtin::X86Pmadd52Lo512
        | Builtin::X86Pmadd52Hi512 => {
            let Some(dst) = ret_dst else {
                engine_abort("vpmadd52 return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, z) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86Pmadd52Lo128 => {
                        crate::arch::x86_64::vpmadd52::<2, false>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Hi128 => {
                        crate::arch::x86_64::vpmadd52::<2, true>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Lo256 => {
                        crate::arch::x86_64::vpmadd52::<4, false>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Hi256 => {
                        crate::arch::x86_64::vpmadd52::<4, true>(dst, x, y, z)
                    }
                    Builtin::X86Pmadd52Lo512 => {
                        crate::arch::x86_64::vpmadd52::<8, false>(dst, x, y, z)
                    }
                    _ => crate::arch::x86_64::vpmadd52::<8, true>(dst, x, y, z),
                }
            }
            true
        }
        Builtin::X86MaxPs128
        | Builtin::X86MinPs128
        | Builtin::X86MaxPs256
        | Builtin::X86MinPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86MaxPs128 => crate::arch::x86_64::maxmin_ps::<4, true>(dst, x, y),
                    Builtin::X86MinPs128 => crate::arch::x86_64::maxmin_ps::<4, false>(dst, x, y),
                    Builtin::X86MaxPs256 => crate::arch::x86_64::maxmin_ps::<8, true>(dst, x, y),
                    _ => crate::arch::x86_64::maxmin_ps::<8, false>(dst, x, y),
                }
            }
            true
        }
        Builtin::X86MaxSd | Builtin::X86MinSd => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.sd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                if matches!(builtin, Builtin::X86MaxSd) {
                    crate::arch::x86_64::maxmin_pd::<1, true>(dst, x, y)
                } else {
                    crate::arch::x86_64::maxmin_pd::<1, false>(dst, x, y)
                }
            }
            true
        }
        Builtin::X86MaxPd128
        | Builtin::X86MinPd128
        | Builtin::X86MaxPd256
        | Builtin::X86MinPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("max/min.pd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                match builtin {
                    Builtin::X86MaxPd128 => crate::arch::x86_64::maxmin_pd::<2, true>(dst, x, y),
                    Builtin::X86MinPd128 => crate::arch::x86_64::maxmin_pd::<2, false>(dst, x, y),
                    Builtin::X86MaxPd256 => crate::arch::x86_64::maxmin_pd::<4, true>(dst, x, y),
                    _ => crate::arch::x86_64::maxmin_pd::<4, false>(dst, x, y),
                }
            }
            true
        }
        Builtin::X86CmpPs128 | Builtin::X86CmpPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cmp.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, imm) = (a(0) as *const u8, a(1) as *const u8, a(2));
            unsafe {
                if matches!(builtin, Builtin::X86CmpPs128) {
                    crate::arch::x86_64::cmp_ps::<4>(dst, x, y, imm)
                } else {
                    crate::arch::x86_64::cmp_ps::<8>(dst, x, y, imm)
                }
            }
            true
        }
        Builtin::X86CmpPd128 | Builtin::X86CmpPd256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cmp.pd return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, imm) = (a(0) as *const u8, a(1) as *const u8, a(2));
            unsafe {
                if matches!(builtin, Builtin::X86CmpPd128) {
                    crate::arch::x86_64::cmp_pd::<2>(dst, x, y, imm)
                } else {
                    crate::arch::x86_64::cmp_pd::<4>(dst, x, y, imm)
                }
            }
            true
        }
        Builtin::X86RoundPs128 | Builtin::X86RoundPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("round.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, imm) = (a(0) as *const u8, a(1));
            unsafe {
                if matches!(builtin, Builtin::X86RoundPs128) {
                    crate::arch::x86_64::round_ps::<4>(dst, x, imm)
                } else {
                    crate::arch::x86_64::round_ps::<8>(dst, x, imm)
                }
            }
            true
        }
        Builtin::X86CvtPs2dq128
        | Builtin::X86CvttPs2dq128
        | Builtin::X86CvtPs2dq256
        | Builtin::X86CvttPs2dq256 => {
            let Some(dst) = ret_dst else {
                engine_abort("cvt(t).ps2dq return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let x = a(0) as *const u8;
            unsafe {
                match builtin {
                    Builtin::X86CvtPs2dq128 => crate::arch::x86_64::cvt_ps2dq::<4, false>(dst, x),
                    Builtin::X86CvttPs2dq128 => crate::arch::x86_64::cvt_ps2dq::<4, true>(dst, x),
                    Builtin::X86CvtPs2dq256 => crate::arch::x86_64::cvt_ps2dq::<8, false>(dst, x),
                    _ => crate::arch::x86_64::cvt_ps2dq::<8, true>(dst, x),
                }
            }
            true
        }
        Builtin::X86BlendvPs128 | Builtin::X86BlendvPs256 => {
            let Some(dst) = ret_dst else {
                engine_abort("blendv.ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, y, m) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
            unsafe {
                if matches!(builtin, Builtin::X86BlendvPs128) {
                    crate::arch::x86_64::blendv_ps::<4>(dst, x, y, m)
                } else {
                    crate::arch::x86_64::blendv_ps::<8>(dst, x, y, m)
                }
            }
            true
        }
        Builtin::X86Lddqu128 | Builtin::X86Lddqu256 => {
            let Some(dst) = ret_dst else {
                engine_abort("lddqu return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let src = a(0) as *const u8;
            unsafe {
                if matches!(builtin, Builtin::X86Lddqu128) {
                    crate::arch::x86_64::lddqu::<16>(dst, src)
                } else {
                    crate::arch::x86_64::lddqu::<32>(dst, src)
                }
            }
            true
        }
        Builtin::X86Cvtps2ph128 | Builtin::X86Cvtps2ph256 => {
            let Some(dst) = ret_dst else {
                engine_abort("vcvtps2ph return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, imm) = (a(0) as *const u8, a(1));
            unsafe {
                if matches!(builtin, Builtin::X86Cvtps2ph128) {
                    crate::arch::x86_64::cvtps2ph::<4>(dst, x, imm)
                } else {
                    crate::arch::x86_64::cvtps2ph::<8>(dst, x, imm)
                }
            }
            true
        }
        Builtin::X86Cvtph2ps128 | Builtin::X86Cvtph2ps256 => {
            let Some(dst) = ret_dst else {
                engine_abort("vcvtph2ps return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let x = a(0) as *const u8;
            unsafe {
                if matches!(builtin, Builtin::X86Cvtph2ps128) {
                    crate::arch::x86_64::cvtph2ps::<4>(dst, x)
                } else {
                    crate::arch::x86_64::cvtph2ps::<8>(dst, x)
                }
            }
            true
        }
        Builtin::X86PsllD128 | Builtin::X86PsrlD128 => {
            let Some(dst) = ret_dst else {
                engine_abort("ps{l,r}l.d return form is not an indirect vector");
            };
            let dst = dst as *mut u8;
            let (x, c) = (a(0) as *const u8, a(1) as *const u8);
            unsafe {
                if matches!(builtin, Builtin::X86PsllD128) {
                    crate::arch::x86_64::pshift32::<4, true>(dst, x, c)
                } else {
                    crate::arch::x86_64::pshift32::<4, false>(dst, x, c)
                }
            }
            true
        }
        _ => false,
    };
    if vector_done {
        edge.set(None);
        return (0, 0);
    }
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
        // Allocation sentinel: no-op.
        Builtin::NoAllocShim => 0,
        // Managed Rust heap (mimalloc backend; real addresses go straight out). Once a custom
        // #[global_allocator] is registered in this module, allocation is a program-level
        // semantic: every image's builtin arm routes through the guest shim to the user's
        // allocator, because a cross-heap free otherwise corrupts mimalloc metadata
        // (SIGSEGV).
        Builtin::RustAlloc => match module.custom_alloc_shims {
            Some(s) => {
                let (lo, _) =
                    call_guarding_terminate(unwind, || call_guest(ctx, s.alloc, &[a(0), a(1)]));
                lo
            }
            None => crate::vm::heap::alloc(a(0), a(1)),
        },
        Builtin::RustAllocZeroed => match module.custom_alloc_shims {
            Some(s) => {
                let (lo, _) = call_guarding_terminate(unwind, || {
                    call_guest(ctx, s.alloc_zeroed, &[a(0), a(1)])
                });
                lo
            }
            None => crate::vm::heap::alloc_zeroed(a(0), a(1)),
        },
        Builtin::RustRealloc => match module.custom_alloc_shims {
            Some(s) => {
                let (lo, _) = call_guarding_terminate(unwind, || {
                    call_guest(ctx, s.realloc, &[a(0), a(1), a(2), a(3)])
                });
                lo
            }
            None => crate::vm::heap::realloc(a(0), a(1), a(2), a(3)),
        },
        Builtin::RustDealloc => {
            match module.custom_alloc_shims {
                Some(s) => {
                    let _ = call_guarding_terminate(unwind, || {
                        call_guest(ctx, s.dealloc, &[a(0), a(1), a(2)])
                    });
                }
                None => crate::vm::heap::dealloc(a(0), a(1), a(2)),
            }
            0
        }
        // Unwind primitive (raise): the host unwinder carries the guest exception pointer.
        Builtin::UnwindRaise => raise_guest(a(0)),
        // Minimal `os::` passthroughs: real addresses, no marshalling.
        Builtin::HostGetenv => crate::os::process::getenv(a(0)),
        Builtin::HostWrite => crate::os::process::write_fd(a(0) as i32, a(1), a(2) as usize) as u64,
        Builtin::HostStrlen => crate::os::process::c_strlen(a(0)),
        Builtin::HostAbort => std::process::abort(),
        // fork: allowed only when the guest is single-threaded (the child is a whole-process
        // copy, so interpreter state is consistent by construction, and with no other guest
        // threads there is no cross-thread lock to deadlock on). A multithreaded fork is
        // rejected loudly -- it is just as much a minefield under native. The exec family goes
        // through the foreign path, not here.
        Builtin::HostFork => {
            if unsafe { crate::vm::ctx::guest_spawned_threads(ctx) } {
                engine_abort(
                    "fork() with guest-spawned threads: after a multithreaded fork only the \
                     forking thread survives and locks held by other threads stay locked \
                     forever in the child (UB under native too). Only a single-threaded guest \
                     is allowed through",
                );
            }
            let pid = crate::os::process::fork();
            if pid == 0 {
                // The child has no writer thread and must never publish into
                // the copied parent generation. This hook is store-only and
                // runs before any JIT service is restarted.
                crate::telemetry::capture::after_fork_child();
                // In the child the compiler thread does not survive fork. The publish wait of
                // the SYNC verification mode (MIRVM_JIT_SYNC) depends on a live compilation
                // service, so restart it: inherited published code pages, slot tables and
                // eh_frames stay valid while the queue and worker are replaced. A non-sync
                // child keeps the unchanged interpret-as-fallback semantics.
                #[cfg(feature = "cranelift")]
                if unsafe { &*(*ctx).shared }.jit.sync {
                    let shared = unsafe { (*ctx).shared_arc() };
                    crate::vm::jit::start(&shared);
                }
            }
            pid as u64
        }
        // atexit family: registers a guest callback and returns 0 (success).
        // __cxa_atexit(fn, arg, dso) calls fn(arg); on_exit(fn, arg) calls fn(status, arg);
        // atexit(fn) calls fn with no arguments. All are stored uniformly as (fn, kind, arg).
        Builtin::HostAtexit => atexit_register(ctx, a(0), AtexitKind::Plain, 0),
        Builtin::HostCxaAtexit => atexit_register(ctx, a(0), AtexitKind::CxaArg, a(1)),
        Builtin::HostOnExit => atexit_register(ctx, a(0), AtexitKind::OnExit, a(1)),
        Builtin::HostSignal => {
            let (signum, handler) = (a(0) as i32, a(1) as usize);
            let resolution =
                if handler == crate::os::signal::SIG_DFL || handler == crate::os::signal::SIG_IGN {
                    crate::vm::thunks::SignalHandlerResolution::Unknown
                } else {
                    resolve_signal_handler(ctx, handler as u64)
                };
            let control = unsafe { (*ctx).shared().control() };
            match crate::vm::signal::install_signal_resolved(control, signum, handler, resolution) {
                Ok(old) => old as u64,
                Err(error) => {
                    if let Some(errno) = error.libc_errno() {
                        crate::os::process::set_errno(errno);
                        crate::os::signal::SIG_ERR as u64
                    } else {
                        engine_abort(&error.to_string())
                    }
                }
            }
        }
        Builtin::HostRaise => crate::vm::ctx::raise_signal(ctx, a(0) as i32) as u64,
        Builtin::HostSigaction => {
            let (signum, act, oldact) = (a(0) as i32, a(1), a(2));
            let action = unsafe { crate::os::signal::Sigaction::copy_from(act) };
            let resolution = action.as_ref().map(|action| {
                let handler = action.handler();
                if handler == crate::os::signal::SIG_DFL || handler == crate::os::signal::SIG_IGN {
                    crate::vm::thunks::SignalHandlerResolution::Unknown
                } else {
                    resolve_signal_handler(ctx, handler as u64)
                }
            });
            let control = unsafe { (*ctx).shared().control() };
            match crate::vm::signal::install_sigaction_resolved(
                control, signum, action, resolution, oldact,
            ) {
                Ok(result) => result as u64,
                Err(error) => {
                    if let Some(errno) = error.libc_errno() {
                        crate::os::process::set_errno(errno);
                        (-1i32) as u64
                    } else {
                        engine_abort(&error.to_string())
                    }
                }
            }
        }
        Builtin::Unsupported(name) => engine_abort(&format!("unsupported builtin `{}`", name.0)),
        Builtin::UnwindDeleteException => {
            // Itanium `_Unwind_Exception`: exception_class @0, cleanup fn @8. A guest panic's
            // cleanup is a frozen fn entry, but a foreign exception may carry a native cleanup
            // too, so the address domain picks interpretation or native FFI.
            let exc = a(0);
            let cleanup = mem_read(exc + 8, Width::W64);
            if cleanup != 0 {
                let cav = [1, exc]; // _URC_FOREIGN_EXCEPTION_CAUGHT
                if module.fn_addrs.contains_key(&cleanup) {
                    call_fn_addr(ctx, cleanup, &cav, "_Unwind_DeleteException");
                } else {
                    let sig = crate::vm::ir::ForeignSig {
                        args: vec![FfiKind::I32, FfiKind::Ptr],
                        ret: FfiKind::Void,
                        fixed: None,
                        thunk_args: vec![],
                        unwind: false,
                    };
                    crate::vm::ffi::call_addr(cleanup as usize, &sig, &cav, None);
                }
            }
            0
        }
        // backtrace shadow frames
        Builtin::UnwindBacktrace => unwind_backtrace(ctx, a(0), a(1)),
        Builtin::UnwindGetIp => mem_read(a(0), Width::W64),
        Builtin::UnwindGetIpInfo => {
            // (ctx, *ip_before_insn) -> IP; *ip_before_insn = 0 (a synthetic frame has no
            // such distinction)
            if a(1) != 0 {
                mem_write(a(1), Width::W32, 0);
            }
            mem_read(a(0), Width::W64)
        }
        Builtin::UnwindGetCfa => mem_read(a(0) + 8, Width::W64),
        // A synthetic IP is the function entry, so return ip itself (the enclosing fn start).
        Builtin::UnwindFindEnclosing => a(0),
        Builtin::CpuHintNop => 0,
        Builtin::Breakpoint => {
            // Real int3: when not being traced this terminates with SIGTRAP (native semantics).
            crate::arch::x86_64::asmstub::int3();
            0
        }
        Builtin::AddCarry64 => unreachable!("addcarry.64 is handled by the pair lane"),
        Builtin::SubBorrow64 => unreachable!("subborrow.64 is handled by the pair lane"),
        Builtin::Xgetbv => crate::arch::x86_64::asmstub::xgetbv(a(0) as u32),
        Builtin::X86Crc32U8 => unsafe {
            u64::from(crate::arch::x86_64::crc32_u8(a(0) as u32, a(1) as u8))
        },
        Builtin::X86Crc32U16 => unsafe {
            u64::from(crate::arch::x86_64::crc32_u16(a(0) as u32, a(1) as u16))
        },
        Builtin::X86Crc32U32 => unsafe {
            u64::from(crate::arch::x86_64::crc32_u32(a(0) as u32, a(1) as u32))
        },
        Builtin::X86Crc32U64 => unsafe { crate::arch::x86_64::crc32_u64(a(0), a(1)) },
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
        | Builtin::X86PsrlD128 => {
            unreachable!("x86 vector builtins are handled by the indirect vector lane")
        }
        Builtin::HostSyscall => crate::os::process::syscall(a(0) as i64, &av[1..]) as u64,
        Builtin::HostSyscallTrace => {
            crate::telemetry::capture::host_syscall(a(0) as i64, &av[1..]) as u64
        }
        // rust_try: a raw unwinder catch. Only a guest panic owned by the current Engine
        // calls catch_fn(data, exc) and returns 1; foreign or host exceptions keep unwinding.
        Builtin::CatchUnwind => {
            let (try_fn, data, catch_fn) = (a(0), a(1), a(2));
            let shared = unsafe { (*ctx).shared_arc() };
            let mut main_catch = crate::vm::ctx::claim_main_panic_catch(ctx, role);
            match crate::vm::unwind::catch_raw(|| {
                call_fn_addr(ctx, try_fn, &[data], "catch_unwind.try")
            }) {
                Ok(_) => 0,
                Err(exception) => match exception.at_guest_catch(&shared) {
                    crate::vm::unwind::GuestCatchDisposition::Guest(payload) => {
                        if let Some(main_catch) = &mut main_catch {
                            main_catch.mark_panicked();
                        }
                        payload.transfer(|_, inner| {
                            call_fn_addr(ctx, catch_fn, &[data, inner], "catch_unwind.catch");
                            1
                        })
                    }
                    crate::vm::unwind::GuestCatchDisposition::Resume(exception) => {
                        exception.resume_or_rethrow()
                    }
                },
            }
        }
    };
    edge.set(None);
    (r, 0)
}
