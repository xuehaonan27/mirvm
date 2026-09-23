//! Engine close: the finalizing seal, restorer snapshots and closed fixed stubs.

use super::*;

const INACTIVE_STUB_CHILD: &str = "MIRVM_SIGNAL_INACTIVE_STUB_CHILD";

const HOST_RAISE_INACTIVE_STUB_CHILD: &str = "MIRVM_SIGNAL_HOST_RAISE_INACTIVE_STUB_CHILD";

const EXACT_RESTORE_CHILD: &str = "MIRVM_SIGNAL_EXACT_RESTORE_CHILD";

#[test]
fn cleared_inbox_event_blocks_the_finalizing_seal_until_delivery_is_held() {
    let control = control();
    let signum = SIGUSR1 as usize;
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
fn close_restores_an_exact_kernel_restorer_snapshot() {
    if std::env::var_os(EXACT_RESTORE_CHILD).is_some() {
        let signum = SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };

        let mut raw = Sigaction::empty(
            detached_close_external_handler as *const () as usize,
            SA_RESTART | RESTORER_FLAG,
        );
        raw.set_restorer(first_test_restorer);
        raw.add_to_mask(SIGUSR2);
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
        "vm::signal::tests::close::close_restores_an_exact_kernel_restorer_snapshot",
        EXACT_RESTORE_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated exact-restorer regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn raw_restore_of_a_closed_fixed_stub_exits_seventy() {
    if std::env::var_os(INACTIVE_STUB_CHILD).is_some() {
        let signum = SIGWINCH;
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
        assert_eq!(kill(getpid(), signum), 0);
        let _ = saved;
        panic!("inactive fixed stub returned from its signal adapter");
    }

    let output = run_signal_test_child(
        "vm::signal::tests::close::raw_restore_of_a_closed_fixed_stub_exits_seventy",
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
            exit_now(124)
        });

        let signum = SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let owner = super::super::super::ctx::Engine::new(Shared::new(Module::default()));
        install_signal(owner.control(), signum, 0x2_4100, Some((14, 0x2_4100))).unwrap();
        let closed_stub = kernel_current(signum).unwrap();
        owner.wait_closed().unwrap();
        closed_stub.replace(signum).unwrap();

        let raiser = super::super::super::ctx::Engine::new(Shared::new(Module::default()));
        let activation = super::super::super::ctx::activate(raiser.shared());
        let exception = super::super::super::unwind::catch_raw(|| {
            super::super::super::ctx::raise_signal(activation.ctx(), signum)
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
        let mut edited = closed_stub;
        edited.add_to_mask(SIGUSR2);
        assert_eq!(edited.install(signum), 0);
        let exception = super::super::super::unwind::catch_raw(|| {
            super::super::super::ctx::raise_signal(activation.ctx(), signum)
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
        "vm::signal::tests::close::wrapped_raise_of_a_raw_restored_closed_stub_faults_without_retrying_forever",
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
fn close_restores_the_predecessor_of_a_detached_current_stub() {
    let signum = SIGURG;
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

    assert_eq!(kill(getpid(), signum), 0);
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
    assert_eq!(kill(getpid(), signum), 0);
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
