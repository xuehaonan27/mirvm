use super::inbox::current_thread_inbox;
use super::*;
use crate::vm::ctx::Shared;
use crate::vm::ir::Module;

use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

static DETACHED_CLOSE_NATIVE_RAN: AtomicUsize = AtomicUsize::new(0);
static RAISE_EXTERNAL_RAN: AtomicUsize = AtomicUsize::new(0);
static REENTRANT_RAISE_RAN: AtomicUsize = AtomicUsize::new(0);
static REENTRANT_INSTALL_RESULT: AtomicI32 = AtomicI32::new(0);
static EXIT_CALLBACK_MASK: AtomicU64 = AtomicU64::new(0);
static EXIT_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
static REENTRANT_INSTALL_CONTROL: LazyLock<Mutex<Option<Arc<EngineControl>>>> =
    LazyLock::new(|| Mutex::new(None));

unsafe extern "C" fn detached_close_native_handler(_signum: i32) {
    DETACHED_CLOSE_NATIVE_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn detached_close_external_handler(_signum: i32) {}

unsafe extern "C" fn raise_external_handler(_signum: i32) {
    RAISE_EXTERNAL_RAN.fetch_add(1, Ordering::SeqCst);
    let control = REENTRANT_INSTALL_CONTROL.lock().unwrap().clone();
    if let Some(control) = control {
        let installed = install_signal(&control, libc::SIGWINCH, 0x2_6000, Some((15, 0x2_6000)));
        REENTRANT_INSTALL_RESULT.store(i32::from(installed.is_ok()), Ordering::SeqCst);
    }

    if crate::os::process::raise(libc::SIGURG) != 0 {
        unsafe { libc::_exit(72) }
    }
}

unsafe extern "C" fn reentrant_raise_handler(_signum: i32) {
    REENTRANT_RAISE_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C-unwind" fn exit_signal_mask_handler(_signum: i32) {
    let mask =
        Sigaction::current_standard_mask_bits().unwrap_or_else(|_| unsafe { libc::_exit(73) });
    EXIT_CALLBACK_MASK.store(mask, Ordering::Release);
    EXIT_CALLBACK_COUNT.fetch_add(1, Ordering::Release);
}

extern "C" fn first_test_restorer() {}

extern "C" fn second_test_restorer() {}

const INSTALL_OVERWRITE_CHILD: &str = "MIRVM_SIGNAL_INSTALL_OVERWRITE_CHILD";
const CURRENT_DELIVERY_CHILD: &str = "MIRVM_SIGNAL_CURRENT_DELIVERY_CHILD";
const INACTIVE_STUB_CHILD: &str = "MIRVM_SIGNAL_INACTIVE_STUB_CHILD";
const HOST_RAISE_INACTIVE_STUB_CHILD: &str = "MIRVM_SIGNAL_HOST_RAISE_INACTIVE_STUB_CHILD";
const RAISE_INSTALL_RACE_CHILD: &str = "MIRVM_SIGNAL_RAISE_INSTALL_RACE_CHILD";
const NORMALIZED_ACTION_CHILD: &str = "MIRVM_SIGNAL_NORMALIZED_ACTION_CHILD";
const THREAD_GENERATIONS_CHILD: &str = "MIRVM_SIGNAL_THREAD_GENERATIONS_CHILD";
const THREAD_EXIT_MASK_CHILD: &str = "MIRVM_SIGNAL_THREAD_EXIT_MASK_CHILD";
const EXACT_RESTORE_CHILD: &str = "MIRVM_SIGNAL_EXACT_RESTORE_CHILD";
const REQUEST_ONLY_ROLLBACK_CHILD: &str = "MIRVM_SIGNAL_REQUEST_ONLY_ROLLBACK_CHILD";
const REQUEST_ONLY_RECONCILE_CHILD: &str = "MIRVM_SIGNAL_REQUEST_ONLY_RECONCILE_CHILD";
const EXACT_COMPENSATION_CHILD: &str = "MIRVM_SIGNAL_EXACT_COMPENSATION_CHILD";
const DETACHED_SNAPSHOT_CHILD: &str = "MIRVM_SIGNAL_DETACHED_SNAPSHOT_CHILD";

fn run_signal_test_child(test_name: &str, child_env: &str) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(child_env, "1")
        .output()
        .expect("failed to start isolated signal test child")
}

struct RestoreSignal {
    signum: i32,
    action: Sigaction,
}

impl Drop for RestoreSignal {
    fn drop(&mut self) {
        let _ = self.action.replace_exact(self.signum);
    }
}

fn control() -> Arc<EngineControl> {
    Arc::clone(Shared::new(Module::default()).control())
}

fn registration(
    control: &Arc<EngineControl>,
    signum: i32,
    func: FuncId,
    visible: Sigaction,
) -> &'static SignalRegistration {
    SignalRegistration::new(
        Arc::clone(control),
        DeferredSignalCallback::Guest(func),
        signum,
        visible,
    )
}

fn descriptor(
    control: &Arc<EngineControl>,
    registration: &'static SignalRegistration,
    visible: Sigaction,
    kernel: Sigaction,
    fallback: SignalChain,
) -> StubDescriptor {
    StubDescriptor {
        install_control: Arc::clone(control),
        install_owner: control.id(),
        callback_owner: registration.control().id(),
        registration,
        visible,
        kernel,
        accepted_kernel: Some(kernel),
        fallback,
    }
}

fn node(
    control: &Arc<EngineControl>,
    guest: Sigaction,
    kernel: Sigaction,
    registration: Option<&'static SignalRegistration>,
) -> DispositionNode {
    DispositionNode {
        install_control: Arc::clone(control),
        install_owner: control.id(),
        callback_owner: registration.map(|registration| registration.control().id()),
        guest,
        kernel,
        accepted_kernel: Some(kernel),
        registration,
    }
}

#[test]
fn ordinary_delivery_does_not_keep_a_kernel_adapter_frame_in_flight() {
    let control = control();
    let visible = Sigaction::for_signal(0x1000);
    let registration = registration(&control, libc::SIGUSR1, 0, visible);
    let delivery = registration.safe_point_delivery().unwrap();

    registration.deactivate();
    registration.wait_for_kernel_deliveries();

    assert!(ptr::eq(delivery.registration(), registration));
    drop(delivery);
}

#[test]
fn new_thread_does_not_allocate_a_cell_for_a_closed_registration() {
    let control = control();
    let registration = registration(&control, libc::SIGWINCH, 0, Sigaction::for_signal(0x2_7000));
    registration.deactivate();
    registration.wait_for_kernel_deliveries();

    let has_closed_cell = std::thread::spawn(move || {
        initialize_current_thread_inbox();
        let inbox = current_thread_inbox().unwrap();
        let has_cell = inbox
            .cell_for(ptr::from_ref(registration).cast_mut())
            .is_some();
        deactivate_current_thread_inbox();
        has_cell
    })
    .join()
    .unwrap();

    assert!(!has_closed_cell);
}

#[test]
fn target_pthread_preserves_each_kernel_selected_registration_generation() {
    if std::env::var_os(THREAD_GENERATIONS_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let control = control();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (drain_tx, drain_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let target = std::thread::spawn(move || {
            initialize_current_thread_inbox();
            ready_tx
                .send(unsafe { libc::pthread_self() } as usize)
                .unwrap();
            drain_rx.recv().unwrap();
            let mut generations = Vec::new();
            while let Some((delivery, delivered_signum)) = take_current_thread_delivery(0) {
                assert_eq!(delivered_signum, signum);
                generations.push(delivery.registration().generation());
                drop(delivery);
            }
            deactivate_current_thread_inbox();
            result_tx.send(generations).unwrap();
        });
        let target_pthread = ready_rx.recv().unwrap() as libc::pthread_t;

        let mut expected = Vec::new();
        for (func, handler) in [(21, 0x2_7100), (22, 0x2_7200), (23, 0x2_7300)] {
            install_signal(&control, signum, handler, Some((func, handler as u64))).unwrap();
            let registration = control
                .signal_inbox
                .registrations()
                .filter(|registration| registration.signum() == signum)
                .max_by_key(|registration| registration.generation())
                .unwrap();
            expected.push(registration.generation());
            assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
            wait_for_test_kernel_frames(registration, 1);
            assert_eq!(registration.thread_pending.load(Ordering::Acquire), 1);
            if func == 23 {
                assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
                wait_for_test_kernel_frames(registration, 2);
                assert_eq!(
                    registration.thread_pending.load(Ordering::Acquire),
                    1,
                    "same-generation traditional signals did not coalesce"
                );
            }
        }

        drain_tx.send(()).unwrap();
        assert_eq!(result_rx.recv().unwrap(), expected);
        target.join().unwrap();
        deactivate_engine(&control).unwrap();
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::target_pthread_preserves_each_kernel_selected_registration_generation",
        THREAD_GENERATIONS_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated target-pthread generation regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_test_kernel_frames(registration: &SignalRegistration, expected: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while registration.kernel_frames.load(Ordering::Acquire) < expected {
        assert!(
            std::time::Instant::now() < deadline,
            "target pthread did not enter the expected fixed adapter frame"
        );
        std::thread::yield_now();
    }
}

#[test]
fn target_pthread_exit_callback_observes_the_pre_exit_signal_mask() {
    if std::env::var_os(THREAD_EXIT_MASK_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let unrelated = libc::SIGUSR2;
        let saved = kernel_current(signum).unwrap();
        let engine = super::super::ctx::Engine::new(Shared::new(Module::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        let (empty_tx, empty_rx) = std::sync::mpsc::channel();
        let (publish_tx, publish_rx) = std::sync::mpsc::channel();
        let target_engine = engine.clone();
        EXIT_CALLBACK_MASK.store(u64::MAX, Ordering::Release);
        EXIT_CALLBACK_COUNT.store(0, Ordering::Release);
        super::super::ctx::set_thread_exit_inbox_empty_hook(Box::new(move || {
            empty_tx.send(()).unwrap();
            publish_rx.recv().unwrap();
        }));
        let target = std::thread::spawn(move || {
            let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, unrelated);
            }
            assert_eq!(
                unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, ptr::null_mut()) },
                0
            );
            let activation = super::super::ctx::activate(target_engine.shared());
            ready_tx
                .send(unsafe { libc::pthread_self() } as usize)
                .unwrap();
            exit_rx.recv().unwrap();
            drop(activation);
        });
        let target_pthread = ready_rx.recv().unwrap() as libc::pthread_t;
        let action = Sigaction::for_signal(exit_signal_mask_handler as *const () as usize);
        let registration = SignalRegistration::new(
            Arc::clone(engine.control()),
            DeferredSignalCallback::ImageNative(exit_signal_mask_handler as *const () as usize),
            signum,
            action,
        );
        engine.control().signal_inbox.register(registration);
        let stub = materialize_signal_stub(registration).unwrap();
        let kernel = action.for_kernel_stub(stub);
        kernel.replace(signum).unwrap();
        assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
        wait_for_test_kernel_frames(registration, 1);
        exit_tx.send(()).unwrap();
        empty_rx.recv().unwrap();
        assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
        wait_for_test_kernel_frames(registration, 2);
        publish_tx.send(()).unwrap();
        target.join().unwrap();

        let observed = EXIT_CALLBACK_MASK.load(Ordering::Acquire);
        assert_eq!(
            observed & (1u64 << unrelated),
            0,
            "thread-exit drain exposed its block-all cutoff to the callback"
        );
        assert_ne!(
            observed & (1u64 << signum),
            0,
            "deferred callback did not observe its normal handler mask"
        );
        assert_eq!(
            EXIT_CALLBACK_COUNT.load(Ordering::Acquire),
            2,
            "a pthread signal published after an empty exit drain was lost"
        );
        saved.replace(signum).unwrap();
        registration.deactivate();
        registration.wait_for_kernel_deliveries();
        engine.wait_closed().unwrap();
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::target_pthread_exit_callback_observes_the_pre_exit_signal_mask",
        THREAD_EXIT_MASK_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated target-pthread exit-mask regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn kernel_normalization_rejects_restorer_and_supported_flag_changes() {
    const SA_RESTORER: i32 = 0x0400_0000;
    const SA_UNSUPPORTED: i32 = 0x0000_0400;
    const SA_EXPOSE_TAGBITS: i32 = 0x0000_0800;
    let mut requested: libc::sigaction = unsafe { std::mem::zeroed() };
    requested.sa_sigaction = 0x1_1000;
    requested.sa_flags = libc::SA_RESTART | SA_RESTORER | SA_UNSUPPORTED | SA_EXPOSE_TAGBITS;
    requested.sa_restorer = Some(first_test_restorer);
    assert_eq!(unsafe { libc::sigemptyset(&mut requested.sa_mask) }, 0);
    let requested = unsafe { Sigaction::copy_from(ptr::from_ref(&requested) as u64) }.unwrap();

    let mut accepted: libc::sigaction = unsafe { std::mem::zeroed() };
    requested.write_to(ptr::from_mut(&mut accepted) as u64);
    accepted.sa_flags &= !SA_UNSUPPORTED;
    let accepted = unsafe { Sigaction::copy_from(ptr::from_ref(&accepted) as u64) }.unwrap();
    assert!(accepted.is_kernel_normalization_of(&requested));

    let mut wrong_restorer: libc::sigaction = unsafe { std::mem::zeroed() };
    accepted.write_to(ptr::from_mut(&mut wrong_restorer) as u64);
    wrong_restorer.sa_restorer = Some(second_test_restorer);
    let wrong_restorer =
        unsafe { Sigaction::copy_from(ptr::from_ref(&wrong_restorer) as u64) }.unwrap();
    assert!(!wrong_restorer.is_kernel_normalization_of(&requested));

    let mut missing_supported: libc::sigaction = unsafe { std::mem::zeroed() };
    accepted.write_to(ptr::from_mut(&mut missing_supported) as u64);
    missing_supported.sa_flags &= !SA_EXPOSE_TAGBITS;
    let missing_supported =
        unsafe { Sigaction::copy_from(ptr::from_ref(&missing_supported) as u64) }.unwrap();
    assert!(!missing_supported.is_kernel_normalization_of(&requested));
}

#[test]
fn rejected_request_only_candidate_rolls_back_its_normalized_kernel_action() {
    if std::env::var_os(REQUEST_ONLY_ROLLBACK_CHILD).is_some() {
        const SA_UNSUPPORTED: i32 = 0x0000_0400;
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let requested = Sigaction::empty(
            detached_close_external_handler as *const () as usize,
            libc::SA_RESTART | SA_UNSUPPORTED,
        );
        requested.replace(signum).unwrap();
        let normalized = kernel_current(signum).unwrap();
        assert!(!normalized.same_disposition(&requested));
        assert!(kernel_request_matches(&normalized, &requested));

        reject_committed_candidate(signum, requested, None, saved, None).unwrap();

        assert!(kernel_current(signum).unwrap().same_disposition(&saved));
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::rejected_request_only_candidate_rolls_back_its_normalized_kernel_action",
        REQUEST_ONLY_ROLLBACK_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated request-only rollback regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn reconcile_solidifies_a_request_only_node_with_the_exact_old_action() {
    if std::env::var_os(REQUEST_ONLY_RECONCILE_CHILD).is_some() {
        const SA_UNSUPPORTED: i32 = 0x0000_0400;
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let control = control();
        let visible = Sigaction::empty(0x2_7200, libc::SA_RESTART | SA_UNSUPPORTED);
        let requested = visible
            .for_kernel_stub(detached_close_external_handler as *const () as usize)
            .with_runtime_restorer();
        assert_eq!(requested.install(signum), 0);
        let actual_old = kernel_current(signum).unwrap();
        assert!(!actual_old.same_disposition(&requested));
        assert!(kernel_request_matches(&actual_old, &requested));

        let registration = registration(&control, signum, 18, visible);
        let mut descriptor = descriptor(
            &control,
            registration,
            visible,
            requested,
            SignalChain {
                base: saved,
                nodes: Vec::new(),
            },
        );
        descriptor.accepted_kernel = None;
        let mut registry = SignalRegistry::default();
        registry.stubs.insert(requested.handler(), descriptor);
        registry.chains.insert(
            signum,
            SignalChain {
                base: saved,
                nodes: vec![DispositionNode {
                    install_control: Arc::clone(&control),
                    install_owner: control.id(),
                    callback_owner: Some(control.id()),
                    guest: visible,
                    kernel: requested,
                    accepted_kernel: None,
                    registration: Some(registration),
                }],
            },
        );

        let old = reconcile_prior_chain(&mut registry, signum, actual_old).unwrap();

        assert_eq!(old.handler(), visible.handler());
        assert!(
            registry.chains[&signum].nodes[0]
                .accepted_kernel
                .is_some_and(|accepted| accepted.same_disposition(&actual_old))
        );
        assert!(
            registry.stubs[&requested.handler()]
                .accepted_kernel
                .is_some_and(|accepted| accepted.same_disposition(&actual_old))
        );
        registration.deactivate();
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::reconcile_solidifies_a_request_only_node_with_the_exact_old_action",
        REQUEST_ONLY_RECONCILE_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated request-only reconciliation regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn compensation_preserves_a_newer_normalized_writer_by_exact_identity() {
    if std::env::var_os(EXACT_COMPENSATION_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let requested =
            Sigaction::for_signal(detached_close_external_handler as *const () as usize)
                .with_runtime_restorer();
        requested.replace(signum).unwrap();
        let installed = kernel_current(signum).unwrap();
        assert!(installed.same_disposition(&requested));

        // libc replaces MIRVM's restorer with its own, producing a second
        // exact kernel snapshot that still satisfies the same request.
        assert_eq!(requested.install(signum), 0);
        let newer = kernel_current(signum).unwrap();
        assert!(!newer.same_disposition(&installed));
        assert!(kernel_request_matches(&newer, &installed));

        compensate_concurrent_writer(signum, installed, saved).unwrap();

        assert!(kernel_current(signum).unwrap().same_disposition(&newer));
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::compensation_preserves_a_newer_normalized_writer_by_exact_identity",
        EXACT_COMPENSATION_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated exact-compensation regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn detached_successor_accepts_distinct_valid_snapshots_of_its_predecessor() {
    if std::env::var_os(DETACHED_SNAPSHOT_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let first_control = control();
        let second_control = control();
        let third_control = control();

        install_signal(&first_control, signum, 0x2_7300, Some((19, 0x2_7300))).unwrap();
        let first_stub = kernel_current(signum).unwrap();
        install_signal(&second_control, signum, 0x2_7400, Some((20, 0x2_7400))).unwrap();
        let second_stub = kernel_current(signum).unwrap();

        let external = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
        assert_eq!(external.install(signum), 0);
        assert_eq!(first_stub.install(signum), 0);

        // Reconciliation sees A after libc has replaced MIRVM's exact
        // restorer. B is detached, so its fallback still retains A's
        // earlier, equally valid exact snapshot.
        let native = Sigaction::for_signal(detached_close_native_handler as *const () as usize);
        install_sigaction_value(
            &first_control,
            signum,
            Some(native),
            Some(super::super::thunks::SignalHandlerResolution::Unknown),
        )
        .unwrap();
        {
            let registry = REGISTRY.lock().unwrap();
            let refreshed = registry.stubs[&first_stub.handler()]
                .accepted_kernel
                .unwrap();
            let detached = registry.stubs[&second_stub.handler()].fallback.nodes[0]
                .accepted_kernel
                .unwrap();
            assert!(!refreshed.same_disposition(&detached));
            assert!(kernel_request_matches(
                &refreshed,
                &registry.stubs[&first_stub.handler()].kernel
            ));
            assert!(kernel_request_matches(
                &detached,
                &registry.stubs[&first_stub.handler()].kernel
            ));
        }

        assert_eq!(second_stub.install(signum), 0);
        let old = install_signal(&third_control, signum, 0x2_7500, Some((21, 0x2_7500)))
            .expect("detached B could not rebuild through A's older valid snapshot");
        assert_eq!(old, 0x2_7400);

        assert_eq!(saved.install(signum), 0);
        deactivate_engine(&first_control).unwrap();
        deactivate_engine(&second_control).unwrap();
        deactivate_engine(&third_control).unwrap();
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::detached_successor_accepts_distinct_valid_snapshots_of_its_predecessor",
        DETACHED_SNAPSHOT_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated detached-snapshot regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cleared_inbox_event_blocks_the_finalizing_seal_until_delivery_is_held() {
    let control = control();
    let signum = libc::SIGUSR1 as usize;
    let registration = registration(&control, signum as i32, 1, Sigaction::for_signal(0x1_0000));
    control.signal_inbox.register(registration);
    registration.publish(signum as i32);
    control.prepare_signal_seal_test();

    let (cleared_tx, cleared_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *AFTER_INBOX_CLEAR_HOOK.lock().unwrap() = Some(AfterInboxClearHook {
        registration: ptr::from_ref(registration) as usize,
        callback: Box::new(move || {
            cleared_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        }),
    });

    let worker_control = Arc::clone(&control);
    let worker = std::thread::spawn(move || {
        worker_control
            .signal_inbox
            .take_delivery(signum)
            .expect("published inbox event disappeared")
    });
    cleared_rx.recv().unwrap();
    let sealed_while_cleared = try_seal_engine(&control);
    resume_tx.send(()).unwrap();
    let taken = worker.join().unwrap();

    assert!(ptr::eq(taken.registration(), registration));
    assert!(
        !sealed_while_cleared,
        "Finalizing crossed the cleared inbox event before its delivery acquired a hold"
    );
    drop(taken);
    assert!(try_seal_engine(&control));
}

#[test]
fn external_raise_handler_can_reenter_signal_install_and_raise() {
    if std::env::var_os(RAISE_INSTALL_RACE_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let reentrant_signum = libc::SIGURG;
        let reentrant_saved = kernel_current(reentrant_signum).unwrap();
        let _reentrant_restore = RestoreSignal {
            signum: reentrant_signum,
            action: reentrant_saved,
        };
        let external = Sigaction::for_signal(raise_external_handler as *const () as usize);
        assert_eq!(external.install(signum), 0);
        let reentrant = Sigaction::for_signal(reentrant_raise_handler as *const () as usize);
        assert_eq!(reentrant.install(reentrant_signum), 0);
        RAISE_EXTERNAL_RAN.store(0, Ordering::SeqCst);
        REENTRANT_RAISE_RAN.store(0, Ordering::SeqCst);
        REENTRANT_INSTALL_RESULT.store(0, Ordering::SeqCst);

        let control = control();
        *REENTRANT_INSTALL_CONTROL.lock().unwrap() = Some(Arc::clone(&control));
        assert_eq!(crate::os::process::raise(signum), 0);
        assert_eq!(RAISE_EXTERNAL_RAN.load(Ordering::SeqCst), 1);
        assert_eq!(REENTRANT_INSTALL_RESULT.load(Ordering::SeqCst), 1);
        assert_eq!(REENTRANT_RAISE_RAN.load(Ordering::SeqCst), 1);
        *REENTRANT_INSTALL_CONTROL.lock().unwrap() = None;
        deactivate_engine(&control).unwrap();
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::external_raise_handler_can_reenter_signal_install_and_raise",
        RAISE_INSTALL_RACE_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated raise/install regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn successful_install_commits_before_an_immediate_raw_overwrite() {
    if std::env::var_os(INSTALL_OVERWRITE_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let raw = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
        let captured_stub = Arc::new(Mutex::new(None));
        let hook_result = Arc::clone(&captured_stub);
        *AFTER_INSTALL_REPLACE_HOOK.lock().unwrap() =
            Some(Box::new(move |hook_signum, requested, actual_old| {
                assert_eq!(hook_signum, signum);
                assert!(actual_old.same_disposition(&saved));
                let displaced = raw.replace(signum).unwrap();
                assert!(kernel_request_matches(&displaced, &requested));
                *hook_result.lock().unwrap() = Some(displaced);
            }));

        let control = control();
        let visible = Sigaction::for_signal(0x2_0000);
        let old = install_sigaction_value(
            &control,
            signum,
            Some(visible),
            Some(super::super::thunks::SignalHandlerResolution::Valid {
                control: Arc::clone(&control),
                func: 9,
            }),
        )
        .unwrap();
        assert!(old.same_disposition(&saved));
        assert!(kernel_current(signum).unwrap().satisfies_request(&raw));

        let captured_stub = captured_stub.lock().unwrap().unwrap();
        let descriptor = REGISTRY.lock().unwrap().stubs[&captured_stub.handler()].clone();
        assert!(
            descriptor.accepted_kernel.is_none(),
            "post-install query crossed the immediate raw overwrite"
        );
        let registration = descriptor.registration;
        assert!(registration.safe_point_delivery().is_some());

        // The raw writer retained the exact oldact even though its action
        // won immediately. Restoring that fixed stub must remain usable.
        assert_eq!(captured_stub.install(signum), 0);
        let delivery = current_delivery(signum).expect("committed stub became inactive");
        assert!(ptr::eq(delivery.registration(), registration));
        drop(delivery);

        deactivate_engine(&control).unwrap();
        assert!(kernel_current(signum).unwrap().satisfies_request(&saved));
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::successful_install_commits_before_an_immediate_raw_overwrite",
        INSTALL_OVERWRITE_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated install-overwrite regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn installed_stub_tracks_the_exact_kernel_normalized_action() {
    if std::env::var_os(NORMALIZED_ACTION_CHILD).is_some() {
        const SA_UNSUPPORTED: i32 = 0x0000_0400;
        const SA_EXPOSE_TAGBITS: i32 = 0x0000_0800;
        const UNKNOWN_PROBE_FLAG: i32 = 0x0000_0200;

        let signum = libc::SIGWINCH;
        let original = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: original,
        };
        let baseline = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
        assert_eq!(baseline.install(signum), 0);
        let saved = kernel_current(signum).unwrap();
        let control = control();
        let requested = Sigaction::empty(
            0x2_7000,
            libc::SA_RESTART | SA_UNSUPPORTED | SA_EXPOSE_TAGBITS | UNKNOWN_PROBE_FLAG,
        );
        let old = install_sigaction_value(
            &control,
            signum,
            Some(requested),
            Some(super::super::thunks::SignalHandlerResolution::Valid {
                control: Arc::clone(&control),
                func: 16,
            }),
        )
        .unwrap();
        assert!(old.same_disposition(&saved));

        let installed = kernel_current(signum).unwrap();
        assert_eq!(
            installed.flags() & (SA_UNSUPPORTED | UNKNOWN_PROBE_FLAG),
            0,
            "kernel did not clear the probing flags used by this regression"
        );
        assert_ne!(
            installed.flags() & SA_EXPOSE_TAGBITS,
            0,
            "kernel cleared the supported SA_EXPOSE_TAGBITS probe"
        );
        let descriptor = REGISTRY.lock().unwrap().stubs[&installed.handler()].clone();
        assert!(
            installed.is_kernel_normalization_of(&descriptor.kernel),
            "kernel action is not a strict normalization of the recorded request"
        );
        assert!(
            descriptor
                .accepted_kernel
                .is_some_and(|accepted| accepted.same_disposition(&installed)),
            "read-only post-install observation did not retain the exact accepted action"
        );

        // A raw writer may retain and later restore the exact fixed-stub
        // oldact. Wrapped query must still expose only the normalized guest
        // action, including the kernel-cleared probing bits.
        let fixed_oldact = saved.replace(signum).unwrap();
        assert!(fixed_oldact.same_disposition(&installed));
        assert_eq!(fixed_oldact.install(signum), 0);
        let visible = install_sigaction_value(&control, signum, None, None).unwrap();
        assert_eq!(visible.handler(), requested.handler());
        assert_eq!(visible.flags() & (SA_UNSUPPORTED | UNKNOWN_PROBE_FLAG), 0);
        assert_ne!(visible.flags() & SA_EXPOSE_TAGBITS, 0);

        // Editing a retained oldact must preserve ordinary mask/flag edits
        // while stripping MIRVM's SA_SIGINFO/restorer adapter details.
        let mut edited: libc::sigaction = unsafe { std::mem::zeroed() };
        fixed_oldact.write_to(ptr::from_mut(&mut edited) as u64);
        edited.sa_flags |= libc::SA_NOCLDSTOP;
        assert_eq!(
            unsafe { libc::sigaddset(&mut edited.sa_mask, libc::SIGUSR2) },
            0
        );
        let edited = unsafe { Sigaction::copy_from(ptr::from_ref(&edited) as u64) }.unwrap();
        let replaced = install_sigaction_value(
            &control,
            signum,
            Some(edited),
            Some(super::super::thunks::SignalHandlerResolution::Unknown),
        )
        .unwrap();
        assert!(replaced.same_disposition(&visible));
        let edited_visible = install_sigaction_value(&control, signum, None, None).unwrap();
        assert_eq!(edited_visible.handler(), requested.handler());
        assert_ne!(edited_visible.flags() & libc::SA_NOCLDSTOP, 0);
        assert_eq!(edited_visible.flags() & libc::SA_SIGINFO, 0);
        assert_eq!(
            edited_visible.flags() & (SA_UNSUPPORTED | UNKNOWN_PROBE_FLAG),
            0
        );
        let mut edited_visible_raw: libc::sigaction = unsafe { std::mem::zeroed() };
        edited_visible.write_to(ptr::from_mut(&mut edited_visible_raw) as u64);
        assert!(edited_visible_raw.sa_restorer.is_none());
        assert_eq!(
            unsafe { libc::sigismember(&edited_visible_raw.sa_mask, libc::SIGUSR2) },
            1
        );

        deactivate_engine(&control).unwrap();
        assert!(kernel_current(signum).unwrap().same_disposition(&saved));
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::installed_stub_tracks_the_exact_kernel_normalized_action",
        NORMALIZED_ACTION_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated normalized-action regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn close_restores_an_exact_kernel_restorer_snapshot() {
    if std::env::var_os(EXACT_RESTORE_CHILD).is_some() {
        const SA_RESTORER: i32 = 0x0400_0000;
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };

        let mut raw: libc::sigaction = unsafe { std::mem::zeroed() };
        raw.sa_sigaction = detached_close_external_handler as *const () as usize;
        raw.sa_flags = libc::SA_RESTART | SA_RESTORER;
        raw.sa_restorer = Some(first_test_restorer);
        assert_eq!(unsafe { libc::sigemptyset(&mut raw.sa_mask) }, 0);
        assert_eq!(
            unsafe { libc::sigaddset(&mut raw.sa_mask, libc::SIGUSR2) },
            0
        );
        let raw = unsafe { Sigaction::copy_from(ptr::from_ref(&raw) as u64) }.unwrap();
        raw.replace_exact(signum).unwrap();
        let baseline = kernel_current(signum).unwrap();
        assert!(baseline.same_disposition(&raw));

        let control = control();
        install_signal(&control, signum, 0x2_7100, Some((17, 0x2_7100))).unwrap();
        deactivate_engine(&control).unwrap();
        assert!(kernel_current(signum).unwrap().same_disposition(&baseline));
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::close_restores_an_exact_kernel_restorer_snapshot",
        EXACT_RESTORE_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated exact-restorer regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn current_delivery_follows_live_lower_and_detached_fixed_stubs() {
    if std::env::var_os(CURRENT_DELIVERY_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let first_control = control();
        let second_control = control();
        let third_control = control();

        install_signal(&first_control, signum, 0x2_1000, Some((10, 0x2_1000))).unwrap();
        let first_stub = kernel_current(signum).unwrap();
        let first_registration = REGISTRY.lock().unwrap().stubs[&first_stub.handler()].registration;
        install_signal(&second_control, signum, 0x2_2000, Some((11, 0x2_2000))).unwrap();

        // A raw restore can make a lower live node current.
        assert_eq!(first_stub.install(signum), 0);
        let lower = current_delivery(signum).expect("live lower stub was not resolved");
        assert!(ptr::eq(lower.registration(), first_registration));
        drop(lower);

        // Replace the managed prefix with native state, then install a new
        // managed node. The first stub now exists only in its descriptor.
        let raw = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
        assert_eq!(raw.install(signum), 0);
        install_signal(&third_control, signum, 0x2_3000, Some((12, 0x2_3000))).unwrap();
        assert_eq!(first_stub.install(signum), 0);
        let detached = current_delivery(signum).expect("detached stub was not resolved");
        assert!(ptr::eq(detached.registration(), first_registration));
        drop(detached);

        assert_eq!(saved.install(signum), 0);
        deactivate_engine(&first_control).unwrap();
        deactivate_engine(&second_control).unwrap();
        deactivate_engine(&third_control).unwrap();
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::current_delivery_follows_live_lower_and_detached_fixed_stubs",
        CURRENT_DELIVERY_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated current-delivery regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn raw_restore_of_a_closed_fixed_stub_exits_seventy() {
    if std::env::var_os(INACTIVE_STUB_CHILD).is_some() {
        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let control = control();
        install_signal(&control, signum, 0x2_4000, Some((13, 0x2_4000))).unwrap();
        let raw = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
        let closed_stub = raw.replace(signum).unwrap();
        deactivate_engine(&control).unwrap();

        // A raw caller kept MIRVM's oldact past the callback owner's
        // lifetime. The process must not continue after silently losing
        // the signal through that dangling callback address.
        assert_eq!(closed_stub.install(signum), 0);
        assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
        let _ = saved;
        panic!("inactive fixed stub returned from its signal adapter");
    }

    let output = run_signal_test_child(
        "vm::signal::tests::raw_restore_of_a_closed_fixed_stub_exits_seventy",
        INACTIVE_STUB_CHILD,
    );
    assert_eq!(
        output.status.code(),
        Some(70),
        "inactive fixed stub child did not fail loud; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn wrapped_raise_of_a_raw_restored_closed_stub_faults_without_retrying_forever() {
    if std::env::var_os(HOST_RAISE_INACTIVE_STUB_CHILD).is_some() {
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
            unsafe { libc::_exit(124) }
        });

        let signum = libc::SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let owner = super::super::ctx::Engine::new(Shared::new(Module::default()));
        install_signal(owner.control(), signum, 0x2_4100, Some((14, 0x2_4100))).unwrap();
        let closed_stub = kernel_current(signum).unwrap();
        owner.wait_closed().unwrap();
        closed_stub.replace(signum).unwrap();

        let raiser = super::super::ctx::Engine::new(Shared::new(Module::default()));
        let activation = super::super::ctx::activate(raiser.shared());
        let exception = super::super::unwind::catch_raw(|| {
            super::super::ctx::raise_signal(activation.ctx(), signum)
        })
        .expect_err("HostRaise unexpectedly returned through a closed fixed stub");
        let fault = exception
            .take_engine_fault(raiser.shared())
            .unwrap_or_else(|exception| exception.resume_or_rethrow())
            .finish();
        assert_eq!(fault.code, 70);
        assert!(fault.message.contains("closed MIRVM fixed stub"));

        // Changing the surrounding action cannot make the closed handler
        // address live again. In particular, a raw oldact editor must not
        // turn the prompt failure above into an endless retry loop.
        let mut edited: libc::sigaction = unsafe { std::mem::zeroed() };
        closed_stub.write_to(ptr::from_mut(&mut edited) as u64);
        assert_eq!(
            unsafe { libc::sigaddset(&mut edited.sa_mask, libc::SIGUSR2) },
            0
        );
        let edited = unsafe { Sigaction::copy_from(ptr::from_ref(&edited) as u64) }.unwrap();
        assert_eq!(edited.install(signum), 0);
        let exception = super::super::unwind::catch_raw(|| {
            super::super::ctx::raise_signal(activation.ctx(), signum)
        })
        .expect_err("HostRaise retried an edited closed fixed stub forever");
        let fault = exception
            .take_engine_fault(raiser.shared())
            .unwrap_or_else(|exception| exception.resume_or_rethrow())
            .finish();
        assert_eq!(fault.code, 70);
        assert!(fault.message.contains("closed MIRVM fixed stub"));
        drop(activation);
        raiser.wait_closed().unwrap();
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::wrapped_raise_of_a_raw_restored_closed_stub_faults_without_retrying_forever",
        HOST_RAISE_INACTIVE_STUB_CHILD,
    );
    assert!(
        output.status.success(),
        "wrapped HostRaise against an inactive fixed stub did not fail promptly: status={:?}\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stale_chain_keeps_the_matching_lower_stub_and_its_accepted_event() {
    let first_control = control();
    let second_control = control();
    let signum = libc::SIGUSR1;
    let base = Sigaction::for_signal(0x2000);
    let first_visible = Sigaction::for_signal(0x3000);
    let second_visible = Sigaction::for_signal(0x4000);
    let first_kernel = first_visible.for_kernel_stub(0x5000);
    let second_kernel = second_visible.for_kernel_stub(0x6000);
    let first = registration(&first_control, signum, 1, first_visible);
    let second = registration(&second_control, signum, 2, second_visible);
    first_control.signal_inbox.register(first);
    second_control.signal_inbox.register(second);
    second.publish(signum);

    let mut registry = SignalRegistry::default();
    registry.stubs.insert(
        first_kernel.handler(),
        descriptor(
            &first_control,
            first,
            first_visible,
            first_kernel,
            SignalChain {
                base,
                nodes: Vec::new(),
            },
        ),
    );
    registry.stubs.insert(
        second_kernel.handler(),
        descriptor(
            &second_control,
            second,
            second_visible,
            second_kernel,
            SignalChain {
                base,
                nodes: vec![DispositionNode {
                    install_control: Arc::clone(&first_control),
                    install_owner: first_control.id(),
                    callback_owner: Some(first_control.id()),
                    guest: first_visible,
                    kernel: first_kernel,
                    accepted_kernel: Some(first_kernel),
                    registration: Some(first),
                }],
            },
        ),
    );
    registry.chains.insert(
        signum,
        SignalChain {
            base,
            nodes: vec![
                DispositionNode {
                    install_control: Arc::clone(&first_control),
                    install_owner: first_control.id(),
                    callback_owner: Some(first_control.id()),
                    guest: first_visible,
                    kernel: first_kernel,
                    accepted_kernel: Some(first_kernel),
                    registration: Some(first),
                },
                DispositionNode {
                    install_control: Arc::clone(&second_control),
                    install_owner: second_control.id(),
                    callback_owner: Some(second_control.id()),
                    guest: second_visible,
                    kernel: second_kernel,
                    accepted_kernel: Some(second_kernel),
                    registration: Some(second),
                },
            ],
        },
    );

    let old = reconcile_prior_chain(&mut registry, signum, first_kernel).unwrap();

    assert!(old.same_disposition(&first_visible));
    let chain = &registry.chains[&signum];
    assert_eq!(chain.nodes.len(), 1);
    assert!(chain.nodes[0].kernel.same_disposition(&first_kernel));
    assert!(second_control.signal_inbox.has_pending());
    // Removing a committed stub from the current logical chain must not
    // close its process-lifetime adapter. Native code may restore it later
    // while both of its owners are still live.
    assert!(second.safe_point_delivery().is_some());
    assert!(first.safe_point_delivery().is_some());
}

#[test]
fn detached_successor_folds_around_a_closed_predecessor_and_checks_full_action() {
    let first_control = control();
    let second_control = control();
    let signum = libc::SIGUSR1;
    let base = Sigaction::for_signal(0x7000);
    let first_visible = Sigaction::for_signal(0x8000);
    let second_visible = Sigaction::for_signal(0x9000);
    let first_kernel = first_visible.for_kernel_stub(0xa000);
    let second_kernel = second_visible.for_kernel_stub(0xb000);
    let first = registration(&first_control, signum, 3, first_visible);
    let second = registration(&second_control, signum, 4, second_visible);
    let mut registry = SignalRegistry::default();
    registry.stubs.insert(
        first_kernel.handler(),
        descriptor(
            &first_control,
            first,
            first_visible,
            first_kernel,
            SignalChain {
                base,
                nodes: Vec::new(),
            },
        ),
    );
    registry.stubs.insert(
        second_kernel.handler(),
        descriptor(
            &second_control,
            second,
            second_visible,
            second_kernel,
            SignalChain {
                base,
                nodes: vec![DispositionNode {
                    install_control: Arc::clone(&first_control),
                    install_owner: first_control.id(),
                    callback_owner: Some(first_control.id()),
                    guest: first_visible,
                    kernel: first_kernel,
                    accepted_kernel: Some(first_kernel),
                    registration: Some(first),
                }],
            },
        ),
    );

    let mut retired = Vec::new();
    deactivate_owner_descriptors(&mut registry, first_control.id(), &mut retired);
    let second_descriptor = &registry.stubs[&second_kernel.handler()];
    assert!(second_descriptor.fallback.base.same_disposition(&base));
    assert!(second_descriptor.fallback.nodes.is_empty());

    let (visible, rebuilt) = plan_stub_chain(&registry, signum, second_kernel)
        .unwrap()
        .unwrap();
    assert!(visible.same_disposition(&second_visible));
    assert!(rebuilt.base.same_disposition(&base));
    assert_eq!(rebuilt.nodes.len(), 1);

    let mismatched = Sigaction::for_signal(second_kernel.handler());
    assert!(plan_stub_chain(&registry, signum, mismatched).is_err());
}

#[test]
fn detached_stub_snapshot_drops_a_closed_external_native_predecessor() {
    let native_control = control();
    let callback_control = control();
    let signum = libc::SIGUSR1;
    let base = Sigaction::for_signal(0xc000);
    let native = Sigaction::for_signal(0xd000);
    let visible = Sigaction::for_signal(0xe000);
    let kernel = visible.for_kernel_stub(0xf000);
    let registration = registration(&callback_control, signum, 5, visible);
    let mut registry = SignalRegistry::default();
    registry.stubs.insert(
        kernel.handler(),
        descriptor(
            &callback_control,
            registration,
            visible,
            kernel,
            SignalChain {
                base,
                nodes: vec![node(&native_control, native, native, None)],
            },
        ),
    );

    let mut retired = Vec::new();
    deactivate_owner_descriptors(&mut registry, native_control.id(), &mut retired);
    let (_, rebuilt) = plan_stub_chain(&registry, signum, kernel).unwrap().unwrap();

    assert!(rebuilt.base.same_disposition(&base));
    assert_eq!(rebuilt.nodes.len(), 1);
    assert!(rebuilt.nodes[0].kernel.same_disposition(&kernel));
}

#[test]
fn exposed_fixed_stub_is_canonicalized_before_becoming_guest_visible_again() {
    let control = control();
    let signum = libc::SIGUSR1;
    let base = Sigaction::for_signal(0x1_0000);
    let visible = Sigaction::for_signal(0x1_1000);
    let kernel = visible.for_kernel_stub(0x1_2000);
    let registration = registration(&control, signum, 6, visible);
    let mut registry = SignalRegistry::default();
    registry.stubs.insert(
        kernel.handler(),
        descriptor(
            &control,
            registration,
            visible,
            kernel,
            SignalChain {
                base,
                nodes: Vec::new(),
            },
        ),
    );

    let exact = canonical_guest_action(&registry, kernel);
    let signal_style = canonical_guest_action(&registry, Sigaction::for_signal(kernel.handler()));

    assert!(exact.same_disposition(&visible));
    assert_eq!(signal_style.handler(), visible.handler());
    assert_eq!(signal_style.flags(), libc::SA_RESTART);
}

#[test]
fn close_restores_the_predecessor_of_a_detached_current_stub() {
    let signum = libc::SIGURG;
    let saved = kernel_current(signum).unwrap();
    let _restore = RestoreSignal {
        signum,
        action: saved,
    };
    let native = Sigaction::for_signal(detached_close_native_handler as *const () as usize);
    assert_eq!(native.install(signum), 0);
    let baseline = kernel_current(signum).unwrap();
    let first_control = control();
    let second_control = control();

    install_signal(&first_control, signum, 0x1_3000, Some((7, 0x1_3000))).unwrap();
    let first_stub = kernel_current(signum).unwrap();
    let external = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
    assert_eq!(external.install(signum), 0);
    install_signal(&second_control, signum, 0x1_4000, Some((8, 0x1_4000))).unwrap();
    assert_eq!(first_stub.install(signum), 0);

    assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
    for _ in 0..10_000 {
        if first_control.signal_inbox.has_pending() {
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        first_control.signal_inbox.has_pending(),
        "a detached process-lifetime stub stopped accepting signals while its owner was live"
    );
    let accepted = first_control
        .signal_inbox
        .take_delivery(signum as usize)
        .expect("detached stub event was not available at the owner safe point");
    let first_registration = REGISTRY.lock().unwrap().stubs[&first_stub.handler()].registration;
    assert!(ptr::eq(accepted.registration(), first_registration));
    drop(accepted);

    deactivate_engine(&first_control).unwrap();
    let after_first_close = kernel_current(signum).unwrap();
    DETACHED_CLOSE_NATIVE_RAN.store(0, Ordering::SeqCst);
    assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
    for _ in 0..10_000 {
        if DETACHED_CLOSE_NATIVE_RAN.load(Ordering::SeqCst) != 0 {
            break;
        }
        std::thread::yield_now();
    }
    let native_ran = DETACHED_CLOSE_NATIVE_RAN.load(Ordering::SeqCst);
    deactivate_engine(&second_control).unwrap();
    let after_second_close = kernel_current(signum).unwrap();

    assert!(after_first_close.same_disposition(&baseline));
    assert_eq!(native_ran, 1);
    assert!(after_second_close.same_disposition(&baseline));
}
