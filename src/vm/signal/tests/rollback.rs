//! Installation: what commits, what rolls back and what a raw overwrite sees.

use super::*;

static RAISE_EXTERNAL_RAN: AtomicUsize = AtomicUsize::new(0);

static REENTRANT_RAISE_RAN: AtomicUsize = AtomicUsize::new(0);

static REENTRANT_INSTALL_RESULT: AtomicI32 = AtomicI32::new(0);

static REENTRANT_INSTALL_CONTROL: LazyLock<Mutex<Option<Arc<EngineControl>>>> =
    LazyLock::new(|| Mutex::new(None));

unsafe extern "C" fn raise_external_handler(_signum: i32) {
    RAISE_EXTERNAL_RAN.fetch_add(1, Ordering::SeqCst);
    let control = REENTRANT_INSTALL_CONTROL.lock().unwrap().clone();
    if let Some(control) = control {
        let installed = install_signal(&control, SIGWINCH, 0x2_6000, Some((15, 0x2_6000)));
        REENTRANT_INSTALL_RESULT.store(i32::from(installed.is_ok()), Ordering::SeqCst);
    }

    if crate::os::process::raise(SIGURG) != 0 {
        exit_now(72)
    }
}

unsafe extern "C" fn reentrant_raise_handler(_signum: i32) {
    REENTRANT_RAISE_RAN.fetch_add(1, Ordering::SeqCst);
}

const INSTALL_OVERWRITE_CHILD: &str = "MIRVM_SIGNAL_INSTALL_OVERWRITE_CHILD";

const RAISE_INSTALL_RACE_CHILD: &str = "MIRVM_SIGNAL_RAISE_INSTALL_RACE_CHILD";

#[cfg(target_os = "linux")]
const NORMALIZED_ACTION_CHILD: &str = "MIRVM_SIGNAL_NORMALIZED_ACTION_CHILD";

#[cfg(target_os = "linux")]
const REQUEST_ONLY_ROLLBACK_CHILD: &str = "MIRVM_SIGNAL_REQUEST_ONLY_ROLLBACK_CHILD";

#[cfg(target_os = "linux")]
const REQUEST_ONLY_RECONCILE_CHILD: &str = "MIRVM_SIGNAL_REQUEST_ONLY_RECONCILE_CHILD";

// The flags this test drives are the Linux kernel's: it is the one that clears a probing bit
// on the way back out, and the only one whose action carries a restorer to change.
#[cfg(target_os = "linux")]
#[test]
fn rejected_request_only_candidate_rolls_back_its_normalized_kernel_action() {
    if std::env::var_os(REQUEST_ONLY_ROLLBACK_CHILD).is_some() {
        let signum = SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let requested = Sigaction::empty(
            detached_close_external_handler as *const () as usize,
            SA_RESTART | SA_UNSUPPORTED,
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
        "vm::signal::tests::rollback::rejected_request_only_candidate_rolls_back_its_normalized_kernel_action",
        REQUEST_ONLY_ROLLBACK_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated request-only rollback regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// The flags this test drives are the Linux kernel's: it is the one that clears a probing bit
// on the way back out, and the only one whose action carries a restorer to change.
#[cfg(target_os = "linux")]
#[test]
fn reconcile_solidifies_a_request_only_node_with_the_exact_old_action() {
    if std::env::var_os(REQUEST_ONLY_RECONCILE_CHILD).is_some() {
        let signum = SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let control = control();
        let visible = Sigaction::empty(0x2_7200, SA_RESTART | SA_UNSUPPORTED);
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
        "vm::signal::tests::rollback::reconcile_solidifies_a_request_only_node_with_the_exact_old_action",
        REQUEST_ONLY_RECONCILE_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated request-only reconciliation regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn external_raise_handler_can_reenter_signal_install_and_raise() {
    if std::env::var_os(RAISE_INSTALL_RACE_CHILD).is_some() {
        let signum = SIGWINCH;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let reentrant_signum = SIGURG;
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
        "vm::signal::tests::rollback::external_raise_handler_can_reenter_signal_install_and_raise",
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
        let signum = SIGWINCH;
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
            Some(
                super::super::super::thunks::SignalHandlerResolution::Valid {
                    control: Arc::clone(&control),
                    func: 9,
                },
            ),
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
        "vm::signal::tests::rollback::successful_install_commits_before_an_immediate_raw_overwrite",
        INSTALL_OVERWRITE_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated install-overwrite regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// The flags this test drives are the Linux kernel's: it is the one that clears a probing bit
// on the way back out, and the only one whose action carries a restorer to change.
#[cfg(target_os = "linux")]
#[test]
fn installed_stub_tracks_the_exact_kernel_normalized_action() {
    if std::env::var_os(NORMALIZED_ACTION_CHILD).is_some() {
        const UNKNOWN_PROBE_FLAG: i32 = 0x0000_0200;

        let signum = SIGWINCH;
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
            SA_RESTART | SA_UNSUPPORTED | SA_EXPOSE_TAGBITS | UNKNOWN_PROBE_FLAG,
        );
        let old = install_sigaction_value(
            &control,
            signum,
            Some(requested),
            Some(
                super::super::super::thunks::SignalHandlerResolution::Valid {
                    control: Arc::clone(&control),
                    func: 16,
                },
            ),
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
        let mut edited = fixed_oldact;
        edited.or_flags(SA_NOCLDSTOP);
        edited.add_to_mask(SIGUSR2);
        let replaced = install_sigaction_value(
            &control,
            signum,
            Some(edited),
            Some(super::super::super::thunks::SignalHandlerResolution::Unknown),
        )
        .unwrap();
        assert!(replaced.same_disposition(&visible));
        let edited_visible = install_sigaction_value(&control, signum, None, None).unwrap();
        assert_eq!(edited_visible.handler(), requested.handler());
        assert_ne!(edited_visible.flags() & SA_NOCLDSTOP, 0);
        assert_eq!(edited_visible.flags() & SA_SIGINFO, 0);
        assert_eq!(
            edited_visible.flags() & (SA_UNSUPPORTED | UNKNOWN_PROBE_FLAG),
            0
        );
        assert!(edited_visible.restorer().is_none());
        assert!(edited_visible.mask_contains(SIGUSR2));

        deactivate_engine(&control).unwrap();
        assert!(kernel_current(signum).unwrap().same_disposition(&saved));
        return;
    }

    let output = run_signal_test_child(
        "vm::signal::tests::rollback::installed_stub_tracks_the_exact_kernel_normalized_action",
        NORMALIZED_ACTION_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated normalized-action regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
