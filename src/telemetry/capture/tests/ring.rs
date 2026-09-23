//! The page ring's accounting: nesting engine contexts, dropping once when the ring is full,
//! recovering after the writer returns retired pages, and keeping the writer's scan short.

use super::*;

#[test]
fn nested_engine_context_is_restored_in_order() {
    let core = Box::leak(Box::new(SessionCore {
        phase: AtomicU8::new(PHASE_ARMED),
        active_roots: AtomicUsize::new(0),
        writer_state: AtomicU32::new(WRITER_AWAKE),
        writer_done: AtomicBool::new(false),
        producers: Mutex::new(Vec::new()),
        active_producers: Mutex::new(Vec::new()),
        next_producer: AtomicU64::new(2),
        next_thread_generation: AtomicU32::new(2),
        page_pool: PagePool::new(STARTER_BYTES),
        sink_loss: AtomicU64::new(0),
        owner_tid: crate::os::process::gettid() as u32,
    }));
    let pages = Box::into_raw(Box::new(PagePair::new()));
    let producer = Box::leak(Box::new(Producer::new(
        core,
        pages,
        1,
        1,
        crate::os::process::gettid() as u32,
        11,
        crate::os::process::errno_location(),
    )));

    unsafe {
        assert!(open_page(producer));
        set_engine(producer, 22);
        set_engine(producer, 11);
        seal_page(producer);
    }
    let page = unsafe { producer.page(0) };
    let bytes = sealed_page_bytes(page);
    let header = PageHeader::decode(&bytes[..PAGE_HEADER_BYTES]).unwrap();
    assert_eq!(header.initial_engine_id, 11);
    assert_eq!(header.first_sequence, 0);
    assert_eq!(header.next_sequence, 2);
    let first =
        EngineContext::decode(&bytes[PAGE_HEADER_BYTES..PAGE_HEADER_BYTES + ENGINE_CONTEXT_BYTES])
            .unwrap();
    let second = EngineContext::decode(
        &bytes[PAGE_HEADER_BYTES + ENGINE_CONTEXT_BYTES
            ..PAGE_HEADER_BYTES + 2 * ENGINE_CONTEXT_BYTES],
    )
    .unwrap();
    assert_eq!(first.engine_id, 22);
    assert_eq!(second.engine_id, 11);
}

#[test]
fn two_page_ring_drops_once_and_recovers_after_return() {
    let core = Box::leak(Box::new(SessionCore {
        phase: AtomicU8::new(PHASE_ARMED),
        active_roots: AtomicUsize::new(0),
        writer_state: AtomicU32::new(WRITER_AWAKE),
        writer_done: AtomicBool::new(false),
        producers: Mutex::new(Vec::new()),
        active_producers: Mutex::new(Vec::new()),
        next_producer: AtomicU64::new(2),
        next_thread_generation: AtomicU32::new(2),
        page_pool: PagePool::new(STARTER_BYTES),
        sink_loss: AtomicU64::new(0),
        owner_tid: crate::os::process::gettid() as u32,
    }));
    let pages = Box::into_raw(Box::new(PagePair::new()));
    let producer = Box::leak(Box::new(Producer::new(
        core,
        pages,
        1,
        1,
        crate::os::process::gettid() as u32,
        7,
        crate::os::process::errno_location(),
    )));
    crate::os::process::set_errno(EDOM);

    for _ in 0..90 {
        let disposition = unsafe { record_syscall_enter(producer, SYS_GETPID, &[]) };
        assert!(matches!(disposition, EnterDisposition::Recorded));
        unsafe { record_syscall_exit(producer, disposition, 1, 0) };
    }
    let dropped = unsafe { record_syscall_enter(producer, SYS_GETPID, &[]) };
    assert!(matches!(dropped, EnterDisposition::DropCapacity));
    unsafe { record_syscall_exit(producer, dropped, 1, 0) };
    assert_eq!(producer.published.tail.load(Ordering::Acquire), 2);
    let first = PageHeader::decode(&unsafe { &*producer.page(0).bytes.get() }[..PAGE_HEADER_BYTES])
        .unwrap();
    let second =
        PageHeader::decode(&unsafe { &*producer.page(1).bytes.get() }[..PAGE_HEADER_BYTES])
            .unwrap();
    assert_eq!((first.first_sequence, first.next_sequence), (0, 90));
    assert_eq!((second.first_sequence, second.next_sequence), (90, 180));
    let cold = unsafe { &*producer.cold.get() };
    assert_eq!(cold.next_sequence, 182);
    assert_eq!(cold.capacity_drops, 2);

    producer.returned.head.store(1, Ordering::Release);
    let recovered = unsafe { record_syscall_enter(producer, SYS_GETPID, &[]) };
    assert!(matches!(recovered, EnterDisposition::Recorded));
    unsafe { record_syscall_exit(producer, recovered, 1, 0) };
    assert_eq!(crate::os::process::errno(), EDOM);
}

#[test]
fn page_less_producer_recovers_after_retired_pages_return_to_pool() {
    const CHILD_ENV: &str = "MIRVM_CAPTURE_POOL_RESCUE_CHILD";
    if let Some(output) = std::env::var_os(CHILD_ENV) {
        let output = PathBuf::from(output);
        let mut session = CaptureSession::start(StartOptions::new(&output, STARTER_BYTES)).unwrap();
        let (owner_ready_tx, owner_ready_rx) = std::sync::mpsc::channel();
        let (owner_seal_tx, owner_seal_rx) = std::sync::mpsc::channel();
        let (owner_sealed_tx, owner_sealed_rx) = std::sync::mpsc::channel();
        let (owner_retire_tx, owner_retire_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let token = activation_enter(0x61);
            let producer = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
            assert!(!producer.is_null());
            assert_eq!(
                host_syscall(SYS_GETPID, &[]),
                crate::os::process::getpid() as i64
            );
            owner_ready_tx.send(producer as usize).unwrap();
            owner_seal_rx.recv().unwrap();
            activation_exit(token, 0);
            owner_sealed_tx.send(()).unwrap();
            owner_retire_rx.recv().unwrap();
            retire_current_thread();
        });
        let owner_producer = owner_ready_rx.recv().unwrap() as *const Producer;

        let (waiting_ready_tx, waiting_ready_rx) = std::sync::mpsc::channel();
        let (waiting_release_tx, waiting_release_rx) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn(move || {
            let token = activation_enter(0x62);
            let producer = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
            assert!(!producer.is_null());
            assert!(unsafe { (*producer).pages.load(Ordering::Acquire).is_null() });
            assert_eq!(
                host_syscall(SYS_GETPID, &[]),
                crate::os::process::getpid() as i64
            );
            let drops_after_first = unsafe { (*(*producer).cold_ptr()).capacity_drops };
            assert_eq!(drops_after_first, 2);
            waiting_ready_tx.send(()).unwrap();
            waiting_release_rx.recv().unwrap();

            let deadline = Instant::now() + Duration::from_secs(2);
            while unsafe { (*producer).pages.load(Ordering::Acquire).is_null() }
                && Instant::now() < deadline
            {
                std::thread::yield_now();
            }
            let offered = unsafe { !(*producer).pages.load(Ordering::Acquire).is_null() };
            if offered {
                assert_eq!(
                    host_syscall(SYS_GETPID, &[]),
                    crate::os::process::getpid() as i64
                );
            }
            let recovered =
                offered && unsafe { (*(*producer).cold_ptr()).capacity_drops == drops_after_first };
            activation_exit(token, 0);
            retire_current_thread();
            recovered
        });
        waiting_ready_rx.recv().unwrap();
        assert_eq!(
            session
                .core
                .page_pool
                .allocated_bytes
                .load(Ordering::Acquire),
            STARTER_BYTES
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        while session.core.writer_state.load(Ordering::Acquire) != WRITER_SLEEPING
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(
            session.core.writer_state.load(Ordering::Acquire),
            WRITER_SLEEPING,
            "writer did not sleep before the releasing producer made progress"
        );

        owner_seal_tx.send(()).unwrap();
        owner_sealed_rx.recv().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while (unsafe {
            (*owner_producer).returned.head.load(Ordering::Acquire)
                != (*owner_producer).published.tail.load(Ordering::Acquire)
        } || session.core.writer_state.load(Ordering::Acquire) != WRITER_SLEEPING)
            && Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(
            unsafe { (*owner_producer).returned.head.load(Ordering::Acquire) },
            unsafe { (*owner_producer).published.tail.load(Ordering::Acquire) },
            "writer did not drain the releasing producer before its retirement"
        );
        assert_eq!(
            session.core.writer_state.load(Ordering::Acquire),
            WRITER_SLEEPING,
            "writer did not sleep before the retirement wakeup"
        );

        owner_retire_tx.send(()).unwrap();
        owner.join().unwrap();
        waiting_release_tx.send(()).unwrap();
        assert!(
            waiting.join().unwrap(),
            "page-less producer never received the retired starter pages"
        );

        let FinishStatus::Finished(summary) = session.finish(Duration::from_secs(2)).unwrap()
        else {
            panic!("capture writer did not stop");
        };
        assert_eq!(summary.producers, 2);
        assert!(summary.encoded_records >= 4);
        assert_eq!(
            session
                .core
                .page_pool
                .allocated_bytes
                .load(Ordering::Acquire),
            0
        );
        let outcome = decode_file(&output, &mut |_| Ok(())).unwrap();
        assert_eq!(outcome.health, Health::Clean, "{:?}", outcome.issues);
        return;
    }

    let output = test_path();
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(
            "telemetry::capture::tests::ring::page_less_producer_recovers_after_retired_pages_return_to_pool",
        )
        .arg("--test-threads=1")
        .env(CHILD_ENV, &output)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_file(output).unwrap();
}

#[test]
fn retired_short_lived_threads_leave_the_writer_scan() {
    const CHILD_ENV: &str = "MIRVM_CAPTURE_RETIRED_SCAN_CHILD";
    const THREADS: usize = 2_048;
    if let Some(output) = std::env::var_os(CHILD_ENV) {
        let output = PathBuf::from(output);
        let mut session = CaptureSession::start(StartOptions::new(&output, 0)).unwrap();
        for _ in 0..THREADS {
            std::thread::spawn(|| {
                let token = activation_enter(0x71);
                assert_eq!(
                    host_syscall(SYS_GETPID, &[]),
                    crate::os::process::getpid() as i64
                );
                activation_exit(token, 0);
                retire_current_thread();
            })
            .join()
            .unwrap();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while writer_active_scan_len(session.core) != 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(
            writer_active_scan_len(session.core),
            0,
            "retired producer descriptors accumulated in the writer scan"
        );

        let FinishStatus::Finished(summary) = session.finish(Duration::from_secs(2)).unwrap()
        else {
            panic!("capture writer did not stop");
        };
        assert_eq!(summary.producers, THREADS as u64);
        assert_eq!(summary.encoded_records, 0);
        assert_eq!(summary.producer_drops, (THREADS * 2) as u64);
        return;
    }

    let output = test_path();
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("telemetry::capture::tests::ring::retired_short_lived_threads_leave_the_writer_scan")
        .arg("--test-threads=1")
        .env(CHILD_ENV, &output)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_file(output).unwrap();
}
