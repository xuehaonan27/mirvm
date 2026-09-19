//! Deferred signal delivery: run target-pthread callbacks at VM safe points and preserve
//! libc `raise(3)` as the signal's linearization point.

use std::sync::Arc;

use super::activation::activate;
use super::engine::{ExecutionLease, Shared};
use super::thread_ctx::{
    CTX_KEY, CloseSignalDrainGuard, Ctx, SignalDrainGuard, SignalMaskGuard, ThreadContexts,
    current_thread_contexts,
};

const SIGNAL_DRAIN_ROUNDS: usize = 8;

impl Drop for SignalDrainGuard {
    fn drop(&mut self) {
        unsafe { (*self.ctx).signal_draining = false };
    }
}

/// Deliver registered signals in ordinary VM state. Each round first takes one traditional signal
/// pending set; new signals produced by the handler enter the next round. Rounds are bounded, and
/// remaining events are left for the next block entry or return safepoint.
pub(crate) fn drain_pending_signals(ctx: *mut Ctx) {
    drain_pending_signals_with_mode(ctx, false);
}

/// Close has already detached these registrations from the kernel. Events in
/// the owner inbox were accepted by an earlier fixed-stub frame, so a worker's
/// inherited pthread mask must not strand them. This changes only MIRVM's
/// choice of an inbox event; it never unblocks a kernel signal or mutates the
/// caller's real mask.
pub(super) fn drain_pending_signals_for_close(ctx: *mut Ctx) {
    drain_pending_signals_with_mode(ctx, true);
}

fn drain_pending_signals_with_mode(ctx: *mut Ctx, closing: bool) {
    if unsafe { (*ctx).signal_draining } {
        return;
    }
    unsafe { (*ctx).signal_draining = true };
    let _draining = SignalDrainGuard { ctx };
    let contexts = current_thread_contexts(ctx);
    let _close_drain = CloseSignalDrainGuard::enter(contexts, closing);
    let shared = unsafe { (*ctx).shared_arc() };
    for _ in 0..SIGNAL_DRAIN_ROUNDS {
        let mut progressed = false;
        let blocked = if closing {
            unsafe { (*contexts).signal_mask }
        } else {
            current_thread_signal_mask(contexts)
        };
        if let Some((delivery, signum)) =
            super::super::signal::take_current_thread_delivery(blocked)
        {
            progressed = true;
            dispatch_signal_delivery_for_ctx(ctx, delivery, signum);
        }
        for signum in 1..super::super::signal::SIGNAL_SLOTS {
            if blocked & (1u64 << signum) != 0 {
                continue;
            }
            let Some(delivery) = shared.control.signal_inbox.take_delivery(signum) else {
                continue;
            };
            progressed = true;
            dispatch_signal_delivery_for_ctx(ctx, delivery, signum as i32);
        }
        if !progressed {
            break;
        }
    }
}

struct CloseSignalUnblockGuard {
    previous: libc::sigset_t,
}

impl Drop for CloseSignalUnblockGuard {
    fn drop(&mut self) {
        let error = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if error != 0 {
            eprintln!("mirvm[m4-engine]: failed to restore close finalizer signal mask: {error}");
            std::process::abort();
        }
    }
}

fn raise_from_close_drain(ctx: *mut Ctx, signum: i32) -> i32 {
    let mut signal: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut signal);
        libc::sigaddset(&mut signal, signum);
    }
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    let error = unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &signal, &mut previous) };
    if error != 0 {
        super::super::interp::engine_abort(&format!(
            "failed to unblock signal {signum} on the close finalizer thread: {error}"
        ));
    }
    let _restore = CloseSignalUnblockGuard { previous };
    real_raise_and_drain(ctx, signum)
}

pub(super) fn dispatch_signal_delivery(
    delivery: super::super::signal::SignalDeliveryGuard,
    signum: i32,
) -> Result<(), super::super::unwind::EngineFaultReport> {
    let registration = delivery.registration();
    let lease =
        ExecutionLease::for_registered_callback(registration.control()).unwrap_or_else(|_| {
            super::super::interp::engine_abort("signal handler belongs to a closed Engine")
        });
    let activation = activate(lease.shared());
    let handler_ctx = activation.ctx();
    let bit = 1u64 << signum;
    let previous = unsafe { (*activation.contexts).signal_mask };
    if previous & bit != 0 {
        drop(activation);
        drop(lease);
        registration.defer(signum);
        return Ok(());
    }
    // Declare the physical guard before the logical guard. If an EngineFault
    // leaves the callback, reverse-order unwinding restores the logical mask
    // first and the real pthread mask second; ActivationGuard may only start a
    // queued finalizer after both have been restored.
    let host_mask = registration
        .action()
        .block_for_handler(signum)
        .unwrap_or_else(|error| {
            super::super::interp::engine_abort(&format!(
                "failed to apply signal {signum} handler mask to the host thread: {error}"
            ))
        });
    unsafe {
        (*activation.contexts).signal_mask = previous | bit | registration.mask_bits();
    }
    let mask = SignalMaskGuard {
        contexts: activation.contexts,
        previous,
        restored: false,
    };
    let callback = super::super::unwind::catch_raw(|| {
        crate::vm::engine::unwind::guard_terminate(|| match registration.callback() {
            super::super::signal::DeferredSignalCallback::Guest(func) => {
                super::super::interp::call_guest(handler_ctx, func, &[signum as u64]);
            }
            super::super::signal::DeferredSignalCallback::ImageNative(address) => {
                let callback: unsafe extern "C-unwind" fn(i32) =
                    unsafe { std::mem::transmute(address) };
                unsafe { callback(signum) };
            }
        });
        // Restoring the mask can synchronously dispatch a raise made by this
        // handler. Keep that nested work inside the callback owner's catch:
        // its EngineFault token must be consumed while this activation is
        // still current, then re-raised for the initiating Engine below.
        mask.finish(handler_ctx, host_mask);
    });
    match callback {
        Ok(()) => Ok(()),
        Err(exception) => {
            let fault = match exception.take_engine_fault(lease.shared()) {
                Ok(fault) => fault.finish(),
                Err(exception) => exception.resume_or_rethrow(),
            };
            // The caught frame already dropped both mask guards while
            // unwinding. Keep the callback owner's activation current until
            // its fault token has been consumed, then re-raise for the
            // initiating Engine below.
            drop(activation);
            drop(lease);
            Err(fault)
        }
    }
}

fn dispatch_signal_delivery_for_ctx(
    ctx: *mut Ctx,
    delivery: super::super::signal::SignalDeliveryGuard,
    signum: i32,
) {
    if let Err(fault) = dispatch_signal_delivery(delivery, signum) {
        super::super::unwind::raise_engine_fault(ctx, fault.message, fault.code);
    }
}

/// Complete target-pthread callbacks without entering the owner process inbox.
/// This path deliberately ignores `Ctx::signal_draining`: a real unblocked
/// `raise(3)` must finish the handler selected by the kernel before it returns,
/// including when called by an already deferred handler.
pub(super) fn drain_current_thread_signal_deliveries(ctx: *mut Ctx) {
    let contexts = current_thread_contexts(ctx);
    loop {
        let blocked = current_thread_signal_mask(contexts);
        let Some((delivery, signum)) = super::super::signal::take_current_thread_delivery(blocked)
        else {
            return;
        };
        dispatch_signal_delivery_for_ctx(ctx, delivery, signum);
    }
}

/// Finish target-pthread work accepted before an owning execution boundary
/// reports an EngineFault. A handler fault may restore its mask and thereby
/// publish another delivery for this pthread. Consume each nested fault in its
/// callback owner's catch, keep draining, and report the last fault observed.
pub(crate) fn drain_current_thread_signal_deliveries_after_fault(
    ctx: *mut Ctx,
    mut fault: super::super::unwind::EngineFaultReport,
) -> super::super::unwind::EngineFaultReport {
    let contexts = current_thread_contexts(ctx);
    loop {
        let blocked = current_thread_signal_mask(contexts);
        let Some((delivery, signum)) = super::super::signal::take_current_thread_delivery(blocked)
        else {
            return fault;
        };
        if let Err(nested) = dispatch_signal_delivery(delivery, signum) {
            fault = nested;
        }
    }
}

fn real_raise_and_drain(ctx: *mut Ctx, signum: i32) -> i32 {
    loop {
        let attempt = super::super::signal::HostRaiseAttempt::begin(signum);
        let result = crate::os::process::raise(signum);
        let retry = attempt.finish();
        if result != 0 {
            return result;
        }
        if retry {
            match super::super::signal::host_raise_retry_is_stale(signum) {
                Ok(true) => super::super::unwind::raise_engine_fault(
                    ctx,
                    format!(
                        "HostRaise for signal {signum} reached a raw-restored closed MIRVM fixed stub"
                    ),
                    70,
                ),
                Ok(false) => {}
                Err(error) => super::super::unwind::raise_engine_fault(
                    ctx,
                    format!("failed to validate HostRaise retry for signal {signum}: {error}"),
                    70,
                ),
            }
            continue;
        }
        drain_current_thread_signal_deliveries(ctx);
        drain_pending_signals(ctx);
        return 0;
    }
}

/// Preserve libc `raise(3)` itself as the signal's linearization point. A
/// managed fixed stub records an SI_TKILL delivery in this pthread's mailbox;
/// the ordinary VM boundary below then runs that callback synchronously when
/// the signal was not blocked.
pub(crate) fn raise_signal(ctx: *mut Ctx, signum: i32) -> i32 {
    if signum > 0 && (signum as usize) < super::super::signal::SIGNAL_SLOTS {
        let contexts = current_thread_contexts(ctx);
        let bit = 1u64 << signum;
        if unsafe { (*contexts).close_signal_drain } && current_physical_signal_mask() & bit != 0 {
            return raise_from_close_drain(ctx, signum);
        }
    }
    real_raise_and_drain(ctx, signum)
}

fn current_thread_signal_mask(contexts: *mut ThreadContexts) -> u64 {
    unsafe { (*contexts).signal_mask | current_physical_signal_mask() }
}

fn current_physical_signal_mask() -> u64 {
    crate::os::signal::Sigaction::current_standard_mask_bits().unwrap_or_else(|error| {
        super::super::interp::engine_abort(&format!(
            "failed to query the host thread signal mask: {error}"
        ))
    })
}

pub(super) fn defer_finalizer_while_signal_masked(shared: &Arc<Shared>) -> bool {
    let Some(key) = CTX_KEY.get().copied() else {
        return false;
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() || unsafe { (*contexts).signal_mask } == 0 {
        return false;
    }
    let pending = unsafe { &mut (*contexts).pending_finalizers };
    if pending.try_reserve(1).is_err() {
        eprintln!("mirvm[m4-engine]: failed to queue a masked Engine finalizer");
        std::process::abort();
    }
    pending.push(Arc::clone(shared));
    true
}

/// Called only after the outermost deferred handler restored the real pthread
/// mask. A nested handler leaves the queue for its outer caller.
pub(super) fn start_pending_signal_finalizers(contexts: *mut ThreadContexts) {
    if unsafe { (*contexts).signal_mask } != 0 {
        return;
    }
    let pending = std::mem::take(unsafe { &mut (*contexts).pending_finalizers });
    for shared in pending {
        super::engine::start_finalizer(shared);
    }
}
