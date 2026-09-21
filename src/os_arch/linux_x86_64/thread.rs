//! Linux thread primitives as x86_64 encodes them.
//!
//! Two things in `os/linux/thread.rs` cannot be written CPU-neutrally: the raw futex
//! wait/wake that must not touch libc `errno` (the syscall instruction and its argument
//! registers are x86_64's), and the width of a user address, which is what makes glibc's
//! "no stack was ever set" sentinel recognizable. Both live here; the pthread and `/proc`
//! halves stay in `os/linux/thread.rs`.

std::arch::global_asm!(
    ".globl mirvm_futex_wait_raw",
    ".hidden mirvm_futex_wait_raw",
    ".type mirvm_futex_wait_raw,@function",
    "mirvm_futex_wait_raw:",
    "mov edx, esi",
    "mov eax, 202",
    "mov esi, 128",
    "mov r10d, 0",
    "syscall",
    "ret",
    ".size mirvm_futex_wait_raw, .-mirvm_futex_wait_raw",
    ".globl mirvm_futex_wake_one_raw",
    ".hidden mirvm_futex_wake_one_raw",
    ".type mirvm_futex_wake_one_raw,@function",
    "mirvm_futex_wake_one_raw:",
    "mov eax, 202",
    "mov esi, 129",
    "mov edx, 1",
    "syscall",
    "ret",
    ".size mirvm_futex_wake_one_raw, .-mirvm_futex_wake_one_raw",
);

unsafe extern "C" {
    fn mirvm_futex_wait_raw(addr: *const u32, expected: u32) -> i64;
    fn mirvm_futex_wake_one_raw(addr: *const u32) -> i64;
}

/// Wait while `*addr == expected`, returning the kernel's raw result. This leaf never writes
/// libc `errno`; callers use it for telemetry wakeups that must be invisible to the guest
/// syscall contract.
pub fn futex_wait_raw(addr: *const u32, expected: u32) -> i64 {
    // SAFETY: the caller keeps the aligned atomic word alive for the wait.
    unsafe { mirvm_futex_wait_raw(addr, expected) }
}

/// Wake at most one waiter, returning the kernel's raw result without touching libc `errno`.
pub fn futex_wake_one_raw(addr: *const u32) -> i64 {
    // SAFETY: the caller keeps the aligned atomic word alive for the syscall.
    unsafe { mirvm_futex_wake_one_raw(addr) }
}

/// glibc detail: an attr that was never setstack'd holds stackaddr=NULL internally, so getstack
/// returns `NULL - stacksize` (a bogus address near the top of u64) instead of NULL. x86_64 user
/// addresses fit in 47 bits, so anything above that range means "unset"; a real user stack
/// address (a guest-provided stack) falls inside it.
pub fn stack_addr_is_unset(lo: usize) -> bool {
    lo == 0 || lo >= 1 << 48
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn raw_futex_wait_passes_expected_without_touching_errno() {
        let word = AtomicU32::new(1);
        unsafe { *libc::__errno_location() = 73 };

        let rc = futex_wait_raw(word.as_ptr(), 0);

        assert_eq!(rc, -(libc::EAGAIN as i64));
        assert_eq!(unsafe { *libc::__errno_location() }, 73);
    }
}
