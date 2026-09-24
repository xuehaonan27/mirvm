//! The signal vocabulary that is the interface rather than one kernel's answer.
//!
//! A guest signal request is a POSIX request: a mask, the three ways to apply one, and the three
//! dispositions every C library spells the same way. None of that changes between kernels, so it is
//! declared here once and a second platform inherits it instead of restating it.
//!
//! What a *kernel* decides stays in the platform directory: the `siginfo` layout, which action
//! flags survive an install, how many traditional signal numbers exist, and what a delivery's
//! `siginfo` says about its direction. `Sigaction` is there too, and for the same reason — every
//! one of its operations reads or writes a `libc::sigaction` field, and those fields are the
//! kernel's own (Linux carries a restorer slot macOS does not). This file declares that the item
//! exists, and [`DeliveryDirection`] is the vocabulary the direction is reported in.
//!
//! The platform half must provide, under the same names a caller already uses:
//! `Sigaction`, `SignalInfo`, `install_segv_dump`, `info_code`, `delivery_direction`,
//! `is_realtime`, `realtime_min`, `STANDARD_SIGNAL_MAX`, `all_blockable_mask`, the fault signal
//! numbers, and the test-only signal and flag constants. What else a platform adds is its own: a
//! kernel that reports a thread-directed delivery in `si_code` names that code (`SI_TKILL`), and
//! one that carries a restorer in the action has the pair declare `RESTORER_FLAG` non-zero.

/// Who a delivery was addressed to, as far as the receiving kernel's `siginfo` reports it.
///
/// The engine keeps a mailbox per pthread and one per Engine because a delivery addressed to a
/// pthread must run there. A kernel that stamps the direction on every delivery lets that be read
/// off [`delivery_direction`]; one that reports the same code and sender for a `pthread_kill` and
/// for a `kill` to its own process cannot, and says [`DeliveryDirection::Ambiguous`] instead. The
/// caller then has to fall back on what it knows about the receiving pthread.
//
// A kernel that reports the direction exactly constructs two of these and one that cannot
// constructs two others, so the third is unconstructed in either build while the delivery path
// still has to handle it.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryDirection {
    /// Addressed to the pthread that received it.
    Thread,
    /// Addressed to the process, so the kernel chose the receiving pthread.
    Process,
    /// The kernel's `siginfo` does not separate the two.
    Ambiguous,
}

/// The two dispositions the C library names with a sentinel rather than a handler.
pub const SIG_DFL: usize = libc::SIG_DFL;
pub const SIG_IGN: usize = libc::SIG_IGN;
/// The value a signal call returns on failure, distinct from any handler address.
pub const SIG_ERR: usize = usize::MAX;

/// A thread's signal mask, as the kernel keeps it.
///
/// The engine orders its own deferred deliveries against this, so it has to be able to build one,
/// hand it to the kernel, and put the previous one back.
#[derive(Clone, Copy)]
pub struct SignalMask(pub(crate) libc::sigset_t);

impl SignalMask {
    /// Nothing blocked.
    pub fn empty() -> Self {
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::sigemptyset(&mut mask) };
        SignalMask(mask)
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

#[cfg(target_os = "linux")]
pub(crate) use super::linux::signal::*;
#[cfg(target_os = "macos")]
pub(crate) use super::macos::signal::*;
