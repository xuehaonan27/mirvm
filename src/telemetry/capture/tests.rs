//! Capture tests: page-ring accounting, fork generations, the pinned/inline hot
//! paths, and the committed wire format as read back by the decoder.

use super::*;
use crate::telemetry::capture_session::{
    REBUILD_RECIPE, RebuildRecipe, clear_rebuild_recipe, pending_rebuild_recipe,
    publish_rebuild_recipe,
};
use crate::telemetry::capture_writer::{
    partial_path, reclaim_session_pages, sealed_page_bytes, write_file_header, writer_main,
    writer_offer_starter, writer_reap_retired,
};
use crate::telemetry::decode::{DecodedKind, Health, decode_file};
use crate::telemetry::format::{EngineContext, PageHeader};
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn test_path() -> PathBuf {
    let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("mirvm-capture-{}-{id}.mlog", std::process::id()))
}

/// L2: the rebuild recipe is plain immutable memory whose address is
/// inherited unchanged by `fork`, so a child can read it without taking any
/// lock or running allocator code.
#[test]
fn rebuild_recipe_is_published_and_readable() {
    let path = std::env::temp_dir().join("events-1-0.mlog");
    let directory = path.parent().unwrap().to_path_buf();
    publish_rebuild_recipe(&path, 4096);
    let recipe = pending_rebuild_recipe().expect("recipe must be published");
    assert_eq!(recipe.directory, directory);
    assert_eq!(recipe.page_budget_bytes, 4096);
    clear_rebuild_recipe();
    assert!(pending_rebuild_recipe().is_none(), "clear must unpublish");
}

/// L2: the published recipe's memory is inherited by `fork`, which is the
/// whole point of publishing an address instead of storing the recipe in a
/// session a child may not touch.
///
/// The parent captures the address before forking and hands it to the child,
/// so this checks the inherited memory rather than the global pointer, which
/// parallel tests may legitimately clear.
#[test]
fn rebuild_recipe_memory_survives_fork() {
    let path = std::env::temp_dir().join("events-1-0.mlog");
    publish_rebuild_recipe(&path, 8192);
    let address = REBUILD_RECIPE.load(Ordering::Acquire);
    assert_ne!(address, 0, "recipe must be published before the fork");

    let mut pipe_fds = [0_i32; 2];
    assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
    // Product fork path, so the child hook runs (a raw libc::fork does not).
    let pid = host_syscall(libc::SYS_fork, &[]) as libc::pid_t;
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe { libc::close(pipe_fds[0]) };
        // SAFETY: the recipe is leaked for the process lifetime, and the
        // child inherited the same address space.
        let seen = unsafe { (*(address as *const RebuildRecipe)).page_budget_bytes };
        let payload = seen.to_le_bytes();
        let written = unsafe { libc::write(pipe_fds[1], payload.as_ptr().cast(), payload.len()) };
        let code = if written == payload.len() as isize {
            0
        } else {
            3
        };
        unsafe { libc::_exit(code) };
    }
    unsafe { libc::close(pipe_fds[1]) };
    let mut payload = [0_u8; 8];
    let got = unsafe { libc::read(pipe_fds[0], payload.as_mut_ptr().cast(), payload.len()) };
    unsafe { libc::close(pipe_fds[0]) };
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status), "child did not exit normally");
    assert_eq!(got, 8, "child did not report");
    assert_eq!(
        u64::from_le_bytes(payload),
        8192,
        "the child must read the parent's recipe through the inherited address"
    );
}

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
        owner_tid: unsafe { libc::gettid() as u32 },
    }));
    let producer = Box::leak(Box::new(Producer::new(
        core,
        core.page_pool.take_starter(),
        id,
        id as u32,
        id as u32,
        id,
        unsafe { libc::__errno_location() },
    )));
    let producer = producer as *mut Producer;
    // Attach through the cold path so the page and budget are real.
    let disposition = unsafe { record_syscall_enter(&*producer, libc::SYS_getpid, &[]) };
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
    let (result, used) = unsafe { host_syscall_pinned(pinned, libc::SYS_getpid, &args) };
    assert_eq!(
        result,
        unsafe { libc::syscall(libc::SYS_getpid) } as i64,
        "the pinned entry must still perform the syscall"
    );
    assert_eq!(
        used, pinned,
        "a pinned call with a usable recorder must report that same recorder back"
    );

    let disposition = unsafe { record_syscall_enter(&*cold, libc::SYS_getpid, &args) };
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
    for (index, syscall) in [libc::SYS_getpid, libc::SYS_getppid]
        .into_iter()
        .enumerate()
    {
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

/// L2: a `fork` child must not reuse the parent's generation, and must keep
/// claiming that same generation for later sessions in the same process.
/// Exercised across a real fork; the parent's value must not move.
#[test]
fn forked_child_advances_the_process_generation_once() {
    // Make the parent own its generation; a session would do the same, but
    // the global "one session per process" rule forbids starting another
    // here while parallel tests run capture sessions.
    let parent_generation = claim_process_generation();

    let mut pipe_fds = [0_i32; 2];
    assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
    // Use the product fork path (HostFork / generic SYS_fork both funnel
    // through `host_syscall`), not a raw libc::fork, so the post-fork hook
    // actually runs.
    let pid = host_syscall(libc::SYS_fork, &[]) as libc::pid_t;
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe { libc::close(pipe_fds[0]) };
        // The child inherits the parent's generation plus the pending mark.
        let first = claim_process_generation();
        let second = claim_process_generation();
        let mut payload = [0_u8; 16];
        payload[..8].copy_from_slice(&first.to_le_bytes());
        payload[8..].copy_from_slice(&second.to_le_bytes());
        let written = unsafe { libc::write(pipe_fds[1], payload.as_ptr().cast(), payload.len()) };
        let code = if written == payload.len() as isize {
            0
        } else {
            3
        };
        unsafe { libc::_exit(code) };
    }
    unsafe { libc::close(pipe_fds[1]) };
    let mut payload = [0_u8; 16];
    let mut read = 0_usize;
    while read < payload.len() {
        let got = unsafe {
            libc::read(
                pipe_fds[0],
                payload[read..].as_mut_ptr().cast(),
                payload.len() - read,
            )
        };
        if got <= 0 {
            break;
        }
        read += got as usize;
    }
    unsafe { libc::close(pipe_fds[0]) };
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status), "child did not exit normally");
    assert_eq!(libc::WEXITSTATUS(status), 0, "child failed to report");
    assert_eq!(read, payload.len(), "short read from the child");

    let child_first = u64::from_le_bytes(payload[..8].try_into().unwrap());
    let child_second = u64::from_le_bytes(payload[8..].try_into().unwrap());
    assert_eq!(
        child_first,
        parent_generation + 1,
        "the child's first session must take the next generation"
    );
    assert_eq!(
        child_second, child_first,
        "later sessions in the same child reuse their own generation"
    );
    assert_eq!(
        claim_process_generation(),
        parent_generation,
        "the child's generation must not leak back into the parent"
    );
}

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
        .arg("telemetry::capture::tests::final_capture_file_is_never_replaced")
        .arg("--test-threads=1")
        .env(CHILD_ENV, output)
        .status()
        .unwrap();
    assert!(status.success());
}

extern "C" fn exit_through_captured_syscall(_: *mut c_void) -> *mut c_void {
    let token = activation_enter(0x51);
    let _ = host_syscall(libc::SYS_exit, &[0]);
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
                libc::pthread_create(
                    thread.as_mut_ptr(),
                    ptr::null(),
                    exit_through_captured_syscall,
                    ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(
            unsafe { libc::pthread_join(thread.assume_init(), ptr::null_mut()) },
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
        .arg("telemetry::capture::tests::nonreturning_thread_syscall_releases_the_capture_root")
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
    let test_name = "telemetry::capture::tests::session_publishes_drains_and_decodes_exact_file";
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
        } if errno == libc::ENOSYS as u32
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

    crate::os::process::set_errno(libc::EDOM);
    let token = activation_enter(17);
    assert_eq!(crate::os::process::errno(), libc::EDOM);
    let result = host_syscall(libc::SYS_getpid, &[]);
    assert_eq!(result, unsafe { libc::getpid() } as i64);
    assert_eq!(crate::os::process::errno(), libc::EDOM);
    assert_eq!(host_syscall(-1, &[]), -1);
    assert_eq!(crate::os::process::errno(), libc::ENOSYS);

    session.request_stop();
    assert!(!is_armed());
    // The old root keeps its producer until it returns even though new
    // roots are no longer admitted.
    let child = host_syscall(libc::SYS_fork, &[]);
    if child == 0 {
        // The copied guard must observe the invalid generation and leave
        // the copied parent page/active-root ledger untouched.
        activation_exit(token, 0);
        unsafe { libc::_exit(0) };
    }
    assert!(child > 0);
    let mut wait_status = 0;
    assert_eq!(
        unsafe { libc::waitpid(child as i32, &mut wait_status, 0) },
        child as i32
    );
    assert!(libc::WIFEXITED(wait_status));
    assert_eq!(libc::WEXITSTATUS(wait_status), 0);
    activation_exit(token, 0);

    let stopped_token = activation_enter(17);
    assert!(stopped_token.session.is_null());
    assert_eq!(
        host_syscall(libc::SYS_getpid, &[]),
        unsafe { libc::getpid() } as i64
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
    crate::os::process::set_errno(libc::EDOM);
    let token = activation_enter(29);
    assert_eq!(crate::os::process::errno(), libc::EDOM);
    assert_eq!(
        host_syscall(libc::SYS_getpid, &[]),
        unsafe { libc::getpid() } as i64
    );
    assert_eq!(crate::os::process::errno(), libc::EDOM);
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
        owner_tid: unsafe { libc::gettid() as u32 },
    }));
    let pages = Box::into_raw(Box::new(PagePair::new()));
    let producer = Box::leak(Box::new(Producer::new(
        core,
        pages,
        1,
        1,
        unsafe { libc::gettid() as u32 },
        11,
        unsafe { libc::__errno_location() },
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
        owner_tid: unsafe { libc::gettid() as u32 },
    }));
    let pages = Box::into_raw(Box::new(PagePair::new()));
    let producer = Box::leak(Box::new(Producer::new(
        core,
        pages,
        1,
        1,
        unsafe { libc::gettid() as u32 },
        7,
        unsafe { libc::__errno_location() },
    )));
    crate::os::process::set_errno(libc::EDOM);

    for _ in 0..90 {
        let disposition = unsafe { record_syscall_enter(producer, libc::SYS_getpid, &[]) };
        assert!(matches!(disposition, EnterDisposition::Recorded));
        unsafe { record_syscall_exit(producer, disposition, 1, 0) };
    }
    let dropped = unsafe { record_syscall_enter(producer, libc::SYS_getpid, &[]) };
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
    let recovered = unsafe { record_syscall_enter(producer, libc::SYS_getpid, &[]) };
    assert!(matches!(recovered, EnterDisposition::Recorded));
    unsafe { record_syscall_exit(producer, recovered, 1, 0) };
    assert_eq!(crate::os::process::errno(), libc::EDOM);
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
                host_syscall(libc::SYS_getpid, &[]),
                unsafe { libc::getpid() } as i64
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
                host_syscall(libc::SYS_getpid, &[]),
                unsafe { libc::getpid() } as i64
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
                    host_syscall(libc::SYS_getpid, &[]),
                    unsafe { libc::getpid() } as i64
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
            "telemetry::capture::tests::page_less_producer_recovers_after_retired_pages_return_to_pool",
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
                    host_syscall(libc::SYS_getpid, &[]),
                    unsafe { libc::getpid() } as i64
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
        .arg("telemetry::capture::tests::retired_short_lived_threads_leave_the_writer_scan")
        .arg("--test-threads=1")
        .env(CHILD_ENV, &output)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_file(output).unwrap();
}

#[test]
fn writer_takes_at_most_one_page_per_producer_per_round() {
    let output = test_path();
    let partial = partial_path(&output);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .unwrap();
    let offset = write_file_header(&mut file, unsafe { libc::getpid() }, 0).unwrap();
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
        owner_tid: unsafe { libc::gettid() as u32 },
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
            unsafe { libc::__errno_location() },
        )));
        for _ in 0..90 {
            let disposition = unsafe { record_syscall_enter(producer, libc::SYS_getpid, &[]) };
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
        owner_tid: unsafe { libc::gettid() as u32 },
    }));
    let producer = Box::leak(Box::new(Producer::new(
        core,
        ptr::null_mut(),
        1,
        1,
        unsafe { libc::gettid() as u32 },
        1,
        unsafe { libc::__errno_location() },
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

fn writer_active_scan_len(core: &SessionCore) -> usize {
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

#[test]
fn stopped_trace_path_falls_back_to_libc_syscall() {
    assert!(TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed).is_null());
    let result = host_syscall(libc::SYS_getpid, &[]);
    assert_eq!(result, unsafe { libc::getpid() } as i64);
}
