//! The two mapping calls macOS answers in its own way.
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

/// An anonymous in-memory file this process can open again through `/dev/fd/<fd>`, or `None` when
/// the kernel refuses to create one.
///
/// The descriptor belongs to the caller. It is how an image with no place on disk is handed to the
/// loader, which only accepts a path. This platform has no `memfd_create`; an unlinked temporary
/// file is the same kind of handle — no path once it exists, released when the last descriptor
/// closes, and writable immediately, which is what `MFD_CLOEXEC` gives Linux. A POSIX
/// shared-memory object is not: this kernel creates one with no length, so a caller could not write
/// to it without first sizing it, which this signature has no way to say.
pub fn anonymous_file(name: &std::ffi::CStr) -> Option<i32> {
    use std::os::unix::ffi::OsStringExt;

    let stamp = name.to_string_lossy().replace('/', "_");
    let template = std::env::temp_dir().join(format!("mirvm-{stamp}-XXXXXX"));
    let mut bytes = template.into_os_string().into_vec();
    bytes.push(0);

    // SAFETY: the template is NUL-terminated and ends in the six characters `mkstemp` requires.
    let fd = unsafe { libc::mkstemp(bytes.as_mut_ptr().cast()) };
    if fd < 0 {
        return None;
    }
    // The name is dropped immediately, so the file has no path for the rest of its life.
    unsafe { libc::unlink(bytes.as_ptr().cast()) };
    // `mkstemp` does not set it on this platform, and the Linux path has it.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Some(fd)
}
