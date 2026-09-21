//! Linux filesystem primitives whose protocol is the kernel's.
//!
//! Each of these is a sequence a caller would otherwise have to spell out: publishing a file
//! without replacing what is already there, writing at an offset through a vector, or asking
//! whether two metadata reads describe the same unchanged file.

use std::ffi::{CStr, OsStr};
use std::io;
use std::path::Path;

/// Create a symbolic link at `link` pointing at `target`.
///
/// This is how the cargo mirror publishes itself under the tool name a guest build script looks
/// for, and how a launcher is published for a test binary.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Give `path` the unix mode bits in `mode`, which is how a generated shell script becomes
/// executable.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// A path's bytes.
///
/// A unix path is a byte string that need not be UTF-8, so it reaches the kernel as bytes; a
/// `String` in between would refuse paths the kernel accepts.
pub fn raw_bytes(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;

    value.as_bytes()
}

/// Rename `from` onto `to`, refusing to replace an existing `to`.
///
/// `std::fs::rename` replaces silently, and a publish step that can overwrite a concurrent
/// writer's file is the one thing its callers must not do; `renameat2` is the only interface that
/// says so atomically.
pub fn rename_noreplace(from: &CStr, to: &CStr) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// One `pwritev`: write the buffers at `offset`, returning how many bytes the kernel took.
///
/// The `-1`/`errno` convention is translated here. What to do about a short write is not: a caller
/// that writes a record has to advance across partial writes itself, and one that writes a single
/// small buffer does not.
pub fn write_vectored_at(fd: i32, buffers: &[libc::iovec], offset: i64) -> io::Result<usize> {
    let written = unsafe { libc::pwritev(fd, buffers.as_ptr(), buffers.len() as i32, offset) };
    if written < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written as usize)
    }
}

/// Whether two metadata reads describe the same file in the same state.
///
/// Length and modification time alone would miss a same-length rewrite that restored the time, so
/// this compares the device, the inode and both timestamps. The caller's content digest is what
/// closes the remaining gap; this only has to be cheap.
#[cfg(unix)]
pub fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

/// Whether two metadata reads describe the same file in the same state, on a platform that reports
/// only length and modification time.
#[cfg(not(unix))]
pub fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}
