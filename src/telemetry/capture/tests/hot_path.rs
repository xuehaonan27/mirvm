//! The recording hot paths: the pinned entry the trace domain reaches through its register, the
//! inline entry a trace JIT calls, the ABI the fast structure promises, and the fallback when no
//! producer is attached.

use super::*;

/// A producer with a starter ring and a live page, for hot-path tests.
fn producer_with_open_page(id: u64) -> *mut Producer {
    let core = Box::leak(Box::new(SessionCore {
        phase: AtomicU8::new(PHASE_ARMED),
        active_roots: AtomicUsize::new(0),
        writer_state: AtomicU32::new(WRITER_AWAKE),
        writer_done: AtomicBool::new(false),
        producers: Mutex::new(Vec::new()),
        active_producers: Mutex::new(Vec::new()),
        next_producer: AtomicU64::new(1),
        next_thread_generation: AtomicU32::new(1),
        page_pool: PagePool::new(STARTER_BYTES * 2),
        sink_loss: AtomicU64::new(0),
        owner_tid: crate::os::process::gettid() as u32,
    }));
    let producer = Box::leak(Box::new(Producer::new(
        core,
        core.page_pool.take_starter(),
        id,
        id as u32,
        id as u32,
        id,
        crate::os::process::errno_location(),
    )));
    let producer = producer as *mut Producer;
    // Attach through the cold path so the page and budget are real.
    let disposition = unsafe { record_syscall_enter(&*producer, SYS_GETPID, &[]) };
    assert!(matches!(disposition, EnterDisposition::Recorded));
    unsafe { record_syscall_exit(&*producer, disposition, 1, 0) };
    producer
}

/// L3 pinned entry: the trace domain reaches the recorder through the
/// register its boundary installed, never through thread-local state. This
/// is the property the pinned path exists for, so it is asserted directly:
/// with no producer in TLS at all, the pinned entry must record exactly the
/// bytes the cold entry records, and must return the real syscall result.
#[test]
fn pinned_entry_records_without_thread_local_state() {
    let cold = producer_with_open_page(17);
    let pinned = producer_with_open_page(18);
    let args = [5_u64, 6, 7, 8, 9, 10];

    let displaced = TLS_ACTIVE_PRODUCER.swap(ptr::null_mut(), Ordering::Relaxed);
    assert!(
        displaced.is_null(),
        "the test harness must not already be recording on this thread"
    );
    let (result, used) = unsafe { host_syscall_pinned(pinned, SYS_GETPID, &args) };
    assert_eq!(
        result,
        crate::os::process::syscall(SYS_GETPID, &[]),
        "the pinned entry must still perform the syscall"
    );
    assert_eq!(
        used, pinned,
        "a pinned call with a usable recorder must report that same recorder back"
    );

    let disposition = unsafe { record_syscall_enter(&*cold, SYS_GETPID, &args) };
    assert!(matches!(disposition, EnterDisposition::Recorded));
    unsafe { record_syscall_exit(&*cold, disposition, result, 0) };

    let cold_page = unsafe { (*cold).active_page() };
    let pinned_page = unsafe { (*pinned).active_page() };
    let cold_len = unsafe { (*cold).fast_ptr().read().cursor } as usize
        - cold_page.bytes.get().cast::<u8>() as usize;
    let pinned_len = unsafe { (*pinned).fast_ptr().read().cursor } as usize
        - pinned_page.bytes.get().cast::<u8>() as usize;
    assert_eq!(
        unsafe { &(&(*pinned_page.bytes.get()))[..pinned_len] },
        unsafe { &(&(*cold_page.bytes.get()))[..cold_len] },
        "the pinned entry must write what the cold entry writes"
    );
}

/// L3 hot path: the inline entry a trace JIT reaches through the pinned
/// `r15` must be byte-identical to the current cold entry for a healthy
/// stream, otherwise the trace domain would change the file format. It must
/// also leave the producer ready for the *next* entry, i.e. the two paths
/// are interchangeable rather than merely similar.
#[test]
fn inline_record_matches_legacy_bytes() {
    let legacy = producer_with_open_page(7);
    let inline = producer_with_open_page(8);
    // SAFETY: both producers are leaked for the process lifetime above.
    let (legacy_ref, inline_ref): (&Producer, &Producer) = unsafe { (&*legacy, &*inline) };
    let args = [11_u64, 22, 33, 44, 55, 66];

    // One entry and one exit through each implementation.
    for (index, syscall) in [SYS_GETPID, SYS_GETPPID].into_iter().enumerate() {
        let disposition = unsafe { record_syscall_enter(legacy_ref, syscall, &args) };
        assert!(matches!(disposition, EnterDisposition::Recorded));
        unsafe { record_syscall_exit(legacy_ref, disposition, index as i64, 0) };

        let hot = unsafe { record_syscall_enter_inline(inline, syscall, &args) };
        assert_eq!(hot, HotEnter::Recorded);
        // A recorded hot entry is exactly `EnterDisposition::Recorded`; the
        // drop dispositions only ever come from the cold path.
        unsafe {
            record_syscall_exit(inline_ref, EnterDisposition::Recorded, index as i64, 0);
        }
    }

    let legacy_page = unsafe { (*legacy).active_page() };
    let inline_page = unsafe { (*inline).active_page() };
    let legacy_len = unsafe { (*legacy).fast_ptr().read().cursor } as usize
        - legacy_page.bytes.get().cast::<u8>() as usize;
    let inline_len = unsafe { (*inline).fast_ptr().read().cursor } as usize
        - inline_page.bytes.get().cast::<u8>() as usize;
    let legacy_bytes = unsafe { &(&(*legacy_page.bytes.get()))[..legacy_len] };
    let inline_bytes = unsafe { &(&(*inline_page.bytes.get()))[..inline_len] };
    assert_eq!(
        inline_bytes, legacy_bytes,
        "inline hot path must write the same bytes as the cold entry"
    );

    // Interchangeability: both producers agree on everything an entry and
    // exit maintain.
    let legacy_fast = unsafe { (*legacy).fast_ptr().read() };
    let inline_fast = unsafe { (*inline).fast_ptr().read() };
    // Cursor is an absolute pointer into each producer's own page, so the
    // comparable value is the used length (already checked byte-for-byte
    // above); the remaining budget must match exactly.
    assert_eq!(inline_fast.pair_budget, legacy_fast.pair_budget);
    let legacy_cold = unsafe { (*legacy).cold_ptr().read() };
    let inline_cold = unsafe { (*inline).cold_ptr().read() };
    assert_eq!(inline_cold.next_sequence, legacy_cold.next_sequence);
    // Each producer owns its own page ring, so compare the state that must
    // agree rather than their distinct page pointers.
    assert_eq!(inline_cold.has_active, legacy_cold.has_active);
    assert_eq!(inline_cold.page_ordinal, legacy_cold.page_ordinal);
}

#[test]
fn producer_fast_abi_is_one_cache_line() {
    assert_eq!(std::mem::size_of::<ProducerFast>(), 64);
    assert_eq!(std::mem::align_of::<ProducerFast>(), 64);
    assert_eq!(std::mem::offset_of!(ProducerFast, errno_ptr), 16);
}

#[test]
fn stopped_trace_path_falls_back_to_libc_syscall() {
    assert!(TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed).is_null());
    let result = host_syscall(SYS_GETPID, &[]);
    assert_eq!(result, crate::os::process::getpid() as i64);
}
