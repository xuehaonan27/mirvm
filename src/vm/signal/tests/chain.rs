//! The disposition chain: predecessor identity, detached successors and restoration.

use super::*;

const EXACT_COMPENSATION_CHILD: &str = "MIRVM_SIGNAL_EXACT_COMPENSATION_CHILD";

const DETACHED_SNAPSHOT_CHILD: &str = "MIRVM_SIGNAL_DETACHED_SNAPSHOT_CHILD";

#[test]
fn compensation_preserves_a_newer_normalized_writer_by_exact_identity() {
    if std::env::var_os(EXACT_COMPENSATION_CHILD).is_some() {
        let signum = SIGWINCH;
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
        "vm::signal::tests::chain::compensation_preserves_a_newer_normalized_writer_by_exact_identity",
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
        let signum = SIGWINCH;
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
            Some(super::super::super::thunks::SignalHandlerResolution::Unknown),
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
        "vm::signal::tests::chain::detached_successor_accepts_distinct_valid_snapshots_of_its_predecessor",
        DETACHED_SNAPSHOT_CHILD,
    );
    assert!(
        output.status.success(),
        "isolated detached-snapshot regression failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stale_chain_keeps_the_matching_lower_stub_and_its_accepted_event() {
    let first_control = control();
    let second_control = control();
    let signum = SIGUSR1;
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
    let signum = SIGUSR1;
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
    let signum = SIGUSR1;
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
    let signum = SIGUSR1;
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
    assert_eq!(signal_style.flags(), SA_RESTART);
}
