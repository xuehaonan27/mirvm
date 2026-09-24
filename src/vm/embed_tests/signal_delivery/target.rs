//! Target-thread delivery: which pthread runs a directed signal and when.

use super::*;

#[test]
fn blocked_host_raise_runs_once_at_the_target_pthreads_next_safe_point() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
        let engine = engine(target_thread_signal_module(), jit);
        super::super::signal::install_signal(
            engine.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_TARGET_GUEST_ADDR as usize,
            Some((0, SIGNAL_TARGET_GUEST_ADDR)),
        )
        .unwrap();

        let execution = engine.clone();
        let observed = std::thread::spawn(move || {
            let target_thread = crate::os::thread::current_thread().as_u64();
            let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = blocker
                .block_for_handler(crate::os::signal::SIGUSR1)
                .unwrap();
            let raised = unsafe { run_export(&execution, "raise", &[]) };
            let while_masked = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            drop(mask_guard);
            let after_unblock = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let first_safe = unsafe { run_export(&execution, "probe", &[]) };
            let after_first_safe = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let second_safe = unsafe { run_export(&execution, "probe", &[]) };
            let after_second_safe = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            (
                target_thread,
                raised,
                while_masked,
                after_unblock,
                first_safe,
                after_first_safe,
                second_safe,
                after_second_safe,
            )
        })
        .join()
        .expect("target pthread panicked");
        let handler_thread = SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if !matches!(observed.1, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.2 != 0
            || observed.3 != 0
            || !matches!(observed.4, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.5 != 1
            || !matches!(observed.6, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.7 != 1
            || handler_thread != observed.0
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: raise={:?}, masked={}, unblocked={}, first={:?}/{}, second={:?}/{}, target={:#x}, handler={handler_thread:#x}, restored={}",
                observed.1,
                observed.2,
                observed.3,
                observed.4,
                observed.5,
                observed.6,
                observed.7,
                observed.0,
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "blocked HostRaise did not stay with its target pthread: {failures:#?}"
    );
}

#[test]
fn raw_pthread_kill_runs_only_on_the_target_pthread_during_concurrent_close() {
    const CHILD: &str = "MIRVM_RAW_PTHREAD_KILL_TARGET_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        for (_mode, jit) in modes() {
            reset_lifecycle_gate();
            SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_WORKER_THREAD.store(0, Ordering::SeqCst);
            let engine = engine(target_thread_signal_module(), jit);
            super::super::signal::install_signal(
                engine.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_TARGET_GUEST_ADDR as usize,
                Some((0, SIGNAL_TARGET_GUEST_ADDR)),
            )
            .unwrap();

            let execution = engine.clone();
            let running =
                std::thread::spawn(move || unsafe { run_export(&execution, "block", &[]) });
            wait_lifecycle_entry();
            let target_thread = SIGNAL_TARGET_WORKER_THREAD.load(Ordering::SeqCst);
            let sender_thread = crate::os::thread::current_thread().as_u64();
            assert_ne!(target_thread, 0);
            assert_ne!(target_thread, sender_thread);
            assert_eq!(
                crate::os::signal::send_to_thread(
                    crate::os::thread::ThreadId::from_raw(target_thread),
                    crate::os::signal::SIGUSR1,
                ),
                0
            );
            wait_for_owner_signal_pending(&engine);

            let closer = engine.clone();
            let closed = std::thread::spawn(move || closer.wait_closed());
            release_lifecycle_entry();
            let completed = running.join().expect("target pthread panicked");
            let closed = closed.join().expect("close worker panicked");
            let handler_ran = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let handler_thread = SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst);
            let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

            assert!(
                matches!(completed, Ok(RunOutcome::Returned(value)) if value.lo == 0),
                "target VM call did not return normally: {completed:?}"
            );
            assert!(closed.is_ok(), "concurrent close failed: {closed:?}");
            assert_eq!(handler_ran, 1);
            assert_eq!(handler_thread, target_thread);
            assert_ne!(handler_thread, sender_thread);
            assert!(restored.same_disposition(&baseline));
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::target::raw_pthread_kill_runs_only_on_the_target_pthread_during_concurrent_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start raw pthread_kill subprocess");
    let deadline = std::time::Instant::now() + CHILD_HANG_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("raw pthread_kill was not drained by its target pthread");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "raw pthread_kill left its target pthread or stalled close:\n{stdout}{stderr}"
    );
}
