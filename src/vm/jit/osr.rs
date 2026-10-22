//! Sparse interpreter backedge polling and the packed continuation boundary.

use crate::vm::ctx::Ctx;
use crate::vm::ir::{Bb, FuncBody};

#[cfg(not(feature = "cranelift"))]
pub(crate) fn try_enter_osr(
    _ctx: *mut Ctx,
    _func: u32,
    _body: &FuncBody,
    _base: usize,
    _bb: Bb,
) -> Option<(u64, u64)> {
    None
}

#[cfg(feature = "cranelift")]
pub(crate) fn try_enter_osr(
    ctx: *mut Ctx,
    func: u32,
    body: &FuncBody,
    base: usize,
    bb: Bb,
) -> Option<(u64, u64)> {
    use super::*;
    let jit = unsafe { &(*ctx).shared.jit };
    if !jit.enabled || !jit.osr_enabled {
        return None;
    }
    let domain = crate::vm::ctx::current_code_domain();
    let entry = jit.slots_for(domain).osr[func as usize].load(Ordering::Acquire);
    if entry == 0 {
        jit.request_osr(domain, func);
        return None;
    }
    if entry == OSR_QUEUED || entry == FAIL_SENTINEL {
        return None;
    }
    // Leave slack for the packed boundary before it performs its own guard. Failing this
    // opportunistic check leaves the existing interpreter activation running.
    let marker = 0u8;
    let need = guard::required_headroom(ir::FuncHeader::Owned(body)) as usize
        + helpers::STACK_GUARD_MARGIN;
    if (&marker as *const u8 as usize).saturating_sub(unsafe { (*ctx).stack_floor }) <= need {
        return None;
    }
    let args = [base as u64, u64::from(bb)];
    let mut ret = [0u64; 2];
    let trace = if domain == CodeDomain::Trace {
        let producer = crate::telemetry::capture::current_producer();
        let trampoline = jit.trace_enter.load(Ordering::Acquire);
        if producer.is_null() || trampoline == 0 {
            return None;
        }
        Some((trampoline, producer))
    } else {
        None
    };
    jit.osr_transfers.fetch_add(1, Ordering::Relaxed);
    helpers::stat(helpers::S_OSR);
    if crate::options::jit_debug() {
        crate::diag_debug!(Jit, "OSR f{func}: transfer at bb{bb} ({domain:?})");
    }
    if let Some((trampoline, producer)) = trace {
        Some(unsafe { call_trace_body(trampoline, producer, entry, &args, &mut ret) })
    } else {
        if crate::os_arch::PLAIN_CODE_PINS_CTX {
            let f: extern "C-unwind" fn(*mut Ctx, *const u64, *mut u64) =
                unsafe { std::mem::transmute(entry as usize) };
            f(ctx, args.as_ptr(), ret.as_mut_ptr());
        } else {
            let f: extern "C-unwind" fn(*const u64, *mut u64) =
                unsafe { std::mem::transmute(entry as usize) };
            f(args.as_ptr(), ret.as_mut_ptr());
        }
        Some((ret[0], ret[1]))
    }
}
