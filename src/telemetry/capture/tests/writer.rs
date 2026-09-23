//! The writer's schedule: at most one page per producer per round, and one page owner when a
//! retirement lands in the same round as an offer.

use super::*;

#[test]
fn writer_takes_at_most_one_page_per_producer_per_round() {
    let output = test_path();
    let partial = partial_path(&output);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .unwrap();
    let offset = write_file_header(&mut file, crate::os::process::getpid(), 0).unwrap();
    let core = Box::leak(Box::new(SessionCore {
        phase: AtomicU8::new(PHASE_STOPPING),
        active_roots: AtomicUsize::new(0),
        writer_state: AtomicU32::new(WRITER_AWAKE),
        writer_done: AtomicBool::new(false),
        producers: Mutex::new(Vec::new()),
        active_producers: Mutex::new(Vec::new()),
        next_producer: AtomicU64::new(3),
        next_thread_generation: AtomicU32::new(3),
        page_pool: PagePool::new(STARTER_BYTES * 2),
        sink_loss: AtomicU64::new(0),
        owner_tid: crate::os::process::gettid() as u32,
    }));
    for producer_id in [1_u64, 2] {
        let pages = core.page_pool.take_starter();
        assert!(!pages.is_null());
        let producer = Box::leak(Box::new(Producer::new(
            core,
            pages,
            producer_id,
            producer_id as u32,
            producer_id as u32,
            producer_id,
            crate::os::process::errno_location(),
        )));
        for _ in 0..90 {
            let disposition = unsafe { record_syscall_enter(producer, SYS_GETPID, &[]) };
            assert!(matches!(disposition, EnterDisposition::Recorded));
            unsafe { record_syscall_exit(producer, disposition, 1, 0) };
        }
        unsafe { seal_page(producer) };
        assert_eq!(producer.published.tail.load(Ordering::Acquire), 2);
        core.producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(producer as *mut Producer as usize);
        core.active_producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(producer as *mut Producer as usize);
    }

    writer_main(core, file, offset, &partial, &output).unwrap();
    let mut producer_order = Vec::new();
    let outcome = decode_file(&output, &mut |event| {
        producer_order.push(event.context.producer_id);
        Ok(())
    })
    .unwrap();
    assert_eq!(outcome.health, Health::Clean, "{:?}", outcome.issues);
    assert_eq!(producer_order.len(), 360);
    assert_eq!(producer_order[0], 1);
    assert_eq!(producer_order[90], 2);
    assert_eq!(producer_order[180], 1);
    assert_eq!(producer_order[270], 2);
    assert_eq!(core.page_pool.allocated_bytes.load(Ordering::Acquire), 0);
    std::fs::remove_file(output).unwrap();
}

#[test]
fn writer_offer_and_retire_in_one_round_keep_one_page_owner() {
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
    let producer = Box::leak(Box::new(Producer::new(
        core,
        ptr::null_mut(),
        1,
        1,
        crate::os::process::gettid() as u32,
        1,
        crate::os::process::errno_location(),
    )));
    let addr = producer as *mut Producer as usize;
    core.producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(addr);
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(addr);

    assert!(writer_offer_starter(core, producer));
    let offered = producer.pages.load(Ordering::Acquire);
    assert!(!offered.is_null());
    producer.retired.retired.store(true, Ordering::Release);
    assert!(writer_reap_retired(core));
    assert!(producer.pages.load(Ordering::Acquire).is_null());
    assert_eq!(writer_active_scan_len(core), 0);
    let free = core
        .page_pool
        .free
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(free.as_slice(), &[offered as usize]);
    drop(free);

    reclaim_session_pages(core);
    assert_eq!(core.page_pool.allocated_bytes.load(Ordering::Acquire), 0);
}
