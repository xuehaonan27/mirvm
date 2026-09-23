//! Entering guest code: the one place the backend is chosen.
//!
//! Every guest call in the engine arrives here -- a `Call`/`CallIndirect` terminator,
//! `run_main`/`run_export`, the c2i helper a compiled body uses to call an uncompiled callee, a
//! native callback re-entering the guest, and the thunk trampolines. A non-zero published slot
//! means compiled code (the packed i2c entry once published) and is called directly; a zero
//! slot counts the call and interprets it. The counter uses Relaxed ordering because a lost
//! count only moves the compilation trigger, while the slot load uses Acquire against the
//! compiler thread's Release.
//!
//! A native caller's arguments arrive in the C shape, so they are converted before the same
//! entry point is used; that conversion is [`crate::vm::ffi::inbound`]'s, not this module's.
//!
//! The mutual reference with `interp` is deliberate and this narrow: the interpreted fallback
//! is `interp::interp_frame`, and the interpreter performs its own calls through
//! [`call_guest`]. Neither reaches any other part of the other.

use crate::vm::ctx::{Ctx, current_code_domain};
use crate::vm::instance::Instance;
use crate::vm::ir::{Module, RetAbi};
use crate::vm::jit::{CodeDomain, FAIL_SENTINEL, call_trace_body};
use crate::vm::unwind::engine_abort;

/// Calls the guest function `func` with already-flattened arguments (a pair takes 2 slots, an
/// indirect argument passes its address) and returns `(lo, hi)`.
pub(crate) fn call_guest(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let jit = unsafe { &(*(*ctx).shared).jit };
    if jit.enabled {
        let call_compiled = |entry: u64| -> (u64, u64) {
            // i2c: the packed entry (published fast -> packed, and the Acquire load has
            // already seen every preceding write)
            type Packed = extern "C-unwind" fn(*const u64, *mut u64);
            let f: Packed = unsafe { std::mem::transmute(entry as usize) };
            let mut ret = [0u64; 2];
            f(args.as_ptr(), ret.as_mut_ptr());
            (ret[0], ret[1])
        };
        // Strict failure sentinel (MIRVM_JIT_SYNC): an admissible function that failed to
        // compile aborts loudly at every later call site. The worker writes the sentinel only
        // in sync mode, so it never appears otherwise.
        let fail_abort = |ctx: *mut Ctx| -> ! {
            let shared = unsafe { &*(*ctx).shared };
            engine_abort(&format!(
                "JIT strict: f{func}({}) meets compilation threshold but failed to be compiled",
                shared.module.funcs[func as usize].name
            ))
        };
        // Dispatch into the current activation's code domain. This is the single
        // point where a trace run stops consulting plain entries; the plain arm
        // borrows the historical fields, so the plain path is unchanged.
        let domain = current_code_domain();
        let domain_slots = jit.slots_for(domain);
        // A trace body addresses recorder state through the register the
        // activation boundary pinned. It may therefore only be entered through
        // that boundary: if this thread has no recorder, or the trace domain has
        // not published its boundary yet (the JIT worker installs it
        // asynchronously), the interpreter -- which records through TLS -- is the
        // correct way to run the function, not a raw entry.
        let enter_trace = |entry: u64| -> Option<(u64, u64)> {
            if domain != CodeDomain::Trace {
                return None;
            }
            let producer = crate::telemetry::capture::current_producer();
            let trampoline = jit.trace_enter.load(std::sync::atomic::Ordering::Acquire);
            if producer.is_null() || trampoline == 0 {
                return None;
            }
            let mut ret = [0u64; 2];
            Some(unsafe { call_trace_body(trampoline, producer, entry, args, &mut ret) })
        };
        // If there's compiled code, then call it
        let mut entry =
            domain_slots.slots[func as usize].load(std::sync::atomic::Ordering::Acquire);
        if entry == FAIL_SENTINEL && jit.sync {
            fail_abort(ctx);
        }
        if entry != 0 && entry != FAIL_SENTINEL {
            match enter_trace(entry) {
                Some(ret) => return ret,
                None if domain == CodeDomain::Plain => {
                    return call_compiled(entry);
                }
                None => {}
            }
        }

        // If function not compiled yet, collect statistics, may send compilation request
        // and interpret it for now.
        let prev = jit.counters[func as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Meets compilation threshold and only send compilation request exactly once.
        // Counter keeps growing later but compilation request would not be send multiple times.
        if prev + 1 == jit.threshold
            && let Some(q) = jit.queue.lock().unwrap().as_ref()
        {
            let _ = q.send(func);
        }
        // SYNC verification mode: once submitted (now or earlier), wait for publication or
        // the failure sentinel. With threshold = 1 this turns "request compilation on the
        // first call" into "compile and publish synchronously on the first call", so the
        // compile-on-every-call differential proves compiled code really ran.
        if jit.sync && prev + 1 >= jit.threshold {
            let mut spins = 0u32;
            loop {
                entry =
                    domain_slots.slots[func as usize].load(std::sync::atomic::Ordering::Acquire);
                if entry == FAIL_SENTINEL {
                    fail_abort(ctx);
                }
                if entry != 0 {
                    match enter_trace(entry) {
                        Some(ret) => return ret,
                        None if domain == CodeDomain::Plain => {
                            return call_compiled(entry);
                        }
                        None => break,
                    }
                }
                spins += 1;
                if spins >= 1 << 28 {
                    engine_abort(
                        "JIT strict release timeout (compiler thread dead or queue broken)",
                    );
                }
                std::thread::yield_now();
            }
        }
    }
    crate::vm::interp::interp_frame(ctx, func, args)
}

/// Calls the guest function whose entry address is `addr`. A target that is not a known guest
/// entry breaks an engine invariant, and `caller` names the site that asked.
pub(crate) fn call_fn_addr(ctx: *mut Ctx, addr: u64, args: &[u64], caller: &str) -> (u64, u64) {
    let instance: &Instance = unsafe { &(*(*ctx).shared).instance };
    let Some(&fid) = instance.fn_addrs.get(&addr) else {
        engine_abort(&format!(
            "indirect call target {addr:#x} is not a known fn entry (caller {caller})"
        ));
    };
    call_guest(ctx, fid, args)
}

/// The return shape a call to `func` must be written back under.
#[inline]
pub(crate) fn ret_abi_of(ctx: *mut Ctx, func: u32) -> RetAbi {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    module.funcs[func as usize].ret
}
