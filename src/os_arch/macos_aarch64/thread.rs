//! macOS thread primitives as aarch64 encodes them.
//!
//! One thing in `os/macos/thread.rs` cannot be written with the C library's own calls: the widest
//! user address, which is what makes a bogus stack address recognizable. It lives here.
//!
//! The raw wait/wake that this pair also owes is not here yet. `__ulock_wait` with `ULF_NO_ERRNO`
//! has the property the contract needs — errors come back as negative numbers and `errno` is never
//! written, measured — but it also refuses a word in a static with `EFAULT` where a word on the
//! stack waits, so the address the telemetry wait/wake may use is not yet established. Writing it
//! before that is settled would put a primitive in the tree that fails on the placement mirvm is
//! most likely to use.

/// arm64 macOS gives user space 47 bits — an address with bit 47 set is refused, bit 46 still maps
/// — so anything above that range is not a user address. This is the width test, not a claim about
/// any particular caller: a real user stack address, including a guest-provided one, falls inside
/// the range.
pub fn stack_addr_is_unset(lo: usize) -> bool {
    lo == 0 || lo >= 1 << 48
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_address_range_boundary() {
        assert!(stack_addr_is_unset(0));
        assert!(!stack_addr_is_unset(1 << 46));
        assert!(!stack_addr_is_unset((1 << 47) - 1));
        assert!(stack_addr_is_unset(1 << 47));
        assert!(stack_addr_is_unset(usize::MAX));
    }
}
