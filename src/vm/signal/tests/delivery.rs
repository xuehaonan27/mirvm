//! Deferred delivery: inbox cells, kernel adapter frames and the target pthread.

use super::*;

#[cfg(target_os = "linux")]
static EXIT_CALLBACK_MASK: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
static EXIT_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(target_os = "linux")]
unsafe extern "C-unwind" fn exit_signal_mask_handler(_signum: i32) {
    let mask = Sigaction::current_standard_mask_bits().unwrap_or_else(|_| exit_now(73));
    EXIT_CALLBACK_MASK.store(mask, Ordering::Release);
    EXIT_CALLBACK_COUNT.fetch_add(1, Ordering::Release);
}

const CURRENT_DELIVERY_CHILD: &str = "MIRVM_SIGNAL_CURRENT_DELIVERY_CHILD";

const THREAD_GENERATIONS_CHILD: &str = "MIRVM_SIGNAL_THREAD_GENERATIONS_CHILD";

#[cfg(target_os = "linux")]
const THREAD_EXIT_MASK_CHILD: &str = "MIRVM_SIGNAL_THREAD_EXIT_MASK_CHILD";

#[test]
fn ordinary_delivery_does_not_keep_a_kernel_adapter_frame_in_flight() {
    let control = control();
    let visible = Sigaction::for_signal(0x1000);
    let registration = registration(&control, SIGUSR1, 0, visible);
    let delivery = registration.safe_point_delivery().unwrap();

    registration.deactivate();
    registration.wait_for_kernel_deliveries();

    assert!(ptr::eq(delivery.registration(), registration));
    drop(delivery);
}

#[test]
fn new_thread_does_not_allocate_a_cell_for_a_closed_registration() {
    let control = control();
    let registration = registration(&control, SIGWINCH, 0, Sigaction::for_signal(0x2_7000));
    registration.deactivate();
    registration.wait_for_kernel_deliveries();

    let has_closed_cell = std::thread::spawn(move || {
        initialize_current_thread_inbox();
        let inbox = current_thread_inbox().unwrap();
        let has_cell = inbox
            .cell_for(ptr::from_ref(registration).cast_mut())
            .is_some();
        current_thread_inbox_handle().deactivate();
        has_cell
    })
    .join()
    .unwrap();

    assert!(!has_closed_cell);
}

#[test]
fn target_pthread_preserves_each_kernel_selected_registration_generation() {
    if std::env::var_os(THREAD_GENERATIONS_CHILD).is_some() {
        let signum = SIGWINCH;
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
            ready_tx.send(current_thread()).unwrap();
            drain_rx.recv().unwrap();
            let mut generations = Vec::new();
            while let Some((delivery, delivered_signum)) = take_current_thread_delivery(0) {
                assert_eq!(delivered_signum, signum);
                generations.push(delivery.registration().generation());
                drop(delivery);
            }
            current_thread_inbox_handle().deactivate();
            result_tx.send(generations).unwrap();
        });
        let target_pthread = ready_rx.recv().unwrap();

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
            assert_eq!(send_to_thread(target_pthread, signum), 0);
            wait_for_test_kernel_frames(registration, 1);
            assert_eq!(registration.thread_pending.load(Ordering::Acquire), 1);
            if func == 23 {
                assert_eq!(send_to_thread(target_pthread, signum), 0);
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
        "vm::signal::tests::delivery::target_pthread_preserves_each_kernel_selected_registration_generation",
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

/// The scenario needs a signal to reach a thread that is already inside its final TSD round, and
/// this platform cannot deliver one there at all: measured, `pthread_kill` answers `ESRCH` for a
/// thread in that window. The same measurement is why `crate::vm::deferred::tests::tsd` scopes its
/// thread-exit case the same way.
#[cfg(target_os = "linux")]
#[test]
fn target_pthread_exit_callback_observes_the_pre_exit_signal_mask() {
    if std::env::var_os(THREAD_EXIT_MASK_CHILD).is_some() {
        let signum = SIGWINCH;
        let unrelated = SIGUSR2;
        let saved = kernel_current(signum).unwrap();
        let engine = super::super::super::ctx::Engine::new(Shared::new(Module::default()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        let (empty_tx, empty_rx) = std::sync::mpsc::channel();
        let (publish_tx, publish_rx) = std::sync::mpsc::channel();
        let target_engine = engine.clone();
        EXIT_CALLBACK_MASK.store(u64::MAX, Ordering::Release);
        EXIT_CALLBACK_COUNT.store(0, Ordering::Release);
        super::super::super::ctx::set_thread_exit_inbox_empty_hook(Box::new(move || {
            empty_tx.send(()).unwrap();
            publish_rx.recv().unwrap();
        }));
        let target = std::thread::spawn(move || {
            let mask = SignalMask::empty().with(unrelated);
            assert!(set_thread_mask(MaskOp::Unblock, &mask).is_ok());
            let activation = super::super::super::ctx::activate(target_engine.shared());
            ready_tx.send(current_thread()).unwrap();
            exit_rx.recv().unwrap();
            drop(activation);
        });
        let target_pthread = ready_rx.recv().unwrap();
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
        assert_eq!(send_to_thread(target_pthread, signum), 0);
        wait_for_test_kernel_frames(registration, 1);
        exit_tx.send(()).unwrap();
        empty_rx.recv().unwrap();
        assert_eq!(send_to_thread(target_pthread, signum), 0);
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
        "vm::signal::tests::delivery::target_pthread_exit_callback_observes_the_pre_exit_signal_mask",
        THREAD_EXIT_MASK_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated target-pthread exit-mask regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn current_delivery_follows_live_lower_and_detached_fixed_stubs() {
    if std::env::var_os(CURRENT_DELIVERY_CHILD).is_some() {
        let signum = SIGWINCH;
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
        "vm::signal::tests::delivery::current_delivery_follows_live_lower_and_detached_fixed_stubs",
        CURRENT_DELIVERY_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated current-delivery regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
