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
//! Inbound FFI marshalling lives here too: a native caller's arguments arrive in the C shape
//! (scalars as-is, aggregates as the true address of their bytes) and are expanded to ABI
//! argument slots before the same entry point is used.
//!
//! The mutual reference with `interp` is deliberate and this narrow: the interpreted fallback
//! is `interp::interp_frame`, and the interpreter performs its own calls through
//! [`call_guest`]. Neither reaches any other part of the other.

use crate::vm::ctx::{Ctx, current_code_domain};
use crate::vm::ir::{FfiAgg, FfiKind, FfiLeaf, FuncBody, Module, ParamAbi, RetAbi};
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
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let Some(&fid) = module.fn_addrs.get(&addr) else {
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
unsafe fn agg_leaf_at(addr: u64, agg: &FfiAgg, idx: usize) -> u64 {
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
