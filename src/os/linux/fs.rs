//! Linux descriptor and filesystem primitives whose protocol is the kernel's.
//!
//! The kernel's file interface has two halves and this module owns both, because a caller that
//! holds a descriptor and a caller that holds a path are asking the same layer the same kind of
//! question: the descriptor half is the `read`/`write`/`pipe`/`close` family, the path half is
//! publishing, linking, mode bits and metadata identity. Splitting them would put one libc call
//! per module and leave the porting surface larger than the knowledge in it.
//!
//! Each of these is a sequence a caller would otherwise have to spell out: publishing a file
//! without replacing what is already there, writing at an offset through a vector, or asking
//! whether two metadata reads describe the same unchanged file.

use std::ffi::{CStr, OsStr};
use std::io;
use std::path::Path;

// The descriptor reads, the close and the pipe are what a caller that has to talk to a `fork`
// child needs; the only such callers today are this crate's tests, which drive a real fork rather
// than a mock. `write` is not among them because the guest write path uses it.
/// `write(2)`: returns the byte count written or -1 (errno semantics stay with the caller).
///
/// The buffer is a caller-side address rather than a slice, because the guest write path holds a
/// guest virtual address that has no Rust slice to borrow.
pub fn write_fd(fd: i32, buf_addr: u64, len: usize) -> i64 {
    unsafe { libc::write(fd, buf_addr as *const libc::c_void, len) as i64 }
}

/// `read(2)`: returns the byte count read, 0 at end of file, or -1 (errno stays with the caller).
#[cfg(test)]
pub fn read_fd(fd: i32, buf_addr: u64, len: usize) -> i64 {
    unsafe { libc::read(fd, buf_addr as *mut libc::c_void, len) as i64 }
}

/// `close(2)`: returns the kernel's code, which a caller may ignore but not misread.
#[cfg(test)]
pub fn close_fd(fd: i32) -> i32 {
    unsafe { libc::close(fd) }
}

/// `pipe(2)`: the read and write ends, in that order.
///
/// `Err` is the library's `errno`, because a caller that has just failed to create the pair is the
/// only one that can say whether that is fatal.
#[cfg(test)]
pub fn pipe() -> Result<[i32; 2], i32> {
    let mut fds = [0_i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
        Ok(fds)
    } else {
        Err(crate::os::process::errno())
    }
}

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

/// One contiguous buffer of a vectored write.
///
/// This is the caller-side shape of the kernel's `iovec`: the layout structure is ABI and stays
/// inside this layer, while the caller names buffers. `Copy` because a partial write advances the
/// caller's own list in place.
#[derive(Clone, Copy)]
pub struct Buffer<'a> {
    bytes: &'a [u8],
}

impl<'a> Buffer<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Buffer { bytes }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// The same buffer with `n` leading bytes already written.
    pub fn advance(self, n: usize) -> Self {
        Buffer {
            bytes: &self.bytes[n..],
        }
    }
}

/// One `pwritev`: write the buffers at `offset`, returning how many bytes the kernel took.
///
/// The `-1`/`errno` convention is translated here. What to do about a short write is not: a caller
/// that writes a record has to advance across partial writes itself, and one that writes a single
/// small buffer does not.
pub fn write_vectored_at(fd: i32, buffers: &[Buffer<'_>], offset: i64) -> io::Result<usize> {
    let iov: Vec<libc::iovec> = buffers
        .iter()
        .map(|buffer| libc::iovec {
            iov_base: buffer.bytes.as_ptr().cast_mut().cast(),
            iov_len: buffer.bytes.len(),
        })
        .collect();
    let written = unsafe { libc::pwritev(fd, iov.as_ptr(), iov.len() as i32, offset) };
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
