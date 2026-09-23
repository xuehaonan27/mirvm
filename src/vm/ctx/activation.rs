//! Activation boundary: the pinned code-domain entry into an Engine.
//!
//! A trace body only runs behind the pinned-register boundary installed here, and a plain body
//! never carries a recorder: the code domain is copied out of `Shared` into `ThreadContexts` for
//! the duration of the activation, and only the trace domain opens a recorder. A nested
//! activation saves and restores that value, so a guest -> native -> guest chain keeps the domain
//! it entered from and never migrates mid-chain.
//!
//! The serial this boundary assigns is what [`super::main_run`] compares a main panic catch
//! against, which keeps a signal handler or thunk re-entry from claiming the outer run's catch.

use std::sync::Arc;

use super::engine::Shared;
use super::signals::start_pending_signal_finalizers;
use super::thread_ctx::{CTX_KEY, Ctx, THREAD_CONTEXT_EXITING, ThreadContexts};

pub struct ActivationGuard {
    pub(super) contexts: *mut ThreadContexts,
    previous: *mut Ctx,
    previous_activation: u64,
    /// Domain of the activation this one nests inside, restored on exit so a
    /// native callback returning to an outer guest chain resumes that chain's
    /// domain rather than the callee's.
    previous_domain: super::super::jit::CodeDomain,
    previous_signal_owner: u64,
    ctx: *mut Ctx,
    engine_id: u64,
    activation: u64,
    telemetry: Option<crate::telemetry::capture::ActivationToken>,
}

impl ActivationGuard {
    pub fn ctx(&self) -> *mut Ctx {
        self.ctx
    }
}

impl Drop for ActivationGuard {
    fn drop(&mut self) {
        let shared = unsafe { (*self.ctx).shared_arc() };
        let drain_deferred;
        let start_signal_finalizers;
        unsafe {
            if (*self.contexts).current != self.ctx
                || (*self.contexts).current_activation != self.activation
                || (*self.contexts).active_engines.last().copied() != Some(self.engine_id)
            {
                eprintln!("mirvm[m4-engine]: Engine activation exited out of order");
                std::process::abort();
            }
            super::super::signal::restore_owner(self.previous_signal_owner);
            (*self.contexts).active_engines.pop();
            (*self.contexts).current = self.previous;
            (*self.contexts).current_activation = self.previous_activation;
            (*self.contexts).domain = self.previous_domain;
            drain_deferred = shared.control().is_closing()
                && !(*self.contexts).active_engines.contains(&self.engine_id);
            // An EngineFault unwinds the handler activation before its outer
            // boundary consumes the fault token. Only that ordinary boundary,
            // after the token is gone, may start work queued under the mask.
            start_signal_finalizers =
                (*self.contexts).signal_mask == 0 && (*self.contexts).in_flight_faults.is_empty();
        }
        if let Some(token) = self.telemetry {
            let restored_engine_id = if self.previous.is_null() {
                0
            } else {
                unsafe { (*self.previous).shared().id }
            };
            crate::telemetry::capture::activation_exit(token, restored_engine_id);
        }
        if drain_deferred && !std::thread::panicking() {
            super::super::deferred::drain_current_thread(&shared);
        }
        if start_signal_finalizers && !std::thread::panicking() {
            start_pending_signal_finalizers(self.contexts);
        }
    }
}

pub fn activate(shared: &Arc<Shared>) -> ActivationGuard {
    super::super::signal::initialize_current_thread_inbox();
    let key = *CTX_KEY.get_or_init(|| {
        #[cfg(sanitize = "thread")]
        let dtor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)> = None;
        #[cfg(not(sanitize = "thread"))]
        let dtor =
            Some(super::thread_ctx::ctx_key_dtor as unsafe extern "C" fn(*mut std::ffi::c_void));
        crate::os::thread::tls_key_create(dtor)
    });
    unsafe {
        let p = crate::os::thread::tls_get(key);
        let contexts = if p.is_null() {
            if THREAD_CONTEXT_EXITING.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("mirvm[m4-engine]: Engine callback entered after pthread teardown");
                std::process::abort();
            }
            let contexts = Box::into_raw(Box::new(ThreadContexts::new()));
            crate::os::thread::tls_set(key, contexts as *mut std::ffi::c_void);
            contexts
        } else {
            p as *mut ThreadContexts
        };
        if (*contexts).active_engines.try_reserve(1).is_err() {
            eprintln!("mirvm[m4-engine]: failed to grow the Engine activation stack");
            std::process::abort();
        }
        let previous = (*contexts).current;
        let previous_activation = (*contexts).current_activation;
        let previous_domain = (*contexts).domain;
        let activation = (*contexts).next_activation;
        if activation == 0 {
            eprintln!("mirvm[m4-engine]: Engine activation counter exhausted");
            std::process::abort();
        }
        (*contexts).next_activation = activation.wrapping_add(1);
        let ctx = (*contexts).attach(shared);
        (*contexts).current_activation = activation;
        (*contexts).active_engines.push(shared.id);
        let previous_signal_owner = super::super::signal::activate_owner(shared.id);
        // The code domain of this activation is the Engine's frozen choice. It is
        // installed for the duration of the activation so dispatch reads one
        // value; a nested activation saves and restores it, which keeps a
        // guest -> native -> guest chain in the domain it entered from.
        let domain = shared.domain;
        (*contexts).domain = domain;
        let telemetry = if domain == super::super::jit::CodeDomain::Trace {
            (*contexts).telemetry_touched = true;
            Some(crate::telemetry::capture::activation_enter(shared.id))
        } else {
            None
        };
        ActivationGuard {
            contexts,
            previous,
            previous_activation,
            previous_domain,
            previous_signal_owner,
            ctx,
            engine_id: shared.id,
            activation,
            telemetry,
        }
    }
}
