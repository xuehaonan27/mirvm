//! Execution environment: `Shared` (read-only after publication) plus the per-thread execution state
//! (`Ctx`, the vmctx).
//!
//! `Shared` is held by the `Engine` through an `Arc`, and every per-thread `Ctx` keeps its own `Arc`
//! as well, so the module stays alive as long as any execution state does. Ctx is passed around as a
//! raw pointer and its fields are borrowed transiently, one at a time.
//!
//! Ctx lives on a self-managed pthread key, and boundary TLS attach is the only gate through which an
//! entry point (run_main / run_export / thunk) reaches the engine.
//!
//! Host `thread_local!` cannot be used instead. Guest TLS destructors (std's run_dtors, thunkified)
//! run in the pthread TSD phase, but host C++ TLS destructors run *before* that phase
//! (glibc start_thread calls __call_tls_dtors, then __nptl_deallocate_tsd). Ctx would already be gone
//! by then, and attaching inside the destructor thunk would touch destroyed host TLS. A self-managed
//! pthread key whose destructor re-sets the specific value for a few rounds (glibc runs at most 4
//! rounds) keeps Ctx alive until after the guest key destructor has run.
//!
//! Split by concern; every item stays reachable at its `crate::vm::engine::ctx::` path:
//!
//! * [`engine`] -- `Shared`, `EngineControl`, `Engine` and close/finalize.
//! * [`thread_ctx`] -- `Ctx`, the `ThreadContexts` registry and the per-thread teardown rounds.
//! * [`activation`] -- the code-domain boundary and the main-run panic boundary.
//! * [`signals`] -- deferred signal delivery.

pub(crate) mod activation;
pub(crate) mod engine;
mod signals;
mod thread_ctx;

#[cfg(test)]
mod tests;

// Re-exports keep every pre-split path and its visibility: `pub use` for what was `pub`, `pub(crate)
// use` for what was `pub(crate)`.
pub(crate) use activation::activate;
#[cfg(test)]
pub(crate) use engine::engine;
#[cfg(test)]
pub(crate) use engine::set_wait_closed_check_hook;
pub(crate) use engine::{
    DeferredHold, EngineControl, ExecutionLease, control_for_engine, current_thread_has_engine,
};
pub use engine::{Engine, EngineClosed, EngineState, Shared, WaitClosedError};
#[cfg(test)]
pub(crate) use engine::{PHASE_CLOSING, PHASE_FINALIZING};
pub(crate) use signals::drain_current_thread_signal_deliveries_after_fault;
pub(crate) use signals::{drain_pending_signals, raise_signal};
pub use thread_ctx::{Ctx, ShadowFrame, attach};
pub(crate) use thread_ctx::{
    EngineFaultToken, begin_engine_fault, current_code_domain, finish_engine_fault,
};
pub(crate) use thread_ctx::{
    begin_main_run, call_main_panic_boundary, claim_main_panic_catch, current,
    current_thread_final_tsd_pass_is_armed, current_thread_is_in_final_tsd_pass,
    guest_spawned_threads, set_fork_baseline,
};
#[cfg(test)]
pub(crate) use thread_ctx::{
    engine_fault_in_flight, set_thread_exit_inbox_empty_hook, test_ctx_key,
};
