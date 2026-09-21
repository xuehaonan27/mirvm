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

const SA_EXPOSE_TAGBITS: i32 = 0x0000_0800;

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

/// Upper bound on Linux's traditional (non-realtime) signal numbers. Realtime
/// signals carry queueing and siginfo semantics and cannot be folded into a VM
/// mailbox that merges only by signal number.
pub const STANDARD_SIGNAL_MAX: i32 = 31;

pub fn is_realtime(signum: i32) -> bool {
    signum >= libc::SIGRTMIN() && signum <= libc::SIGRTMAX()
}

/// Whether a delivery came from a thread-directed kill (`tkill`/`tgkill`) rather than a
/// process-directed `kill`.
///
/// The engine's mailbox merges deliveries by signal number, which is only sound for a delivery the
/// kernel addressed to one thread, so it has to ask. The answer lives in `siginfo`, whose layout is
/// the kernel's, which is why the question is asked here and not there.
pub fn sent_by_thread_kill(info: *const std::ffi::c_void) -> bool {
    if info.is_null() {
        return false;
    }
    unsafe { (*info.cast::<libc::siginfo_t>()).si_code == libc::SI_TKILL }
}

/// A sigaction structure (layout knowledge encapsulated). The engine edits a
/// copy's handler and writes it back to the kernel; the original guest structure
/// stays untouched because the guest may reuse or read it back.
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
