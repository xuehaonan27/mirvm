//! macOS on aarch64: the kernel ABI as this CPU encodes it.
//!
//! See the module header of `crate::os_arch` for the boundary contract, for why this pair has a
//! directory instead of living on one axis, and for which part of each subsystem is the pair's.
//! `addrspace` is numbers only: the structure they describe is declared one level up.

pub mod addrspace;
pub mod syscall_asm;
pub mod thread;
