//! Process-wide syscall capture.
//!
//! The event path owns one active page and publishes only sealed pages. The
//! writer owns published pages until their complete chunk has reached the
//! kernel. No writer field shares a cache line with [`ProducerFast`].

use std::cell::UnsafeCell;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::format::{
    CHUNK_FOOTER_BYTES, CHUNK_HEADER_BYTES, CLOCK_NONE, ChunkFooter, ChunkHeader,
    ENGINE_CONTEXT_BYTES, EngineContext, FileHeader, PAGE_BYTES_4K, PAGE_HEADER_BYTES,
    PRODUCER_END_BYTES, PageHeader, ProducerEnd, SESSION_END_BYTES, SYSCALL_ENTER_BYTES,
    SYSCALL_EXIT_BYTES, SYSCALL_PAIR_BYTES, SessionEnd, SyscallEnter, SyscallExit,
    SyscallSemantics, WireError,
};

const PAGE_BYTES: usize = PAGE_BYTES_4K as usize;
const STARTER_BYTES: usize = PAGE_BYTES * 2;

const PHASE_ARMED: u8 = 1;
const PHASE_STOPPING: u8 = 2;
const PHASE_SINK_FAILED: u8 = 3;
const PHASE_FINISHED: u8 = 4;

const WRITER_AWAKE: u32 = 0;
const WRITER_SLEEPING: u32 = 1;

static ACTIVE: AtomicPtr<SessionCore> = AtomicPtr::new(ptr::null_mut());
static START_LOCK: Mutex<()> = Mutex::new(());

#[thread_local]
static TLS_ACTIVE_PRODUCER: AtomicPtr<Producer> = AtomicPtr::new(ptr::null_mut());
#[thread_local]
static TLS_CACHED_PRODUCER: AtomicPtr<Producer> = AtomicPtr::new(ptr::null_mut());
#[thread_local]
static TLS_CACHED_SESSION: AtomicPtr<SessionCore> = AtomicPtr::new(ptr::null_mut());
#[thread_local]
static TLS_ACTIVATION_DEPTH: AtomicU32 = AtomicU32::new(0);

/// Stable first cache line consumed by future raw trace sites.
#[repr(C, align(64))]
pub(crate) struct ProducerFast {
    pub(crate) cursor: u64,
    pub(crate) pair_budget: u64,
    pub(crate) errno_ptr: u64,
    _reserved: [u8; 40],
}

const _: () = assert!(std::mem::size_of::<ProducerFast>() == 64);
const _: () = assert!(std::mem::align_of::<ProducerFast>() == 64);
const _: () = assert!(std::mem::offset_of!(ProducerFast, cursor) == 0);
const _: () = assert!(std::mem::offset_of!(ProducerFast, pair_budget) == 8);
const _: () = assert!(std::mem::offset_of!(ProducerFast, errno_ptr) == 16);

#[repr(C, align(4096))]
struct Page {
    bytes: UnsafeCell<[u8; PAGE_BYTES]>,
}

impl Page {
    fn new() -> Self {
        Self {
            bytes: UnsafeCell::new([0; PAGE_BYTES]),
        }
    }
}

unsafe impl Sync for Page {}

struct PagePair {
    pages: [Page; 2],
}

impl PagePair {
    fn new() -> Self {
        Self {
            pages: [Page::new(), Page::new()],
        }
    }
}

struct PagePool {
    budget_bytes: usize,
    allocated_bytes: AtomicUsize,
    free: Mutex<Vec<usize>>,
}

impl PagePool {
    fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            allocated_bytes: AtomicUsize::new(0),
            free: Mutex::new(Vec::new()),
        }
    }

    fn take_starter(&self) -> *mut PagePair {
        if let Some(addr) = self.free.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            return addr as *mut PagePair;
        }
        if self
            .allocated_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(STARTER_BYTES)
                    .filter(|next| *next <= self.budget_bytes)
            })
            .is_err()
        {
            return ptr::null_mut();
        }
        Box::into_raw(Box::new(PagePair::new()))
    }

    fn return_starter(&self, pages: *mut PagePair) {
        if pages.is_null() {
            return;
        }
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        free.push(pages as usize);
    }

    unsafe fn release_all(&self) {
        let pages = std::mem::take(&mut *self.free.lock().unwrap_or_else(|e| e.into_inner()));
        let allocated = self.allocated_bytes.swap(0, Ordering::AcqRel);
        debug_assert_eq!(allocated, pages.len().saturating_mul(STARTER_BYTES));
        for addr in pages {
            unsafe { drop(Box::from_raw(addr as *mut PagePair)) };
        }
    }
}

#[repr(C, align(64))]
struct ProducerCold {
    has_active: bool,
    context_unsynced: bool,
    _pad0: [u8; 6],
    active_page: *mut Page,
    next_publish: u64,
    next_sequence: u64,
    page_first_sequence: u64,
    page_ordinal: u64,
    current_engine: u64,
    initial_engine: u64,
    marker_count: u32,
    _pad1: u32,
    capacity_drops: u64,
    context_drops: u64,
    recursive_drops: u64,
}

#[repr(C, align(64))]
struct PublishedLine {
    tail: AtomicU64,
    _pad: [u8; 56],
}

#[repr(C, align(64))]
struct ReturnedLine {
    head: AtomicU64,
    _pad: [u8; 56],
}

#[repr(C, align(64))]
struct WriterLine {
    head: UnsafeCell<u64>,
    committed_records: UnsafeCell<u64>,
    sink_loss: UnsafeCell<u64>,
    _pad: [u8; 40],
}

#[repr(C, align(64))]
struct RetiredLine {
    retired: AtomicBool,
    _pad: [u8; 63],
}

struct Producer {
    fast: UnsafeCell<ProducerFast>,
    cold: UnsafeCell<ProducerCold>,
    published: PublishedLine,
    returned: ReturnedLine,
    writer: WriterLine,
    retired: RetiredLine,
    session: *const SessionCore,
    pages: AtomicPtr<PagePair>,
    producer_id: u64,
    thread_generation: u32,
    tid: u32,
}

// Producer and writer access disjoint fields/pages according to the two SPSC
// indices. The release/acquire ownership transfers guard page byte access.
unsafe impl Sync for Producer {}
unsafe impl Send for Producer {}

impl Producer {
    fn new(
        session: *const SessionCore,
        pages: *mut PagePair,
        producer_id: u64,
        thread_generation: u32,
        tid: u32,
        engine_id: u64,
        errno_ptr: *mut i32,
    ) -> Self {
        Self {
            fast: UnsafeCell::new(ProducerFast {
                cursor: 0,
                pair_budget: 0,
                errno_ptr: errno_ptr as u64,
                _reserved: [0; 40],
            }),
            cold: UnsafeCell::new(ProducerCold {
                has_active: false,
                context_unsynced: false,
                _pad0: [0; 6],
                active_page: ptr::null_mut(),
                next_publish: 0,
                next_sequence: 0,
                page_first_sequence: 0,
                page_ordinal: 0,
                current_engine: engine_id,
                initial_engine: engine_id,
                marker_count: 0,
                _pad1: 0,
                capacity_drops: 0,
                context_drops: 0,
                recursive_drops: 0,
            }),
            published: PublishedLine {
                tail: AtomicU64::new(0),
                _pad: [0; 56],
            },
            returned: ReturnedLine {
                head: AtomicU64::new(0),
                _pad: [0; 56],
            },
            writer: WriterLine {
                head: UnsafeCell::new(0),
                committed_records: UnsafeCell::new(0),
                sink_loss: UnsafeCell::new(0),
                _pad: [0; 40],
            },
            retired: RetiredLine {
                retired: AtomicBool::new(false),
                _pad: [0; 63],
            },
            session,
            pages: AtomicPtr::new(pages),
            producer_id,
            thread_generation,
            tid,
        }
    }

    #[inline]
    fn cold_ptr(&self) -> *mut ProducerCold {
        self.cold.get()
    }

    #[inline]
    fn fast_ptr(&self) -> *mut ProducerFast {
        self.fast.get()
    }

    #[inline]
    unsafe fn page(&self, slot: usize) -> &Page {
        let pages = self.pages.load(Ordering::Relaxed);
        debug_assert!(!pages.is_null());
        unsafe { &(*pages).pages[slot] }
    }

    #[inline]
    unsafe fn active_page(&self) -> &Page {
        let page = unsafe { (*self.cold_ptr()).active_page };
        debug_assert!(!page.is_null());
        unsafe { &*page }
    }
}

struct SessionCore {
    phase: AtomicU8,
    active_roots: AtomicUsize,
    writer_state: AtomicU32,
    writer_done: AtomicBool,
    producers: Mutex<Vec<usize>>,
    active_producers: Mutex<Vec<usize>>,
    next_producer: AtomicU64,
    next_thread_generation: AtomicU32,
    page_pool: PagePool,
    sink_loss: AtomicU64,
    owner_tid: u32,
}

impl SessionCore {
    fn wake_writer(&self) {
        if self.writer_state.swap(WRITER_AWAKE, Ordering::AcqRel) == WRITER_SLEEPING {
            let _ = crate::os::thread::futex_wake_one_raw(self.writer_state.as_ptr());
        }
    }

    fn try_enter_root(&self) -> bool {
        if self.phase.load(Ordering::Acquire) != PHASE_ARMED {
            return false;
        }
        self.active_roots.fetch_add(1, Ordering::AcqRel);
        if self.phase.load(Ordering::Acquire) == PHASE_ARMED {
            true
        } else {
            self.active_roots.fetch_sub(1, Ordering::Release);
            self.wake_writer();
            false
        }
    }
}

/// Internal construction options. The byte cap is a hard process budget, not
/// a correctness switch; a producer that cannot obtain pages remains attached
/// and automatically retries when the writer returns pages to the pool.
pub(crate) struct StartOptions {
    pub(crate) output: PathBuf,
    pub(crate) page_budget_bytes: usize,
}

impl StartOptions {
    pub(crate) fn new(output: impl Into<PathBuf>, page_budget_bytes: usize) -> Self {
        Self {
            output: output.into(),
            page_budget_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CaptureSummary {
    pub(crate) encoded_records: u64,
    pub(crate) producer_drops: u64,
    pub(crate) sink_loss: u64,
    pub(crate) producers: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FinishStatus {
    Finished(CaptureSummary),
    TimedOut,
}

pub(crate) struct CaptureSession {
    core: &'static SessionCore,
    writer: Option<JoinHandle<io::Result<CaptureSummary>>>,
    owner_pid: libc::pid_t,
}

impl CaptureSession {
    pub(crate) fn start(options: StartOptions) -> io::Result<Self> {
        let _start = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if !ACTIVE.load(Ordering::Acquire).is_null() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a capture session is already active or draining",
            ));
        }
        match std::fs::symlink_metadata(&options.output) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the final capture file already exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let partial = partial_path(&options.output);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        let owner_pid = unsafe { libc::getpid() };
        let owner_tid = unsafe { libc::gettid() as u32 };
        let offset = write_file_header(&mut file, owner_pid)?;

        let core = Box::leak(Box::new(SessionCore {
            phase: AtomicU8::new(PHASE_ARMED),
            active_roots: AtomicUsize::new(0),
            writer_state: AtomicU32::new(WRITER_AWAKE),
            writer_done: AtomicBool::new(false),
            producers: Mutex::new(Vec::new()),
            active_producers: Mutex::new(Vec::new()),
            next_producer: AtomicU64::new(1),
            next_thread_generation: AtomicU32::new(1),
            page_pool: PagePool::new(options.page_budget_bytes),
            sink_loss: AtomicU64::new(0),
            owner_tid,
        }));
        let core_addr = core as *const SessionCore as usize;
        let final_path = options.output;
        let writer = match std::thread::Builder::new()
            .name("mirvm-capture".into())
            .spawn(move || {
                // SAFETY: SessionCore is intentionally process-lifetime stable.
                let core = unsafe { &*(core_addr as *const SessionCore) };
                writer_main(core, file, offset, &partial, &final_path)
            }) {
            Ok(writer) => writer,
            Err(error) => {
                core.phase.store(PHASE_FINISHED, Ordering::Release);
                return Err(error);
            }
        };
        ACTIVE.store(core, Ordering::Release);
        Ok(Self {
            core,
            writer: Some(writer),
            owner_pid,
        })
    }

    pub(crate) fn request_stop(&self) {
        if unsafe { libc::getpid() } != self.owner_pid {
            return;
        }
        let _ = self.core.phase.compare_exchange(
            PHASE_ARMED,
            PHASE_STOPPING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.core.wake_writer();
    }

    pub(crate) fn finish(&mut self, timeout: Duration) -> io::Result<FinishStatus> {
        if unsafe { libc::getpid() } != self.owner_pid {
            return Ok(FinishStatus::TimedOut);
        }
        self.request_stop();
        let started = Instant::now();
        while !self.core.writer_done.load(Ordering::Acquire) {
            if started.elapsed() >= timeout {
                return Ok(FinishStatus::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let Some(writer) = self.writer.take() else {
            return Err(io::Error::other("capture writer was already joined"));
        };
        let summary = writer
            .join()
            .map_err(|_| io::Error::other("capture writer panicked"))??;
        Ok(FinishStatus::Finished(summary))
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.request_stop();
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ActivationToken {
    producer: *mut Producer,
    session: *const SessionCore,
    outermost: bool,
    previous_engine: u64,
}

pub(crate) fn is_armed() -> bool {
    let core = ACTIVE.load(Ordering::Acquire);
    !core.is_null() && unsafe { (*core).phase.load(Ordering::Acquire) == PHASE_ARMED }
}

pub(crate) fn activation_enter(engine_id: u64) -> ActivationToken {
    let active = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
    if !active.is_null() {
        TLS_ACTIVATION_DEPTH.fetch_add(1, Ordering::Relaxed);
        let previous_engine = unsafe { (*(*active).cold_ptr()).current_engine };
        if previous_engine != engine_id {
            unsafe { set_engine(&*active, engine_id) };
        }
        return ActivationToken {
            producer: active,
            session: unsafe { (*active).session },
            outermost: false,
            previous_engine,
        };
    }

    let core = ACTIVE.load(Ordering::Acquire);
    if core.is_null() || !unsafe { (*core).try_enter_root() } {
        return ActivationToken {
            producer: ptr::null_mut(),
            session: ptr::null(),
            outermost: false,
            previous_engine: 0,
        };
    }

    let producer = producer_for_session(core, engine_id);
    TLS_ACTIVE_PRODUCER.store(producer, Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(1, Ordering::Relaxed);
    if !producer.is_null() {
        unsafe {
            let producer = &*producer;
            (*producer.cold_ptr()).current_engine = engine_id;
            let _ = open_page(producer);
        }
    }
    ActivationToken {
        producer,
        session: core,
        outermost: true,
        previous_engine: 0,
    }
}

pub(crate) fn activation_exit(token: ActivationToken, restored_engine_id: u64) {
    if token.session.is_null() {
        return;
    }
    // A fork child cleared ACTIVE before returning to trace code. Stack guards
    // copied from the parent must never seal or account into that generation.
    if ACTIVE.load(Ordering::Acquire) != token.session.cast_mut() {
        return;
    }
    // SYS_exit/SYS_exit_group transfer the root ledger before entering the
    // kernel because no Rust guard will run on their successful path. A
    // seccomp policy may unexpectedly make such a syscall return; the copied
    // guard must then remain inert instead of decrementing the root twice.
    if TLS_ACTIVATION_DEPTH.load(Ordering::Relaxed) == 0 {
        return;
    }
    if token.outermost {
        if !token.producer.is_null() {
            unsafe { seal_page(&*token.producer) };
        }
        TLS_ACTIVE_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
        TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
        let core = unsafe { &*token.session };
        core.active_roots.fetch_sub(1, Ordering::Release);
        core.wake_writer();
        return;
    }

    TLS_ACTIVATION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    debug_assert_eq!(token.previous_engine, restored_engine_id);
    if !token.producer.is_null() {
        unsafe { set_engine(&*token.producer, restored_engine_id) };
    }
}

/// Record the libc-semantics HostSyscall pair. With no active producer this is
/// exactly the original syscall path; stopped trace-capable Engines therefore
/// remain transparent.
pub(crate) fn host_syscall(nr: i64, args: &[u64]) -> i64 {
    // A real rt_sigreturn site belongs to the kernel signal frame and may not
    // touch the ordinary per-pthread page. The raw-site implementation in 1B
    // enforces the same bypass before it reaches this libc-oriented helper.
    if nr == libc::SYS_rt_sigreturn {
        return crate::os::process::syscall(nr, args);
    }
    let producer = TLS_ACTIVE_PRODUCER.load(Ordering::Relaxed);
    if producer.is_null() {
        let result = crate::os::process::syscall(nr, args);
        if nr == libc::SYS_fork && result == 0 {
            after_fork_child();
        }
        return result;
    }

    let disposition = unsafe { record_syscall_enter(&*producer, nr, args) };
    if nr == libc::SYS_exit || nr == libc::SYS_exit_group {
        unsafe { prepare_nonreturning_syscall(&*producer) };
        return crate::os::process::syscall(nr, args);
    }
    let result = crate::os::process::syscall(nr, args);
    if nr == libc::SYS_fork && result == 0 {
        after_fork_child();
        return result;
    }
    let errno = if result == -1 {
        unsafe { *((*(*producer).fast_ptr()).errno_ptr as *const i32) }
    } else {
        0
    };
    unsafe { record_syscall_exit(&*producer, disposition, result, errno) };
    result
}

unsafe fn prepare_nonreturning_syscall(producer: &Producer) {
    unsafe { seal_page(producer) };
    TLS_ACTIVE_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_SESSION.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
    producer.retired.retired.store(true, Ordering::Release);

    let core = unsafe { &*producer.session };
    let previous = core.active_roots.fetch_sub(1, Ordering::Release);
    if previous == 0 {
        std::process::abort();
    }
    // If the session owner itself leaves through SYS_exit, no handle remains
    // to request a stop. Disarm new roots so the writer can finish once any
    // other already-active roots return.
    if producer.tid == core.owner_tid {
        let _ = core.phase.compare_exchange(
            PHASE_ARMED,
            PHASE_STOPPING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    core.wake_writer();
}

pub(crate) fn retire_current_thread() {
    let active = TLS_ACTIVE_PRODUCER.swap(ptr::null_mut(), Ordering::Relaxed);
    let cached = TLS_CACHED_PRODUCER.swap(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_SESSION.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
    let producer = if !active.is_null() { active } else { cached };
    if producer.is_null() {
        return;
    }
    unsafe {
        // A cached producer is already quiescent: outer activation exit sealed
        // it before clearing TLS_ACTIVE_PRODUCER. Do not touch its cold ledger
        // here, because a stopping writer may already be summarizing it.
        if !active.is_null() {
            seal_page(&*producer);
        }
        (*producer).retired.retired.store(true, Ordering::Release);
        (&*(*producer).session).wake_writer();
    }
}

/// The child has no copy of the writer thread. Only stable TLS/global atomic
/// stores are permitted here; inherited locks, pages and file handles remain
/// unreachable until an ordinary boundary creates a new process generation.
pub(crate) fn after_fork_child() {
    ACTIVE.store(ptr::null_mut(), Ordering::Release);
    TLS_ACTIVE_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_PRODUCER.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_CACHED_SESSION.store(ptr::null_mut(), Ordering::Relaxed);
    TLS_ACTIVATION_DEPTH.store(0, Ordering::Relaxed);
}

fn producer_for_session(core: *mut SessionCore, engine_id: u64) -> *mut Producer {
    let cached_session = TLS_CACHED_SESSION.load(Ordering::Relaxed);
    let cached = TLS_CACHED_PRODUCER.load(Ordering::Relaxed);
    if cached_session == core && !cached.is_null() {
        return cached;
    }

    let errno_ptr = unsafe { libc::__errno_location() };
    let saved_errno = unsafe { *errno_ptr };
    let core_ref = unsafe { &*core };
    let pages = core_ref.page_pool.take_starter();
    let producer_id = core_ref.next_producer.fetch_add(1, Ordering::Relaxed);
    let thread_generation = core_ref
        .next_thread_generation
        .fetch_add(1, Ordering::Relaxed);
    let tid = unsafe { libc::gettid() as u32 };
    let producer = Box::into_raw(Box::new(Producer::new(
        core,
        pages,
        producer_id,
        thread_generation,
        tid,
        engine_id,
        errno_ptr,
    )));
    core_ref
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(producer as usize);
    core_ref
        .active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(producer as usize);
    TLS_CACHED_SESSION.store(core, Ordering::Relaxed);
    TLS_CACHED_PRODUCER.store(producer, Ordering::Relaxed);
    // Allocation, registry locking and gettid are cold attach work, but none
    // may perturb the guest-visible libc errno value.
    unsafe { *errno_ptr = saved_errno };
    producer
}

unsafe fn open_page(producer: &Producer) -> bool {
    if producer.pages.load(Ordering::Acquire).is_null() {
        return false;
    }
    let cold = unsafe { &mut *producer.cold_ptr() };
    if cold.has_active {
        return true;
    }
    let returned = producer.returned.head.load(Ordering::Acquire);
    if cold.next_publish.wrapping_sub(returned) >= 2 {
        return false;
    }
    let slot = (cold.next_publish & 1) as usize;
    let page = unsafe { producer.page(slot) };
    let base = page.bytes.get().cast::<u8>() as usize;
    let fast = unsafe { &mut *producer.fast_ptr() };
    fast.cursor = (base + PAGE_HEADER_BYTES) as u64;
    fast.pair_budget = ((PAGE_BYTES - PAGE_HEADER_BYTES) / SYSCALL_PAIR_BYTES) as u64;
    cold.active_page = page as *const Page as *mut Page;
    cold.has_active = true;
    cold.context_unsynced = false;
    cold.page_first_sequence = cold.next_sequence;
    cold.initial_engine = cold.current_engine;
    cold.marker_count = 0;
    true
}

unsafe fn seal_page(producer: &Producer) {
    let cold = unsafe { &mut *producer.cold_ptr() };
    if !cold.has_active {
        return;
    }
    let page = unsafe { producer.active_page() };
    let base = page.bytes.get().cast::<u8>() as usize;
    let cursor = unsafe { (*producer.fast_ptr()).cursor as usize };
    let used = cursor.saturating_sub(base + PAGE_HEADER_BYTES);
    if used == 0 {
        cold.has_active = false;
        cold.active_page = ptr::null_mut();
        unsafe {
            (*producer.fast_ptr()).cursor = 0;
            (*producer.fast_ptr()).pair_budget = 0;
        }
        return;
    }

    let encoded_header = PageHeader {
        producer_id: producer.producer_id,
        first_sequence: cold.page_first_sequence,
        next_sequence: cold.next_sequence,
        initial_engine_id: cold.initial_engine,
        page_ordinal: cold.page_ordinal,
        thread_generation: producer.thread_generation,
        os_tid: producer.tid,
        page_bytes: PAGE_BYTES_4K,
        used_bytes: used as u32,
    }
    .to_le_bytes();
    let Ok(encoded_header) = encoded_header else {
        // All fields above are bounded by the fixed 4 KiB page protocol.
        std::process::abort();
    };
    (unsafe { &mut *page.bytes.get() })[..PAGE_HEADER_BYTES].copy_from_slice(&encoded_header);

    cold.has_active = false;
    cold.active_page = ptr::null_mut();
    cold.page_ordinal = cold.page_ordinal.wrapping_add(1);
    cold.next_publish = cold.next_publish.wrapping_add(1);
    unsafe {
        (*producer.fast_ptr()).cursor = 0;
        (*producer.fast_ptr()).pair_budget = 0;
    }
    producer
        .published
        .tail
        .store(cold.next_publish, Ordering::Release);
    unsafe { (&*producer.session).wake_writer() };
}

unsafe fn set_engine(producer: &Producer, engine_id: u64) {
    {
        let cold = unsafe { &mut *producer.cold_ptr() };
        if cold.current_engine == engine_id && !cold.context_unsynced {
            return;
        }
        cold.current_engine = engine_id;
    }

    if !unsafe { (&*producer.cold.get()).has_active } {
        if unsafe { !open_page(producer) } {
            unsafe { mark_context_drop(producer) };
        }
        // A new page carries the new Engine in its header, so no marker is
        // emitted in either the success or page-less case.
        return;
    }

    let page = unsafe { producer.active_page() };
    let base = page.bytes.get().cast::<u8>() as usize;
    let cursor = unsafe { (*producer.fast_ptr()).cursor as usize };
    if PAGE_BYTES - (cursor - base) < ENGINE_CONTEXT_BYTES {
        unsafe { seal_page(producer) };
        if unsafe { !open_page(producer) } {
            unsafe { mark_context_drop(producer) };
        }
        return;
    }

    let bytes = unsafe { &mut *page.bytes.get() };
    bytes[cursor - base..cursor - base + ENGINE_CONTEXT_BYTES]
        .copy_from_slice(&EngineContext { engine_id }.to_le_bytes());
    let fast = unsafe { &mut *producer.fast_ptr() };
    fast.cursor += ENGINE_CONTEXT_BYTES as u64;
    fast.pair_budget = ((base + PAGE_BYTES - fast.cursor as usize) / SYSCALL_PAIR_BYTES) as u64;
    let cold = unsafe { &mut *producer.cold_ptr() };
    cold.marker_count = cold.marker_count.wrapping_add(1);
    cold.next_sequence = cold.next_sequence.wrapping_add(1);
    cold.context_unsynced = false;
}

unsafe fn mark_context_drop(producer: &Producer) {
    let cold = unsafe { &mut *producer.cold_ptr() };
    cold.context_unsynced = true;
    cold.context_drops = cold.context_drops.wrapping_add(1);
    cold.next_sequence = cold.next_sequence.wrapping_add(1);
}

#[derive(Clone, Copy)]
enum EnterDisposition {
    Recorded,
    DropCapacity,
    DropContext,
}

unsafe fn record_syscall_enter(producer: &Producer, nr: i64, args: &[u64]) -> EnterDisposition {
    let was_context_unsynced = unsafe { (&*producer.cold.get()).context_unsynced };
    let needs_page = {
        let cold = unsafe { &mut *producer.cold_ptr() };
        cold.context_unsynced
            || !cold.has_active
            || unsafe { (*producer.fast_ptr()).pair_budget == 0 }
    };
    if needs_page {
        let has_active = unsafe { (*producer.cold_ptr()).has_active };
        if has_active {
            unsafe { seal_page(producer) };
        }
        if unsafe { !open_page(producer) } {
            let cold = unsafe { &mut *producer.cold_ptr() };
            if was_context_unsynced {
                cold.context_drops = cold.context_drops.wrapping_add(1);
            } else {
                cold.capacity_drops = cold.capacity_drops.wrapping_add(1);
            }
            cold.next_sequence = cold.next_sequence.wrapping_add(1);
            return if was_context_unsynced {
                EnterDisposition::DropContext
            } else {
                EnterDisposition::DropCapacity
            };
        }
    }

    let cold = unsafe { &mut *producer.cold_ptr() };
    if cold.context_unsynced {
        cold.context_drops = cold.context_drops.wrapping_add(1);
        cold.next_sequence = cold.next_sequence.wrapping_add(1);
        return EnterDisposition::DropContext;
    }
    let fast = unsafe { &mut *producer.fast_ptr() };
    if fast.pair_budget == 0 {
        cold.capacity_drops = cold.capacity_drops.wrapping_add(1);
        cold.next_sequence = cold.next_sequence.wrapping_add(1);
        return EnterDisposition::DropCapacity;
    }
    fast.pair_budget -= 1;
    let page = unsafe { producer.active_page() };
    let base = page.bytes.get().cast::<u8>() as usize;
    let offset = fast.cursor as usize - base;
    let mut encoded_args = [0_u64; 6];
    for (i, encoded) in encoded_args.iter_mut().enumerate() {
        *encoded = args.get(i).copied().unwrap_or(0);
    }
    let encoded = SyscallEnter {
        semantics: SyscallSemantics::Libc,
        nr,
        args: encoded_args,
    }
    .to_le_bytes();
    let bytes = unsafe { &mut *page.bytes.get() };
    bytes[offset..offset + SYSCALL_ENTER_BYTES].copy_from_slice(&encoded);
    fast.cursor += SYSCALL_ENTER_BYTES as u64;
    cold.next_sequence = cold.next_sequence.wrapping_add(1);
    EnterDisposition::Recorded
}

unsafe fn record_syscall_exit(
    producer: &Producer,
    disposition: EnterDisposition,
    result: i64,
    errno: i32,
) {
    let cold = unsafe { &mut *producer.cold_ptr() };
    match disposition {
        EnterDisposition::DropCapacity => {
            cold.capacity_drops = cold.capacity_drops.wrapping_add(1);
            cold.next_sequence = cold.next_sequence.wrapping_add(1);
            return;
        }
        EnterDisposition::DropContext => {
            cold.context_drops = cold.context_drops.wrapping_add(1);
            cold.next_sequence = cold.next_sequence.wrapping_add(1);
            return;
        }
        EnterDisposition::Recorded => {}
    }
    let fast = unsafe { &mut *producer.fast_ptr() };
    let page = unsafe { producer.active_page() };
    let base = page.bytes.get().cast::<u8>() as usize;
    let offset = fast.cursor as usize - base;
    let encoded = SyscallExit {
        semantics: SyscallSemantics::Libc,
        result,
        errno: (result == -1).then_some(errno as u32),
    }
    .to_le_bytes();
    let Ok(encoded) = encoded else {
        std::process::abort();
    };
    let bytes = unsafe { &mut *page.bytes.get() };
    bytes[offset..offset + SYSCALL_EXIT_BYTES].copy_from_slice(&encoded);
    fast.cursor += SYSCALL_EXIT_BYTES as u64;
    cold.next_sequence = cold.next_sequence.wrapping_add(1);
}

fn writer_main(
    core: &'static SessionCore,
    file: File,
    mut offset: i64,
    partial_path: &Path,
    final_path: &Path,
) -> io::Result<CaptureSummary> {
    let fd = file.as_raw_fd();
    let mut sink_error: Option<io::Error> = None;
    let mut stats = WriterStats::default();
    loop {
        let mut did_work = false;
        let producers = core
            .active_producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for producer_addr in producers {
            let producer = unsafe { &*(producer_addr as *const Producer) };
            if writer_has_page(producer) {
                did_work = true;
                let page = writer_page(producer);
                let bytes = sealed_page_bytes(page);
                let record_count = page_record_count(bytes);
                if sink_error.is_none() {
                    match pwrite_chunk(fd, &mut offset, stats.chunk_ordinal, bytes, 1, 1) {
                        Ok(()) => {
                            unsafe {
                                *producer.writer.committed_records.get() =
                                    (*producer.writer.committed_records.get())
                                        .wrapping_add(record_count);
                            }
                            stats.chunk_ordinal = stats.chunk_ordinal.wrapping_add(1);
                            stats.chunks_committed = stats.chunks_committed.wrapping_add(1);
                            stats.pages_committed = stats.pages_committed.wrapping_add(1);
                        }
                        Err(error) => {
                            core.phase.store(PHASE_SINK_FAILED, Ordering::Release);
                            note_sink_loss(core, producer, record_count);
                            sink_error = Some(error);
                        }
                    }
                } else {
                    note_sink_loss(core, producer, record_count);
                }
                writer_return_page(producer);
            }
            if producer.pages.load(Ordering::Acquire).is_null()
                && writer_offer_starter(core, producer)
            {
                did_work = true;
            }
        }
        if writer_reap_retired(core) {
            did_work = true;
        }

        let stopping = core.phase.load(Ordering::Acquire) != PHASE_ARMED;
        if stopping && core.active_roots.load(Ordering::Acquire) == 0 && !any_published(core) {
            break;
        }
        if did_work {
            continue;
        }

        core.writer_state.swap(WRITER_SLEEPING, Ordering::AcqRel);
        if any_published(core)
            || (core.phase.load(Ordering::Acquire) != PHASE_ARMED
                && core.active_roots.load(Ordering::Acquire) == 0)
        {
            core.writer_state.store(WRITER_AWAKE, Ordering::Release);
            continue;
        }
        let _ = crate::os::thread::futex_wait_raw(core.writer_state.as_ptr(), WRITER_SLEEPING);
        core.writer_state.store(WRITER_AWAKE, Ordering::Release);
    }

    reclaim_session_pages(core);
    let summary = summarize(core);
    let result = if let Some(error) = sink_error {
        Err(error)
    } else {
        write_end_chunk(core, fd, &mut offset, &stats, summary)
            .and_then(|()| {
                drop(file);
                publish_without_replace(partial_path, final_path)
            })
            .map(|()| summary)
    };
    core.phase.store(PHASE_FINISHED, Ordering::Release);
    let core_ptr = core as *const SessionCore as *mut SessionCore;
    let _ = ACTIVE.compare_exchange(
        core_ptr,
        ptr::null_mut(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    core.writer_done.store(true, Ordering::Release);
    result
}

fn reclaim_session_pages(core: &SessionCore) {
    debug_assert_eq!(core.active_roots.load(Ordering::Acquire), 0);
    let producers = core
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for addr in producers {
        let producer = unsafe { &*(addr as *const Producer) };
        debug_assert!(!unsafe { (&*producer.cold.get()).has_active });
        debug_assert!(!writer_has_page(producer));
        let pages = producer.pages.swap(ptr::null_mut(), Ordering::AcqRel);
        core.page_pool.return_starter(pages);
    }
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    unsafe { core.page_pool.release_all() };
}

#[derive(Default)]
struct WriterStats {
    chunk_ordinal: u64,
    chunks_committed: u64,
    pages_committed: u64,
}

fn note_sink_loss(core: &SessionCore, producer: &Producer, records: u64) {
    core.sink_loss.fetch_add(records, Ordering::Relaxed);
    unsafe {
        *producer.writer.sink_loss.get() = (*producer.writer.sink_loss.get()).wrapping_add(records);
    }
}

fn writer_has_page(producer: &Producer) -> bool {
    let head = unsafe { *producer.writer.head.get() };
    head != producer.published.tail.load(Ordering::Acquire)
}

fn writer_page(producer: &Producer) -> &Page {
    let head = unsafe { *producer.writer.head.get() };
    unsafe { producer.page((head & 1) as usize) }
}

fn writer_return_page(producer: &Producer) {
    let head = unsafe { &mut *producer.writer.head.get() };
    *head = head.wrapping_add(1);
    producer.returned.head.store(*head, Ordering::Release);
}

fn writer_offer_starter(core: &SessionCore, producer: &Producer) -> bool {
    if producer.retired.retired.load(Ordering::Acquire) {
        return false;
    }
    let pages = core.page_pool.take_starter();
    if pages.is_null() {
        return false;
    }
    if producer.retired.retired.load(Ordering::Acquire) {
        core.page_pool.return_starter(pages);
        return false;
    }
    if producer
        .pages
        .compare_exchange(ptr::null_mut(), pages, Ordering::Release, Ordering::Acquire)
        .is_err()
    {
        core.page_pool.return_starter(pages);
        return false;
    }
    true
}

fn writer_reap_retired(core: &SessionCore) -> bool {
    let mut removed = false;
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|addr| {
            let producer = unsafe { &*(*addr as *const Producer) };
            if !producer.retired.retired.load(Ordering::Acquire) || writer_has_page(producer) {
                return true;
            }
            let pages = producer.pages.swap(ptr::null_mut(), Ordering::AcqRel);
            core.page_pool.return_starter(pages);
            removed = true;
            false
        });
    removed
}

fn any_published(core: &SessionCore) -> bool {
    core.active_producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .any(|addr| writer_has_page(unsafe { &*(addr as *const Producer) }))
}

fn sealed_page_bytes(page: &Page) -> &[u8] {
    let bytes = unsafe { &*page.bytes.get() };
    let used = u32::from_le_bytes(bytes[60..64].try_into().unwrap()) as usize;
    &bytes[..PAGE_HEADER_BYTES + used]
}

fn page_record_count(bytes: &[u8]) -> u64 {
    if bytes.len() < PAGE_HEADER_BYTES {
        return 0;
    }
    let first = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let next = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
    next.wrapping_sub(first)
}

fn summarize(core: &SessionCore) -> CaptureSummary {
    let producers = core
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut summary = CaptureSummary {
        producers: producers.len() as u64,
        sink_loss: core.sink_loss.load(Ordering::Relaxed),
        ..CaptureSummary::default()
    };
    for addr in producers {
        let producer = unsafe { &*(addr as *const Producer) };
        // No trace root remains, so the producer-only ledger is quiescent.
        let cold = unsafe { &*producer.cold.get() };
        let drops = cold
            .capacity_drops
            .wrapping_add(cold.context_drops)
            .wrapping_add(cold.recursive_drops);
        summary.producer_drops = summary.producer_drops.wrapping_add(drops);
        summary.encoded_records = summary
            .encoded_records
            .wrapping_add(cold.next_sequence.wrapping_sub(drops));
    }
    summary
}

fn pwrite_chunk(
    fd: i32,
    offset: &mut i64,
    chunk_ordinal: u64,
    payload: &[u8],
    block_count: u32,
    page_count: u32,
) -> io::Result<()> {
    let header = ChunkHeader::new(chunk_ordinal, payload.len() as u64, block_count, page_count)
        .map_err(wire_io)?;
    let mut encoded_header = header.to_le_bytes();
    let footer = ChunkFooter::for_slices(&header, &encoded_header, [payload]).map_err(wire_io)?;
    let mut encoded_footer = footer.to_le_bytes();
    let mut iov = [
        libc::iovec {
            iov_base: encoded_header.as_mut_ptr().cast(),
            iov_len: CHUNK_HEADER_BYTES,
        },
        libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        },
        libc::iovec {
            iov_base: encoded_footer.as_mut_ptr().cast(),
            iov_len: CHUNK_FOOTER_BYTES,
        },
    ];
    pwritev_all(fd, &mut iov, offset)
}

fn pwritev_all(fd: i32, iov: &mut [libc::iovec], offset: &mut i64) -> io::Result<()> {
    let mut first = 0;
    while first < iov.len() {
        let count = iov.len() - first;
        let written = unsafe { libc::pwritev(fd, iov[first..].as_ptr(), count as i32, *offset) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "capture pwritev made no progress",
            ));
        }
        *offset = offset
            .checked_add(written as i64)
            .ok_or_else(|| io::Error::other("capture file offset overflow"))?;
        let mut remaining = written as usize;
        while remaining != 0 {
            if remaining >= iov[first].iov_len {
                remaining -= iov[first].iov_len;
                first += 1;
            } else {
                iov[first].iov_base = unsafe {
                    (iov[first].iov_base as *mut u8)
                        .add(remaining)
                        .cast::<libc::c_void>()
                };
                iov[first].iov_len -= remaining;
                remaining = 0;
            }
        }
    }
    Ok(())
}

fn partial_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.as_os_str().to_os_string();
    name.push(".partial");
    PathBuf::from(name)
}

fn publish_without_replace(partial_path: &Path, final_path: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let partial = CString::new(partial_path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "partial capture path contains a NUL byte",
        )
    })?;
    let final_path = CString::new(final_path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "final capture path contains a NUL byte",
        )
    })?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            partial.as_ptr(),
            libc::AT_FDCWD,
            final_path.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

fn write_file_header(file: &mut File, pid: libc::pid_t) -> io::Result<i64> {
    let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let mut session_id = [0_u8; 16];
    session_id[..8].copy_from_slice(&(pid as u64).to_le_bytes());
    session_id[8..].copy_from_slice(&sequence.to_le_bytes());
    let build_id = u64::from_str_radix(env!("MIRVM_BUILD_ID"), 16)
        .map_err(|error| io::Error::other(format!("invalid MIRVM_BUILD_ID: {error}")))?;
    let bytes = FileHeader {
        pointer_width: std::mem::size_of::<usize>() as u8,
        clock_kind: CLOCK_NONE,
        pid: pid as u32,
        session_id,
        build_id,
        process_generation: 0,
        segment_number: 0,
        monotonic_anchor: 0,
        wall_unix_ns: 0,
        clock_frequency_num: 0,
        clock_frequency_den: 0,
    }
    .to_le_bytes();
    file.write_all(&bytes)?;
    Ok(bytes.len() as i64)
}

fn write_end_chunk(
    core: &SessionCore,
    fd: i32,
    offset: &mut i64,
    stats: &WriterStats,
    summary: CaptureSummary,
) -> io::Result<()> {
    let producers = core
        .producers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let payload_len = producers
        .len()
        .checked_mul(PRODUCER_END_BYTES)
        .and_then(|bytes| bytes.checked_add(SESSION_END_BYTES))
        .ok_or_else(|| io::Error::other("capture end ledger length overflow"))?;
    let mut payload = Vec::with_capacity(payload_len);
    let mut drop_capacity = 0_u64;
    let mut drop_context = 0_u64;
    let mut drop_recursive = 0_u64;
    for addr in &producers {
        let producer = unsafe { &*(*addr as *const Producer) };
        let cold = unsafe { &*producer.cold.get() };
        let committed = unsafe { *producer.writer.committed_records.get() };
        let sink_loss = unsafe { *producer.writer.sink_loss.get() };
        let drops = cold
            .capacity_drops
            .checked_add(cold.context_drops)
            .and_then(|n| n.checked_add(cold.recursive_drops))
            .ok_or_else(|| io::Error::other("producer drop count overflow"))?;
        let encoded = cold
            .next_sequence
            .checked_sub(drops)
            .ok_or_else(|| io::Error::other("producer sequence ledger underflow"))?;
        let page_count = cold.page_ordinal;
        let encoded_end = ProducerEnd {
            producer_id: producer.producer_id,
            next_sequence: cold.next_sequence,
            attempted: cold.next_sequence,
            encoded,
            committed,
            drop_capacity: cold.capacity_drops,
            drop_context: cold.context_drops,
            drop_recursive: cold.recursive_drops,
            sink_loss,
            page_count,
            last_page_ordinal: page_count.checked_sub(1).unwrap_or(u64::MAX),
            thread_generation: producer.thread_generation,
            os_tid: producer.tid,
            flags: 0,
        }
        .to_le_bytes()
        .map_err(wire_io)?;
        payload.extend_from_slice(&encoded_end);
        drop_capacity = drop_capacity.wrapping_add(cold.capacity_drops);
        drop_context = drop_context.wrapping_add(cold.context_drops);
        drop_recursive = drop_recursive.wrapping_add(cold.recursive_drops);
    }

    let committed = summary
        .encoded_records
        .checked_sub(summary.sink_loss)
        .ok_or_else(|| io::Error::other("session sink ledger underflow"))?;
    let encoded_session_end = SessionEnd {
        producer_count: producers.len() as u64,
        attempted: summary
            .encoded_records
            .checked_add(summary.producer_drops)
            .ok_or_else(|| io::Error::other("session attempt count overflow"))?,
        encoded: summary.encoded_records,
        committed,
        drop_capacity,
        drop_context,
        drop_recursive,
        sink_loss: summary.sink_loss,
        chunks_committed: stats.chunks_committed.wrapping_add(1),
        pages_committed: stats.pages_committed,
        bytes_committed: (*offset as u64)
            .checked_add(CHUNK_HEADER_BYTES as u64)
            .and_then(|n| n.checked_add(payload_len as u64))
            .and_then(|n| n.checked_add(CHUNK_FOOTER_BYTES as u64))
            .ok_or_else(|| io::Error::other("session committed byte count overflow"))?,
        write_error_count: 0,
        first_write_errno: 0,
        last_write_errno: 0,
        flags: 0,
    }
    .to_le_bytes()
    .map_err(wire_io)?;
    payload.extend_from_slice(&encoded_session_end);
    debug_assert_eq!(payload.len(), payload_len);
    pwrite_chunk(
        fd,
        offset,
        stats.chunk_ordinal,
        &payload,
        (producers.len() + 1) as u32,
        0,
    )
}

fn wire_io(error: WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::decode::{DecodedKind, Health, decode_file};
    use crate::telemetry::format::{EngineContext, PageHeader};
    use std::ffi::c_void;
    use std::process::Command;
    use std::sync::atomic::AtomicU64;

    static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

    fn test_path() -> PathBuf {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mirvm-capture-{}-{id}.mlog", std::process::id()))
    }

    #[test]
    fn producer_fast_abi_is_one_cache_line() {
        assert_eq!(std::mem::size_of::<ProducerFast>(), 64);
        assert_eq!(std::mem::align_of::<ProducerFast>(), 64);
        assert_eq!(std::mem::offset_of!(ProducerFast, errno_ptr), 16);
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
        let test_name =
            "telemetry::capture::tests::session_publishes_drains_and_decodes_exact_file";
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
        let FinishStatus::Finished(summary) = no_pages.finish(Duration::from_secs(2)).unwrap()
        else {
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
        let first = EngineContext::decode(
            &bytes[PAGE_HEADER_BYTES..PAGE_HEADER_BYTES + ENGINE_CONTEXT_BYTES],
        )
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
        let first =
            PageHeader::decode(&unsafe { &*producer.page(0).bytes.get() }[..PAGE_HEADER_BYTES])
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
            let mut session =
                CaptureSession::start(StartOptions::new(&output, STARTER_BYTES)).unwrap();
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
                let recovered = offered
                    && unsafe { (*(*producer).cold_ptr()).capacity_drops == drops_after_first };
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
        let offset = write_file_header(&mut file, unsafe { libc::getpid() }).unwrap();
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
}
