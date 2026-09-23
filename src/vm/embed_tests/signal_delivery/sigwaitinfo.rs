//! sigwaitinfo: consuming a blocked host signal without leaving thread state behind.

use super::*;

static SIGNAL_EXTERNAL_WAIT_ENTERED: AtomicU64 = AtomicU64::new(0);

static SIGNAL_EXTERNAL_WAIT_RELEASED: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn external_siginfo_wait_for_replacement(
    _signum: i32,
    _info: crate::os::signal::SignalInfo,
    _context: *mut std::ffi::c_void,
) {
    SIGNAL_EXTERNAL_WAIT_ENTERED.store(1, Ordering::SeqCst);
    while SIGNAL_EXTERNAL_WAIT_RELEASED.load(Ordering::SeqCst) == 0 {
        std::hint::spin_loop();
    }
}

#[test]
fn sigwaitinfo_consumes_blocked_host_raise_without_leaving_thread_signal_state() {
    const CHILD: &str = "MIRVM_SIGWAIT_HOST_RAISE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_mode, jit) in modes() {
            let (_restore, baseline) =
                SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
            SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
            let engine = engine(physically_masked_signal_module(), jit);
            super::super::signal::install_signal(
                engine.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_OWNER_GUEST_ADDR as usize,
                Some((0, SIGNAL_OWNER_GUEST_ADDR)),
            )
            .unwrap();

            let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = blocker
                .block_for_handler(crate::os::signal::SIGUSR1)
                .unwrap();
            let raised = unsafe { run_export(&engine, "raise", &[]) };

            let waited_set =
                crate::os::signal::SignalMask::empty().with(crate::os::signal::SIGUSR1);
            let raised_pending = crate::os::signal::wait_pending(&waited_set);
            assert_eq!(
                raised_pending.as_ref().map(|pending| pending.signum()),
                Ok(crate::os::signal::SIGUSR1)
            );
            let raised_code = raised_pending.unwrap().code();

            assert_eq!(
                crate::os::signal::send_to_current_thread(crate::os::signal::SIGUSR1),
                0
            );
            let killed_pending = crate::os::signal::wait_pending(&waited_set);
            assert_eq!(
                killed_pending.as_ref().map(|pending| pending.signum()),
                Ok(crate::os::signal::SIGUSR1)
            );
            let killed_code = killed_pending.unwrap().code();
            let safe = unsafe { run_export(&engine, "probe", &[]) };
            let handler_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
            engine.wait_closed().unwrap();
            let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
            drop(mask_guard);

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(raised_code, crate::os::signal::SI_TKILL);
            assert_eq!(killed_code, crate::os::signal::SI_TKILL);
            assert!(matches!(safe, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(
                handler_ran, 0,
                "sigwaitinfo-consumed signals reached the guest callback"
            );
            assert!(restored.same_disposition(&baseline));
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::sigwaitinfo::sigwaitinfo_consumes_blocked_host_raise_without_leaving_thread_signal_state";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start sigwaitinfo HostRaise subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("sigwaitinfo did not consume the blocked HostRaise");
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
        "blocked HostRaise did not preserve sigwaitinfo semantics:\n{stdout}{stderr}"
    );
}

#[test]
fn external_siginfo_handler_can_wait_for_another_threads_host_signal() {
    const CHILD: &str = "MIRVM_EXTERNAL_HANDLER_HOST_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
            .expect("failed to save waiting SA_SIGINFO disposition");
        let _restore = SavedSignalDisposition {
            signum: crate::os::signal::SIGUSR1,
            action: saved,
        };
        let waiting = {
            let mut action = raw_sigaction(
                external_siginfo_wait_for_replacement as *const () as usize,
                &[],
            );
            action.or_flags(crate::os::signal::SA_SIGINFO);
            action
        };
        assert_eq!(waiting.install(crate::os::signal::SIGUSR1), 0);
        let waiting_installed =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        for (_mode, jit) in modes() {
            SIGNAL_EXTERNAL_WAIT_ENTERED.store(0, Ordering::SeqCst);
            SIGNAL_EXTERNAL_WAIT_RELEASED.store(0, Ordering::SeqCst);
            let raiser = engine(physically_masked_signal_module(), jit);
            let installer = engine(first_external_native_signal_module(), jit);
            let install_execution = installer.clone();
            let replacement = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while SIGNAL_EXTERNAL_WAIT_ENTERED.load(Ordering::SeqCst) == 0 {
                    if std::time::Instant::now() >= deadline {
                        SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                        panic!("external SA_SIGINFO handler was not entered");
                    }
                    std::thread::yield_now();
                }
                let installed = unsafe { run_export(&install_execution, "install", &[]) };
                SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                installed
            });

            let raised = unsafe { run_export(&raiser, "raise", &[]) };
            let installed = replacement
                .join()
                .expect("HostSignal replacement thread panicked");
            installer.wait_closed().unwrap();
            raiser.wait_closed().unwrap();
            let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert!(
                matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == external_siginfo_wait_for_replacement as *const () as usize as u64),
                "HostSignal returned the wrong old handler: {installed:?}"
            );
            assert!(restored.same_disposition(&waiting_installed));
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::sigwaitinfo::external_siginfo_handler_can_wait_for_another_threads_host_signal";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start external-handler HostSignal subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("HostSignal deadlocked behind the external handler it had to release");
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
        "external handler and HostSignal did not make progress independently:\n{stdout}{stderr}"
    );
}

#[test]
fn external_siginfo_handler_can_wait_for_another_threads_engine_close() {
    const CHILD: &str = "MIRVM_EXTERNAL_HANDLER_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
            .expect("failed to save waiting SA_SIGINFO disposition");
        let _restore = SavedSignalDisposition {
            signum: crate::os::signal::SIGUSR1,
            action: saved,
        };
        let waiting = {
            let mut action = raw_sigaction(
                external_siginfo_wait_for_replacement as *const () as usize,
                &[],
            );
            action.or_flags(crate::os::signal::SA_SIGINFO);
            action
        };

        for (_mode, jit) in modes() {
            assert_eq!(waiting.install(crate::os::signal::SIGUSR1), 0);
            let waiting_installed =
                crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
            let owner = engine(first_external_native_signal_module(), jit);
            let installed = unsafe { run_export(&owner, "install", &[]) };
            assert!(
                matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == external_siginfo_wait_for_replacement as *const () as usize as u64),
                "owner did not replace the waiting handler: {installed:?}"
            );
            assert_eq!(waiting.install(crate::os::signal::SIGUSR1), 0);

            SIGNAL_EXTERNAL_WAIT_ENTERED.store(0, Ordering::SeqCst);
            SIGNAL_EXTERNAL_WAIT_RELEASED.store(0, Ordering::SeqCst);
            let raiser = engine(physically_masked_signal_module(), jit);
            let closing_owner = owner.clone();
            let close = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while SIGNAL_EXTERNAL_WAIT_ENTERED.load(Ordering::SeqCst) == 0 {
                    if std::time::Instant::now() >= deadline {
                        SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                        panic!("external SA_SIGINFO handler was not entered");
                    }
                    std::thread::yield_now();
                }
                let closed = closing_owner.wait_closed();
                SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                closed
            });

            let raised = unsafe { run_export(&raiser, "raise", &[]) };
            let closed = close.join().expect("Engine close thread panicked");
            raiser.wait_closed().unwrap();
            let current = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert!(closed.is_ok(), "owner Engine close failed: {closed:?}");
            assert!(
                current.same_disposition(&waiting_installed),
                "owner close overwrote the raw external handler"
            );
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::sigwaitinfo::external_siginfo_handler_can_wait_for_another_threads_engine_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start external-handler close subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Engine close deadlocked behind the external handler waiting for it");
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
        "external handler and Engine close did not make progress independently:\n{stdout}{stderr}"
    );
}
