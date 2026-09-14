//! TSan adapter for the product telemetry modules needed by `src/vm`.

#[path = "../../src/telemetry/capture.rs"]
pub(crate) mod capture;
#[cfg(test)]
#[path = "../../src/telemetry/decode.rs"]
pub(crate) mod decode;
#[path = "../../src/telemetry/format.rs"]
pub(crate) mod format;

pub(crate) fn run_capture_lifecycle_case() -> bool {
    use std::sync::mpsc;
    use std::time::{Duration, SystemTime};

    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let output = std::env::temp_dir().join(format!(
        "mirvm-tsan-capture-{}-{nonce}.mlog",
        std::process::id()
    ));
    let mut session = capture::CaptureSession::start(capture::StartOptions::new(&output, 8 << 10))
        .expect("TSan capture session did not start");

    let (owner_ready_tx, owner_ready_rx) = mpsc::channel();
    let (owner_release_tx, owner_release_rx) = mpsc::channel();
    let owner = std::thread::spawn(move || {
        let token = capture::activation_enter(0x81);
        let ok = capture::host_syscall(libc::SYS_getpid, &[]) == unsafe { libc::getpid() } as i64;
        owner_ready_tx.send(()).unwrap();
        owner_release_rx.recv().unwrap();
        capture::activation_exit(token, 0);
        capture::retire_current_thread();
        ok
    });
    owner_ready_rx.recv().unwrap();

    let (waiting_ready_tx, waiting_ready_rx) = mpsc::channel();
    let (waiting_release_tx, waiting_release_rx) = mpsc::channel();
    let waiting = std::thread::spawn(move || {
        let token = capture::activation_enter(0x82);
        let pid = unsafe { libc::getpid() } as i64;
        let mut ok = capture::host_syscall(libc::SYS_getpid, &[]) == pid;
        waiting_ready_tx.send(()).unwrap();
        waiting_release_rx.recv().unwrap();
        for _ in 0..1_024 {
            ok &= capture::host_syscall(libc::SYS_getpid, &[]) == pid;
            std::thread::yield_now();
        }
        capture::activation_exit(token, 0);
        capture::retire_current_thread();
        ok
    });
    waiting_ready_rx.recv().unwrap();

    owner_release_tx.send(()).unwrap();
    let owner_ok = owner.join().unwrap();
    waiting_release_tx.send(()).unwrap();
    let waiting_ok = waiting.join().unwrap();
    let summary = match session.finish(Duration::from_secs(5)).unwrap() {
        capture::FinishStatus::Finished(summary) => summary,
        capture::FinishStatus::TimedOut => panic!("TSan capture writer did not stop"),
    };
    std::fs::remove_file(output).unwrap();

    let ok = owner_ok
        && waiting_ok
        && summary.producers == 2
        && summary.producer_drops >= 2
        && summary.encoded_records > 2;
    if ok {
        println!("PASS caseE capture page publish/return/retire/rescue lifecycle");
    } else {
        eprintln!("FAIL caseE capture lifecycle summary: {summary:?}");
    }
    ok
}
