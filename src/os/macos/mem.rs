//! The two mapping calls macOS answers in its own way.
//!
//! Everything else of [`crate::os::mem`] is the C library's on this platform too.

use crate::os::mem::Prot;

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
    let result = unsafe {
        libc::vm_allocate(
            libc::mach_task_self(),
            &mut placed,
            size,
            libc::VM_FLAGS_FIXED,
        )
    };
    if result != libc::KERN_SUCCESS || placed != addr {
        return None;
    }
    if crate::os::mem::protect(placed as *mut u8, size, prot).is_err() {
        unsafe { libc::vm_deallocate(libc::mach_task_self(), placed, size) };
        return None;
    }
    Some(placed as *mut u8)
}

/// An anonymous in-memory file this process can open again through `/dev/fd/<fd>`, or `None` when
/// the kernel refuses to create one.
///
/// The descriptor belongs to the caller. It is how an image with no place on disk is handed to the
/// loader, which only accepts a path. This platform has no `memfd_create`; a POSIX shared-memory
/// object unlinked immediately after creation is the same kind of handle — unnamed, unreachable by
/// path, and released when the last descriptor closes.
pub fn anonymous_file(name: &std::ffi::CStr) -> Option<i32> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::ffi::CString::new(format!(
        "/mirvm-{}-{unique}-{}",
        unsafe { libc::getpid() },
        name.to_string_lossy()
    ))
    .ok()?;
    let fd = unsafe {
        libc::shm_open(
            path.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
    };
    if fd < 0 {
        return None;
    }
    unsafe { libc::shm_unlink(path.as_ptr()) };
    Some(fd)
}
