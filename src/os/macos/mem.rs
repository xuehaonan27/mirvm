//! The one mapping call macOS answers in its own way.
//!
//! Everything else of [`crate::os::mem`] is the C library's on this platform too.

use crate::os::mem::Prot;

/// `VM_FLAGS_FIXED`: place the region at exactly this address, or fail.
///
/// Named here for the same reason the task port is: `libc`'s Mach constants carry a deprecation
/// notice pointing at a crate mirvm does not depend on. The Mach header defines this flag as 0 —
/// the absent overwrite flag is what makes the placement refuse an occupied range.
const VM_FLAGS_FIXED: libc::c_int = 0;

/// Preferred fixed base address mapping, refusing to replace what is already mapped.
/// Some on success, None on failure.
///
/// This kernel has no `MAP_FIXED_NOREPLACE`, so the placement goes through the Mach VM interface
/// instead: `vm_allocate` with `VM_FLAGS_FIXED` takes the range only when it is free and refuses it
/// otherwise, which is the same guarantee. The kernel hands back read-write memory, so the caller's
/// protection is applied afterwards; a range that cannot take it is released rather than returned
/// half-configured.
pub fn map_fixed_preferred(addr: usize, size: usize, prot: Prot) -> Option<*mut u8> {
    let mut placed: libc::vm_address_t = addr;
    let result =
        unsafe { libc::vm_allocate(super::task_self(), &mut placed, size, VM_FLAGS_FIXED) };
    if result != libc::KERN_SUCCESS || placed != addr {
        return None;
    }
    if crate::os::mem::protect(placed as *mut u8, size, prot).is_err() {
        unsafe { libc::vm_deallocate(super::task_self(), placed, size) };
        return None;
    }
    Some(placed as *mut u8)
}
