//! `wait_closed` while a signal is pending on the calling pthread.

use super::*;

#[test]
fn wait_closed_fails_fast_for_a_signal_pending_on_the_current_pthread() {
    const CHILD: &str = "MIRVM_WAIT_CLOSED_TARGET_SIGNAL_CHILD";
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
            let engine = engine(target_thread_signal_module(), jit);
            super::super::signal::install_signal(
                engine.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_TARGET_GUEST_ADDR as usize,
                Some((0, SIGNAL_TARGET_GUEST_ADDR)),
            )
            .unwrap();

            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (wait_tx, wait_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let target_engine = engine.clone();
            let target = std::thread::spawn(move || {
                let attached = unsafe { run_export(&target_engine, "probe", &[]) };
                ready_tx
                    .send((crate::os::thread::current_thread().as_u64(), attached))
                    .unwrap();
                wait_rx.recv().unwrap();
                result_tx.send(target_engine.wait_closed()).unwrap();
            });
            let (target_pthread, attached) = ready_rx.recv().unwrap();
            assert!(matches!(attached, Ok(RunOutcome::Returned(value)) if value.lo == 0));

            let active_engine = engine.clone();
            let active =
                std::thread::spawn(move || unsafe { run_export(&active_engine, "block", &[]) });
            wait_lifecycle_entry();

            let (checked_tx, checked_rx) = std::sync::mpsc::channel();
            let (published_tx, published_rx) = std::sync::mpsc::channel();
            super::super::ctx::set_wait_closed_check_hook(Box::new(move || {
                checked_tx.send(()).unwrap();
                published_rx.recv().unwrap();
            }));
            wait_tx.send(()).unwrap();
            checked_rx.recv().unwrap();
            assert_eq!(
                crate::os::signal::send_to_thread(
                    crate::os::thread::ThreadId::from_raw(target_pthread),
                    crate::os::signal::SIGUSR1,
                ),
                0
            );
            wait_for_owner_signal_pending(&engine);
            published_tx.send(()).unwrap();

            let wait_result = result_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("wait_closed blocked on its current pthread's target signal");
            assert_eq!(
                wait_result,
                Err(super::super::ctx::WaitClosedError::ActiveOnCurrentThread)
            );
            target.join().expect("target pthread panicked");
            assert_eq!(SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst), 1);
            assert_eq!(
                SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst),
                target_pthread
            );

            release_lifecycle_entry();
            let active = active.join().expect("active pthread panicked");
            assert!(matches!(active, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            engine.wait_closed().unwrap();
            assert!(
                crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline)
            );
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::wait_closed::wait_closed_fails_fast_for_a_signal_pending_on_the_current_pthread";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start current-pthread signal wait_closed subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("wait_closed blocked on a target signal owned by its current pthread");
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
        "current-pthread signal wait_closed regression failed:\n{stdout}{stderr}"
    );
}
