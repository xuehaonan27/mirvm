//! macOS thread primitives as aarch64 encodes them.
//!
//! Two things in `os/macos/thread.rs` cannot be written with the C library's own calls: a wait/wake
//! on a user word that must not disturb libc `errno`, and the widest user address, which is what
//! makes a bogus stack address recognizable. Both live here; the pthread and Mach halves stay in
//! `os/macos/thread.rs`.
//!
//! The wait is `__ulock_wait`, this kernel's futex-shaped call. `ULF_NO_ERRNO` is what makes it
//! usable from a leaf that must be invisible to the guest: the result comes back as a negative
//! number instead of being posted to `errno`, measured as -ETIMEDOUT on a timeout and -ENOENT for
//! a wake with no waiter, with `errno` untouched in every case. A timeout of zero waits forever.
//!
//! Two things differ from Linux's futex and a caller has to know them. A wait whose expected value
//! has already changed returns 0 rather than -EAGAIN, so 0 means "stop waiting" and never "was
//! woken" — the caller re-reads the word either way. And the kernel reads the word through the
//! process's own mapping, so a page this process has never written answers EFAULT; that is inherent
//! to correct use, because a caller writes the word before waiting on it.

/// The comparison wait: block while `*addr == value`.
const UL_COMPARE_AND_WAIT: u32 = 1;
/// Report the error as a negative number instead of posting it to `errno`.
const ULF_NO_ERRNO: u32 = 0x0100_0000;

unsafe extern "C" {
    /// Blocks while `*addr == value`. `timeout_us` of zero waits indefinitely. Returns 0 when the
    /// wait ends and a negative `errno` otherwise; never writes `errno`.
    fn __ulock_wait(operation: u32, addr: *const u32, value: u64, timeout_us: u32) -> i32;
    /// Releases one waiter, returning 0 when it did and a negative `errno` otherwise; never writes
    /// `errno`.
    fn __ulock_wake(operation: u32, addr: *const u32, wake_value: u64) -> i32;
}

/// Wait while `*addr == expected`, returning the kernel's raw result.
///
/// This leaf never writes libc `errno`; callers use it for telemetry wakeups that must be invisible
/// to the guest syscall contract.
pub fn futex_wait_raw(addr: *const u32, expected: u32) -> i64 {
    // SAFETY: the caller keeps the aligned atomic word alive and written for the wait.
    unsafe { __ulock_wait(UL_COMPARE_AND_WAIT | ULF_NO_ERRNO, addr, expected as u64, 0) as i64 }
}

/// Wake at most one waiter, returning the kernel's raw result without touching libc `errno`.
pub fn futex_wake_one_raw(addr: *const u32) -> i64 {
    // SAFETY: the caller keeps the aligned atomic word alive for the syscall.
    unsafe { __ulock_wake(UL_COMPARE_AND_WAIT | ULF_NO_ERRNO, addr, 0) as i64 }
}

/// arm64 macOS gives user space 47 bits — an address with bit 47 set is refused, bit 46 still maps
/// — so anything at or above that width is not a user address. This is the width test, not a claim
/// about any particular caller: a real user stack address, including a guest-provided one, falls
/// inside the range.
pub fn stack_addr_is_unset(lo: usize) -> bool {
    lo == 0 || lo >= 1 << 47
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn raw_wait_keeps_errno_and_reports_a_changed_word() {
        // The word is written before the wait, which is what makes the kernel's read succeed.
        let word = AtomicU32::new(1);
        unsafe { *libc::__error() = 73 };

        let rc = futex_wait_raw(word.as_ptr(), 0);

        // The expected value had already changed, so the wait ends at once; this pair reports that
        // as 0 rather than Linux's -EAGAIN.
        assert_eq!(rc, 0);
        assert_eq!(unsafe { *libc::__error() }, 73);
    }

    #[test]
    fn raw_wake_releases_a_waiter() {
        static WORD: AtomicU32 = AtomicU32::new(0);
        WORD.store(0, Ordering::SeqCst);

        let waiter = std::thread::spawn(|| futex_wait_raw(WORD.as_ptr(), 0));
        std::thread::sleep(std::time::Duration::from_millis(100));
        WORD.store(1, Ordering::SeqCst);
        let woke = futex_wake_one_raw(WORD.as_ptr());

        assert_eq!(waiter.join().unwrap(), 0, "the waiter should be released");
        assert_eq!(woke, 0, "one waiter was released");
    }

    #[test]
    fn user_address_range_boundary() {
        assert!(stack_addr_is_unset(0));
        assert!(!stack_addr_is_unset(1 << 46));
        assert!(!stack_addr_is_unset((1 << 47) - 1));
        assert!(stack_addr_is_unset(1 << 47));
        assert!(stack_addr_is_unset(usize::MAX));
    }
}
