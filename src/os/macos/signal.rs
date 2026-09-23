//! macOS signal primitives.
//!
//! The same division as the other platform's half: the signal and action flags, the
//! `libc::sigaction` wrapper and its query/install paths, and the structure layout encapsulated so
//! the engine can edit a copy without touching the guest's.
//!
//! What this kernel decides differently, and therefore what this file says differently:
//!
//! - There is no realtime signal range. The traditional numbering is the whole space, so
//!   [`is_realtime`] answers `false` for every number rather than for a suffix of them.
//! - `si_code` carries `SI_USER` for a `kill`, a `raise` and a `pthread_kill` alike, so
//!   [`sent_by_thread_kill`] cannot read the direction out of it and reads `si_pid` instead.
//! - The kernel strips the two unmaskable signals out of `sa_mask` exactly as the other one does
//!   (measured), so the guest-visible copy is normalized the same way.
//!
//! The half of the knowledge the CPU supplies — the entry stub, the restorer flag and restorer
//! slot, and the `ucontext_t` fields the debug dump reads — lives in [`crate::os_arch::signal`],
//! because the kernel defines the interface and the CPU encodes it. This file stays CPU-neutral and
//! reaches it through that name.

use crate::os::signal::{MaskOp, SignalMask, ThreadSignalMaskGuard, set_thread_mask};
use crate::os_arch::signal as arch;

/// The kernel's `siginfo_t` as an `SA_SIGINFO` handler receives it.
///
/// A handler's signature is the kernel's, so the argument cannot be made opaque; naming the pointer
/// type here is what lets the engine's adapters stay free of the platform's type names.
pub type SignalInfo = *mut libc::siginfo_t;

/// The kernel's `si_code` for a delivery a user sent, by any of `kill`, `raise` and
/// `pthread_kill`.
///
/// Spelled here because this platform's `libc` bindings do not name it. POSIX fixes the value at
/// zero and the kernel's own `siginfo.h` agrees.
const SI_USER: i32 = 0;

/// The `si_code` this kernel stamps on a delivery the C library's `raise` produced.
///
/// It is the generic code, which this kernel reports for every delivery a process sends itself, so
/// it does not separate a `raise` from a process-directed `kill`; what does that here is the
/// sender, as [`sent_by_thread_kill`] explains. A caller that asserts which delivery happened
/// therefore takes each platform's own answer.
#[cfg(test)]
pub const RAISE_DELIVERY_CODE: i32 = SI_USER;

/// The kernel's `si_code` for a delivery a handler received.
///
/// A null `info` is not an error: an action installed without `SA_SIGINFO` reaches its handler with
/// no siginfo at all, and that is a fact about the delivery rather than a failure.
pub fn info_code(info: SignalInfo) -> i32 {
    if info.is_null() {
        return 0;
    }
    unsafe { (*info).si_code }
}

/// The SEGV dump is installed and implemented by the pair: its handler reads the fault instruction
/// and the fault address out of the architecture's own `ucontext_t`.
pub use crate::os_arch::signal::install_segv_dump;

// This kernel promises that these long-established flags survive `sigaction`. There is no
// `SA_EXPOSE_TAGBITS` here, and the flag the other platform carries for probing which optional bits
// a kernel understands has no counterpart either.
const STABLE_KERNEL_FLAGS: i32 = libc::SA_NOCLDSTOP
    | libc::SA_NOCLDWAIT
    | libc::SA_SIGINFO
    | libc::SA_ONSTACK
    | libc::SA_RESTART
    | libc::SA_NODEFER
    | libc::SA_RESETHAND;

// Synchronous fault signals.
pub const SIGSEGV: i32 = libc::SIGSEGV;
pub const SIGBUS: i32 = libc::SIGBUS;
pub const SIGFPE: i32 = libc::SIGFPE;
pub const SIGILL: i32 = libc::SIGILL;
pub const SIGTRAP: i32 = libc::SIGTRAP;

// The signals and action flags below are the platform's, but no product path names one: the engine
// works from the number the guest passed. They are here for a caller that has to spell a signal or
// a flag, which today are this crate's tests; the fault signals above are not, because delivery
// branches on them.
#[cfg(test)]
pub const SIGHUP: i32 = libc::SIGHUP;
#[cfg(test)]
pub const SIGKILL: i32 = libc::SIGKILL;
#[cfg(test)]
pub const SIGSTOP: i32 = libc::SIGSTOP;
#[cfg(test)]
pub const SIGTERM: i32 = libc::SIGTERM;
#[cfg(test)]
pub const SIGUSR1: i32 = libc::SIGUSR1;
#[cfg(test)]
pub const SIGUSR2: i32 = libc::SIGUSR2;
#[cfg(test)]
pub const SIGURG: i32 = libc::SIGURG;
#[cfg(test)]
pub const SIGWINCH: i32 = libc::SIGWINCH;
#[cfg(test)]
pub const SA_NOCLDSTOP: i32 = libc::SA_NOCLDSTOP;
#[cfg(test)]
pub const SA_RESTART: i32 = libc::SA_RESTART;
#[cfg(test)]
pub const SA_SIGINFO: i32 = libc::SA_SIGINFO;

/// Upper bound on this kernel's traditional signal numbers: `SIGUSR2` is the last one it assigns.
///
/// Queued deliveries need a realtime range and a `siginfo` queue, and this kernel has neither, which
/// is what makes every number outside this range unusable rather than merely non-traditional.
pub const STANDARD_SIGNAL_MAX: i32 = 31;

/// The first number past the traditional range, which is where this kernel's signal space ends.
///
/// It is not a "minimum realtime signal" the way the other platform's is, because there is no
/// The first number past the traditional range, which is where this kernel's signal space ends.
///
/// It is not a "minimum realtime signal" the way the other platform's is, because there is no
/// realtime range here; it is the first number the kernel refuses. The mask builders below use it
/// as the exclusive upper bound of the traditional range, which is the same role the other
/// platform's answer plays there.
///
/// NOTE: a caller that wants a number this kernel accepts for an ordinary signal has to name one,
/// and the tests that reach for this as "a real signal to query" will not get one here.
pub fn realtime_min() -> i32 {
    STANDARD_SIGNAL_MAX + 1
}

/// Whether `signum` needs the queued delivery this engine does not implement.
///
/// Every number this kernel accepts is a traditional one, so the answer is always no; a number
/// outside [`STANDARD_SIGNAL_MAX`] is refused by [`crate::vm::signal::install`]'s range check
/// before this is reached.
pub fn is_realtime(_signum: i32) -> bool {
    false
}

/// Whether a delivery reaches the thread it was addressed to.
///
/// This is the one place the two platforms disagree about a *fact* rather than about a spelling.
/// Linux reports `SI_TKILL` for a `tkill`/`tgkill` and `SI_USER` for a process-directed `kill`, so
/// the answer is the `si_code`. This kernel reports `SI_USER` for `kill`, `raise` and
/// `pthread_kill` alike, with the sender's pid in `si_pid` (measured), so `si_code` carries no
/// direction at all.
///
/// What separates the two cases here is the sender: a delivery this process sent itself is one it
/// addressed to a thread of its own, and a delivery from another process is process-directed.
///
/// NOTE: a guest `kill(getpid(), signum)` is process-directed and is reported here as
/// thread-directed. The two readings differ in which registry the engine credits, and the thread
/// the kernel chose is this one either way.
pub fn sent_by_thread_kill(info: SignalInfo) -> bool {
    if info.is_null() {
        return false;
    }
    // `info_code` is the same read the delivery path makes; the direction is not in it here, so
    // the sender is what separates a delivery this process addressed to a thread from one another
    // process sent.
    info_code(info) == SI_USER && unsafe { (*info).si_pid } == unsafe { libc::getpid() }
}

/// `kill(2)`: deliver `signum` to the process `pid`, returning the library's code and leaving
/// `errno` for the caller.
#[cfg(test)]
pub fn kill(pid: i32, signum: i32) -> i32 {
    unsafe { libc::kill(pid, signum) }
}

/// `pthread_kill`: deliver `signum` to one host thread, returning the library's code.
///
/// Address a delivery to a thread rather than to the process whenever the intended recipient is
/// known, because a process-directed delivery lands on whichever thread the kernel picks.
#[cfg(test)]
pub fn send_to_thread(thread: crate::os::thread::ThreadId, signum: i32) -> i32 {
    unsafe { libc::pthread_kill(thread.raw(), signum) }
}

/// Deliver `signum` to the calling thread.
#[cfg(test)]
pub fn send_to_current_thread(signum: i32) -> i32 {
    send_to_thread(crate::os::thread::current_thread(), signum)
}

/// A sigaction structure (layout knowledge encapsulated). The engine edits a
/// copy's handler and writes it back to the kernel; the original guest structure
/// stays untouched because the guest may reuse or read it back.
///
/// `repr(transparent)`: an address of one of these is handed to the kernel's `sigaction`, and an
/// interposed `sigaction` receives its caller's own address for the same structure.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Sigaction(libc::sigaction);

/// Everything that can be blocked: every traditional signal except the two POSIX forbids.
///
/// The range is this kernel's numbering rather than the mask type's, which is why the constructor
/// is a function here and not a method on the mask.
pub fn all_blockable_mask() -> SignalMask {
    let mut mask = SignalMask::empty();
    for signum in 1..realtime_min() {
        if matches!(signum, libc::SIGKILL | libc::SIGSTOP) {
            continue;
        }
        unsafe { libc::sigaddset(&mut mask.0, signum) };
    }
    mask
}

/// One delivery the blocking wait took out of the pending set.
#[cfg(test)]
pub struct PendingSignal {
    signum: i32,
    code: i32,
}

#[cfg(test)]
impl PendingSignal {
    /// The signal that was pending.
    pub fn signum(&self) -> i32 {
        self.signum
    }

    /// The kernel's `si_code`.
    pub fn code(&self) -> i32 {
        self.code
    }
}

/// `sigwait` with no timeout: take one signal pending in `mask`, blocking until one arrives.
///
/// This platform has no `sigtimedwait`, so the wait cannot report a `siginfo`; the code is reported
/// as `SI_USER`, which is what a delivery from this process carries anyway. A caller that consumed
/// a delivery this way has to know that a subsequent observation would see nothing. `mask` must be
/// blocked on the calling thread.
#[cfg(test)]
pub fn wait_pending(mask: &SignalMask) -> Result<PendingSignal, i32> {
    let mut signum = 0 as libc::c_int;
    let result = unsafe { libc::sigwait(&mask.0, &mut signum) };
    if result != 0 {
        Err(result)
    } else {
        Ok(PendingSignal {
            signum,
            code: SI_USER,
        })
    }
}

impl Sigaction {
    fn same_mask(&self, other: &Self) -> bool {
        (1..realtime_min())
            .filter(|&signum| signum != libc::SIGKILL && signum != libc::SIGSTOP)
            .all(|signum| unsafe {
                libc::sigismember(&self.0.sa_mask, signum)
                    == libc::sigismember(&other.0.sa_mask, signum)
            })
    }

    /// Copies from a guest-side `act` pointer (pointer 0 -> None, matching the
    /// kernel's act=NULL semantics).
    ///
    /// # Safety
    /// When `ptr` is non-zero it must point to a complete sigaction structure in
    /// the guest address space (true address model).
    pub unsafe fn copy_from(ptr: u64) -> Option<Self> {
        if ptr == 0 {
            return None;
        }
        Some(Sigaction(unsafe { *(ptr as *const libc::sigaction) }))
    }

    pub fn handler(&self) -> usize {
        self.0.sa_sigaction
    }

    /// Standard signals currently blocked by the calling pthread. MIRVM uses
    /// this together with its logical deferred-handler mask before choosing a
    /// safe-point delivery.
    pub fn current_standard_mask_bits() -> Result<u64, i32> {
        let mut current: libc::sigset_t = unsafe { std::mem::zeroed() };
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut current) };
        if result != 0 {
            return Err(result);
        }
        let mut bits = 0u64;
        for signum in 1..realtime_min() {
            if unsafe { libc::sigismember(&current, signum) } == 1 {
                bits |= 1u64 << signum;
            }
        }
        Ok(bits)
    }

    /// This kernel silently removes the two unmaskable signals from `sa_mask`, as the other one
    /// does (measured). Keep the guest-visible copy identical to the disposition the kernel
    /// actually accepted instead of remembering impossible mask bits.
    pub fn normalized_for_kernel(mut self) -> Self {
        unsafe {
            libc::sigdelset(&mut self.0.sa_mask, libc::SIGKILL);
            libc::sigdelset(&mut self.0.sa_mask, libc::SIGSTOP);
        }
        self
    }

    pub fn has_unsupported_guest_flags(&self) -> bool {
        self.0.sa_flags
            & (libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_NODEFER | libc::SA_RESETHAND)
            != 0
    }

    pub fn flags(&self) -> i32 {
        self.0.sa_flags
    }

    /// Add `flags` to the action. Used by a caller that edits an action the kernel
    /// returned, where the kernel's own bits have to survive.
    #[cfg(test)]
    pub fn or_flags(&mut self, flags: i32) {
        self.0.sa_flags |= flags;
    }

    /// The restorer this action names. None: this kernel's actions carry none, so a caller-visible
    /// disposition never shows one.
    #[cfg(test)]
    pub fn restorer(&self) -> Option<extern "C" fn()> {
        None
    }

    /// No-op: this kernel's `libc::sigaction` has no restorer field to put one in, which is also why
    /// the pair beside this file provides no writer for one.
    #[cfg(test)]
    pub fn set_restorer(&mut self, _restorer: extern "C" fn()) {}

    /// Add `signum` to the mask this action installs while its handler runs.
    #[cfg(test)]
    pub fn add_to_mask(&mut self, signum: i32) {
        unsafe { libc::sigaddset(&mut self.0.sa_mask, signum) };
    }

    /// Block every signal in the mask, which is what a set built for the blocking wait wants: the
    /// two the kernel cannot block are dropped by the kernel, not by this call.
    #[cfg(test)]
    pub fn fill_mask(&mut self) {
        unsafe { libc::sigfillset(&mut self.0.sa_mask) };
    }

    /// Whether the action's mask blocks `signum`.
    #[cfg(test)]
    pub fn mask_contains(&self, signum: i32) -> bool {
        unsafe { libc::sigismember(&self.0.sa_mask, signum) == 1 }
    }

    pub fn empty(handler: usize, flags: i32) -> Self {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler;
        action.sa_flags = flags;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        Self(action)
    }

    /// BSD `signal()` uses persistent handlers with restartable syscalls, and this C library
    /// implements it that way too. The kernel itself adds `signum` to the temporary mask.
    pub fn for_signal(handler: usize) -> Self {
        Self::empty(handler, libc::SA_RESTART)
    }

    /// Keep the guest-visible mask and supported flags, but route the kernel
    /// frame to a MIRVM-owned SA_SIGINFO stub. Guest SA_SIGINFO itself is
    /// rejected before this conversion.
    pub fn for_kernel_stub(mut self, handler: usize) -> Self {
        self.0.sa_sigaction = handler;
        self.0.sa_flags |= libc::SA_SIGINFO;
        self
    }

    /// Give an internal fixed-stub action an exact restorer identity. This kernel has no restorer
    /// slot, so the action is already as exact as it can be.
    pub fn with_runtime_restorer(mut self) -> Self {
        arch::set_runtime_restorer(&mut self.0);
        self
    }

    /// Translate a known kernel-stub action back to its caller-visible form.
    /// Keep mask and ordinary flag changes made to a raw oldact, but remove the
    /// adapter ABI bits that were not in `visible`.
    pub fn canonicalized_from_kernel_stub(mut self, kernel: &Self, visible: &Self) -> Self {
        arch::canonicalize_from_kernel_stub(&mut self.0, &kernel.0, &visible.0);
        self
    }

    pub fn standard_mask_bits(&self) -> u64 {
        let mut bits = 0u64;
        for signum in 1..realtime_min() {
            if unsafe { libc::sigismember(&self.0.sa_mask, signum) } == 1 {
                bits |= 1u64 << signum;
            }
        }
        bits
    }

    /// Apply this action's mask plus the delivered signal to the real host
    /// thread while a deferred handler runs. This prevents a genuine native
    /// disposition from interrupting the handler contrary to POSIX `sa_mask`.
    pub fn block_for_handler(&self, signum: i32) -> Result<ThreadSignalMaskGuard, i32> {
        let mask = SignalMask(self.0.sa_mask).with(signum);
        set_thread_mask(MaskOp::Block, &mask).map(ThreadSignalMaskGuard::restore_on_drop)
    }

    pub fn query(signum: i32) -> Result<Self, i32> {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::sigaction(signum, std::ptr::null(), &mut action) };
        if result == 0 {
            Ok(Self(action))
        } else {
            Err(crate::os::process::errno())
        }
    }

    #[cfg(test)]
    pub fn install(&self, signum: i32) -> i32 {
        unsafe { libc::sigaction(signum, &self.0, std::ptr::null_mut()) }
    }

    /// Install a caller request and return the action that immediately
    /// preceded it. External requests go through libc so they keep libc's
    /// normal `sigaction` semantics; MIRVM's own fixed-stub action carries no
    /// restorer on this platform, so it takes the same path.
    pub fn replace(&self, signum: i32) -> Result<Self, i32> {
        if arch::uses_runtime_restorer(&self.0) {
            return arch::replace_exact(&self.0, signum).map(Self);
        }

        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::sigaction(signum, &self.0, &mut old) };
        if result == 0 {
            Ok(Self(old))
        } else {
            Err(crate::os::process::errno())
        }
    }

    /// Restore a disposition previously read from the kernel without letting
    /// libc replace its flags or restorer. This is for old-action snapshots,
    /// never for a fresh guest/native request.
    pub fn replace_exact(&self, signum: i32) -> Result<Self, i32> {
        arch::replace_exact(&self.0, signum).map(Self)
    }

    pub fn write_to(&self, ptr: u64) {
        if ptr != 0 {
            unsafe { (ptr as *mut libc::sigaction).write(self.0) };
        }
    }

    pub fn same_disposition(&self, other: &Self) -> bool {
        self.handler() == other.handler()
            && self.flags() == other.flags()
            && arch::same_restorer(&self.0, &other.0)
            && self.same_mask(other)
    }

    /// Compare a post-install query with the action requested through libc. On this platform libc
    /// adds nothing to the action, so the comparison is the request plus the kernel's own
    /// normalization.
    #[cfg(test)]
    pub fn satisfies_request(&self, requested: &Self) -> bool {
        self.handler() == requested.handler()
            && (self.flags() & !arch::RESTORER_FLAG) == (requested.flags() & !arch::RESTORER_FLAG)
            && self.same_mask(requested)
    }

    /// Whether an action returned through `oldact` can be the exact kernel
    /// result of installing `requested`. This kernel accepts the established
    /// flags and normalizes the mask, while unknown probing bits do not exist
    /// here to be cleared.
    pub fn is_kernel_normalization_of(&self, requested: &Self) -> bool {
        let actual_flags = self.flags() & !arch::RESTORER_FLAG;
        let requested_flags = requested.flags() & !arch::RESTORER_FLAG;
        self.handler() == requested.handler()
            && self.same_mask(requested)
            && arch::restorer_matches(&self.0, &requested.0)
            && actual_flags & !requested_flags == 0
            && actual_flags & STABLE_KERNEL_FLAGS == requested_flags & STABLE_KERNEL_FLAGS
    }
}
