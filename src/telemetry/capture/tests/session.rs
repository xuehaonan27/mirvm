//! The session lifecycle: publishing without replacing, releasing the root when a thread leaves
//! through a non-returning syscall, and the exact file a drained session publishes.

use super::*;

#[test]
fn final_capture_file_is_never_replaced() {
    const CHILD_ENV: &str = "MIRVM_CAPTURE_NOREPLACE_TEST_CHILD";
    if let Some(output) = std::env::var_os(CHILD_ENV) {
        let output = PathBuf::from(output);
        let partial = partial_path(&output);
        let mut session = CaptureSession::start(StartOptions::new(&output, STARTER_BYTES))
            .expect("capture session did not start");
        std::fs::write(&output, b"existing final").unwrap();

        let error = session
            .finish(Duration::from_secs(2))
            .expect_err("capture replaced an existing final file");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&output).unwrap(), b"existing final");
        assert!(partial.is_file());

        std::fs::remove_file(&partial).unwrap();
        let error = CaptureSession::start(StartOptions::new(&output, STARTER_BYTES))
            .err()
            .expect("capture started with an existing final file");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&output).unwrap(), b"existing final");
        assert!(!partial.exists());
        std::fs::remove_file(output).unwrap();
        return;
    }

    let output = test_path();
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("telemetry::capture::tests::session::final_capture_file_is_never_replaced")
        .arg("--test-threads=1")
        .env(CHILD_ENV, output)
        .status()
        .unwrap();
    assert!(status.success());
}

extern "C" fn exit_through_captured_syscall(_: *mut c_void) -> *mut c_void {
    let token = activation_enter(0x51);
    let _ = host_syscall(SYS_EXIT, &[0]);
    activation_exit(token, 0);
    std::process::abort();
}

#[test]
fn nonreturning_thread_syscall_releases_the_capture_root() {
    const CHILD_ENV: &str = "MIRVM_CAPTURE_SYS_EXIT_CHILD";
    if let Some(output) = std::env::var_os(CHILD_ENV) {
        let output = PathBuf::from(output);
        let mut session = CaptureSession::start(StartOptions::new(&output, STARTER_BYTES))
            .expect("capture session did not start");
        let mut thread = std::mem::MaybeUninit::uninit();
        assert_eq!(
            unsafe {
                crate::os::thread::spawn_raw(
                    thread.as_mut_ptr(),
                    ptr::null(),
                    exit_through_captured_syscall,
                    ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(
            crate::os::thread::join_raw(unsafe { thread.assume_init() }),
            0
        );
        let FinishStatus::Finished(summary) = session.finish(Duration::from_secs(2)).unwrap()
        else {
            panic!("capture writer stayed blocked on a thread that exited in SYS_exit");
        };
        assert_eq!(summary.encoded_records, 1);
        let outcome = decode_file(&output, &mut |_| Ok(())).unwrap();
        assert_eq!(outcome.health, Health::Clean, "{:?}", outcome.issues);
        assert_eq!(outcome.report.records, 1);
        assert_eq!(outcome.report.incomplete_enters, 1);
        return;
    }

    let output = test_path();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("telemetry::capture::tests::session::nonreturning_thread_syscall_releases_the_capture_root")
        .arg("--test-threads=1")
        .env(CHILD_ENV, &output)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("captured SYS_exit subprocess did not finish within five seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    std::fs::remove_file(output).unwrap();
}

#[test]
fn session_publishes_drains_and_decodes_exact_file() {
    const CHILD_ENV: &str = "MIRVM_CAPTURE_SESSION_TEST_CHILD";
    if let Some(output) = std::env::var_os(CHILD_ENV) {
        run_session_child(PathBuf::from(output));
        return;
    }

    let output = test_path();
    let partial = partial_path(&output);
    let test_name =
        "telemetry::capture::tests::session::session_publishes_drains_and_decodes_exact_file";
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(test_name)
        .arg("--test-threads=1")
        .env(CHILD_ENV, &output)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!partial.exists());

    let mut events = Vec::new();
    let outcome = decode_file(&output, &mut |event| {
        events.push(event.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(outcome.health, Health::Clean, "{:?}", outcome.issues);
    assert!(outcome.issues.is_empty());
    assert_eq!(outcome.report.records, 6);
    assert_eq!(events.len(), 6);
    assert!(matches!(events[0].kind, DecodedKind::SyscallEnter(_)));
    assert!(matches!(events[1].kind, DecodedKind::SyscallExit { .. }));
    assert!(matches!(events[2].kind, DecodedKind::SyscallEnter(_)));
    assert!(matches!(
        events[3].kind,
        DecodedKind::SyscallExit {
            record: SyscallExit {
                errno: Some(errno),
                ..
            },
            ..
        } if errno == ENOSYS as u32
    ));
    assert!(matches!(events[4].kind, DecodedKind::SyscallEnter(_)));
    assert!(matches!(events[5].kind, DecodedKind::SyscallExit { .. }));
    let end = outcome.report.session_end.unwrap();
    assert_eq!(end.attempted, 6);
    assert_eq!(end.encoded, 6);
    assert_eq!(end.committed, 6);
    assert_eq!(end.sink_loss, 0);
    std::fs::remove_file(output).unwrap();
}

fn run_session_child(output: PathBuf) {
    let mut session = CaptureSession::start(StartOptions::new(&output, STARTER_BYTES)).unwrap();
    let duplicate = test_path();
    let error = CaptureSession::start(StartOptions::new(&duplicate, STARTER_BYTES))
        .err()
        .expect("a second process session was accepted");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(!partial_path(&duplicate).exists());

    crate::os::process::set_errno(EDOM);
    let token = activation_enter(17);
    assert_eq!(crate::os::process::errno(), EDOM);
    let result = host_syscall(SYS_GETPID, &[]);
    assert_eq!(result, crate::os::process::getpid() as i64);
    assert_eq!(crate::os::process::errno(), EDOM);
    assert_eq!(host_syscall(-1, &[]), -1);
    assert_eq!(crate::os::process::errno(), ENOSYS);

    session.request_stop();
    assert!(!is_armed());
    // The old root keeps its producer until it returns even though new
    // roots are no longer admitted.
    // The copied guard must observe the invalid generation and leave the
    // copied parent page/active-root ledger untouched.
    fork_and_wait(|| activation_exit(token, 0));
    activation_exit(token, 0);

    let stopped_token = activation_enter(17);
    assert!(stopped_token.session.is_null());
    assert_eq!(
        host_syscall(SYS_GETPID, &[]),
        crate::os::process::getpid() as i64
    );
    activation_exit(stopped_token, 0);

    let status = session.finish(Duration::from_secs(2)).unwrap();
    let FinishStatus::Finished(summary) = status else {
        panic!("capture writer did not stop");
    };
    assert_eq!(summary.encoded_records, 6);
    assert_eq!(summary.producer_drops, 0);

    // A hard budget is a loss bound, never a guest correctness switch.
    let mut no_pages = CaptureSession::start(StartOptions::new(&duplicate, 0)).unwrap();
    crate::os::process::set_errno(EDOM);
    let token = activation_enter(29);
    assert_eq!(crate::os::process::errno(), EDOM);
    assert_eq!(
        host_syscall(SYS_GETPID, &[]),
        crate::os::process::getpid() as i64
    );
    assert_eq!(crate::os::process::errno(), EDOM);
    activation_exit(token, 0);
    let FinishStatus::Finished(summary) = no_pages.finish(Duration::from_secs(2)).unwrap() else {
        panic!("page-less capture writer did not stop");
    };
    assert_eq!(summary.producers, 1);
    assert_eq!(summary.encoded_records, 0);
    assert_eq!(summary.producer_drops, 2);
    let outcome = decode_file(&duplicate, &mut |_| Ok(())).unwrap();
    assert_eq!(outcome.health, Health::Clean, "{:?}", outcome.issues);
    let end = outcome.report.session_end.unwrap();
    assert_eq!(end.producer_count, 1);
    assert_eq!(end.attempted, 2);
    assert_eq!(end.encoded, 0);
    assert_eq!(end.drop_capacity, 2);
    std::fs::remove_file(duplicate).unwrap();
}
