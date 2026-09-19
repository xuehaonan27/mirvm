//! Per-thread signal mailboxes and the registration cells that feed them.

use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use super::{DeferredSignalCallback, SIGNAL_SLOTS};
use crate::os::signal::Sigaction;
use crate::vm::engine::ctx::EngineControl;

const DELIVERY_ACTIVE: usize = 1usize << (usize::BITS - 1);
const DELIVERY_COUNT: usize = !DELIVERY_ACTIVE;

/// Stable, process-lifetime metadata addressed directly by a signal stub.
///
/// Every guest installation gets a fresh registration. Both this allocation and
/// its executable stub are deliberately leaked: a kernel frame may already hold
/// the old handler address when another thread replaces or closes the Engine.
pub(crate) struct SignalRegistration {
    pub(super) control: Arc<EngineControl>,
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
    pub(super) kernel_frames: AtomicUsize,
    /// Number of target-pthread slots that still contain this registration.
    /// Close reads this through the callback owner's registration list; only
    /// the target pthread may turn one of these counts into a lifecycle hold.
    pub(super) thread_pending: AtomicUsize,
    pending: [AtomicBool; SIGNAL_SLOTS],
    next_owner: AtomicPtr<SignalRegistration>,
}

impl SignalRegistration {
    pub(super) fn new(
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

    pub(super) fn signum(&self) -> i32 {
        self.signum
    }

    pub(crate) fn mask_bits(&self) -> u64 {
        self.mask_bits
    }

    pub(super) fn generation(&self) -> u64 {
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

    pub(super) fn accepts_kernel_delivery(&self) -> bool {
        self.kernel_delivery.load(Ordering::Acquire) & DELIVERY_ACTIVE != 0
    }

    #[cfg(test)]
    pub(super) fn safe_point_delivery(&'static self) -> Option<SignalDeliveryGuard> {
        if self.kernel_delivery.load(Ordering::Acquire) & DELIVERY_ACTIVE == 0 {
            return None;
        }
        let hold = super::super::ctx::DeferredHold::acquire(&self.control, true).ok()?;
        if self.kernel_delivery.load(Ordering::Acquire) & DELIVERY_ACTIVE == 0 {
            drop(hold);
            return None;
        }
        Some(SignalDeliveryGuard {
            registration: self,
            _hold: hold,
        })
    }

    pub(super) fn deactivate(&self) {
        self.kernel_delivery
            .fetch_and(DELIVERY_COUNT, Ordering::AcqRel);
    }

    pub(super) fn wait_for_kernel_deliveries(&self) {
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

    pub(super) fn publish(&self, signum: i32) {
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
pub(super) struct ThreadSignalCell {
    registration: AtomicPtr<SignalRegistration>,
    pending: AtomicBool,
    next: AtomicPtr<ThreadSignalCell>,
}

pub(super) struct ThreadSignalInbox {
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
    pub(super) fn cell_for(
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

pub(super) fn current_thread_inbox() -> Option<&'static ThreadSignalInbox> {
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
        let Ok(hold) = super::super::ctx::DeferredHold::acquire(registration.control(), true)
        else {
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
    _hold: super::super::ctx::DeferredHold,
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

    pub(super) fn register(&self, registration: &'static SignalRegistration) {
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

    pub(super) fn registrations(&self) -> SignalRegistrationIter {
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
            let Ok(hold) = super::super::ctx::DeferredHold::acquire(&registration.control, true)
            else {
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
                super::run_after_inbox_clear_hook(registration);
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

pub(super) struct SignalRegistrationIter {
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
