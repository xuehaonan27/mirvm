//! Linux on x86_64: the kernel ABI as this CPU encodes it.
//!
//! See the module header of `crate::os_arch` for the boundary contract and for why this pair
//! has a directory instead of living on one axis.

pub mod addrspace;
pub mod signal;
pub mod thread;
