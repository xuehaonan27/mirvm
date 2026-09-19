//! Close racing calls that are already in flight.
//!
//! Part 1 races `Engine::close` against eight threads hammering `interp::run_export`. Every
//! result must be either the export's real value or the structured
//! `RunError { kind: RunErrorKind::EngineClosed }` -- never a panic, never a wrong value -- and
//! once `wait_closed` has returned the seal must be absolute: every later entry is rejected and
//! the Engine has left the registry.
//!
//! Part 2 pins the deferred-hold window without a race: a `DeferredHold` keeps the Engine in
//! `Closing` after `close()`, where an unregistered public entry is rejected while a
//! registered-callback lease is still admitted, and the Engine finalizes only after the hold is
//! dropped. The hold is the same atomic count `DeferredHold::acquire` shares with executions, so
//! this is the exact window native pthread/TSD registration relies on.
//!
//! The registry check reads `control_for_engine`, not the `#[cfg(test)]` `ctx::engine`: the
//! harness is a non-test build of `src/vm`, but both read the same `ENGINES` map entry that
//! `finalize_shared` removes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use crate::vm::ctx::{
    DeferredHold, Engine, EngineState, ExecutionLease, Shared, control_for_engine,
};
use crate::vm::deferred::{TsdRegistration, prepare_pthread_operation};
use crate::vm::interp::{self, RawReturn, RunError, RunErrorKind, RunOutcome};
use crate::vm::ir::{
    Block, FuncBody, Module, Operand, RetAbi, Rvalue, ScalarPlace, Slot, Stmt, Terminator, Width,
};

/// Hand-built export that returns immediately: `_0 ret @0`, one block returning `VALUE`.
const EXPORT: &str = "tsan_close_race_probe";
const VALUE: u64 = 0x5eed;
const RACERS: usize = 8;
const POST_CLOSE_ENTRIES: usize = 32;
/// Evidence that every racer is inside its loop before `close()` starts, so the close really
/// races live entries instead of racing thread startup.
const PROGRESS_BEFORE_CLOSE: u64 = RACERS as u64 * 64;

fn probe_module() -> Module {
    let ret = Slot {
        off: 0,
        width: Width::W64,
    };
    let body = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(ret),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![Stmt::Assign {
                dst: ScalarPlace::Slot(ret),
                rv: Rvalue::Use(Operand::Imm {
                    bits: VALUE,
                    width: Width::W64,
                }),
            }],
            term: Terminator::Return,
        }],
        name: "tsan_close_race::probe".into(),
    };
    let mut module = Module {
        funcs: vec![body].into(),
        ..Module::default()
    };
    module.exports.insert(EXPORT.into(), 0);
    module
}

#[derive(Default, Clone, Copy)]
struct Tally {
    returned: u64,
    closed: u64,
    bad: u64,
    post_closed: u64,
    post_bad: u64,
}

impl Tally {
    fn record(&mut self, result: Result<RunOutcome<RawReturn>, RunError>) {
        match result {
            Ok(RunOutcome::Returned(raw)) if raw.lo == VALUE && raw.hi == 0 => self.returned += 1,
            Ok(_) => self.bad += 1,
            Err(error) if error.kind == RunErrorKind::EngineClosed => self.closed += 1,
            Err(_) => self.bad += 1,
        }
    }
}

/// One racing thread: enter the Engine until told to stop, then enter `POST_CLOSE_ENTRIES`
/// more times. The stop flag is only set after `wait_closed`, so those trailing entries verify
/// the post-seal rejection on every racer, not just on the closing thread.
fn racer(
    engine: Arc<Engine>,
    start: Arc<Barrier>,
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
) -> Tally {
    let mut tally = Tally::default();
    start.wait();
    while !stop.load(Ordering::Acquire) {
        let result = unsafe { interp::run_export(&engine, EXPORT, &[]) };
        tally.record(result);
        progress.fetch_add(1, Ordering::Release);
    }
    for _ in 0..POST_CLOSE_ENTRIES {
        let result = unsafe { interp::run_export(&engine, EXPORT, &[]) };
        match result {
            Err(error) if error.kind == RunErrorKind::EngineClosed => tally.post_closed += 1,
            _ => tally.post_bad += 1,
        }
    }
    tally
}

/// Deterministic deferred-hold window. `close()` drains the registry while a hold is
/// outstanding; Finalizing cannot pass the hold, so the only way out is dropping it.
fn deferred_hold_window() -> (bool, bool, bool) {
    let engine = Engine::new(Shared::new(probe_module()));
    let id = engine.shared().id;
    let control = Arc::clone(engine.control());
    let hold = DeferredHold::acquire(&control, true).expect("deferred hold");

    engine.close();
    let closing = engine.state() == EngineState::Closing;
    let plain_rejected = matches!(
        unsafe { interp::run_export(&engine, EXPORT, &[]) },
        Err(ref error) if error.kind == RunErrorKind::EngineClosed
    );
    // A callback that already owns a hold may still enter while Closing.
    let callback_admitted = ExecutionLease::for_registered_callback(&control).is_ok();

    drop(hold);
    let waited = engine.wait_closed().is_ok();
    let closed = engine.state() == EngineState::Closed;
    let left_registry = control_for_engine(id).is_none();
    (
        closing && plain_rejected && callback_admitted && waited && closed,
        closing,
        left_registry,
    )
}

/// The deferred TSD window: `close` scans the registry while a tracked `pthread_setspecific`
/// operation is still in flight. The open operation makes `cleanup_if_idle` refuse to revoke the
/// key, so the Engine stays in `Closing`; completing the operation and deleting the key releases
/// the registration's hold and lets teardown finish.
fn deferred_tsd_close_window() -> bool {
    let engine = Engine::new(Shared::new(probe_module()));
    let id = engine.shared().id;
    let shared = Arc::clone(engine.shared());
    let registration = TsdRegistration::pending(engine.control()).expect("TSD registration");
    let mut key: libc::pthread_key_t = 0;
    if unsafe { libc::pthread_key_create(&mut key, None) } != 0 {
        return false;
    }
    registration.commit(key);

    let value = 0x7a5d_u64;
    let Some(set) = prepare_pthread_operation(&shared, "pthread_setspecific", &[key as u64, value])
    else {
        return false;
    };
    // Left open on purpose: the product window is exactly an operation in flight while close
    // scans the registry, which is what keeps the registration's deferred hold alive.
    let set_result = unsafe { libc::pthread_setspecific(key, value as *const libc::c_void) };

    engine.close();
    let closing = engine.state() == EngineState::Closing;
    let plain_rejected = matches!(
        unsafe { interp::run_export(&engine, EXPORT, &[]) },
        Err(ref error) if error.kind == RunErrorKind::EngineClosed
    );
    set.complete(set_result as u64);

    let Some(delete) = prepare_pthread_operation(&shared, "pthread_key_delete", &[key as u64])
    else {
        return false;
    };
    let delete_result = unsafe { libc::pthread_key_delete(key) };
    delete.complete(delete_result as u64);

    let waited = engine.wait_closed().is_ok();
    let closed = engine.state() == EngineState::Closed;
    let left_registry = control_for_engine(id).is_none();
    closing
        && plain_rejected
        && set_result == 0
        && delete_result == 0
        && waited
        && closed
        && left_registry
}

pub(crate) fn run_close_race() -> bool {
    let engine = Arc::new(Engine::new(Shared::new(probe_module())));
    let id = engine.shared().id;
    let start = Arc::new(Barrier::new(RACERS + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::with_capacity(RACERS);
    for _ in 0..RACERS {
        let engine = Arc::clone(&engine);
        let start = Arc::clone(&start);
        let stop = Arc::clone(&stop);
        let progress = Arc::clone(&progress);
        handles.push(std::thread::spawn(move || {
            racer(engine, start, stop, progress)
        }));
    }

    start.wait();
    while progress.load(Ordering::Acquire) < PROGRESS_BEFORE_CLOSE {
        std::hint::spin_loop();
    }
    engine.close();
    let not_running = engine.state() != EngineState::Running;
    let mid_flight_rejected = matches!(
        unsafe { interp::run_export(&engine, EXPORT, &[]) },
        Err(ref error) if error.kind == RunErrorKind::EngineClosed
    );
    let waited = engine.wait_closed().is_ok();
    let post_seal_rejected = matches!(
        unsafe { interp::run_export(&engine, EXPORT, &[]) },
        Err(ref error) if error.kind == RunErrorKind::EngineClosed
    );
    let closed = engine.state() == EngineState::Closed;
    let left_registry = control_for_engine(id).is_none();

    stop.store(true, Ordering::Release);
    let (mut returned, mut closed_calls, mut bad, mut post_closed, mut post_bad, mut panicked) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for handle in handles {
        match handle.join() {
            Ok(tally) => {
                returned += tally.returned;
                closed_calls += tally.closed;
                bad += tally.bad;
                post_closed += tally.post_closed;
                post_bad += tally.post_bad;
            }
            Err(_) => panicked += 1,
        }
    }

    let (deferred_ok, deferred_closing, deferred_registry) = deferred_hold_window();
    let deferred_tsd_ok = deferred_tsd_close_window();
    let racy_ok = panicked == 0
        && bad == 0
        && post_bad == 0
        && post_closed == RACERS as u64 * POST_CLOSE_ENTRIES as u64
        && not_running
        && mid_flight_rejected
        && waited
        && post_seal_rejected
        && closed
        && left_registry
        && returned > 0
        && closed_calls > 0;
    let ok = racy_ok && deferred_ok && deferred_tsd_ok;
    let verdict = if ok { "PASS" } else { "FAIL" };
    println!(
        "{verdict} engine-close-race returned={returned} rejected={closed_calls} bad={bad} \
         post_closed={post_closed} post_bad={post_bad} panicked={panicked} \
         seal={} registry_left={} deferred_window={deferred_ok} \
         deferred_closing={deferred_closing} deferred_registry_left={deferred_registry} \
         deferred_tsd_window={deferred_tsd_ok}",
        not_running && mid_flight_rejected && waited && post_seal_rejected && closed,
        left_registry
    );
    ok
}
