//! Interpreter runtime services: signal-handler entry resolution, backtrace frame
//! merging with symbol IPs, and the atexit family (a per-Engine registry with LIFO
//! callback execution).

use super::call::call_fn_addr;
use super::*;
use crate::os::unwind;

/// Resolves a guest signal-handler address to its AS-trampoline code address.
///
/// An asynchronous signal is safe to run to completion and reuses the thunk factory
/// (attach + `interp_frame`, signature `(i32) -> void`). A guest handler for a synchronous
/// fault signal (SEGV/BUS/FPE/ILL/TRAP) cannot be honoured -- a host fault is
/// indistinguishable from a guest fault, so faking recovery would silently produce wrong
/// values -- and the installer refuses it loudly. The handler must be a known guest fn entry;
/// a non-guest address is not accepted.
pub(super) fn resolve_signal_handler(
    ctx: *mut Ctx,
    handler: u64,
) -> crate::vm::thunks::SignalHandlerResolution {
    let shared: &'static Shared = unsafe { &*(*ctx).shared };
    crate::vm::thunks::resolve_signal_handler(shared, handler)
}

// ===== backtrace shadow frames =====
/// Conservative fallback IP when no ELF symbol image is available: high above the user
/// address space and not page-aligned, so it cannot collide with a real code or data
/// address. A normally loaded Engine uses a symbolizable ELF address instead.
const FUNC_IP_BASE: u64 = 0x5f5f_0000_0000_0000;
fn fallback_func_ip(func: u32) -> u64 {
    FUNC_IP_BASE + (func as u64) * 64
}

pub(super) fn func_synth_ip(ctx: *mut Ctx, func: u32) -> u64 {
    unsafe { &*(*ctx).shared }
        .module
        .backtrace_ips
        .get(func as usize)
        .copied()
        .unwrap_or_else(|| fallback_func_ip(func))
}

#[repr(C)]
struct GuestUnwindContext {
    ip: u64,
    cfa: u64,
}

#[repr(C)]
struct HostFrame {
    ip: u64,
    cfa: u64,
}

extern "C" fn collect_host_frame(ctx: unwind::Context, arg: unwind::Context) -> i32 {
    let frames = unsafe { &mut *(arg as *mut Vec<HostFrame>) };
    frames.push(HostFrame {
        ip: unsafe { unwind::frame_ip(ctx) } as u64,
        cfa: unsafe { unwind::frame_cfa(ctx) } as u64,
    });
    0
}

/// `_Unwind_Backtrace(trace_fn, arg)`: the system unwinder reads the live JIT machine
/// frames, which are then merged with the interpreter's shadow frames by host stack
/// position. The callback still only ever sees a controlled guest context, never engine
/// host frames.
pub(super) fn unwind_backtrace(ctx: *mut Ctx, trace_fn: u64, arg: u64) -> u64 {
    let shared = unsafe { &*(*ctx).shared };
    let mut host: Vec<HostFrame> = Vec::new();
    unsafe {
        unwind::backtrace(
            collect_host_frame,
            &mut host as *mut Vec<HostFrame> as unwind::Context,
        );
    }

    // The callback may re-enter the guest and change the live stack, so snapshot everything
    // first. The x86_64 stack grows down, so a smaller CFA is an inner frame; a JIT guest
    // call registers only its fast body, so wrapper frames never appear twice.
    let mut frames: Vec<GuestUnwindContext> = unsafe {
        (*ctx)
            .shadow
            .iter()
            .map(|frame| GuestUnwindContext {
                ip: frame.ip,
                cfa: frame.cfa,
            })
            .collect()
    };
    frames.extend(host.into_iter().filter_map(|frame| {
        shared
            .jit
            .guest_func_at(frame.ip.saturating_sub(1))
            .map(|func| GuestUnwindContext {
                ip: func_synth_ip(ctx, func),
                cfa: frame.cfa,
            })
    }));
    frames.sort_unstable_by_key(|frame| frame.cfa);

    // The standard implementation trims the guest frame that is currently calling
    // _Unwind_Backtrace. Our synthetic symbol address cannot be compared directly with its
    // entry pointer, so skip the innermost guest frame here instead.
    for frame in frames.into_iter().skip(1) {
        let frame_ptr = &frame as *const GuestUnwindContext as u64;
        let r = call_fn_addr(ctx, trace_fn, &[frame_ptr, arg], "_Unwind_Backtrace").0;
        if r != 0 {
            break; // _URC_FOREIGN_EXCEPTION_CAUGHT / _URC_FAILURE and friends: stop
        }
    }
    5 // _URC_END_OF_STACK
}

// ===== atexit family =====
// glibc does not export `atexit` for a guest dlsym, so the engine keeps its own LIFO
// registry plus one native trampoline, mounted through the libc `atexit` the engine itself
// links against (not through dlsym). At process teardown libc calls the trampoline on the
// main thread, which interprets the guest callbacks one by one in LIFO order under a fresh
// Ctx attach.
#[derive(Clone, Copy)]
pub(super) enum AtexitKind {
    Plain,  // atexit: fn()
    CxaArg, // __cxa_atexit: fn(arg)
    OnExit, // on_exit: fn(status=0, arg)
}
pub(super) struct AtexitEntry {
    func: u64,
    kind: AtexitKind,
    arg: u64,
}
static ATEXIT: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, Vec<AtexitEntry>>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

pub(super) fn discard_atexit_callbacks(engine_id: u64) {
    ATEXIT.lock().unwrap().remove(&(engine_id as usize));
}

#[cfg(test)]
pub(super) fn seed_atexit_callback(engine_id: u64) {
    ATEXIT
        .lock()
        .unwrap()
        .entry(engine_id as usize)
        .or_default()
        .push(AtexitEntry {
            func: 0,
            kind: AtexitKind::Plain,
            arg: 0,
        });
}

#[cfg(test)]
pub(super) fn has_atexit_callbacks(engine_id: u64) -> bool {
    ATEXIT.lock().unwrap().contains_key(&(engine_id as usize))
}

pub(super) fn atexit_register(ctx: *mut Ctx, func: u64, kind: AtexitKind, arg: u64) -> u64 {
    // fn must be a known guest entry; a non-guest callback is refused, not silently dropped.
    let shared = unsafe { &*(*ctx).shared };
    let module: &Module = &shared.module;
    if !module.fn_addrs.contains_key(&func) {
        engine_abort(&format!(
            "atexit callback {func:#x} is not a known guest fn entry"
        ));
    }
    let mut reg = ATEXIT.lock().unwrap();
    reg.entry(shared.id as usize)
        .or_default()
        .push(AtexitEntry { func, kind, arg });
    0
}

/// Virtual process teardown for this Engine: runs its own guest callbacks in LIFO order.
pub(super) fn run_atexit_callbacks(ctx: *mut Ctx, status: i32) {
    let shared = unsafe { &*(*ctx).shared };
    let key = shared.id as usize;
    // LIFO: the last registered callback runs first (C semantics).
    loop {
        let entry = {
            let mut reg = ATEXIT.lock().unwrap();
            let entry = reg.get_mut(&key).and_then(Vec::pop);
            if reg.get(&key).is_some_and(Vec::is_empty) {
                reg.remove(&key);
            }
            entry
        };
        let Some(entry) = entry else { break };
        let args: &[u64] = match entry.kind {
            AtexitKind::Plain => &[],
            AtexitKind::CxaArg => &[entry.arg],
            AtexitKind::OnExit => &[status as u64, entry.arg],
        };
        // A guest callback panic escaping into the C exit path aborts, as under native.
        let _ = call_fn_addr(ctx, entry.func, args, "atexit");
    }
}
