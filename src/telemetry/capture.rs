//! Process-wide syscall capture.
//!
//! The event path owns one active page and publishes only sealed pages. The
//! writer owns published pages until their complete chunk has reached the
//! kernel. No writer field shares a cache line with [`ProducerFast`].
//!
//! This module holds the shared process state and the per-thread producer
//! primitives; `capture_session` owns the session lifecycle and `capture_writer`
//! the file writer.

use std::cell::UnsafeCell;
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

use super::format::{
    ENGINE_CONTEXT_BYTES, EngineContext, PAGE_BYTES_4K, PAGE_HEADER_BYTES, PageHeader,
    SYSCALL_ENTER_BYTES, SYSCALL_EXIT_BYTES, SYSCALL_PAIR_BYTES, SyscallEnter, SyscallExit,
    SyscallSemantics,
};

pub(crate) use super::capture_session::{
    ActivationToken, CaptureSession, FinishStatus, StartOptions, activation_enter, activation_exit,
    after_fork_child, claim_process_generation, current_producer, fork_child_guard, host_syscall,
    host_syscall_pinned, is_armed, rebuild_on_boundary, retire_current_thread,
};

const PAGE_BYTES: usize = PAGE_BYTES_4K as usize;
pub(crate) const STARTER_BYTES: usize = PAGE_BYTES * 2;

pub(super) const PHASE_ARMED: u8 = 1;
pub(super) const PHASE_STOPPING: u8 = 2;
pub(super) const PHASE_SINK_FAILED: u8 = 3;
pub(super) const PHASE_FINISHED: u8 = 4;

pub(super) const WRITER_AWAKE: u32 = 0;
pub(super) const WRITER_SLEEPING: u32 = 1;

pub(super) static ACTIVE: AtomicPtr<SessionCore> = AtomicPtr::new(ptr::null_mut());
pub(super) static START_LOCK: Mutex<()> = Mutex::new(());

#[thread_local]
pub(super) static TLS_ACTIVE_PRODUCER: AtomicPtr<Producer> = AtomicPtr::new(ptr::null_mut());
#[thread_local]
pub(super) static TLS_CACHED_PRODUCER: AtomicPtr<Producer> = AtomicPtr::new(ptr::null_mut());
#[thread_local]
pub(super) static TLS_CACHED_SESSION: AtomicPtr<SessionCore> = AtomicPtr::new(ptr::null_mut());
#[thread_local]
pub(super) static TLS_ACTIVATION_DEPTH: AtomicU32 = AtomicU32::new(0);

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
pub(super) struct Page {
    pub(super) bytes: UnsafeCell<[u8; PAGE_BYTES]>,
}

impl Page {
    fn new() -> Self {
        Self {
            bytes: UnsafeCell::new([0; PAGE_BYTES]),
        }
    }
}

unsafe impl Sync for Page {}

pub(super) struct PagePair {
    pages: [Page; 2],
}

impl PagePair {
    fn new() -> Self {
        Self {
            pages: [Page::new(), Page::new()],
        }
    }
}

pub(super) struct PagePool {
    budget_bytes: usize,
    allocated_bytes: AtomicUsize,
    free: Mutex<Vec<usize>>,
}

impl PagePool {
    pub(super) fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            allocated_bytes: AtomicUsize::new(0),
            free: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn take_starter(&self) -> *mut PagePair {
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

    pub(super) fn return_starter(&self, pages: *mut PagePair) {
        if pages.is_null() {
            return;
        }
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        free.push(pages as usize);
    }

    pub(super) unsafe fn release_all(&self) {
        let pages = std::mem::take(&mut *self.free.lock().unwrap_or_else(|e| e.into_inner()));
        let allocated = self.allocated_bytes.swap(0, Ordering::AcqRel);
        debug_assert_eq!(allocated, pages.len().saturating_mul(STARTER_BYTES));
        for addr in pages {
            unsafe { drop(Box::from_raw(addr as *mut PagePair)) };
        }
    }
}

#[repr(C, align(64))]
pub(super) struct ProducerCold {
    pub(super) has_active: bool,
    context_unsynced: bool,
    _pad0: [u8; 6],
    active_page: *mut Page,
    next_publish: u64,
    pub(super) next_sequence: u64,
    page_first_sequence: u64,
    pub(super) page_ordinal: u64,
    pub(super) current_engine: u64,
    initial_engine: u64,
    marker_count: u32,
    _pad1: u32,
    pub(super) capacity_drops: u64,
    pub(super) context_drops: u64,
    pub(super) recursive_drops: u64,
}

#[repr(C, align(64))]
pub(super) struct PublishedLine {
    pub(super) tail: AtomicU64,
    _pad: [u8; 56],
}

#[repr(C, align(64))]
pub(super) struct ReturnedLine {
    pub(super) head: AtomicU64,
    _pad: [u8; 56],
}

#[repr(C, align(64))]
pub(super) struct WriterLine {
    pub(super) head: UnsafeCell<u64>,
    pub(super) committed_records: UnsafeCell<u64>,
    pub(super) sink_loss: UnsafeCell<u64>,
    _pad: [u8; 40],
}

#[repr(C, align(64))]
pub(super) struct RetiredLine {
    pub(super) retired: AtomicBool,
    _pad: [u8; 63],
}

pub(crate) struct Producer {
    fast: UnsafeCell<ProducerFast>,
    pub(super) cold: UnsafeCell<ProducerCold>,
    pub(super) published: PublishedLine,
    pub(super) returned: ReturnedLine,
    pub(super) writer: WriterLine,
    pub(super) retired: RetiredLine,
    pub(super) session: *const SessionCore,
    pub(super) pages: AtomicPtr<PagePair>,
    pub(super) producer_id: u64,
    pub(super) thread_generation: u32,
    pub(super) tid: u32,
}

// Producer and writer access disjoint fields/pages according to the two SPSC
// indices. The release/acquire ownership transfers guard page byte access.
unsafe impl Sync for Producer {}
unsafe impl Send for Producer {}

impl Producer {
    pub(super) fn new(
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
    pub(super) fn cold_ptr(&self) -> *mut ProducerCold {
        self.cold.get()
    }

    #[inline]
    pub(super) fn fast_ptr(&self) -> *mut ProducerFast {
        self.fast.get()
    }

    #[inline]
    pub(super) unsafe fn page(&self, slot: usize) -> &Page {
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

pub(super) struct SessionCore {
    pub(super) phase: AtomicU8,
    pub(super) active_roots: AtomicUsize,
    pub(super) writer_state: AtomicU32,
    pub(super) writer_done: AtomicBool,
    pub(super) producers: Mutex<Vec<usize>>,
    pub(super) active_producers: Mutex<Vec<usize>>,
    pub(super) next_producer: AtomicU64,
    pub(super) next_thread_generation: AtomicU32,
    pub(super) page_pool: PagePool,
    pub(super) sink_loss: AtomicU64,
    pub(super) owner_tid: u32,
}

impl SessionCore {
    pub(super) fn wake_writer(&self) {
        if self.writer_state.swap(WRITER_AWAKE, Ordering::AcqRel) == WRITER_SLEEPING {
            let _ = crate::os::thread::futex_wake_one_raw(self.writer_state.as_ptr());
        }
    }

    pub(super) fn try_enter_root(&self) -> bool {
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CaptureSummary {
    pub(crate) encoded_records: u64,
    pub(crate) producer_drops: u64,
    pub(crate) sink_loss: u64,
    pub(crate) producers: u64,
}

pub(super) unsafe fn open_page(producer: &Producer) -> bool {
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

pub(super) unsafe fn seal_page(producer: &Producer) {
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

pub(super) unsafe fn set_engine(producer: &Producer, engine_id: u64) {
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
pub(super) enum EnterDisposition {
    Recorded,
    DropCapacity,
    DropContext,
}

/// Outcome of the inline syscall-entry hot path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HotEnter {
    /// The record was written into the current page.
    Recorded,
    /// The producer has no usable page or budget; the caller must take the cold
    /// slow path, which opens or seals pages and accounts the drop.
    NeedsColdPath,
}

/// Syscall-entry hot path for the trace code domain.
///
/// This is the body a trace JIT reaches through the pinned `r15`
/// `ProducerHot*`: it touches only the one-cache-line [`ProducerFast`] (cursor,
/// pair_budget) and the current page. It performs no TLS lookup, no global
/// session check and, for a healthy stream, no `ProducerCold` access at all --
/// the sequence counter is advanced here only because drops and published pages
/// both need it, and a healthy page seals with the value it already carries.
///
/// Returns [`HotEnter::NeedsColdPath`] whenever the caller must fall back:
/// no active page, no budget, or an unsynchronised context. The cold path owns
/// page rotation, drop accounting and sequence gap semantics, so the fast path
/// never has to reproduce them.
///
/// # Safety
///
/// `producer` must stay alive and owned by the calling thread for the duration
/// of the call, exactly like [`record_syscall_enter`].
pub(crate) unsafe fn record_syscall_enter_inline(
    producer: *mut Producer,
    nr: i64,
    args: &[u64],
) -> HotEnter {
    let cold_ptr = unsafe { (*producer).cold_ptr() };
    if unsafe { (*cold_ptr).context_unsynced } {
        return HotEnter::NeedsColdPath;
    }
    let fast = unsafe { &mut *(*producer).fast_ptr() };
    if fast.pair_budget == 0 {
        return HotEnter::NeedsColdPath;
    }
    let cold = unsafe { &mut *cold_ptr };
    if !cold.has_active {
        return HotEnter::NeedsColdPath;
    }
    fast.pair_budget -= 1;
    let page = unsafe { (*producer).active_page() };
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
    HotEnter::Recorded
}

pub(super) unsafe fn record_syscall_enter(
    producer: &Producer,
    nr: i64,
    args: &[u64],
) -> EnterDisposition {
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

pub(super) unsafe fn record_syscall_exit(
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

#[cfg(test)]
mod tests;
