//! The interpreted frame: `interp_frame` (model A: host recursion with a real stack byte guard)
//! and `run_cleanup`, which runs a landing pad's cleanup chain.
//!
//! Choosing between compiled code and this frame is [`crate::vm::dispatch`]; the interpreter
//! reaches it through `call_guest`, the same entry every other caller uses, and the builtin
//! bodies it executes are [`crate::vm::semantics::builtin`].

use super::runblocks::run_blocks;
use super::*;

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
            ip: crate::vm::backtrace::func_synth_ip(ctx, func),
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
