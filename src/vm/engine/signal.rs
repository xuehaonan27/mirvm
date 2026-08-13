//! Process-wide signal dispositions with per-Engine deferred delivery.

use std::collections::{HashMap, HashSet};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

#[cfg(test)]
use std::sync::atomic::AtomicI32;

use super::ctx::EngineControl;
use super::ir::FuncId;
use crate::os::signal::Sigaction;

pub(crate) const SIGNAL_SLOTS: usize = (crate::os::signal::STANDARD_SIGNAL_MAX as usize) + 1;

const DELIVERY_ACTIVE: usize = 1usize << (usize::BITS - 1);
const DELIVERY_COUNT: usize = !DELIVERY_ACTIVE;

/// Code executed later at an ordinary VM safe point. Even handlers originating
/// in MIRVM-produced native images use this path: running them in the kernel
/// signal frame would let wrapped libc calls and P1 callbacks re-enter the VM
/// while it is interrupted at an arbitrary instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeferredSignalCallback {
    Guest(FuncId),
    ImageNative(usize),
}

/// Stable, process-lifetime metadata addressed directly by a signal stub.
///
/// Every guest installation gets a fresh registration. Both this allocation and
/// its executable stub are deliberately leaked: a kernel frame may already hold
/// the old handler address when another thread replaces or closes the Engine.
pub(crate) struct SignalRegistration {
    control: Arc<EngineControl>,
    callback: DeferredSignalCallback,
    action: Sigaction,
    signum: i32,
    mask_bits: u64,
    generation: u64,
    /// High bit permits new kernel deliveries; low bits count fixed-adapter
    /// frames that passed the gate. Ordinary safe-point handlers use an Engine
    /// lifecycle hold instead, so registry cleanup can wait here without
    /// deadlocking a handler that calls sigaction itself.
    kernel_delivery: AtomicUsize,
    #[cfg(test)]
    kernel_frames: AtomicUsize,
    /// Number of target-pthread slots that still contain this registration.
    /// Close reads this through the callback owner's registration list; only
    /// the target pthread may turn one of these counts into a lifecycle hold.
    thread_pending: AtomicUsize,
    pending: [AtomicBool; SIGNAL_SLOTS],
    next_owner: AtomicPtr<SignalRegistration>,
}

impl SignalRegistration {
    fn new(
        control: Arc<EngineControl>,
        callback: DeferredSignalCallback,
        signum: i32,
        action: Sigaction,
    ) -> &'static Self {
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        if generation == 0 {
            eprintln!("mirvm[m4-engine]: signal registration generation exhausted");
            std::process::abort();
        }
        let registration = Box::leak(Box::new(Self {
            control,
            callback,
            action,
            signum,
            mask_bits: action.standard_mask_bits(),
            generation,
            kernel_delivery: AtomicUsize::new(DELIVERY_ACTIVE),
            #[cfg(test)]
            kernel_frames: AtomicUsize::new(0),
            thread_pending: AtomicUsize::new(0),
            pending: [const { AtomicBool::new(false) }; SIGNAL_SLOTS],
            next_owner: AtomicPtr::new(ptr::null_mut()),
        }));
        register_thread_signal_registration(registration);
        registration
    }

    pub(crate) fn control(&self) -> &Arc<EngineControl> {
        &self.control
    }

    pub(crate) fn callback(&self) -> DeferredSignalCallback {
        self.callback
    }

    pub(crate) fn action(&self) -> Sigaction {
        self.action
    }

    fn signum(&self) -> i32 {
        self.signum
    }

    pub(crate) fn mask_bits(&self) -> u64 {
        self.mask_bits
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn defer(&self, signum: i32) {
        if signum <= 0 || signum as usize >= SIGNAL_SLOTS {
            unsafe { libc::_exit(70) }
        }
        self.publish(signum);
    }

    fn try_kernel_delivery(&'static self) -> Option<KernelDeliveryGuard> {
        let mut state = self.kernel_delivery.load(Ordering::Acquire);
        loop {
            if state & DELIVERY_ACTIVE == 0 {
                return None;
            }
            if state & DELIVERY_COUNT == DELIVERY_COUNT {
                unsafe { libc::_exit(70) }
            }
            match self.kernel_delivery.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(KernelDeliveryGuard { registration: self }),
                Err(observed) => state = observed,
            }
        }
    }

    fn accepts_kernel_delivery(&self) -> bool {
        self.kernel_delivery.load(Ordering::Acquire) & DELIVERY_ACTIVE != 0
    }

    #[cfg(test)]
    fn safe_point_delivery(&'static self) -> Option<SignalDeliveryGuard> {
        if self.kernel_delivery.load(Ordering::Acquire) & DELIVERY_ACTIVE == 0 {
            return None;
        }
        let hold = super::ctx::DeferredHold::acquire(&self.control, true).ok()?;
        if self.kernel_delivery.load(Ordering::Acquire) & DELIVERY_ACTIVE == 0 {
            drop(hold);
            return None;
        }
        Some(SignalDeliveryGuard {
            registration: self,
            _hold: hold,
        })
    }

    fn deactivate(&self) {
        self.kernel_delivery
            .fetch_and(DELIVERY_COUNT, Ordering::AcqRel);
    }

    fn wait_for_kernel_deliveries(&self) {
        let mut spins = 0usize;
        while self.kernel_delivery.load(Ordering::Acquire) & DELIVERY_COUNT != 0 {
            if spins < 64 {
                std::hint::spin_loop();
                spins += 1;
            } else {
                std::thread::yield_now();
            }
        }
    }

    fn publish(&self, signum: i32) {
        // Traditional signals coalesce while pending. Realtime signals are
        // rejected at installation because they require a real event queue.
        unsafe { self.pending.get_unchecked(signum as usize) }.store(true, Ordering::Release);
    }

    fn publish_thread_directed(&'static self, inbox: &'static ThreadSignalInbox, _signum: i32) {
        let registration = ptr::from_ref(self).cast_mut();
        let Some(cell) = inbox.cell_for(registration) else {
            unsafe { libc::_exit(70) }
        };
        if cell
            .pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && self.thread_pending.fetch_add(1, Ordering::Release) == usize::MAX
        {
            unsafe { libc::_exit(70) }
        }
    }
}

struct KernelDeliveryGuard {
    registration: &'static SignalRegistration,
}

impl Drop for KernelDeliveryGuard {
    fn drop(&mut self) {
        self.registration
            .kernel_delivery
            .fetch_sub(1, Ordering::Release);
    }
}

/// Process-lifetime target-pthread mailbox. Its address is published through
/// native ELF TLS before a pthread enters MIRVM and is never freed, so a fixed
/// signal adapter can reach it without pthread APIs, allocation, or locks.
struct ThreadSignalCell {
    registration: AtomicPtr<SignalRegistration>,
    pending: AtomicBool,
    next: AtomicPtr<ThreadSignalCell>,
}

struct ThreadSignalInbox {
    delivery: AtomicUsize,
    cells: AtomicPtr<ThreadSignalCell>,
}

impl ThreadSignalInbox {
    fn new() -> Self {
        Self {
            delivery: AtomicUsize::new(DELIVERY_ACTIVE),
            cells: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[inline(always)]
    fn try_delivery(&'static self) -> Option<ThreadInboxDeliveryGuard> {
        let mut state = self.delivery.load(Ordering::Acquire);
        loop {
            if state & DELIVERY_ACTIVE == 0 {
                return None;
            }
            if state & DELIVERY_COUNT == DELIVERY_COUNT {
                unsafe { libc::_exit(70) }
            }
            match self.delivery.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(ThreadInboxDeliveryGuard { inbox: self }),
                Err(observed) => state = observed,
            }
        }
    }

    #[inline(always)]
    fn cell_for(
        &'static self,
        registration: *mut SignalRegistration,
    ) -> Option<&'static ThreadSignalCell> {
        let mut cell = self.cells.load(Ordering::Acquire);
        while !cell.is_null() {
            let current = unsafe { &*cell };
            if current.registration.load(Ordering::Relaxed) == registration {
                return Some(current);
            }
            cell = current.next.load(Ordering::Acquire);
        }
        None
    }

    fn deactivate(&self) {
        self.delivery.fetch_and(DELIVERY_COUNT, Ordering::AcqRel);
    }

    fn wait_for_deliveries(&self) {
        let mut spins = 0usize;
        while self.delivery.load(Ordering::Acquire) & DELIVERY_COUNT != 0 {
            if spins < 64 {
                std::hint::spin_loop();
                spins += 1;
            } else {
                std::thread::yield_now();
            }
        }
    }

    fn has_pending(&self) -> bool {
        let mut cell = self.cells.load(Ordering::Acquire);
        while !cell.is_null() {
            let current = unsafe { &*cell };
            if current.pending.load(Ordering::Acquire) {
                return true;
            }
            cell = current.next.load(Ordering::Acquire);
        }
        false
    }
}

#[derive(Default)]
struct ThreadCellRegistry {
    inboxes: Vec<&'static ThreadSignalInbox>,
    registrations: Vec<&'static SignalRegistration>,
}

static THREAD_CELLS: LazyLock<Mutex<ThreadCellRegistry>> =
    LazyLock::new(|| Mutex::new(ThreadCellRegistry::default()));

fn link_thread_signal_cell(
    inbox: &'static ThreadSignalInbox,
    registration: &'static SignalRegistration,
) {
    let mut head = inbox.cells.load(Ordering::Acquire);
    let cell = Box::leak(Box::new(ThreadSignalCell {
        registration: AtomicPtr::new(ptr::from_ref(registration).cast_mut()),
        pending: AtomicBool::new(false),
        next: AtomicPtr::new(head),
    }));
    let cell = ptr::from_mut(cell);
    loop {
        unsafe { (*cell).next.store(head, Ordering::Relaxed) };
        match inbox
            .cells
            .compare_exchange_weak(head, cell, Ordering::Release, Ordering::Acquire)
        {
            Ok(_) => return,
            Err(observed) => head = observed,
        }
    }
}

fn register_thread_signal_registration(registration: &'static SignalRegistration) {
    let mut cells = THREAD_CELLS.lock().unwrap();
    for &inbox in &cells.inboxes {
        if inbox.delivery.load(Ordering::Acquire) & DELIVERY_ACTIVE != 0 {
            link_thread_signal_cell(inbox, registration);
        }
    }
    cells.registrations.push(registration);
}

struct ThreadInboxDeliveryGuard {
    inbox: &'static ThreadSignalInbox,
}

impl Drop for ThreadInboxDeliveryGuard {
    fn drop(&mut self) {
        self.inbox.delivery.fetch_sub(1, Ordering::Release);
    }
}

#[thread_local]
static THREAD_SIGNAL_INBOX: AtomicPtr<ThreadSignalInbox> = AtomicPtr::new(ptr::null_mut());

const HOST_RAISE_RETRY: u64 = 1 << 63;
const HOST_RAISE_SIGNUM_MASK: u64 = u32::MAX as u64;

// Packed so an interrupt can never observe a new signum with an old outcome.
// This is only a retry landing pad for the close/stale-stub race; it never
// authenticates an event or distinguishes raise from pthread_kill.
#[thread_local]
static HOST_RAISE_STATE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct HostRaiseAttempt {
    previous: u64,
    finished: bool,
}

impl HostRaiseAttempt {
    pub(crate) fn begin(signum: i32) -> Self {
        let state = signum as u32 as u64;
        let previous = HOST_RAISE_STATE.swap(state, Ordering::AcqRel);
        Self {
            previous,
            finished: false,
        }
    }

    pub(crate) fn finish(mut self) -> bool {
        let state = HOST_RAISE_STATE.swap(self.previous, Ordering::AcqRel);
        self.finished = true;
        state & HOST_RAISE_RETRY != 0
    }
}

impl Drop for HostRaiseAttempt {
    fn drop(&mut self) {
        if !self.finished {
            HOST_RAISE_STATE.store(self.previous, Ordering::Release);
        }
    }
}

#[inline(always)]
fn retry_current_host_raise(signum: i32) -> bool {
    let mut state = HOST_RAISE_STATE.load(Ordering::Acquire);
    loop {
        if state & HOST_RAISE_SIGNUM_MASK != signum as u32 as u64 {
            return false;
        }
        if state & HOST_RAISE_RETRY != 0 {
            return true;
        }
        match HOST_RAISE_STATE.compare_exchange_weak(
            state,
            state | HOST_RAISE_RETRY,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => state = observed,
        }
    }
}

pub(crate) fn initialize_current_thread_inbox() {
    if !THREAD_SIGNAL_INBOX.load(Ordering::Acquire).is_null() {
        return;
    }
    let inbox = Box::into_raw(Box::new(ThreadSignalInbox::new()));
    let stable_inbox = unsafe { &*inbox };
    let mut cells = THREAD_CELLS.lock().unwrap();
    for &registration in &cells.registrations {
        if registration.accepts_kernel_delivery() {
            link_thread_signal_cell(stable_inbox, registration);
        }
    }
    cells.inboxes.push(stable_inbox);
    THREAD_SIGNAL_INBOX.store(inbox, Ordering::Release);
}

fn current_thread_inbox() -> Option<&'static ThreadSignalInbox> {
    unsafe { THREAD_SIGNAL_INBOX.load(Ordering::Acquire).as_ref() }
}

/// Take one target-pthread event without exposing a clear-before-hold window to
/// Engine close. This function must be called only by the target pthread.
pub(crate) fn take_current_thread_delivery(blocked: u64) -> Option<(SignalDeliveryGuard, i32)> {
    let inbox = current_thread_inbox()?;
    loop {
        let mut selected: *mut ThreadSignalCell = ptr::null_mut();
        let mut cell = inbox.cells.load(Ordering::Acquire);
        while !cell.is_null() {
            let current = unsafe { &*cell };
            let registration = current.registration.load(Ordering::Relaxed);
            if registration.is_null() {
                eprintln!("mirvm[m4-engine]: target-pthread signal cell is corrupt");
                std::process::abort();
            }
            let registration = unsafe { &*registration };
            let signum = registration.signum() as usize;
            if signum >= SIGNAL_SLOTS {
                eprintln!("mirvm[m4-engine]: target-pthread signal cell has an invalid signal");
                std::process::abort();
            }
            if blocked & (1u64 << signum) == 0
                && current.pending.load(Ordering::Acquire)
                && (selected.is_null()
                    || registration.generation()
                        < unsafe { &*(*selected).registration.load(Ordering::Relaxed) }
                            .generation())
            {
                selected = cell;
            }
            cell = current.next.load(Ordering::Acquire);
        }
        let cell = unsafe { selected.as_ref()? };
        let registration = cell.registration.load(Ordering::Relaxed);
        let registration = unsafe { &*registration };
        let Ok(hold) = super::ctx::DeferredHold::acquire(registration.control(), true) else {
            if cell.pending.load(Ordering::Acquire) {
                eprintln!("mirvm[m4-engine]: target-pthread signal outlived its Engine");
                std::process::abort();
            }
            continue;
        };
        let delivery = SignalDeliveryGuard {
            registration,
            _hold: hold,
        };
        if cell
            .pending
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if registration.thread_pending.fetch_sub(1, Ordering::AcqRel) == 0 {
                eprintln!("mirvm[m4-engine]: target-pthread signal count underflowed");
                std::process::abort();
            }
            return Some((delivery, registration.signum()));
        }
        drop(delivery);
    }
}

pub(crate) fn current_thread_has_pending() -> bool {
    current_thread_inbox().is_some_and(ThreadSignalInbox::has_pending)
}

pub(crate) fn current_thread_has_pending_for_engine(owner: u64) -> bool {
    let Some(inbox) = current_thread_inbox() else {
        return false;
    };
    let mut cell = inbox.cells.load(Ordering::Acquire);
    while !cell.is_null() {
        let current = unsafe { &*cell };
        let registration = current.registration.load(Ordering::Relaxed);
        if registration.is_null() {
            eprintln!("mirvm[m4-engine]: target-pthread signal cell is corrupt");
            std::process::abort();
        }
        if current.pending.load(Ordering::Acquire)
            && unsafe { &*registration }.control().id() == owner
        {
            return true;
        }
        cell = current.next.load(Ordering::Acquire);
    }
    false
}

/// Called after the target pthread drained every slot in its final TSD round.
pub(crate) fn deactivate_current_thread_inbox() {
    let Some(inbox) = current_thread_inbox() else {
        return;
    };
    inbox.deactivate();
    inbox.wait_for_deliveries();
    if inbox.has_pending() {
        eprintln!("mirvm[m4-engine]: target pthread exited with a pending signal");
        std::process::abort();
    }
}

/// Keeps the callback Engine in Closing (rather than Finalizing) between an
/// ordinary-state registry/inbox lookup and the callback's execution lease.
pub(crate) struct SignalDeliveryGuard {
    registration: &'static SignalRegistration,
    _hold: super::ctx::DeferredHold,
}

impl SignalDeliveryGuard {
    pub(crate) fn registration(&self) -> &'static SignalRegistration {
        self.registration
    }
}

pub(crate) struct SignalInbox {
    registrations: AtomicPtr<SignalRegistration>,
}

impl SignalInbox {
    pub(crate) const fn new() -> Self {
        Self {
            registrations: AtomicPtr::new(ptr::null_mut()),
        }
    }

    fn register(&self, registration: &'static SignalRegistration) {
        let registration_ptr = ptr::from_ref(registration).cast_mut();
        let mut head = self.registrations.load(Ordering::Acquire);
        loop {
            registration.next_owner.store(head, Ordering::Relaxed);
            match self.registrations.compare_exchange_weak(
                head,
                registration_ptr,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => head = observed,
            }
        }
    }

    fn registrations(&self) -> SignalRegistrationIter {
        SignalRegistrationIter {
            next: self.registrations.load(Ordering::Acquire),
        }
    }

    /// Take one actual kernel delivery, preferring the oldest installed handler
    /// generation for this signal. Registration nodes never move or disappear.
    ///
    /// The lifecycle hold is acquired while the pending bit is still visible
    /// to close. Once the CAS clears that bit, the hold itself prevents the
    /// Engine from crossing into Finalizing until dispatch owns its execution
    /// lease. These two steps must remain one API.
    pub(crate) fn take_delivery(&self, signum: usize) -> Option<SignalDeliveryGuard> {
        loop {
            let mut selected: Option<&'static SignalRegistration> = None;
            for registration in self.registrations() {
                if !registration.pending[signum].load(Ordering::Acquire) {
                    continue;
                }
                if selected.is_none_or(|old| registration.generation() < old.generation()) {
                    selected = Some(registration);
                }
            }
            let registration = selected?;
            let pending = &registration.pending[signum];
            let Ok(hold) = super::ctx::DeferredHold::acquire(&registration.control, true) else {
                // Another consumer can clear the same coalesced bit and let
                // close finish before this stale candidate acquires its hold.
                // A still-pending bit, however, is lifecycle-visible and must
                // have prevented that transition.
                if pending.load(Ordering::Acquire) {
                    eprintln!("mirvm[m4-engine]: accepted signal outlived its Engine");
                    std::process::abort();
                }
                continue;
            };
            let delivery = SignalDeliveryGuard {
                registration,
                _hold: hold,
            };
            if pending
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                #[cfg(test)]
                run_after_inbox_clear_hook(registration);
                return Some(delivery);
            }
            drop(delivery);
        }
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.registrations().any(|registration| {
            registration.thread_pending.load(Ordering::Acquire) != 0
                || registration
                    .pending
                    .iter()
                    .any(|pending| pending.load(Ordering::Acquire))
        })
    }
}

struct SignalRegistrationIter {
    next: *mut SignalRegistration,
}

impl Iterator for SignalRegistrationIter {
    type Item = &'static SignalRegistration;

    fn next(&mut self) -> Option<Self::Item> {
        let registration = unsafe { self.next.as_ref()? };
        self.next = registration.next_owner.load(Ordering::Acquire);
        Some(registration)
    }
}

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

// Source-compatible no-ops while the activation-side bookkeeping is removed.
// Product signal frames never consult thread-local state.
pub(crate) fn activate_owner(_owner: u64) -> u64 {
    0
}

pub(crate) fn restore_owner(_owner: u64) {}

/// Called from the fixed product stub. The body after `try_delivery` is
/// deliberately limited to atomics and reads of process-lifetime memory.
pub(crate) unsafe fn record_async_signal(
    registration: *mut SignalRegistration,
    signum: i32,
    info: *mut libc::siginfo_t,
) {
    if registration.is_null() || signum <= 0 || signum as usize >= SIGNAL_SLOTS {
        unsafe { libc::_exit(70) }
    }
    let registration = unsafe { &*registration };
    if registration.signum() != signum {
        unsafe { libc::_exit(70) }
    }
    let Some(_delivery) = registration.try_kernel_delivery() else {
        if retry_current_host_raise(signum) {
            return;
        }
        unsafe { libc::_exit(70) }
    };
    let code = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_code }
    };
    if code == libc::SI_TKILL {
        let Some(inbox) = current_thread_inbox() else {
            if retry_current_host_raise(signum) {
                return;
            }
            unsafe { libc::_exit(70) }
        };
        let Some(_thread_delivery) = inbox.try_delivery() else {
            if retry_current_host_raise(signum) {
                return;
            }
            unsafe { libc::_exit(70) }
        };
        registration.publish_thread_directed(inbox, signum);
        #[cfg(test)]
        registration.kernel_frames.fetch_add(1, Ordering::Release);
        return;
    }
    registration.publish(signum);
    #[cfg(test)]
    registration.kernel_frames.fetch_add(1, Ordering::Release);
}

#[derive(Clone)]
struct DispositionNode {
    install_control: Arc<EngineControl>,
    install_owner: u64,
    callback_owner: Option<u64>,
    guest: Sigaction,
    /// Exact request passed at the single installation linearization point.
    kernel: Sigaction,
    /// Exact post-install kernel form, known only when a read-only query still
    /// observed this candidate before a raw writer replaced it.
    accepted_kernel: Option<Sigaction>,
    registration: Option<&'static SignalRegistration>,
}

#[derive(Clone)]
struct SignalChain {
    base: Sigaction,
    nodes: Vec<DispositionNode>,
}

#[derive(Clone)]
struct StubDescriptor {
    install_control: Arc<EngineControl>,
    install_owner: u64,
    callback_owner: u64,
    registration: &'static SignalRegistration,
    visible: Sigaction,
    /// Exact request passed at the single installation linearization point.
    kernel: Sigaction,
    /// Exact post-install kernel form when it was observed without writing the
    /// target signal a second time.
    accepted_kernel: Option<Sigaction>,
    /// Complete logical prefix that this stub replaced when it committed. A
    /// plain Sigaction is insufficient because native nodes also have an
    /// installer owner that must be removed from detached history at close.
    fallback: SignalChain,
}

#[derive(Default)]
struct SignalRegistry {
    chains: HashMap<i32, SignalChain>,
    stubs: HashMap<usize, StubDescriptor>,
}

static REGISTRY: LazyLock<Mutex<SignalRegistry>> =
    LazyLock::new(|| Mutex::new(SignalRegistry::default()));

#[cfg(test)]
type AfterInstallReplaceHook = Box<dyn FnOnce(i32, Sigaction, Sigaction) + Send>;

#[cfg(test)]
static AFTER_INSTALL_REPLACE_HOOK: LazyLock<Mutex<Option<AfterInstallReplaceHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
struct AfterInboxClearHook {
    registration: usize,
    callback: Box<dyn FnOnce() + Send>,
}

#[cfg(test)]
static AFTER_INBOX_CLEAR_HOOK: LazyLock<Mutex<Option<AfterInboxClearHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
fn run_after_install_replace_hook(signum: i32, requested: Sigaction, actual_old: Sigaction) {
    let hook = AFTER_INSTALL_REPLACE_HOOK.lock().unwrap().take();
    if let Some(hook) = hook {
        hook(signum, requested, actual_old);
    }
}

#[cfg(test)]
fn run_after_inbox_clear_hook(registration: &'static SignalRegistration) {
    let registration = ptr::from_ref(registration) as usize;
    let hook = {
        let mut hook = AFTER_INBOX_CLEAR_HOOK.lock().unwrap();
        if hook
            .as_ref()
            .is_some_and(|hook| hook.registration == registration)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        (hook.callback)();
    }
}

#[derive(Debug)]
pub(crate) enum SignalError {
    /// libc rejected an otherwise ordinary signal operation. The embedding
    /// surface must return the libc sentinel and preserve this errno.
    Libc {
        operation: &'static str,
        signum: i32,
        errno: i32,
    },
    /// MIRVM cannot faithfully implement the requested semantics or detected
    /// corruption of its own disposition bookkeeping.
    Contract(String),
}

impl SignalError {
    fn libc(operation: &'static str, signum: i32, errno: i32) -> Self {
        Self::Libc {
            operation,
            signum,
            errno,
        }
    }

    fn contract(message: impl Into<String>) -> Self {
        Self::Contract(message.into())
    }

    pub(crate) fn libc_errno(&self) -> Option<i32> {
        match self {
            Self::Libc { errno, .. } => Some(*errno),
            Self::Contract(_) => None,
        }
    }
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Libc {
                operation,
                signum,
                errno,
            } => write!(
                f,
                "{operation} for signal {signum} failed: {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::Contract(message) => f.write_str(message),
        }
    }
}

type SignalResult<T> = Result<T, SignalError>;

fn validate_guest_action(signum: i32, action: &Sigaction) -> Result<(), String> {
    if signum <= 0 || signum as usize >= SIGNAL_SLOTS {
        return Err(format!(
            "guest handler for signal {signum} is outside the supported traditional signal range"
        ));
    }
    if matches!(
        signum,
        crate::os::signal::SIGSEGV
            | crate::os::signal::SIGBUS
            | crate::os::signal::SIGFPE
            | crate::os::signal::SIGILL
            | crate::os::signal::SIGTRAP
    ) {
        return Err(format!(
            "guest handler for synchronous fault signal {signum} is unsupported because host and guest faults cannot be distinguished"
        ));
    }
    if crate::os::signal::is_realtime(signum) {
        return Err(format!(
            "guest handler for realtime signal {signum} requires queued siginfo delivery"
        ));
    }
    if action.has_unsupported_guest_flags() {
        return Err(format!(
            "guest sigaction for signal {signum} uses unsupported SA_SIGINFO, SA_ONSTACK, SA_NODEFER, or SA_RESETHAND semantics"
        ));
    }
    Ok(())
}

fn kernel_current(signum: i32) -> SignalResult<Sigaction> {
    Sigaction::query(signum).map_err(|errno| SignalError::libc("sigaction query", signum, errno))
}

/// Recheck a HostRaise retry outside the signal frame. A close may have won
/// after the kernel selected its old fixed stub, in which case retrying the
/// now-current disposition is correct. If a raw writer instead restored that
/// same inactive stub, another raise would only repeat forever.
pub(crate) fn host_raise_retry_is_stale(signum: i32) -> SignalResult<bool> {
    let current = kernel_current(signum)?;
    let registry = REGISTRY.lock().unwrap();
    let Some(descriptor) = registry.stubs.get(&current.handler()) else {
        return Ok(false);
    };
    // A raw writer may restore a closed stub while also changing its mask,
    // flags, or restorer. The inactive handler identity alone makes another
    // raise terminal: the kernel would select the same dead registration
    // again, regardless of whether the surrounding disposition still matches
    // MIRVM's recorded action.
    Ok(descriptor.registration.signum() == signum
        && !descriptor.registration.accepts_kernel_delivery())
}

// libc supplies its private SA_RESTORER detail while installing an action.
// `expected` may therefore be the caller's exact request while `actual` is a
// kernel oldact captured by a raw writer. All caller-controlled flags and the
// full mask still have to match.
fn kernel_request_matches(actual: &Sigaction, expected: &Sigaction) -> bool {
    actual.same_disposition(expected) || actual.is_kernel_normalization_of(expected)
}

fn recorded_kernel_matches(
    actual: &Sigaction,
    requested: &Sigaction,
    accepted: Option<&Sigaction>,
) -> bool {
    accepted.is_some_and(|accepted| actual.same_disposition(accepted))
        || kernel_request_matches(actual, requested)
}

fn node_kernel_matches(actual: &Sigaction, node: &DispositionNode) -> bool {
    recorded_kernel_matches(actual, &node.kernel, node.accepted_kernel.as_ref())
}

fn descriptor_kernel_matches(actual: &Sigaction, descriptor: &StubDescriptor) -> bool {
    recorded_kernel_matches(
        actual,
        &descriptor.kernel,
        descriptor.accepted_kernel.as_ref(),
    )
}

fn node_kernel_action(node: &DispositionNode) -> Sigaction {
    node.accepted_kernel.unwrap_or(node.kernel)
}

fn visible_node_action(node: &DispositionNode, actual: Sigaction) -> Sigaction {
    if node.registration.is_some() {
        actual.canonicalized_from_kernel_stub(&node.kernel, &node.guest)
    } else {
        actual
    }
}

fn visible_current(
    registry: &SignalRegistry,
    signum: i32,
    kernel: Sigaction,
) -> SignalResult<Sigaction> {
    if let Some(top) = registry
        .chains
        .get(&signum)
        .and_then(|chain| chain.nodes.last())
        .filter(|top| node_kernel_matches(&kernel, top))
    {
        return Ok(visible_node_action(top, kernel));
    }
    let Some(stub) = registry.stubs.get(&kernel.handler()) else {
        return Ok(kernel);
    };
    if stub.registration.signum() == signum && descriptor_kernel_matches(&kernel, stub) {
        Ok(kernel.canonicalized_from_kernel_stub(&stub.kernel, &stub.visible))
    } else {
        Err(SignalError::contract(format!(
            "signal {signum} disposition uses a MIRVM fixed-stub address with incompatible flags, mask, or restorer"
        )))
    }
}

fn canonical_guest_action(registry: &SignalRegistry, action: Sigaction) -> Sigaction {
    let Some(descriptor) = registry.stubs.get(&action.handler()) else {
        return action;
    };
    action.canonicalized_from_kernel_stub(&descriptor.kernel, &descriptor.visible)
}

enum ResolvedCallback {
    ExternalNative,
    Deferred {
        control: Arc<EngineControl>,
        callback: DeferredSignalCallback,
    },
}

fn resolve_callback(
    registry: &SignalRegistry,
    action: Sigaction,
    resolution: super::thunks::SignalHandlerResolution,
) -> SignalResult<ResolvedCallback> {
    use super::thunks::SignalHandlerResolution;

    let address = action.handler();
    if address == crate::os::signal::SIG_DFL || address == crate::os::signal::SIG_IGN {
        return match resolution {
            SignalHandlerResolution::Unknown => Ok(ResolvedCallback::ExternalNative),
            _ => Err(SignalError::contract(
                "SIG_DFL/SIG_IGN was also resolved as MIRVM code",
            )),
        };
    }

    match resolution {
        SignalHandlerResolution::Valid { control, func } => Ok(ResolvedCallback::Deferred {
            control,
            callback: DeferredSignalCallback::Guest(func),
        }),
        SignalHandlerResolution::KnownInvalid => Err(SignalError::contract(format!(
            "signal handler {address:#x} is a MIRVM callback with an incompatible ABI or a closed owner"
        ))),
        SignalHandlerResolution::Unknown => {
            if let Some(descriptor) = registry.stubs.get(&address) {
                let registration = descriptor.registration;
                if !registration.control.accepts_signal_install() {
                    return Err(SignalError::contract(format!(
                        "signal handler {address:#x} belongs to a closed MIRVM Engine"
                    )));
                }
                return Ok(ResolvedCallback::Deferred {
                    control: Arc::clone(registration.control()),
                    callback: registration.callback(),
                });
            }
            if let Some(control) = super::native_instance::owner_of_executable_address(address) {
                if !control.accepts_signal_install() {
                    return Err(SignalError::contract(format!(
                        "signal handler {address:#x} belongs to a closed MIRVM native image"
                    )));
                }
                return Ok(ResolvedCallback::Deferred {
                    control,
                    callback: DeferredSignalCallback::ImageNative(address),
                });
            }
            // MIRVM-owned addresses have already been classified above. What
            // remains is an ordinary host function pointer, including the
            // common first-use case where guest code obtained it from dlsym.
            Ok(ResolvedCallback::ExternalNative)
        }
    }
}

fn reject_committed_candidate(
    signum: i32,
    requested: Sigaction,
    accepted: Option<Sigaction>,
    previous: Sigaction,
    registration: Option<&'static SignalRegistration>,
) -> SignalResult<()> {
    // The successful replace already exposed `requested`: a raw writer may
    // have retained it even when compensation restores `previous` now. Keep
    // its descriptor for process lifetime and close its delivery gate so a
    // later raw restoration fails loudly instead of dropping a signal.
    let _ = rollback_committed_candidate(signum, requested, accepted, previous)?;
    if let Some(registration) = registration {
        registration.deactivate();
        registration.wait_for_kernel_deliveries();
    }
    Ok(())
}

fn descriptor_belongs_to(descriptor: &StubDescriptor, owner: u64) -> bool {
    descriptor.install_owner == owner || descriptor.callback_owner == owner
}

fn node_belongs_to(node: &DispositionNode, owner: u64) -> bool {
    node.install_owner == owner || node.callback_owner == Some(owner)
}

/// Keep every live fixed stub's saved predecessor aligned with the current
/// logical chain. This is what makes non-LIFO removal work: removing A from
/// base <- A <- B rewrites B's fallback to base.
fn sync_chain_descriptors(registry: &mut SignalRegistry, signum: i32) {
    let Some(chain) = registry.chains.get(&signum).cloned() else {
        return;
    };
    let mut fallback = SignalChain {
        base: chain.base,
        nodes: Vec::new(),
    };
    for node in chain.nodes {
        if let Some(registration) = node.registration {
            let descriptor = registry
                .stubs
                .get_mut(&node.kernel.handler())
                .unwrap_or_else(|| {
                    panic!(
                        "signal {signum} live fixed stub is missing its process-lifetime descriptor"
                    )
                });
            assert!(ptr::eq(descriptor.registration, registration));
            descriptor.kernel = node.kernel;
            descriptor.accepted_kernel = node.accepted_kernel;
            descriptor.fallback = fallback.clone();
        }
        fallback.nodes.push(node);
    }
}

/// Closing an Engine also invalidates detached process-lifetime stubs that are
/// no longer in a live chain. Fold every surviving descriptor around those
/// stubs so a later raw restoration cannot revive a dead predecessor.
fn deactivate_owner_descriptors(
    registry: &mut SignalRegistry,
    owner: u64,
    registrations: &mut Vec<&'static SignalRegistration>,
) {
    for descriptor in registry.stubs.values() {
        if descriptor_belongs_to(descriptor, owner) {
            descriptor.registration.deactivate();
            registrations.push(descriptor.registration);
        }
    }
    for descriptor in registry.stubs.values_mut() {
        descriptor
            .fallback
            .nodes
            .retain(|node| !node_belongs_to(node, owner));
    }
}

fn validate_rebuilt_node(
    registry: &SignalRegistry,
    signum: i32,
    node: &DispositionNode,
) -> SignalResult<()> {
    if node.install_owner != node.install_control.id()
        || !node.install_control.accepts_signal_install()
    {
        return Err(SignalError::contract(format!(
            "signal {signum} disposition contains an installation from a closed MIRVM Engine"
        )));
    }
    let Some(registration) = node.registration else {
        if node.callback_owner.is_some() {
            return Err(SignalError::contract(format!(
                "signal {signum} native disposition has a callback owner"
            )));
        }
        return Ok(());
    };
    let Some(descriptor) = registry.stubs.get(&node.kernel.handler()) else {
        return Err(SignalError::contract(format!(
            "signal {signum} fallback contains an unknown MIRVM fixed stub"
        )));
    };
    if !node.kernel.same_disposition(&descriptor.kernel)
        || node.install_owner != descriptor.install_owner
        || node.install_control.id() != descriptor.install_control.id()
        || descriptor.install_owner != descriptor.install_control.id()
        || node
            .accepted_kernel
            .is_some_and(|accepted| !recorded_kernel_matches(&accepted, &node.kernel, None))
        || descriptor
            .accepted_kernel
            .is_some_and(|accepted| !recorded_kernel_matches(&accepted, &descriptor.kernel, None))
        || !ptr::eq(registration, descriptor.registration)
        || node.callback_owner != Some(descriptor.callback_owner)
        || descriptor.callback_owner != registration.control().id()
        || !registration.control().accepts_signal_install()
    {
        return Err(SignalError::contract(format!(
            "signal {signum} fallback contains an invalid or closed MIRVM fixed stub"
        )));
    }
    Ok(())
}

/// Rebuild the managed prefix ending at `top_kernel` from the complete logical
/// snapshot saved when that process-lifetime fixed stub committed.
fn plan_stub_chain(
    registry: &SignalRegistry,
    signum: i32,
    top_kernel: Sigaction,
) -> SignalResult<Option<(Sigaction, SignalChain)>> {
    let Some(top_descriptor) = registry.stubs.get(&top_kernel.handler()).cloned() else {
        return Ok(None);
    };
    if !descriptor_kernel_matches(&top_kernel, &top_descriptor) {
        return Err(SignalError::contract(format!(
            "signal {signum} disposition uses a MIRVM fixed-stub address with incompatible flags, mask, or restorer"
        )));
    }
    if top_descriptor.registration.signum() != signum {
        return Err(SignalError::contract(format!(
            "signal {signum} disposition contains a MIRVM fixed stub for signal {}",
            top_descriptor.registration.signum()
        )));
    }
    let mut chain = top_descriptor.fallback.clone();
    for node in &chain.nodes {
        validate_rebuilt_node(registry, signum, node)?;
    }
    let mut top = DispositionNode {
        install_control: Arc::clone(&top_descriptor.install_control),
        install_owner: top_descriptor.install_owner,
        callback_owner: Some(top_descriptor.callback_owner),
        guest: top_descriptor.visible,
        kernel: top_descriptor.kernel,
        accepted_kernel: top_descriptor.accepted_kernel,
        registration: Some(top_descriptor.registration),
    };
    validate_rebuilt_node(registry, signum, &top)?;
    // `top_kernel` has already matched this descriptor's strict request or a
    // previously accepted snapshot. It is the exact action observed now, so
    // retain it only after validating the descriptor's own stored identity.
    top.accepted_kernel = Some(top_kernel);
    chain.nodes.push(top);
    let visible =
        top_kernel.canonicalized_from_kernel_stub(&top_descriptor.kernel, &top_descriptor.visible);
    Ok(Some((visible, chain)))
}

fn managed_chain_for_current(
    registry: &SignalRegistry,
    signum: i32,
    current: Sigaction,
) -> SignalResult<Option<SignalChain>> {
    if let Some((chain, position)) = registry.chains.get(&signum).and_then(|chain| {
        chain
            .nodes
            .iter()
            .rposition(|node| node_kernel_matches(&current, node))
            .map(|position| (chain, position))
    }) {
        let mut prefix = chain.clone();
        prefix.nodes.truncate(position + 1);
        prefix.nodes[position].accepted_kernel = Some(current);
        return Ok(Some(prefix));
    }
    let Some(descriptor) = registry.stubs.get(&current.handler()) else {
        return Ok(None);
    };
    if descriptor.registration.signum() != signum
        || !descriptor_kernel_matches(&current, descriptor)
    {
        // A raw writer reused an internal address with a different action.
        // It is not the MIRVM disposition described by this registry entry,
        // so close must leave it untouched.
        return Ok(None);
    }
    plan_stub_chain(registry, signum, current).map(|planned| planned.map(|(_, chain)| chain))
}

/// Replace only the disposition observed by the caller. `sigaction` has no
/// compare-and-swap operation, so an intervening raw writer is detected from
/// the old action returned at the replacement linearization point. In that
/// case, restore the most recent displaced writer and ask the caller to retry.
fn replace_observed(
    signum: i32,
    observed: Sigaction,
    replacement: Sigaction,
) -> SignalResult<bool> {
    let actual_old = replacement
        .replace_exact(signum)
        .map_err(|errno| SignalError::libc("sigaction restore", signum, errno))?;
    if actual_old.same_disposition(&observed) {
        return Ok(true);
    }

    compensate_concurrent_writer(signum, replacement, actual_old)?;
    Ok(false)
}

/// Roll back a candidate that was installed as a libc request. The first
/// comparison accepts libc/kernel normalization of that request; after this
/// point every value came from an exact kernel snapshot and must compare
/// exactly during compensation.
fn rollback_committed_candidate(
    signum: i32,
    requested: Sigaction,
    accepted: Option<Sigaction>,
    previous: Sigaction,
) -> SignalResult<bool> {
    let actual_old = previous
        .replace_exact(signum)
        .map_err(|errno| SignalError::libc("sigaction rollback", signum, errno))?;
    if recorded_kernel_matches(&actual_old, &requested, accepted.as_ref()) {
        return Ok(true);
    }
    compensate_concurrent_writer(signum, previous, actual_old)?;
    Ok(false)
}

fn compensate_concurrent_writer(
    signum: i32,
    mut installed: Sigaction,
    mut displaced: Sigaction,
) -> SignalResult<()> {
    loop {
        let actual = displaced.replace_exact(signum).map_err(|errno| {
            SignalError::libc("sigaction concurrent-writer restore", signum, errno)
        })?;
        if actual.same_disposition(&installed) {
            return Ok(());
        }
        // Another raw writer arrived before our compensating replacement.
        // Preserve that newer action instead, repeating until one replacement
        // observes exactly the action installed by the preceding step.
        installed = displaced;
        displaced = actual;
    }
}

fn chain_has_owner(chain: &SignalChain, owner: u64) -> bool {
    chain.nodes.iter().any(|node| node_belongs_to(node, owner))
}

fn relevant_signal_numbers(registry: &SignalRegistry, owner: u64) -> Vec<i32> {
    let mut signums = registry
        .chains
        .iter()
        .filter(|(_, chain)| chain_has_owner(chain, owner))
        .map(|(&signum, _)| signum)
        .collect::<HashSet<_>>();
    signums.extend(
        registry
            .stubs
            .values()
            .filter(|descriptor| {
                descriptor_belongs_to(descriptor, owner)
                    || chain_has_owner(&descriptor.fallback, owner)
            })
            .map(|descriptor| descriptor.registration.signum()),
    );
    let mut signums = signums.into_iter().collect::<Vec<_>>();
    signums.sort_unstable();
    signums
}

fn remove_owner_for_signal(
    registry: &mut SignalRegistry,
    signum: i32,
    owner: u64,
) -> SignalResult<bool> {
    loop {
        let observed = kernel_current(signum)?;
        let managed_current = managed_chain_for_current(registry, signum, observed)?;
        let current_is_managed = managed_current.is_some();
        let Some(mut chain) = managed_current.or_else(|| registry.chains.get(&signum).cloned())
        else {
            return Ok(false);
        };
        let removed_current_top = current_is_managed
            && chain
                .nodes
                .last()
                .is_some_and(|node| node_belongs_to(node, owner));
        let old_len = chain.nodes.len();
        chain.nodes.retain(|node| !node_belongs_to(node, owner));
        let removed = chain.nodes.len() != old_len;

        if removed_current_top {
            let replacement = chain.nodes.last().map_or(chain.base, node_kernel_action);
            if !replace_observed(signum, observed, replacement)? {
                continue;
            }
        }

        if chain.nodes.is_empty() {
            registry.chains.remove(&signum);
        } else {
            registry.chains.insert(signum, chain);
            sync_chain_descriptors(registry, signum);
        }
        return Ok(removed);
    }
}

/// Reconcile MIRVM's logical chain with the disposition atomically returned by
/// sigaction(new, old). Native code may have restored a lower or detached
/// process-lifetime fixed stub since MIRVM last observed this signal.
fn reconcile_prior_chain(
    registry: &mut SignalRegistry,
    signum: i32,
    actual_old: Sigaction,
) -> SignalResult<Sigaction> {
    let matching_position = registry.chains.get(&signum).and_then(|chain| {
        chain
            .nodes
            .iter()
            .rposition(|node| node_kernel_matches(&actual_old, node))
    });
    if let Some(position) = matching_position {
        let chain = registry.chains.get_mut(&signum).unwrap();
        chain.nodes.truncate(position + 1);
        let visible = visible_node_action(&chain.nodes[position], actual_old);
        chain.nodes[position].accepted_kernel = Some(actual_old);
        sync_chain_descriptors(registry, signum);
        return Ok(visible);
    }

    // Validate the detached stub and its complete fallback prefix before
    // altering the stale logical chain. If validation fails, the caller can
    // still roll the just-installed candidate back to `actual_old`.
    let rebuilt = plan_stub_chain(registry, signum, actual_old)?;
    registry.chains.remove(&signum);
    if let Some((visible, chain)) = rebuilt {
        registry.chains.insert(signum, chain);
        sync_chain_descriptors(registry, signum);
        Ok(visible)
    } else {
        Ok(actual_old)
    }
}

/// Install/query a guest-visible sigaction while keeping MIRVM's fixed stub
/// out of oldact. `sigaction(new, &old)` is the installation linearization
/// point; the `old` it returns, not an earlier query, decides whether the
/// existing ownership chain is still current.
fn install_sigaction_value(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    resolution: Option<super::thunks::SignalHandlerResolution>,
) -> SignalResult<Sigaction> {
    let mut registry = REGISTRY.lock().unwrap();
    let current = kernel_current(signum)?;
    let old = visible_current(&registry, signum, current)?;
    let Some(requested_action) = action.map(Sigaction::normalized_for_kernel) else {
        return Ok(old);
    };

    if !control.accepts_signal_install() {
        return Err(SignalError::contract(
            "signal installer Engine is finalizing or closed",
        ));
    }
    let resolution = resolution.unwrap_or(super::thunks::SignalHandlerResolution::Unknown);
    let resolved = resolve_callback(&registry, requested_action, resolution)?;
    let guest_action = canonical_guest_action(&registry, requested_action);
    let special = matches!(
        guest_action.handler(),
        crate::os::signal::SIG_DFL | crate::os::signal::SIG_IGN
    );
    if !special && matches!(resolved, ResolvedCallback::Deferred { .. }) {
        validate_guest_action(signum, &guest_action).map_err(SignalError::contract)?;
    }

    let mut target_hold = None;
    let candidate = match resolved {
        ResolvedCallback::ExternalNative => None,
        ResolvedCallback::Deferred {
            control: callback_control,
            callback,
        } => {
            // The registry lock serialises this target-phase check with close's
            // final seal. Once committed, target close necessarily sees the
            // node before it can enter Finalizing.
            if callback_control.id() == control.id() {
                if !callback_control.accepts_signal_install() {
                    return Err(SignalError::contract(format!(
                        "signal handler {:#x} belongs to a closing MIRVM Engine",
                        guest_action.handler()
                    )));
                }
            } else {
                target_hold = Some(
                    super::ctx::DeferredHold::acquire(&callback_control, false).map_err(|_| {
                        SignalError::contract(format!(
                            "signal handler {:#x} belongs to a closing MIRVM Engine",
                            guest_action.handler()
                        ))
                    })?,
                );
            }
            let registration =
                SignalRegistration::new(callback_control, callback, signum, guest_action);
            let stub = match materialize_signal_stub(registration) {
                Ok(stub) => stub,
                Err(error) => {
                    registration.deactivate();
                    return Err(SignalError::contract(error));
                }
            };
            registration.control.signal_inbox.register(registration);
            Some((registration, guest_action.for_kernel_stub(stub), stub))
        }
    };
    let requested_kernel = candidate.map_or(guest_action, |(_, kernel, _)| {
        kernel.with_runtime_restorer()
    });
    let actual_old = match requested_kernel.replace(signum) {
        Ok(old) => old,
        Err(errno) => {
            if let Some((registration, _, _)) = candidate {
                registration.deactivate();
                registration.wait_for_kernel_deliveries();
            }
            return Err(SignalError::libc("sigaction install", signum, errno));
        }
    };

    #[cfg(test)]
    run_after_install_replace_hook(signum, requested_kernel, actual_old);

    // The successful replace above is the only write of this candidate. A
    // following query can record the exact kernel-normalized form only if it
    // still observes the same handler and every normalized field is valid. If
    // a raw writer already won, leave it untouched and retain only the request
    // so a captured oldact can still be recognized later.
    let accepted_kernel = Sigaction::query(signum).ok().filter(|observed| {
        observed.handler() == requested_kernel.handler()
            && kernel_request_matches(observed, &requested_kernel)
    });

    // A successful sigaction replace is the installation linearization
    // point. Commit the process-lifetime descriptor before doing any logical
    // reconciliation: a raw writer can already have captured this exact stub
    // address and may restore it later.
    if let Some((registration, _, stub)) = candidate {
        registry.stubs.insert(
            stub,
            StubDescriptor {
                install_control: Arc::clone(control),
                install_owner: control.id(),
                callback_owner: registration.control.id(),
                registration,
                visible: guest_action,
                kernel: requested_kernel,
                accepted_kernel,
                fallback: SignalChain {
                    base: actual_old,
                    nodes: Vec::new(),
                },
            },
        );
    }

    let visible_old = match reconcile_prior_chain(&mut registry, signum, actual_old) {
        Ok(old) => old,
        Err(error) => {
            reject_committed_candidate(
                signum,
                requested_kernel,
                accepted_kernel,
                actual_old,
                candidate.map(|(registration, _, _)| registration),
            )?;
            return Err(error);
        }
    };

    let callback_owner = candidate.map(|(registration, _, _)| registration.control.id());
    let fallback = registry
        .chains
        .get(&signum)
        .cloned()
        .unwrap_or(SignalChain {
            base: actual_old,
            nodes: Vec::new(),
        });
    let chain = registry
        .chains
        .entry(signum)
        .or_insert_with(|| SignalChain {
            base: actual_old,
            nodes: Vec::new(),
        });
    chain.nodes.push(DispositionNode {
        install_control: Arc::clone(control),
        install_owner: control.id(),
        callback_owner,
        guest: guest_action,
        kernel: requested_kernel,
        accepted_kernel,
        registration: candidate.map(|(registration, _, _)| registration),
    });
    if let Some((_, _, stub)) = candidate {
        registry.stubs.get_mut(&stub).unwrap().fallback = fallback;
    }
    sync_chain_descriptors(&mut registry, signum);
    drop(target_hold);
    Ok(visible_old)
}

#[cfg(test)]
pub(crate) fn install_sigaction(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    guest: Option<(FuncId, u64)>,
    oldact: u64,
) -> SignalResult<i32> {
    let resolution = guest.map(|(func, _)| super::thunks::SignalHandlerResolution::Valid {
        control: Arc::clone(control),
        func,
    });
    install_sigaction_resolved(control, signum, action, resolution, oldact)
}

pub(crate) fn install_sigaction_resolved(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    resolution: Option<super::thunks::SignalHandlerResolution>,
    oldact: u64,
) -> SignalResult<i32> {
    let old = install_sigaction_value(control, signum, action, resolution)?;
    old.write_to(oldact);
    Ok(0)
}

#[cfg(test)]
pub(crate) fn install_signal(
    control: &Arc<EngineControl>,
    signum: i32,
    handler: usize,
    guest: Option<(FuncId, u64)>,
) -> SignalResult<usize> {
    let resolution = guest.map_or(
        super::thunks::SignalHandlerResolution::Unknown,
        |(func, _)| super::thunks::SignalHandlerResolution::Valid {
            control: Arc::clone(control),
            func,
        },
    );
    install_signal_resolved(control, signum, handler, resolution)
}

pub(crate) fn install_signal_resolved(
    control: &Arc<EngineControl>,
    signum: i32,
    handler: usize,
    resolution: super::thunks::SignalHandlerResolution,
) -> SignalResult<usize> {
    install_sigaction_value(
        control,
        signum,
        Some(Sigaction::for_signal(handler)),
        Some(resolution),
    )
    .map(|old| old.handler())
}

#[cfg(test)]
pub(crate) fn current_delivery(signum: i32) -> Option<SignalDeliveryGuard> {
    let registry = REGISTRY.lock().unwrap();
    let current = kernel_current(signum).ok()?;
    let registration = registry
        .chains
        .get(&signum)
        .into_iter()
        .flat_map(|chain| chain.nodes.iter().rev())
        .find(|node| node_kernel_matches(&current, node))
        .and_then(|node| node.registration)
        .or_else(|| {
            registry
                .stubs
                .get(&current.handler())
                .filter(|descriptor| {
                    descriptor.registration.signum() == signum
                        && descriptor_kernel_matches(&current, descriptor)
                })
                .map(|descriptor| descriptor.registration)
        });
    registration.and_then(SignalRegistration::safe_point_delivery)
}

/// Remove all dispositions owned by an Engine, restore the surviving top (or
/// the original native action), and wait until every old kernel frame has left
/// the fixed adapter. Pending events are intentionally retained for a final
/// ordinary-state drain by the close path.
pub(crate) fn deactivate_engine(control: &EngineControl) -> SignalResult<bool> {
    let mut removed_any = false;
    {
        let mut registry = REGISTRY.lock().unwrap();
        let mut registrations = Vec::new();
        let signums = relevant_signal_numbers(&registry, control.id());
        for &signum in &signums {
            removed_any |= remove_owner_for_signal(&mut registry, signum, control.id())?;
        }

        removed_any |= registry
            .stubs
            .values()
            .any(|descriptor| descriptor_belongs_to(descriptor, control.id()));
        deactivate_owner_descriptors(&mut registry, control.id(), &mut registrations);

        // A raw writer can restore a detached stub during the first scan.
        // Audit the same finite signal set after closing its delivery gate; an
        // exact current stub is still reconstructable from its descriptor and
        // is restored before the Engine is allowed to reach Finalizing.
        for signum in signums {
            removed_any |= remove_owner_for_signal(&mut registry, signum, control.id())?;
        }

        registrations.sort_unstable_by_key(|registration| ptr::from_ref(*registration) as usize);
        registrations.dedup_by_key(|registration| ptr::from_ref(*registration) as usize);
        // Keep the registry locked until every adapter frame that passed the
        // gate has published. A callback owner cannot seal Finalizing in the
        // small interval between node removal and pending publication.
        for registration in registrations {
            registration.wait_for_kernel_deliveries();
        }
    }
    Ok(removed_any)
}

pub(crate) fn has_engine_registrations(control: &EngineControl) -> bool {
    REGISTRY.lock().unwrap().chains.values().any(|chain| {
        chain.nodes.iter().any(|node| {
            node.install_owner == control.id() || node.callback_owner == Some(control.id())
        })
    })
}

/// Seal signal registration and the Engine lifecycle in one registry critical
/// section. Every install rechecks both owners under this same lock, so no late
/// callback can appear between the final empty check and Finalizing.
pub(crate) fn try_seal_engine(control: &EngineControl) -> bool {
    let registry = REGISTRY.lock().unwrap();
    if registry.chains.values().any(|chain| {
        chain.nodes.iter().any(|node| {
            node.install_owner == control.id() || node.callback_owner == Some(control.id())
        })
    }) || control.signal_inbox.has_pending()
    {
        return false;
    }
    control.begin_finalizing_with_permit()
}

pub(crate) fn has_engine_pending(control: &EngineControl) -> bool {
    control.signal_inbox.has_pending()
}

/// Runtime bridge used by self-produced native archive images. The hidden
/// owner argument identifies the Engine whose P1 entry address may appear as
/// `handler`; handlers inside a MIRVM-produced image are deferred too.
pub(crate) unsafe extern "C-unwind" fn native_signal(
    signum: i32,
    handler: usize,
    owner: u64,
) -> usize {
    let Some(control) = super::ctx::control_for_engine(owner) else {
        set_errno(libc::ESRCH);
        return crate::os::signal::SIG_ERR;
    };
    let Ok(lease) = super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(libc::ESRCH);
        return crate::os::signal::SIG_ERR;
    };
    let _activation = super::ctx::activate(lease.shared());
    let resolution = super::thunks::resolve_signal_handler(lease.shared(), handler as u64);
    match install_sigaction_value(
        &control,
        signum,
        Some(Sigaction::for_signal(handler)),
        Some(resolution),
    ) {
        Ok(old) => old.handler(),
        Err(SignalError::Libc { errno, .. }) => {
            set_errno(errno);
            crate::os::signal::SIG_ERR
        }
        Err(SignalError::Contract(message)) => super::interp::engine_abort(&message),
    }
}

pub(crate) unsafe extern "C-unwind" fn native_sigaction(
    signum: i32,
    action: *const libc::sigaction,
    oldact: *mut libc::sigaction,
    owner: u64,
) -> i32 {
    let Some(control) = super::ctx::control_for_engine(owner) else {
        set_errno(libc::ESRCH);
        return -1;
    };
    let Ok(lease) = super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(libc::ESRCH);
        return -1;
    };
    let _activation = super::ctx::activate(lease.shared());
    let action = unsafe { Sigaction::copy_from(action as u64) };
    let resolution = action.as_ref().map(|action| {
        super::thunks::resolve_signal_handler(lease.shared(), action.handler() as u64)
    });
    match install_sigaction_value(&control, signum, action, resolution) {
        Ok(old) => {
            old.write_to(oldact as u64);
            0
        }
        Err(SignalError::Libc { errno, .. }) => {
            set_errno(errno);
            -1
        }
        Err(SignalError::Contract(message)) => super::interp::engine_abort(&message),
    }
}

pub(crate) unsafe extern "C-unwind" fn native_raise(signum: i32, owner: u64) -> i32 {
    let Some(control) = super::ctx::control_for_engine(owner) else {
        set_errno(libc::ESRCH);
        return -1;
    };
    let Ok(lease) = super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(libc::ESRCH);
        return -1;
    };
    let activation = super::ctx::activate(lease.shared());
    super::ctx::raise_signal(activation.ctx(), signum)
}

fn set_errno(value: i32) {
    crate::os::process::set_errno(value);
}

fn materialize_signal_stub(registration: &'static SignalRegistration) -> Result<usize, String> {
    // SysV x86_64 signal entry already has (signum, siginfo, ucontext) in
    // rdi/rsi/rdx. Supply the fourth argument in rcx and tail-jump so the
    // kernel/glibc restorer sees the original stack:
    //   movabs rcx, registration; movabs rax, adapter; jmp rax
    const STUB_SIZE: usize = 22;
    let page_size = crate::os::mem::page_size();
    let page = crate::os::mem::map_anon(page_size, crate::os::mem::Prot::RW, false);
    if page.is_null() {
        return Err("mmap for fixed signal stub failed".into());
    }
    let mut code = [0u8; STUB_SIZE];
    code[0..2].copy_from_slice(&[0x48, 0xb9]);
    code[2..10].copy_from_slice(&(ptr::from_ref(registration) as usize as u64).to_le_bytes());
    code[10..12].copy_from_slice(&[0x48, 0xb8]);
    code[12..20].copy_from_slice(&(signal_adapter as *const () as usize as u64).to_le_bytes());
    code[20..22].copy_from_slice(&[0xff, 0xe0]);
    unsafe { ptr::copy_nonoverlapping(code.as_ptr(), page, code.len()) };
    if let Err(error) = crate::os::mem::protect(page, page_size, crate::os::mem::Prot::RX) {
        unsafe { crate::os::mem::unmap(page, page_size) };
        return Err(format!("failed to seal fixed signal stub RX: {error}"));
    }
    Ok(page as usize)
}

unsafe extern "C" fn signal_adapter(
    signum: i32,
    info: *mut libc::siginfo_t,
    _context: *mut libc::c_void,
    registration: *mut SignalRegistration,
) {
    unsafe { record_async_signal(registration, signum, info) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::engine::ctx::Shared;
    use crate::vm::engine::ir::Module;

    static DETACHED_CLOSE_NATIVE_RAN: AtomicUsize = AtomicUsize::new(0);
    static RAISE_EXTERNAL_RAN: AtomicUsize = AtomicUsize::new(0);
    static REENTRANT_RAISE_RAN: AtomicUsize = AtomicUsize::new(0);
    static REENTRANT_INSTALL_RESULT: AtomicI32 = AtomicI32::new(0);
    static EXIT_CALLBACK_MASK: AtomicU64 = AtomicU64::new(0);
    static EXIT_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
    static REENTRANT_INSTALL_CONTROL: LazyLock<Mutex<Option<Arc<EngineControl>>>> =
        LazyLock::new(|| Mutex::new(None));

    unsafe extern "C" fn detached_close_native_handler(_signum: i32) {
        DETACHED_CLOSE_NATIVE_RAN.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn detached_close_external_handler(_signum: i32) {}

    unsafe extern "C" fn raise_external_handler(_signum: i32) {
        RAISE_EXTERNAL_RAN.fetch_add(1, Ordering::SeqCst);
        let control = REENTRANT_INSTALL_CONTROL.lock().unwrap().clone();
        if let Some(control) = control {
            let installed =
                install_signal(&control, libc::SIGWINCH, 0x2_6000, Some((15, 0x2_6000)));
            REENTRANT_INSTALL_RESULT.store(i32::from(installed.is_ok()), Ordering::SeqCst);
        }

        if crate::os::process::raise(libc::SIGURG) != 0 {
            unsafe { libc::_exit(72) }
        }
    }

    unsafe extern "C" fn reentrant_raise_handler(_signum: i32) {
        REENTRANT_RAISE_RAN.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C-unwind" fn exit_signal_mask_handler(_signum: i32) {
        let mask =
            Sigaction::current_standard_mask_bits().unwrap_or_else(|_| unsafe { libc::_exit(73) });
        EXIT_CALLBACK_MASK.store(mask, Ordering::Release);
        EXIT_CALLBACK_COUNT.fetch_add(1, Ordering::Release);
    }

    extern "C" fn first_test_restorer() {}

    extern "C" fn second_test_restorer() {}

    const INSTALL_OVERWRITE_CHILD: &str = "MIRVM_SIGNAL_INSTALL_OVERWRITE_CHILD";
    const CURRENT_DELIVERY_CHILD: &str = "MIRVM_SIGNAL_CURRENT_DELIVERY_CHILD";
    const INACTIVE_STUB_CHILD: &str = "MIRVM_SIGNAL_INACTIVE_STUB_CHILD";
    const HOST_RAISE_INACTIVE_STUB_CHILD: &str = "MIRVM_SIGNAL_HOST_RAISE_INACTIVE_STUB_CHILD";
    const RAISE_INSTALL_RACE_CHILD: &str = "MIRVM_SIGNAL_RAISE_INSTALL_RACE_CHILD";
    const NORMALIZED_ACTION_CHILD: &str = "MIRVM_SIGNAL_NORMALIZED_ACTION_CHILD";
    const THREAD_GENERATIONS_CHILD: &str = "MIRVM_SIGNAL_THREAD_GENERATIONS_CHILD";
    const THREAD_EXIT_MASK_CHILD: &str = "MIRVM_SIGNAL_THREAD_EXIT_MASK_CHILD";
    const EXACT_RESTORE_CHILD: &str = "MIRVM_SIGNAL_EXACT_RESTORE_CHILD";
    const REQUEST_ONLY_ROLLBACK_CHILD: &str = "MIRVM_SIGNAL_REQUEST_ONLY_ROLLBACK_CHILD";
    const REQUEST_ONLY_RECONCILE_CHILD: &str = "MIRVM_SIGNAL_REQUEST_ONLY_RECONCILE_CHILD";
    const EXACT_COMPENSATION_CHILD: &str = "MIRVM_SIGNAL_EXACT_COMPENSATION_CHILD";
    const DETACHED_SNAPSHOT_CHILD: &str = "MIRVM_SIGNAL_DETACHED_SNAPSHOT_CHILD";

    fn run_signal_test_child(test_name: &str, child_env: &str) -> std::process::Output {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(child_env, "1")
            .output()
            .expect("failed to start isolated signal test child")
    }

    struct RestoreSignal {
        signum: i32,
        action: Sigaction,
    }

    impl Drop for RestoreSignal {
        fn drop(&mut self) {
            let _ = self.action.replace_exact(self.signum);
        }
    }

    fn control() -> Arc<EngineControl> {
        Arc::clone(Shared::new(Module::default()).control())
    }

    fn registration(
        control: &Arc<EngineControl>,
        signum: i32,
        func: FuncId,
        visible: Sigaction,
    ) -> &'static SignalRegistration {
        SignalRegistration::new(
            Arc::clone(control),
            DeferredSignalCallback::Guest(func),
            signum,
            visible,
        )
    }

    fn descriptor(
        control: &Arc<EngineControl>,
        registration: &'static SignalRegistration,
        visible: Sigaction,
        kernel: Sigaction,
        fallback: SignalChain,
    ) -> StubDescriptor {
        StubDescriptor {
            install_control: Arc::clone(control),
            install_owner: control.id(),
            callback_owner: registration.control().id(),
            registration,
            visible,
            kernel,
            accepted_kernel: Some(kernel),
            fallback,
        }
    }

    fn node(
        control: &Arc<EngineControl>,
        guest: Sigaction,
        kernel: Sigaction,
        registration: Option<&'static SignalRegistration>,
    ) -> DispositionNode {
        DispositionNode {
            install_control: Arc::clone(control),
            install_owner: control.id(),
            callback_owner: registration.map(|registration| registration.control().id()),
            guest,
            kernel,
            accepted_kernel: Some(kernel),
            registration,
        }
    }

    #[test]
    fn ordinary_delivery_does_not_keep_a_kernel_adapter_frame_in_flight() {
        let control = control();
        let visible = Sigaction::for_signal(0x1000);
        let registration = registration(&control, libc::SIGUSR1, 0, visible);
        let delivery = registration.safe_point_delivery().unwrap();

        registration.deactivate();
        registration.wait_for_kernel_deliveries();

        assert!(ptr::eq(delivery.registration(), registration));
        drop(delivery);
    }

    #[test]
    fn new_thread_does_not_allocate_a_cell_for_a_closed_registration() {
        let control = control();
        let registration =
            registration(&control, libc::SIGWINCH, 0, Sigaction::for_signal(0x2_7000));
        registration.deactivate();
        registration.wait_for_kernel_deliveries();

        let has_closed_cell = std::thread::spawn(move || {
            initialize_current_thread_inbox();
            let inbox = current_thread_inbox().unwrap();
            let has_cell = inbox
                .cell_for(ptr::from_ref(registration).cast_mut())
                .is_some();
            deactivate_current_thread_inbox();
            has_cell
        })
        .join()
        .unwrap();

        assert!(!has_closed_cell);
    }

    #[test]
    fn target_pthread_preserves_each_kernel_selected_registration_generation() {
        if std::env::var_os(THREAD_GENERATIONS_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let control = control();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (drain_tx, drain_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let target = std::thread::spawn(move || {
                initialize_current_thread_inbox();
                ready_tx
                    .send(unsafe { libc::pthread_self() } as usize)
                    .unwrap();
                drain_rx.recv().unwrap();
                let mut generations = Vec::new();
                while let Some((delivery, delivered_signum)) = take_current_thread_delivery(0) {
                    assert_eq!(delivered_signum, signum);
                    generations.push(delivery.registration().generation());
                    drop(delivery);
                }
                deactivate_current_thread_inbox();
                result_tx.send(generations).unwrap();
            });
            let target_pthread = ready_rx.recv().unwrap() as libc::pthread_t;

            let mut expected = Vec::new();
            for (func, handler) in [(21, 0x2_7100), (22, 0x2_7200), (23, 0x2_7300)] {
                install_signal(&control, signum, handler, Some((func, handler as u64))).unwrap();
                let registration = control
                    .signal_inbox
                    .registrations()
                    .filter(|registration| registration.signum() == signum)
                    .max_by_key(|registration| registration.generation())
                    .unwrap();
                expected.push(registration.generation());
                assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
                wait_for_test_kernel_frames(registration, 1);
                assert_eq!(registration.thread_pending.load(Ordering::Acquire), 1);
                if func == 23 {
                    assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
                    wait_for_test_kernel_frames(registration, 2);
                    assert_eq!(
                        registration.thread_pending.load(Ordering::Acquire),
                        1,
                        "same-generation traditional signals did not coalesce"
                    );
                }
            }

            drain_tx.send(()).unwrap();
            assert_eq!(result_rx.recv().unwrap(), expected);
            target.join().unwrap();
            deactivate_engine(&control).unwrap();
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::target_pthread_preserves_each_kernel_selected_registration_generation",
            THREAD_GENERATIONS_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated target-pthread generation regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn wait_for_test_kernel_frames(registration: &SignalRegistration, expected: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while registration.kernel_frames.load(Ordering::Acquire) < expected {
            assert!(
                std::time::Instant::now() < deadline,
                "target pthread did not enter the expected fixed adapter frame"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn target_pthread_exit_callback_observes_the_pre_exit_signal_mask() {
        if std::env::var_os(THREAD_EXIT_MASK_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let unrelated = libc::SIGUSR2;
            let saved = kernel_current(signum).unwrap();
            let engine = super::super::ctx::Engine::new(Shared::new(Module::default()));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (exit_tx, exit_rx) = std::sync::mpsc::channel();
            let (empty_tx, empty_rx) = std::sync::mpsc::channel();
            let (publish_tx, publish_rx) = std::sync::mpsc::channel();
            let target_engine = engine.clone();
            EXIT_CALLBACK_MASK.store(u64::MAX, Ordering::Release);
            EXIT_CALLBACK_COUNT.store(0, Ordering::Release);
            super::super::ctx::set_thread_exit_inbox_empty_hook(Box::new(move || {
                empty_tx.send(()).unwrap();
                publish_rx.recv().unwrap();
            }));
            let target = std::thread::spawn(move || {
                let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
                unsafe {
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, unrelated);
                }
                assert_eq!(
                    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, ptr::null_mut()) },
                    0
                );
                let activation = super::super::ctx::activate(target_engine.shared());
                ready_tx
                    .send(unsafe { libc::pthread_self() } as usize)
                    .unwrap();
                exit_rx.recv().unwrap();
                drop(activation);
            });
            let target_pthread = ready_rx.recv().unwrap() as libc::pthread_t;
            let action = Sigaction::for_signal(exit_signal_mask_handler as *const () as usize);
            let registration = SignalRegistration::new(
                Arc::clone(engine.control()),
                DeferredSignalCallback::ImageNative(exit_signal_mask_handler as *const () as usize),
                signum,
                action,
            );
            engine.control().signal_inbox.register(registration);
            let stub = materialize_signal_stub(registration).unwrap();
            let kernel = action.for_kernel_stub(stub);
            kernel.replace(signum).unwrap();
            assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
            wait_for_test_kernel_frames(registration, 1);
            exit_tx.send(()).unwrap();
            empty_rx.recv().unwrap();
            assert_eq!(unsafe { libc::pthread_kill(target_pthread, signum) }, 0);
            wait_for_test_kernel_frames(registration, 2);
            publish_tx.send(()).unwrap();
            target.join().unwrap();

            let observed = EXIT_CALLBACK_MASK.load(Ordering::Acquire);
            assert_eq!(
                observed & (1u64 << unrelated),
                0,
                "thread-exit drain exposed its block-all cutoff to the callback"
            );
            assert_ne!(
                observed & (1u64 << signum),
                0,
                "deferred callback did not observe its normal handler mask"
            );
            assert_eq!(
                EXIT_CALLBACK_COUNT.load(Ordering::Acquire),
                2,
                "a pthread signal published after an empty exit drain was lost"
            );
            saved.replace(signum).unwrap();
            registration.deactivate();
            registration.wait_for_kernel_deliveries();
            engine.wait_closed().unwrap();
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::target_pthread_exit_callback_observes_the_pre_exit_signal_mask",
            THREAD_EXIT_MASK_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated target-pthread exit-mask regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn kernel_normalization_rejects_restorer_and_supported_flag_changes() {
        const SA_RESTORER: i32 = 0x0400_0000;
        const SA_UNSUPPORTED: i32 = 0x0000_0400;
        const SA_EXPOSE_TAGBITS: i32 = 0x0000_0800;
        let mut requested: libc::sigaction = unsafe { std::mem::zeroed() };
        requested.sa_sigaction = 0x1_1000;
        requested.sa_flags = libc::SA_RESTART | SA_RESTORER | SA_UNSUPPORTED | SA_EXPOSE_TAGBITS;
        requested.sa_restorer = Some(first_test_restorer);
        assert_eq!(unsafe { libc::sigemptyset(&mut requested.sa_mask) }, 0);
        let requested = unsafe { Sigaction::copy_from(ptr::from_ref(&requested) as u64) }.unwrap();

        let mut accepted: libc::sigaction = unsafe { std::mem::zeroed() };
        requested.write_to(ptr::from_mut(&mut accepted) as u64);
        accepted.sa_flags &= !SA_UNSUPPORTED;
        let accepted = unsafe { Sigaction::copy_from(ptr::from_ref(&accepted) as u64) }.unwrap();
        assert!(accepted.is_kernel_normalization_of(&requested));

        let mut wrong_restorer: libc::sigaction = unsafe { std::mem::zeroed() };
        accepted.write_to(ptr::from_mut(&mut wrong_restorer) as u64);
        wrong_restorer.sa_restorer = Some(second_test_restorer);
        let wrong_restorer =
            unsafe { Sigaction::copy_from(ptr::from_ref(&wrong_restorer) as u64) }.unwrap();
        assert!(!wrong_restorer.is_kernel_normalization_of(&requested));

        let mut missing_supported: libc::sigaction = unsafe { std::mem::zeroed() };
        accepted.write_to(ptr::from_mut(&mut missing_supported) as u64);
        missing_supported.sa_flags &= !SA_EXPOSE_TAGBITS;
        let missing_supported =
            unsafe { Sigaction::copy_from(ptr::from_ref(&missing_supported) as u64) }.unwrap();
        assert!(!missing_supported.is_kernel_normalization_of(&requested));
    }

    #[test]
    fn rejected_request_only_candidate_rolls_back_its_normalized_kernel_action() {
        if std::env::var_os(REQUEST_ONLY_ROLLBACK_CHILD).is_some() {
            const SA_UNSUPPORTED: i32 = 0x0000_0400;
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let requested = Sigaction::empty(
                detached_close_external_handler as *const () as usize,
                libc::SA_RESTART | SA_UNSUPPORTED,
            );
            requested.replace(signum).unwrap();
            let normalized = kernel_current(signum).unwrap();
            assert!(!normalized.same_disposition(&requested));
            assert!(kernel_request_matches(&normalized, &requested));

            reject_committed_candidate(signum, requested, None, saved, None).unwrap();

            assert!(kernel_current(signum).unwrap().same_disposition(&saved));
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::rejected_request_only_candidate_rolls_back_its_normalized_kernel_action",
            REQUEST_ONLY_ROLLBACK_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated request-only rollback regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn reconcile_solidifies_a_request_only_node_with_the_exact_old_action() {
        if std::env::var_os(REQUEST_ONLY_RECONCILE_CHILD).is_some() {
            const SA_UNSUPPORTED: i32 = 0x0000_0400;
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let control = control();
            let visible = Sigaction::empty(0x2_7200, libc::SA_RESTART | SA_UNSUPPORTED);
            let requested = visible
                .for_kernel_stub(detached_close_external_handler as *const () as usize)
                .with_runtime_restorer();
            assert_eq!(requested.install(signum), 0);
            let actual_old = kernel_current(signum).unwrap();
            assert!(!actual_old.same_disposition(&requested));
            assert!(kernel_request_matches(&actual_old, &requested));

            let registration = registration(&control, signum, 18, visible);
            let mut descriptor = descriptor(
                &control,
                registration,
                visible,
                requested,
                SignalChain {
                    base: saved,
                    nodes: Vec::new(),
                },
            );
            descriptor.accepted_kernel = None;
            let mut registry = SignalRegistry::default();
            registry.stubs.insert(requested.handler(), descriptor);
            registry.chains.insert(
                signum,
                SignalChain {
                    base: saved,
                    nodes: vec![DispositionNode {
                        install_control: Arc::clone(&control),
                        install_owner: control.id(),
                        callback_owner: Some(control.id()),
                        guest: visible,
                        kernel: requested,
                        accepted_kernel: None,
                        registration: Some(registration),
                    }],
                },
            );

            let old = reconcile_prior_chain(&mut registry, signum, actual_old).unwrap();

            assert_eq!(old.handler(), visible.handler());
            assert!(
                registry.chains[&signum].nodes[0]
                    .accepted_kernel
                    .is_some_and(|accepted| accepted.same_disposition(&actual_old))
            );
            assert!(
                registry.stubs[&requested.handler()]
                    .accepted_kernel
                    .is_some_and(|accepted| accepted.same_disposition(&actual_old))
            );
            registration.deactivate();
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::reconcile_solidifies_a_request_only_node_with_the_exact_old_action",
            REQUEST_ONLY_RECONCILE_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated request-only reconciliation regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn compensation_preserves_a_newer_normalized_writer_by_exact_identity() {
        if std::env::var_os(EXACT_COMPENSATION_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let requested =
                Sigaction::for_signal(detached_close_external_handler as *const () as usize)
                    .with_runtime_restorer();
            requested.replace(signum).unwrap();
            let installed = kernel_current(signum).unwrap();
            assert!(installed.same_disposition(&requested));

            // libc replaces MIRVM's restorer with its own, producing a second
            // exact kernel snapshot that still satisfies the same request.
            assert_eq!(requested.install(signum), 0);
            let newer = kernel_current(signum).unwrap();
            assert!(!newer.same_disposition(&installed));
            assert!(kernel_request_matches(&newer, &installed));

            compensate_concurrent_writer(signum, installed, saved).unwrap();

            assert!(kernel_current(signum).unwrap().same_disposition(&newer));
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::compensation_preserves_a_newer_normalized_writer_by_exact_identity",
            EXACT_COMPENSATION_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated exact-compensation regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn detached_successor_accepts_distinct_valid_snapshots_of_its_predecessor() {
        if std::env::var_os(DETACHED_SNAPSHOT_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let first_control = control();
            let second_control = control();
            let third_control = control();

            install_signal(&first_control, signum, 0x2_7300, Some((19, 0x2_7300))).unwrap();
            let first_stub = kernel_current(signum).unwrap();
            install_signal(&second_control, signum, 0x2_7400, Some((20, 0x2_7400))).unwrap();
            let second_stub = kernel_current(signum).unwrap();

            let external =
                Sigaction::for_signal(detached_close_external_handler as *const () as usize);
            assert_eq!(external.install(signum), 0);
            assert_eq!(first_stub.install(signum), 0);

            // Reconciliation sees A after libc has replaced MIRVM's exact
            // restorer. B is detached, so its fallback still retains A's
            // earlier, equally valid exact snapshot.
            let native = Sigaction::for_signal(detached_close_native_handler as *const () as usize);
            install_sigaction_value(
                &first_control,
                signum,
                Some(native),
                Some(super::super::thunks::SignalHandlerResolution::Unknown),
            )
            .unwrap();
            {
                let registry = REGISTRY.lock().unwrap();
                let refreshed = registry.stubs[&first_stub.handler()]
                    .accepted_kernel
                    .unwrap();
                let detached = registry.stubs[&second_stub.handler()].fallback.nodes[0]
                    .accepted_kernel
                    .unwrap();
                assert!(!refreshed.same_disposition(&detached));
                assert!(kernel_request_matches(
                    &refreshed,
                    &registry.stubs[&first_stub.handler()].kernel
                ));
                assert!(kernel_request_matches(
                    &detached,
                    &registry.stubs[&first_stub.handler()].kernel
                ));
            }

            assert_eq!(second_stub.install(signum), 0);
            let old = install_signal(&third_control, signum, 0x2_7500, Some((21, 0x2_7500)))
                .expect("detached B could not rebuild through A's older valid snapshot");
            assert_eq!(old, 0x2_7400);

            assert_eq!(saved.install(signum), 0);
            deactivate_engine(&first_control).unwrap();
            deactivate_engine(&second_control).unwrap();
            deactivate_engine(&third_control).unwrap();
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::detached_successor_accepts_distinct_valid_snapshots_of_its_predecessor",
            DETACHED_SNAPSHOT_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated detached-snapshot regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn cleared_inbox_event_blocks_the_finalizing_seal_until_delivery_is_held() {
        let control = control();
        let signum = libc::SIGUSR1 as usize;
        let registration =
            registration(&control, signum as i32, 1, Sigaction::for_signal(0x1_0000));
        control.signal_inbox.register(registration);
        registration.publish(signum as i32);
        control.prepare_signal_seal_test();

        let (cleared_tx, cleared_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        *AFTER_INBOX_CLEAR_HOOK.lock().unwrap() = Some(AfterInboxClearHook {
            registration: ptr::from_ref(registration) as usize,
            callback: Box::new(move || {
                cleared_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
            }),
        });

        let worker_control = Arc::clone(&control);
        let worker = std::thread::spawn(move || {
            worker_control
                .signal_inbox
                .take_delivery(signum)
                .expect("published inbox event disappeared")
        });
        cleared_rx.recv().unwrap();
        let sealed_while_cleared = try_seal_engine(&control);
        resume_tx.send(()).unwrap();
        let taken = worker.join().unwrap();

        assert!(ptr::eq(taken.registration(), registration));
        assert!(
            !sealed_while_cleared,
            "Finalizing crossed the cleared inbox event before its delivery acquired a hold"
        );
        drop(taken);
        assert!(try_seal_engine(&control));
    }

    #[test]
    fn external_raise_handler_can_reenter_signal_install_and_raise() {
        if std::env::var_os(RAISE_INSTALL_RACE_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let reentrant_signum = libc::SIGURG;
            let reentrant_saved = kernel_current(reentrant_signum).unwrap();
            let _reentrant_restore = RestoreSignal {
                signum: reentrant_signum,
                action: reentrant_saved,
            };
            let external = Sigaction::for_signal(raise_external_handler as *const () as usize);
            assert_eq!(external.install(signum), 0);
            let reentrant = Sigaction::for_signal(reentrant_raise_handler as *const () as usize);
            assert_eq!(reentrant.install(reentrant_signum), 0);
            RAISE_EXTERNAL_RAN.store(0, Ordering::SeqCst);
            REENTRANT_RAISE_RAN.store(0, Ordering::SeqCst);
            REENTRANT_INSTALL_RESULT.store(0, Ordering::SeqCst);

            let control = control();
            *REENTRANT_INSTALL_CONTROL.lock().unwrap() = Some(Arc::clone(&control));
            assert_eq!(crate::os::process::raise(signum), 0);
            assert_eq!(RAISE_EXTERNAL_RAN.load(Ordering::SeqCst), 1);
            assert_eq!(REENTRANT_INSTALL_RESULT.load(Ordering::SeqCst), 1);
            assert_eq!(REENTRANT_RAISE_RAN.load(Ordering::SeqCst), 1);
            *REENTRANT_INSTALL_CONTROL.lock().unwrap() = None;
            deactivate_engine(&control).unwrap();
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::external_raise_handler_can_reenter_signal_install_and_raise",
            RAISE_INSTALL_RACE_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated raise/install regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn successful_install_commits_before_an_immediate_raw_overwrite() {
        if std::env::var_os(INSTALL_OVERWRITE_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let raw = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
            let captured_stub = Arc::new(Mutex::new(None));
            let hook_result = Arc::clone(&captured_stub);
            *AFTER_INSTALL_REPLACE_HOOK.lock().unwrap() =
                Some(Box::new(move |hook_signum, requested, actual_old| {
                    assert_eq!(hook_signum, signum);
                    assert!(actual_old.same_disposition(&saved));
                    let displaced = raw.replace(signum).unwrap();
                    assert!(kernel_request_matches(&displaced, &requested));
                    *hook_result.lock().unwrap() = Some(displaced);
                }));

            let control = control();
            let visible = Sigaction::for_signal(0x2_0000);
            let old = install_sigaction_value(
                &control,
                signum,
                Some(visible),
                Some(super::super::thunks::SignalHandlerResolution::Valid {
                    control: Arc::clone(&control),
                    func: 9,
                }),
            )
            .unwrap();
            assert!(old.same_disposition(&saved));
            assert!(kernel_current(signum).unwrap().satisfies_request(&raw));

            let captured_stub = captured_stub.lock().unwrap().unwrap();
            let descriptor = REGISTRY.lock().unwrap().stubs[&captured_stub.handler()].clone();
            assert!(
                descriptor.accepted_kernel.is_none(),
                "post-install query crossed the immediate raw overwrite"
            );
            let registration = descriptor.registration;
            assert!(registration.safe_point_delivery().is_some());

            // The raw writer retained the exact oldact even though its action
            // won immediately. Restoring that fixed stub must remain usable.
            assert_eq!(captured_stub.install(signum), 0);
            let delivery = current_delivery(signum).expect("committed stub became inactive");
            assert!(ptr::eq(delivery.registration(), registration));
            drop(delivery);

            deactivate_engine(&control).unwrap();
            assert!(kernel_current(signum).unwrap().satisfies_request(&saved));
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::successful_install_commits_before_an_immediate_raw_overwrite",
            INSTALL_OVERWRITE_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated install-overwrite regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn installed_stub_tracks_the_exact_kernel_normalized_action() {
        if std::env::var_os(NORMALIZED_ACTION_CHILD).is_some() {
            const SA_UNSUPPORTED: i32 = 0x0000_0400;
            const SA_EXPOSE_TAGBITS: i32 = 0x0000_0800;
            const UNKNOWN_PROBE_FLAG: i32 = 0x0000_0200;

            let signum = libc::SIGWINCH;
            let original = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: original,
            };
            let baseline =
                Sigaction::for_signal(detached_close_external_handler as *const () as usize);
            assert_eq!(baseline.install(signum), 0);
            let saved = kernel_current(signum).unwrap();
            let control = control();
            let requested = Sigaction::empty(
                0x2_7000,
                libc::SA_RESTART | SA_UNSUPPORTED | SA_EXPOSE_TAGBITS | UNKNOWN_PROBE_FLAG,
            );
            let old = install_sigaction_value(
                &control,
                signum,
                Some(requested),
                Some(super::super::thunks::SignalHandlerResolution::Valid {
                    control: Arc::clone(&control),
                    func: 16,
                }),
            )
            .unwrap();
            assert!(old.same_disposition(&saved));

            let installed = kernel_current(signum).unwrap();
            assert_eq!(
                installed.flags() & (SA_UNSUPPORTED | UNKNOWN_PROBE_FLAG),
                0,
                "kernel did not clear the probing flags used by this regression"
            );
            assert_ne!(
                installed.flags() & SA_EXPOSE_TAGBITS,
                0,
                "kernel cleared the supported SA_EXPOSE_TAGBITS probe"
            );
            let descriptor = REGISTRY.lock().unwrap().stubs[&installed.handler()].clone();
            assert!(
                installed.is_kernel_normalization_of(&descriptor.kernel),
                "kernel action is not a strict normalization of the recorded request"
            );
            assert!(
                descriptor
                    .accepted_kernel
                    .is_some_and(|accepted| accepted.same_disposition(&installed)),
                "read-only post-install observation did not retain the exact accepted action"
            );

            // A raw writer may retain and later restore the exact fixed-stub
            // oldact. Wrapped query must still expose only the normalized guest
            // action, including the kernel-cleared probing bits.
            let fixed_oldact = saved.replace(signum).unwrap();
            assert!(fixed_oldact.same_disposition(&installed));
            assert_eq!(fixed_oldact.install(signum), 0);
            let visible = install_sigaction_value(&control, signum, None, None).unwrap();
            assert_eq!(visible.handler(), requested.handler());
            assert_eq!(visible.flags() & (SA_UNSUPPORTED | UNKNOWN_PROBE_FLAG), 0);
            assert_ne!(visible.flags() & SA_EXPOSE_TAGBITS, 0);

            // Editing a retained oldact must preserve ordinary mask/flag edits
            // while stripping MIRVM's SA_SIGINFO/restorer adapter details.
            let mut edited: libc::sigaction = unsafe { std::mem::zeroed() };
            fixed_oldact.write_to(ptr::from_mut(&mut edited) as u64);
            edited.sa_flags |= libc::SA_NOCLDSTOP;
            assert_eq!(
                unsafe { libc::sigaddset(&mut edited.sa_mask, libc::SIGUSR2) },
                0
            );
            let edited = unsafe { Sigaction::copy_from(ptr::from_ref(&edited) as u64) }.unwrap();
            let replaced = install_sigaction_value(
                &control,
                signum,
                Some(edited),
                Some(super::super::thunks::SignalHandlerResolution::Unknown),
            )
            .unwrap();
            assert!(replaced.same_disposition(&visible));
            let edited_visible = install_sigaction_value(&control, signum, None, None).unwrap();
            assert_eq!(edited_visible.handler(), requested.handler());
            assert_ne!(edited_visible.flags() & libc::SA_NOCLDSTOP, 0);
            assert_eq!(edited_visible.flags() & libc::SA_SIGINFO, 0);
            assert_eq!(
                edited_visible.flags() & (SA_UNSUPPORTED | UNKNOWN_PROBE_FLAG),
                0
            );
            let mut edited_visible_raw: libc::sigaction = unsafe { std::mem::zeroed() };
            edited_visible.write_to(ptr::from_mut(&mut edited_visible_raw) as u64);
            assert!(edited_visible_raw.sa_restorer.is_none());
            assert_eq!(
                unsafe { libc::sigismember(&edited_visible_raw.sa_mask, libc::SIGUSR2) },
                1
            );

            deactivate_engine(&control).unwrap();
            assert!(kernel_current(signum).unwrap().same_disposition(&saved));
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::installed_stub_tracks_the_exact_kernel_normalized_action",
            NORMALIZED_ACTION_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated normalized-action regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn close_restores_an_exact_kernel_restorer_snapshot() {
        if std::env::var_os(EXACT_RESTORE_CHILD).is_some() {
            const SA_RESTORER: i32 = 0x0400_0000;
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };

            let mut raw: libc::sigaction = unsafe { std::mem::zeroed() };
            raw.sa_sigaction = detached_close_external_handler as *const () as usize;
            raw.sa_flags = libc::SA_RESTART | SA_RESTORER;
            raw.sa_restorer = Some(first_test_restorer);
            assert_eq!(unsafe { libc::sigemptyset(&mut raw.sa_mask) }, 0);
            assert_eq!(
                unsafe { libc::sigaddset(&mut raw.sa_mask, libc::SIGUSR2) },
                0
            );
            let raw = unsafe { Sigaction::copy_from(ptr::from_ref(&raw) as u64) }.unwrap();
            raw.replace_exact(signum).unwrap();
            let baseline = kernel_current(signum).unwrap();
            assert!(baseline.same_disposition(&raw));

            let control = control();
            install_signal(&control, signum, 0x2_7100, Some((17, 0x2_7100))).unwrap();
            deactivate_engine(&control).unwrap();
            assert!(kernel_current(signum).unwrap().same_disposition(&baseline));
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::close_restores_an_exact_kernel_restorer_snapshot",
            EXACT_RESTORE_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated exact-restorer regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn current_delivery_follows_live_lower_and_detached_fixed_stubs() {
        if std::env::var_os(CURRENT_DELIVERY_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let first_control = control();
            let second_control = control();
            let third_control = control();

            install_signal(&first_control, signum, 0x2_1000, Some((10, 0x2_1000))).unwrap();
            let first_stub = kernel_current(signum).unwrap();
            let first_registration =
                REGISTRY.lock().unwrap().stubs[&first_stub.handler()].registration;
            install_signal(&second_control, signum, 0x2_2000, Some((11, 0x2_2000))).unwrap();

            // A raw restore can make a lower live node current.
            assert_eq!(first_stub.install(signum), 0);
            let lower = current_delivery(signum).expect("live lower stub was not resolved");
            assert!(ptr::eq(lower.registration(), first_registration));
            drop(lower);

            // Replace the managed prefix with native state, then install a new
            // managed node. The first stub now exists only in its descriptor.
            let raw = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
            assert_eq!(raw.install(signum), 0);
            install_signal(&third_control, signum, 0x2_3000, Some((12, 0x2_3000))).unwrap();
            assert_eq!(first_stub.install(signum), 0);
            let detached = current_delivery(signum).expect("detached stub was not resolved");
            assert!(ptr::eq(detached.registration(), first_registration));
            drop(detached);

            assert_eq!(saved.install(signum), 0);
            deactivate_engine(&first_control).unwrap();
            deactivate_engine(&second_control).unwrap();
            deactivate_engine(&third_control).unwrap();
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::current_delivery_follows_live_lower_and_detached_fixed_stubs",
            CURRENT_DELIVERY_CHILD,
        );
        assert!(
            output.status.success(),
            "isolated current-delivery regression failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn raw_restore_of_a_closed_fixed_stub_exits_seventy() {
        if std::env::var_os(INACTIVE_STUB_CHILD).is_some() {
            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let control = control();
            install_signal(&control, signum, 0x2_4000, Some((13, 0x2_4000))).unwrap();
            let raw = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
            let closed_stub = raw.replace(signum).unwrap();
            deactivate_engine(&control).unwrap();

            // A raw caller kept MIRVM's oldact past the callback owner's
            // lifetime. The process must not continue after silently losing
            // the signal through that dangling callback address.
            assert_eq!(closed_stub.install(signum), 0);
            assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
            let _ = saved;
            panic!("inactive fixed stub returned from its signal adapter");
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::raw_restore_of_a_closed_fixed_stub_exits_seventy",
            INACTIVE_STUB_CHILD,
        );
        assert_eq!(
            output.status.code(),
            Some(70),
            "inactive fixed stub child did not fail loud; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn wrapped_raise_of_a_raw_restored_closed_stub_faults_without_retrying_forever() {
        if std::env::var_os(HOST_RAISE_INACTIVE_STUB_CHILD).is_some() {
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(5));
                unsafe { libc::_exit(124) }
            });

            let signum = libc::SIGWINCH;
            let saved = kernel_current(signum).unwrap();
            let _restore = RestoreSignal {
                signum,
                action: saved,
            };
            let owner = super::super::ctx::Engine::new(Shared::new(Module::default()));
            install_signal(owner.control(), signum, 0x2_4100, Some((14, 0x2_4100))).unwrap();
            let closed_stub = kernel_current(signum).unwrap();
            owner.wait_closed().unwrap();
            closed_stub.replace(signum).unwrap();

            let raiser = super::super::ctx::Engine::new(Shared::new(Module::default()));
            let activation = super::super::ctx::activate(raiser.shared());
            let exception = super::super::unwind::catch_raw(|| {
                super::super::ctx::raise_signal(activation.ctx(), signum)
            })
            .expect_err("HostRaise unexpectedly returned through a closed fixed stub");
            let fault = exception
                .take_engine_fault(raiser.shared())
                .unwrap_or_else(|exception| exception.resume_or_rethrow())
                .finish();
            assert_eq!(fault.code, 70);
            assert!(fault.message.contains("closed MIRVM fixed stub"));

            // Changing the surrounding action cannot make the closed handler
            // address live again. In particular, a raw oldact editor must not
            // turn the prompt failure above into an endless retry loop.
            let mut edited: libc::sigaction = unsafe { std::mem::zeroed() };
            closed_stub.write_to(ptr::from_mut(&mut edited) as u64);
            assert_eq!(
                unsafe { libc::sigaddset(&mut edited.sa_mask, libc::SIGUSR2) },
                0
            );
            let edited = unsafe { Sigaction::copy_from(ptr::from_ref(&edited) as u64) }.unwrap();
            assert_eq!(edited.install(signum), 0);
            let exception = super::super::unwind::catch_raw(|| {
                super::super::ctx::raise_signal(activation.ctx(), signum)
            })
            .expect_err("HostRaise retried an edited closed fixed stub forever");
            let fault = exception
                .take_engine_fault(raiser.shared())
                .unwrap_or_else(|exception| exception.resume_or_rethrow())
                .finish();
            assert_eq!(fault.code, 70);
            assert!(fault.message.contains("closed MIRVM fixed stub"));
            drop(activation);
            raiser.wait_closed().unwrap();
            return;
        }

        let output = run_signal_test_child(
            "vm::engine::signal::tests::wrapped_raise_of_a_raw_restored_closed_stub_faults_without_retrying_forever",
            HOST_RAISE_INACTIVE_STUB_CHILD,
        );
        assert!(
            output.status.success(),
            "wrapped HostRaise against an inactive fixed stub did not fail promptly: status={:?}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn stale_chain_keeps_the_matching_lower_stub_and_its_accepted_event() {
        let first_control = control();
        let second_control = control();
        let signum = libc::SIGUSR1;
        let base = Sigaction::for_signal(0x2000);
        let first_visible = Sigaction::for_signal(0x3000);
        let second_visible = Sigaction::for_signal(0x4000);
        let first_kernel = first_visible.for_kernel_stub(0x5000);
        let second_kernel = second_visible.for_kernel_stub(0x6000);
        let first = registration(&first_control, signum, 1, first_visible);
        let second = registration(&second_control, signum, 2, second_visible);
        first_control.signal_inbox.register(first);
        second_control.signal_inbox.register(second);
        second.publish(signum);

        let mut registry = SignalRegistry::default();
        registry.stubs.insert(
            first_kernel.handler(),
            descriptor(
                &first_control,
                first,
                first_visible,
                first_kernel,
                SignalChain {
                    base,
                    nodes: Vec::new(),
                },
            ),
        );
        registry.stubs.insert(
            second_kernel.handler(),
            descriptor(
                &second_control,
                second,
                second_visible,
                second_kernel,
                SignalChain {
                    base,
                    nodes: vec![DispositionNode {
                        install_control: Arc::clone(&first_control),
                        install_owner: first_control.id(),
                        callback_owner: Some(first_control.id()),
                        guest: first_visible,
                        kernel: first_kernel,
                        accepted_kernel: Some(first_kernel),
                        registration: Some(first),
                    }],
                },
            ),
        );
        registry.chains.insert(
            signum,
            SignalChain {
                base,
                nodes: vec![
                    DispositionNode {
                        install_control: Arc::clone(&first_control),
                        install_owner: first_control.id(),
                        callback_owner: Some(first_control.id()),
                        guest: first_visible,
                        kernel: first_kernel,
                        accepted_kernel: Some(first_kernel),
                        registration: Some(first),
                    },
                    DispositionNode {
                        install_control: Arc::clone(&second_control),
                        install_owner: second_control.id(),
                        callback_owner: Some(second_control.id()),
                        guest: second_visible,
                        kernel: second_kernel,
                        accepted_kernel: Some(second_kernel),
                        registration: Some(second),
                    },
                ],
            },
        );

        let old = reconcile_prior_chain(&mut registry, signum, first_kernel).unwrap();

        assert!(old.same_disposition(&first_visible));
        let chain = &registry.chains[&signum];
        assert_eq!(chain.nodes.len(), 1);
        assert!(chain.nodes[0].kernel.same_disposition(&first_kernel));
        assert!(second_control.signal_inbox.has_pending());
        // Removing a committed stub from the current logical chain must not
        // close its process-lifetime adapter. Native code may restore it later
        // while both of its owners are still live.
        assert!(second.safe_point_delivery().is_some());
        assert!(first.safe_point_delivery().is_some());
    }

    #[test]
    fn detached_successor_folds_around_a_closed_predecessor_and_checks_full_action() {
        let first_control = control();
        let second_control = control();
        let signum = libc::SIGUSR1;
        let base = Sigaction::for_signal(0x7000);
        let first_visible = Sigaction::for_signal(0x8000);
        let second_visible = Sigaction::for_signal(0x9000);
        let first_kernel = first_visible.for_kernel_stub(0xa000);
        let second_kernel = second_visible.for_kernel_stub(0xb000);
        let first = registration(&first_control, signum, 3, first_visible);
        let second = registration(&second_control, signum, 4, second_visible);
        let mut registry = SignalRegistry::default();
        registry.stubs.insert(
            first_kernel.handler(),
            descriptor(
                &first_control,
                first,
                first_visible,
                first_kernel,
                SignalChain {
                    base,
                    nodes: Vec::new(),
                },
            ),
        );
        registry.stubs.insert(
            second_kernel.handler(),
            descriptor(
                &second_control,
                second,
                second_visible,
                second_kernel,
                SignalChain {
                    base,
                    nodes: vec![DispositionNode {
                        install_control: Arc::clone(&first_control),
                        install_owner: first_control.id(),
                        callback_owner: Some(first_control.id()),
                        guest: first_visible,
                        kernel: first_kernel,
                        accepted_kernel: Some(first_kernel),
                        registration: Some(first),
                    }],
                },
            ),
        );

        let mut retired = Vec::new();
        deactivate_owner_descriptors(&mut registry, first_control.id(), &mut retired);
        let second_descriptor = &registry.stubs[&second_kernel.handler()];
        assert!(second_descriptor.fallback.base.same_disposition(&base));
        assert!(second_descriptor.fallback.nodes.is_empty());

        let (visible, rebuilt) = plan_stub_chain(&registry, signum, second_kernel)
            .unwrap()
            .unwrap();
        assert!(visible.same_disposition(&second_visible));
        assert!(rebuilt.base.same_disposition(&base));
        assert_eq!(rebuilt.nodes.len(), 1);

        let mismatched = Sigaction::for_signal(second_kernel.handler());
        assert!(plan_stub_chain(&registry, signum, mismatched).is_err());
    }

    #[test]
    fn detached_stub_snapshot_drops_a_closed_external_native_predecessor() {
        let native_control = control();
        let callback_control = control();
        let signum = libc::SIGUSR1;
        let base = Sigaction::for_signal(0xc000);
        let native = Sigaction::for_signal(0xd000);
        let visible = Sigaction::for_signal(0xe000);
        let kernel = visible.for_kernel_stub(0xf000);
        let registration = registration(&callback_control, signum, 5, visible);
        let mut registry = SignalRegistry::default();
        registry.stubs.insert(
            kernel.handler(),
            descriptor(
                &callback_control,
                registration,
                visible,
                kernel,
                SignalChain {
                    base,
                    nodes: vec![node(&native_control, native, native, None)],
                },
            ),
        );

        let mut retired = Vec::new();
        deactivate_owner_descriptors(&mut registry, native_control.id(), &mut retired);
        let (_, rebuilt) = plan_stub_chain(&registry, signum, kernel).unwrap().unwrap();

        assert!(rebuilt.base.same_disposition(&base));
        assert_eq!(rebuilt.nodes.len(), 1);
        assert!(rebuilt.nodes[0].kernel.same_disposition(&kernel));
    }

    #[test]
    fn exposed_fixed_stub_is_canonicalized_before_becoming_guest_visible_again() {
        let control = control();
        let signum = libc::SIGUSR1;
        let base = Sigaction::for_signal(0x1_0000);
        let visible = Sigaction::for_signal(0x1_1000);
        let kernel = visible.for_kernel_stub(0x1_2000);
        let registration = registration(&control, signum, 6, visible);
        let mut registry = SignalRegistry::default();
        registry.stubs.insert(
            kernel.handler(),
            descriptor(
                &control,
                registration,
                visible,
                kernel,
                SignalChain {
                    base,
                    nodes: Vec::new(),
                },
            ),
        );

        let exact = canonical_guest_action(&registry, kernel);
        let signal_style =
            canonical_guest_action(&registry, Sigaction::for_signal(kernel.handler()));

        assert!(exact.same_disposition(&visible));
        assert_eq!(signal_style.handler(), visible.handler());
        assert_eq!(signal_style.flags(), libc::SA_RESTART);
    }

    #[test]
    fn close_restores_the_predecessor_of_a_detached_current_stub() {
        let signum = libc::SIGURG;
        let saved = kernel_current(signum).unwrap();
        let _restore = RestoreSignal {
            signum,
            action: saved,
        };
        let native = Sigaction::for_signal(detached_close_native_handler as *const () as usize);
        assert_eq!(native.install(signum), 0);
        let baseline = kernel_current(signum).unwrap();
        let first_control = control();
        let second_control = control();

        install_signal(&first_control, signum, 0x1_3000, Some((7, 0x1_3000))).unwrap();
        let first_stub = kernel_current(signum).unwrap();
        let external = Sigaction::for_signal(detached_close_external_handler as *const () as usize);
        assert_eq!(external.install(signum), 0);
        install_signal(&second_control, signum, 0x1_4000, Some((8, 0x1_4000))).unwrap();
        assert_eq!(first_stub.install(signum), 0);

        assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
        for _ in 0..10_000 {
            if first_control.signal_inbox.has_pending() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            first_control.signal_inbox.has_pending(),
            "a detached process-lifetime stub stopped accepting signals while its owner was live"
        );
        let accepted = first_control
            .signal_inbox
            .take_delivery(signum as usize)
            .expect("detached stub event was not available at the owner safe point");
        let first_registration = REGISTRY.lock().unwrap().stubs[&first_stub.handler()].registration;
        assert!(ptr::eq(accepted.registration(), first_registration));
        drop(accepted);

        deactivate_engine(&first_control).unwrap();
        let after_first_close = kernel_current(signum).unwrap();
        DETACHED_CLOSE_NATIVE_RAN.store(0, Ordering::SeqCst);
        assert_eq!(unsafe { libc::kill(libc::getpid(), signum) }, 0);
        for _ in 0..10_000 {
            if DETACHED_CLOSE_NATIVE_RAN.load(Ordering::SeqCst) != 0 {
                break;
            }
            std::thread::yield_now();
        }
        let native_ran = DETACHED_CLOSE_NATIVE_RAN.load(Ordering::SeqCst);
        deactivate_engine(&second_control).unwrap();
        let after_second_close = kernel_current(signum).unwrap();

        assert!(after_first_close.same_disposition(&baseline));
        assert_eq!(native_ran, 1);
        assert!(after_second_close.same_disposition(&baseline));
    }
}
