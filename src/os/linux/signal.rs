//! Linux signal primitives.
//! - signal/sigaction passthrough
//! - sigaction structure copy for handler read/write
//! - SEGV troubleshooting dump installation
//!
//! Primitives are not adjudicated:
//! - Guest handler whitelist (sync fault signal rejection)
//! - AS-trampoline materialization
//! - DFL/IGN decision
//!
//! These decisions are all left to the engine. This module only handles kernel
//! calls and structure layout knowledge.
//!
//! The half of that knowledge which x86_64 supplies -- the `SA_RESTORER` flag and the restorer
//! slot, the `rt_sigreturn` restorer and its byte pattern, the raw `rt_sigaction` request
//! layout, and the `ucontext_t` register indices the debug dump reads -- lives in
//! [`crate::os_arch::signal`], because Linux defines the interface and the CPU encodes it. This
//! file stays CPU-neutral and reaches it through that name.

use crate::os_arch::signal as arch;

pub const SIG_DFL: usize = libc::SIG_DFL;
pub const SIG_IGN: usize = libc::SIG_IGN;
pub const SIG_ERR: usize = usize::MAX;

/// The kernel's `siginfo_t` as an `SA_SIGINFO` handler receives it.
///
/// A handler's signature is the kernel's, so the argument cannot be made opaque; naming the pointer
/// type here is what lets the engine's adapters stay free of the platform's type names.
pub type SignalInfo = *mut libc::siginfo_t;

/// The `si_code` Linux reports for a delivery one thread addressed to another (`tkill`/`tgkill`),
/// which is what distinguishes it from a process-directed `kill`.
pub const SI_TKILL: i32 = libc::SI_TKILL;

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

/// The kernel flag asking whether the running kernel understands the tag-bit action bits. Linux
/// clears it on the way back out, so it is never part of an installed disposition.
pub const SA_EXPOSE_TAGBITS: i32 = 0x0000_0800;

/// The kernel flag that reports which optional action bits the running kernel understands, and
/// clears itself on the way back out. Never part of an installed disposition.
#[cfg(test)]
pub const SA_UNSUPPORTED: i32 = 0x0000_0400;

/// The SEGV dump is installed and implemented by the pair: its handler reads the fault RIP and
/// the fault address out of the architecture's own `ucontext_t`.
pub use crate::os_arch::signal::install_segv_dump;

// Linux promises that these long-established flags survive rt_sigaction.
// SA_UNSUPPORTED and future probing bits are deliberately excluded: a current
// kernel may clear them while reporting which optional bits it understands.
const STABLE_KERNEL_FLAGS: i32 = libc::SA_NOCLDSTOP
    | libc::SA_NOCLDWAIT
    | libc::SA_SIGINFO
    | libc::SA_ONSTACK
    | libc::SA_RESTART
    | libc::SA_NODEFER
    | libc::SA_RESETHAND
    | SA_EXPOSE_TAGBITS;

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

/// The lowest realtime signal number, which varies with the C library's own reservations, so it is
/// a call rather than a constant.
pub fn realtime_min() -> i32 {
    libc::SIGRTMIN()
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

/// Upper bound on Linux's traditional (non-realtime) signal numbers. Realtime
/// signals carry queueing and siginfo semantics and cannot be folded into a VM
/// mailbox that merges only by signal number.
pub const STANDARD_SIGNAL_MAX: i32 = 31;

pub fn is_realtime(signum: i32) -> bool {
    signum >= realtime_min() && signum <= libc::SIGRTMAX()
}

/// Whether a delivery came from a thread-directed kill (`tkill`/`tgkill`) rather than a
/// process-directed `kill`.
///
/// The engine's mailbox merges deliveries by signal number, which is only sound for a delivery the
/// kernel addressed to one thread, so it has to ask. The answer lives in `siginfo`, whose layout is
/// the kernel's, which is why the question is asked here and not there.
pub fn sent_by_thread_kill(info: SignalInfo) -> bool {
    info_code(info) == SI_TKILL
}

/// A sigaction structure (layout knowledge encapsulated). The engine edits a
/// copy's handler and writes it back to the kernel; the original guest structure
/// stays untouched because the guest may reuse or read it back.
///
/// `repr(transparent)`: an address of one of these is handed to the kernel's `rt_sigaction`, and
/// an interposed `sigaction` receives its caller's own address for the same structure.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Sigaction(libc::sigaction);

/// A thread's signal mask, as the kernel keeps it.
///
/// The engine orders its own deferred deliveries against this, so it has to be able to build one,
/// hand it to the kernel, and put the previous one back.
#[derive(Clone, Copy)]
pub struct SignalMask(libc::sigset_t);

impl SignalMask {
    /// Nothing blocked.
    pub fn empty() -> Self {
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut mask) };
        SignalMask(mask)
    }

    /// Everything that can be blocked: every traditional signal except the two POSIX forbids.
    pub fn all_blockable() -> Self {
        let mut mask = Self::empty();
        for signum in 1..=STANDARD_SIGNAL_MAX {
            if matches!(signum, libc::SIGKILL | libc::SIGSTOP) {
                continue;
            }
            unsafe { libc::sigaddset(&mut mask.0, signum) };
        }
        mask
    }

    /// The same mask plus one signal.
    pub fn with(mut self, signum: i32) -> Self {
        unsafe { libc::sigaddset(&mut self.0, signum) };
        self
    }
}

/// One delivery `rt_sigtimedwait` took out of the pending set.
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

    /// The kernel's `si_code`, comparable with [`SI_TKILL`].
    pub fn code(&self) -> i32 {
        self.code
    }
}

/// `rt_sigtimedwait` with no timeout: take one signal pending in `mask`, blocking until one
/// arrives.
///
/// This is the raw wait rather than a handler, because it is the only way to learn *which* signal
/// the kernel had pending, and a caller that consumed a delivery this way has to know that a
/// subsequent observation would see nothing. `mask` must be blocked on the calling thread.
#[cfg(test)]
pub fn wait_pending(mask: &SignalMask) -> Result<PendingSignal, i32> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::syscall(
            libc::SYS_rt_sigtimedwait,
            &mask.0,
            &mut info,
            std::ptr::null::<libc::timespec>(),
            std::mem::size_of::<u64>(),
        )
    };
    if result < 0 {
        Err(crate::os::process::errno())
    } else {
        Ok(PendingSignal {
            signum: result as i32,
            code: info.si_code,
        })
    }
}

/// Which transition [`set_thread_mask`] performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskOp {
    Block,
    Unblock,
    Set,
}

/// Apply `op` to the calling thread's signal mask, returning the mask it had.
///
/// `Err` is the raw `pthread_sigmask` return code, which is not `errno`; only the caller knows
/// whether that is fatal.
pub fn set_thread_mask(op: MaskOp, mask: &SignalMask) -> Result<SignalMask, i32> {
    let how = match op {
        MaskOp::Block => libc::SIG_BLOCK,
        MaskOp::Unblock => libc::SIG_UNBLOCK,
        MaskOp::Set => libc::SIG_SETMASK,
    };
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::pthread_sigmask(how, &mask.0, &mut previous) };
    if result == 0 {
        Ok(SignalMask(previous))
    } else {
        Err(result)
    }
}

/// Puts a thread's mask back when the guard goes out of scope.
///
/// A failure here leaves the thread carrying a mask nobody asked for — a guest `sa_mask` silently
/// ignored, or the close path's block-all mask still in place — so it aborts rather than
/// continuing with a POSIX guarantee quietly broken.
pub struct ThreadSignalMaskGuard {
    previous: SignalMask,
}

impl ThreadSignalMaskGuard {
    /// Restore `previous` when the guard drops.
    pub fn restore_on_drop(previous: SignalMask) -> Self {
        ThreadSignalMaskGuard { previous }
    }
}

impl Drop for ThreadSignalMaskGuard {
    fn drop(&mut self) {
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous.0, std::ptr::null_mut())
        };
        if result != 0 {
            eprintln!("mirvm[m4-engine]: failed to restore host signal mask: {result}");
            std::process::abort();
        }
    }
}

impl Sigaction {
    fn same_mask(&self, other: &Self) -> bool {
        (1..=libc::SIGRTMAX())
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
        for signum in 1..=STANDARD_SIGNAL_MAX {
            if unsafe { libc::sigismember(&current, signum) } == 1 {
                bits |= 1u64 << signum;
            }
        }
        Ok(bits)
    }

    /// Linux silently removes the two unmaskable signals from `sa_mask`.
    /// Keep the guest-visible copy identical to the disposition the kernel
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

    /// Remove `flags` from the action, which is how a caller reproduces a kernel normalization.
    #[cfg(test)]
    pub fn clear_flags(&mut self, flags: i32) {
        self.0.sa_flags &= !flags;
    }

    /// The restorer this action names, if any. `None` is what a caller-visible disposition shows
    /// once the pair's adapter bits have been stripped.
    #[cfg(test)]
    pub fn restorer(&self) -> Option<extern "C" fn()> {
        self.0.sa_restorer
    }

    /// Put `restorer` in the action's slot, which is how a snapshot read from the kernel is
    /// reproduced exactly.
    #[cfg(test)]
    pub fn set_restorer(&mut self, restorer: extern "C" fn()) {
        arch::set_restorer(&mut self.0, restorer);
    }

    /// Add `signum` to the mask this action installs while its handler runs.
    #[cfg(test)]
    pub fn add_to_mask(&mut self, signum: i32) {
        unsafe { libc::sigaddset(&mut self.0.sa_mask, signum) };
    }

    /// Block every signal in the mask, which is what a set built for `rt_sigtimedwait` wants: the
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

    /// glibc `signal()` uses BSD-style persistent handlers with restartable
    /// syscalls. The kernel itself adds `signum` to the temporary mask.
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

    /// Give an internal fixed-stub action an exact restorer identity. Public
    /// native dispositions still go through libc unchanged.
    pub fn with_runtime_restorer(mut self) -> Self {
        arch::set_runtime_restorer(&mut self.0);
        self
    }

    /// Translate a known kernel-stub action back to its caller-visible form.
    /// Keep mask and ordinary flag changes made to a raw oldact, but remove the
    /// adapter ABI bits and libc restorer state that were not in `visible`.
    pub fn canonicalized_from_kernel_stub(mut self, kernel: &Self, visible: &Self) -> Self {
        arch::canonicalize_from_kernel_stub(&mut self.0, &kernel.0, &visible.0);
        self
    }

    pub fn standard_mask_bits(&self) -> u64 {
        let mut bits = 0u64;
        for signum in 1..=STANDARD_SIGNAL_MAX {
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
    /// normal `sigaction` semantics. MIRVM's own fixed-stub action already
    /// carries an exact kernel restorer and therefore uses `rt_sigaction`.
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

    /// Compare a post-install query with the action requested through libc.
    /// libc adds the restorer flag and a restorer pointer on the architectures
    /// that carry one.
    #[cfg(test)]
    pub fn satisfies_request(&self, requested: &Self) -> bool {
        self.handler() == requested.handler()
            && (self.flags() & !arch::RESTORER_FLAG) == (requested.flags() & !arch::RESTORER_FLAG)
            && self.same_mask(requested)
    }

    /// Whether an action returned through `oldact` can be the exact kernel
    /// result of installing `requested`. Linux may add a restorer and may clear
    /// SA_UNSUPPORTED or unknown probing bits, while its established flags and
    /// the normalized mask must remain intact.
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
