use std::sync::{Arc, Barrier, Weak, mpsc};

use super::thread_ctx::attach;
use super::{Engine, Shared};
use crate::vm::ir::{Block, FuncBody, Module, RetAbi, Terminator};

fn context_export_module() -> Module {
    let mut module = Module {
        funcs: vec![FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            }],
            name: "context_export".into(),
        }]
        .into(),
        ..Module::default()
    };
    module.exports.insert("context_export".into(), 0);
    module
}

#[test]
fn one_host_thread_keeps_distinct_contexts_for_distinct_engines() {
    let first = Arc::new(Shared::new(Module::default()));
    let second = Arc::new(Shared::new(Module::default()));

    let first_ctx = attach(&first);
    let second_ctx = attach(&second);

    assert_ne!(first_ctx, second_ctx);
    assert_eq!(unsafe { (*first_ctx).shared }, Arc::as_ptr(&first));
    assert_eq!(unsafe { (*second_ctx).shared }, Arc::as_ptr(&second));
}

#[test]
fn first_attach_prunes_slots_from_threads_that_already_exited() {
    let shared = Arc::new(Shared::new(Module::default()));
    for _ in 0..2 {
        let worker_shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _ = attach(&worker_shared);
        })
        .join()
        .unwrap();
        let slots = shared.ctx_slots.lock().unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].strong_count(), 0);
    }

    let _ = attach(&shared);
    let slots = shared.ctx_slots.lock().unwrap();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].strong_count(), 1);
}

#[test]
fn closing_releases_context_owned_by_a_long_lived_thread() {
    let engine = Engine::new(Shared::new(context_export_module()));
    let weak = Arc::downgrade(engine.shared());
    let (attached_tx, attached_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker_engine = engine.clone();
    let worker = std::thread::spawn(move || {
        let outcome =
            unsafe { super::super::interp::run_export(&worker_engine, "context_export", &[]) }
                .unwrap();
        assert!(matches!(
            outcome,
            super::super::interp::RunOutcome::Returned(_)
        ));
        drop(worker_engine);
        attached_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });

    attached_rx.recv().unwrap();
    engine.wait_closed().unwrap();
    drop(engine);
    assert!(
        weak.upgrade().is_none(),
        "an inactive worker Ctx retained Shared after Engine close"
    );

    release_tx.send(()).unwrap();
    worker.join().unwrap();
}

#[test]
fn nested_engine_activation_restores_the_outer_context() {
    let first = Arc::new(Shared::new(Module::default()));
    let second = Arc::new(Shared::new(Module::default()));
    let outer = super::activate(&first);
    assert_eq!(super::current(), outer.ctx());
    assert!(super::current_thread_has_engine(first.id));
    {
        let inner = super::activate(&second);
        assert_eq!(super::current(), inner.ctx());
        assert!(super::current_thread_has_engine(first.id));
        assert!(super::current_thread_has_engine(second.id));
    }
    assert_eq!(super::current(), outer.ctx());
    assert!(super::current_thread_has_engine(first.id));
    assert!(!super::current_thread_has_engine(second.id));
}

#[test]
fn closing_reentry_and_wait_see_an_outer_engine_activation() {
    let first = Engine::new(Shared::new(Module::default()));
    let second = Engine::new(Shared::new(Module::default()));
    let outer_lease = first.execution_lease().unwrap();
    let outer = super::activate(first.shared());
    let inner = super::activate(second.shared());

    first.close();
    assert_eq!(first.state(), super::EngineState::Closing);
    assert_eq!(
        first.wait_closed(),
        Err(super::WaitClosedError::ActiveOnCurrentThread)
    );
    let reentry = super::ExecutionLease::for_thunk(first.control())
        .expect("an outer active Engine may re-enter while Closing");

    drop(reentry);
    drop(inner);
    drop(outer);
    drop(outer_lease);
    first.wait_closed().unwrap();
    second.wait_closed().unwrap();
}

#[test]
fn nested_main_runs_keep_panic_classification_separate() {
    let shared = Arc::new(Shared::new(Module::default()));
    let activation = super::activate(&shared);
    let ctx = activation.ctx();
    let outer = super::begin_main_run(ctx);

    let inner = super::begin_main_run(ctx);
    super::call_main_panic_boundary(ctx, || {
        let mut catch =
            super::claim_main_panic_catch(ctx, crate::vm::ir::BuiltinCallRole::MainPanicCatcher)
                .unwrap();
        assert!(
            super::claim_main_panic_catch(ctx, crate::vm::ir::BuiltinCallRole::MainPanicCatcher,)
                .is_none()
        );
        catch.mark_panicked();
    });
    assert!(inner.finish());

    super::call_main_panic_boundary(ctx, || {
        let _catch =
            super::claim_main_panic_catch(ctx, crate::vm::ir::BuiltinCallRole::MainPanicCatcher)
                .unwrap();
    });
    assert!(!outer.finish());
}

#[test]
fn main_catcher_requires_exact_role_and_same_activation() {
    use crate::vm::ir::BuiltinCallRole;

    let shared = Arc::new(Shared::new(Module::default()));
    let activation = super::activate(&shared);
    let ctx = activation.ctx();
    let run = super::begin_main_run(ctx);
    super::call_main_panic_boundary(ctx, || {
        assert!(super::claim_main_panic_catch(ctx, BuiltinCallRole::Normal).is_none());
        {
            let reentrant = super::activate(&shared);
            assert!(
                super::claim_main_panic_catch(reentrant.ctx(), BuiltinCallRole::MainPanicCatcher,)
                    .is_none()
            );
        }
        let _catch = super::claim_main_panic_catch(ctx, BuiltinCallRole::MainPanicCatcher).unwrap();
    });
    assert!(!run.finish());
}

#[test]
fn engine_unregisters_and_releases_shared_state_on_drop() {
    let (id, weak): (u64, Weak<Shared>) = {
        let mut shared = Shared::new(Module::default());
        shared.jit.enabled = true;
        let engine = Engine::new(shared);
        let id = engine.shared().id;
        assert!(super::engine(id).is_some());
        crate::vm::atexit::seed_callback(id);
        assert!(crate::vm::atexit::has_callbacks(id));
        (id, Arc::downgrade(engine.shared()))
    };
    assert!(super::engine(id).is_none());
    assert!(!crate::vm::atexit::has_callbacks(id));
    assert!(weak.upgrade().is_none());
}

#[test]
fn simultaneous_idle_close_has_one_synchronous_finalizer() {
    for _ in 0..256 {
        let engine = Engine::new(Shared::new(Module::default()));
        let weak = Arc::downgrade(engine.shared());
        let start = Arc::new(Barrier::new(3));

        let first_engine = engine.clone();
        let first_start = Arc::clone(&start);
        let first = std::thread::spawn(move || {
            first_start.wait();
            first_engine.close();
        });
        let second_engine = engine.clone();
        let second_start = Arc::clone(&start);
        let second = std::thread::spawn(move || {
            second_start.wait();
            second_engine.close();
        });

        start.wait();
        first.join().expect("first close thread panicked");
        second.join().expect("second close thread panicked");
        engine.wait_closed().unwrap();
        drop(engine);
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn finalizing_transition_keeps_an_idle_lifecycle_permit() {
    let control = super::EngineControl::new(u64::MAX);
    control
        .lifecycle
        .store(super::PHASE_CLOSING, std::sync::atomic::Ordering::Release);

    assert!(control.begin_finalizer_permit());
    assert_eq!(
        control.lifecycle.load(std::sync::atomic::Ordering::Acquire),
        super::PHASE_CLOSING | 1
    );
    assert_eq!(
        control
            .active_executions
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "the close permit must not hide an idle Engine from TSD cleanup"
    );
    assert!(control.begin_finalizing_with_permit());
    assert_eq!(
        control.lifecycle.load(std::sync::atomic::Ordering::Acquire),
        super::PHASE_FINALIZING | 1
    );
    assert!(!control.begin_active_execution(true));

    control.finish_execution();
    assert_eq!(
        control.lifecycle.load(std::sync::atomic::Ordering::Acquire),
        super::PHASE_FINALIZING
    );
}

#[test]
fn failed_finalizer_execution_rolls_back_the_active_count() {
    let control = super::EngineControl::new(u64::MAX - 1);
    control.lifecycle.store(
        super::PHASE_CLOSING | 1,
        std::sync::atomic::Ordering::Release,
    );

    assert!(!control.begin_finalizer_execution());
    assert_eq!(
        control
            .active_executions
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

/// L3: the code domain is frozen when an Engine is created, and arming a
/// session later must NOT migrate it. That is the design's confirmed first
/// version (§5.2.3): plain activations keep running plain, and only threads
/// entering guest after arming pick up trace. Getting this backwards would
/// silently move a running guest into a domain whose bodies expect recorder
/// state the running frame does not have.
#[test]
fn code_domain_is_frozen_at_engine_creation() {
    use crate::vm::jit::CodeDomain;

    // No session armed here, so this Engine is born plain and stays plain.
    let plain = Shared::new(context_export_module());
    assert_eq!(plain.domain, CodeDomain::Plain);
    assert_eq!(plain.domain, CodeDomain::Plain);

    let path = std::env::temp_dir().join(format!(
        "mirvm-domain-{}-{}.mlog",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let session = crate::telemetry::capture::CaptureSession::start(
        crate::telemetry::capture::StartOptions::new(
            &path,
            crate::telemetry::capture::STARTER_BYTES,
        ),
    );
    let Ok(session) = session else {
        // Another parallel test owns the process-wide session; the frozen
        // domain of an already-created Engine is what matters and is asserted
        // above, so skip the arming half rather than fight the global.
        return;
    };
    // Arming after creation cannot migrate the Engine that already exists.
    assert_eq!(
        plain.domain,
        CodeDomain::Plain,
        "an Engine created before arming must not migrate to trace"
    );

    // An Engine created while the session is armed is trace for its lifetime.
    let trace = Shared::new(context_export_module());
    assert_eq!(trace.domain, CodeDomain::Trace);
    assert_eq!(trace.domain, CodeDomain::Trace);
    drop(session);
    std::fs::remove_file(&path).ok();
}

/// L2 prerequisite: MIRVM's own service threads are invisible to the fork
/// guard. A baseline pinned while one is live must stay valid after it
/// exits, and a baseline inherited across a pid change must be repaired
/// instead of rejecting the child's later forks.
///
/// Assertions are deliberately independent of the absolute OS thread count:
/// `cargo test` runs tests in parallel threads, so only differences within
/// one thread of control and self-consistency between the two functions are
/// meaningful.
#[test]
fn service_threads_are_excluded_from_the_guest_fork_baseline() {
    use crate::os::thread::ServiceThreadGuard;

    // The accounting itself, independent of the process thread count.
    assert_eq!(super::thread_ctx::guest_threads_from(5, 0), 5);
    assert_eq!(super::thread_ctx::guest_threads_from(5, 2), 3);
    assert_eq!(
        super::thread_ctx::guest_threads_from(1, 4),
        0,
        "an over-counted service total must saturate, never wrap"
    );

    // The fork guard's subtraction must be the only consumer of the service
    // count; nothing here asserts an absolute value because other tests run
    // capture writers (hence service threads) in parallel.
    let service = ServiceThreadGuard::register();

    // A baseline pinned with a service thread live stays pinned afterwards,
    // so the writer exiting cannot retroactively reject a fork.
    let shared = Shared::new(context_export_module());
    super::set_fork_baseline(&shared);
    let pinned = shared
        .fork_baseline_threads
        .load(std::sync::atomic::Ordering::SeqCst);
    drop(service);
    assert_eq!(
        shared
            .fork_baseline_threads
            .load(std::sync::atomic::Ordering::SeqCst),
        pinned,
        "service thread exit must not invalidate the pinned baseline"
    );

    shared
        .fork_baseline_pid
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let repaired = super::thread_ctx::guest_thread_count_for(&shared);
    let stamped = shared
        .fork_baseline_pid
        .load(std::sync::atomic::Ordering::SeqCst);
    // Compare the stamp against the repair's own result rather than a second
    // `getpid()`: a parallel test may `fork`, which would change the answer
    // between the two reads.
    assert_ne!(stamped, 1, "the repair must re-stamp the current pid");
    assert_eq!(
        repaired,
        shared
            .fork_baseline_threads
            .load(std::sync::atomic::Ordering::SeqCst),
        "the repair must pin what it returned"
    );
}
