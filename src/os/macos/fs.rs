//! The macOS answer for the file interface's one non-POSIX call.
//!
//! Everything else of [`crate::os::fs`] is the C library's on this platform too.

use std::ffi::CStr;
use std::io;

/// Rename `from` onto `to`, refusing to replace an existing `to`.
///
/// `std::fs::rename` replaces silently, and a publish step that can overwrite a concurrent writer's
/// file is the one thing its callers must not do; `renameatx_np` is the interface that says so
/// atomically. Linux reaches the same guarantee through `renameat2`.
pub fn rename_noreplace(from: &CStr, to: &CStr) -> io::Result<()> {
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
