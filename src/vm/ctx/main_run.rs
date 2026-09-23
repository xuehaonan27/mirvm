//! The main-run bookkeeping: what `run_main` promises about a guest `main`.
//!
//! A normal guest `return 101` must not be confused with guest std converting a main panic into
//! 101, so every nested `run_main` on this thread records its own state and the main panic catch
//! is claimed only by the intrinsic the lowering phase marked, and only inside the activation
//! that opened the boundary. The boundary records that activation's serial number, and a signal
//! handler or native thunk re-entry runs under a different one, so it cannot borrow an outer
//! boundary to claim a main panic.
//!
//! The state is [`Ctx::main_runs`], so it is per-thread-per-Engine like the rest of the vmctx.

use super::thread_ctx::{CTX_KEY, Ctx, ThreadContexts};

#[derive(Default)]
pub(super) struct MainRunState {
    pub(super) boundary_activation: Option<u64>,
    pub(super) catcher_claimed: bool,
    pub(super) catcher_active: bool,
    pub(super) panicked: bool,
}

pub(crate) struct MainRunGuard {
    pub(super) ctx: *mut Ctx,
    pub(super) index: usize,
    pub(super) finished: bool,
}

impl MainRunGuard {
    pub(crate) fn finish(mut self) -> bool {
        let states = unsafe { &mut (*self.ctx).main_runs };
        if states.len() != self.index + 1 {
            eprintln!("mirvm[m4-engine]: nested main execution state was finished out of order");
            std::process::abort();
        }
        let state = states.pop().unwrap();
        self.finished = true;
        state.panicked
    }
}

impl Drop for MainRunGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let states = unsafe { &mut (*self.ctx).main_runs };
        if states.len() != self.index + 1 {
            eprintln!("mirvm[m4-engine]: nested main execution state unwound out of order");
            std::process::abort();
        }
        states.pop();
    }
}

pub(crate) fn begin_main_run(ctx: *mut Ctx) -> MainRunGuard {
    let states = unsafe { &mut (*ctx).main_runs };
    let index = states.len();
    states.push(MainRunState::default());
    MainRunGuard {
        ctx,
        index,
        finished: false,
    }
}

pub(super) struct MainBoundaryGuard {
    pub(super) ctx: *mut Ctx,
    pub(super) index: usize,
    pub(super) finished: bool,
}

impl MainBoundaryGuard {
    fn finish(mut self) {
        let state = unsafe { &mut (&mut (*self.ctx).main_runs)[self.index] };
        if !state.catcher_claimed || state.catcher_active {
            crate::vm::unwind::engine_abort(
                "fixed std main panic catch call did not pass the expected catch_unwind intrinsic",
            );
        }
        state.boundary_activation = None;
        self.finished = true;
    }
}

impl Drop for MainBoundaryGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let states = unsafe { &mut (*self.ctx).main_runs };
        if let Some(state) = states.get_mut(self.index) {
            state.boundary_activation = None;
            state.catcher_active = false;
        }
    }
}

/// Execute the standard main catch call marked by the IR. The boundary records the activation
/// number of the current Engine entry; signal handlers or native thunk re-entries receive a
/// different number and cannot borrow the outer boundary to claim a main panic.
pub(crate) fn call_main_panic_boundary<R>(ctx: *mut Ctx, f: impl FnOnce() -> R) -> R {
    let states = unsafe { &mut (*ctx).main_runs };
    let Some(index) = states.len().checked_sub(1) else {
        crate::vm::unwind::engine_abort("main panic catch call appeared outside run_main");
    };
    let state = &mut states[index];
    if state.boundary_activation.is_some() {
        crate::vm::unwind::engine_abort(
            "duplicate entry into main panic catch boundary during the same main execution",
        );
    }
    state.boundary_activation = Some(current_activation(ctx));
    state.catcher_claimed = false;
    let guard = MainBoundaryGuard {
        ctx,
        index,
        finished: false,
    };
    let result = f();
    guard.finish();
    result
}

pub(crate) struct MainCatchGuard {
    pub(super) ctx: *mut Ctx,
    pub(super) index: usize,
}

impl MainCatchGuard {
    pub(crate) fn mark_panicked(&mut self) {
        unsafe { (&mut (*self.ctx).main_runs)[self.index].panicked = true };
    }
}

impl Drop for MainCatchGuard {
    fn drop(&mut self) {
        let states = unsafe { &mut (*self.ctx).main_runs };
        let Some(state) = states.get_mut(self.index) else {
            eprintln!("mirvm[m4-engine]: main catch state disappeared while active");
            std::process::abort();
        };
        state.catcher_active = false;
    }
}

/// Only the intrinsic precisely marked by the lowering phase and still within the same Engine
/// activation may claim the main catch. Ordinary catches, signal handlers, and native thunk
/// re-entries all return `None`.
pub(crate) fn claim_main_panic_catch(
    ctx: *mut Ctx,
    role: super::super::ir::BuiltinCallRole,
) -> Option<MainCatchGuard> {
    if role != super::super::ir::BuiltinCallRole::MainPanicCatcher {
        return None;
    }
    let activation = current_activation(ctx);
    let states = unsafe { &mut (*ctx).main_runs };
    let index = states.len().checked_sub(1)?;
    let state = &mut states[index];
    if state.boundary_activation != Some(activation) || state.catcher_claimed {
        return None;
    }
    state.catcher_claimed = true;
    state.catcher_active = true;
    Some(MainCatchGuard { ctx, index })
}

/// The serial number of the activation this `ctx` is currently running under. A main panic
/// boundary and its catch must belong to the same one, so this aborts rather than guess when
/// the thread's registry disagrees.
pub(super) fn current_activation(ctx: *mut Ctx) -> u64 {
    let Some(key) = CTX_KEY.get().copied() else {
        crate::vm::unwind::engine_abort("main panic catch happened outside Engine activation");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null()
        || unsafe { (*contexts).current != ctx || (*contexts).current_activation == 0 }
    {
        crate::vm::unwind::engine_abort(
            "main panic catch does not belong to the current Engine activation",
        );
    }
    unsafe { (*contexts).current_activation }
}
