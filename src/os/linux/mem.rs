//! The two mapping calls Linux answers in its own way.
//!
//! Everything else of [`crate::os::mem`] is the C library's on this platform too.

use crate::os::mem::Prot;

/// Preferred fixed base address mapping (MAP_FIXED_NOREPLACE).
/// Some on success, None on failure.
/// Engine cacheability base (same approach as JVM CDS). Faulty base address
/// means silent fault value, therefore never overwrite existing mappings.
pub fn map_fixed_preferred(addr: usize, size: usize, prot: Prot) -> Option<*mut u8> {
    let p = unsafe {
        libc::mmap(
            addr as *mut libc::c_void,
            size,
            prot.0,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return None;
    }
    Some(p as *mut u8)
}

/// An anonymous in-memory file this process can open again through `/proc/self/fd/<fd>`, or
/// `None` when the kernel refuses to create one.
///
/// The descriptor belongs to the caller. It is how an image with no place on disk is handed to the
/// loader, which only accepts a path.
pub fn anonymous_file(name: &std::ffi::CStr) -> Option<i32> {
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    (fd >= 0).then_some(fd)
}
