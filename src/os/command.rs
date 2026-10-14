//! Process replacement and native command strings on the supported POSIX platforms.

#[cfg(not(feature = "compiler"))]
pub(crate) fn exec(command: &mut std::process::Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    command.exec()
}

pub(crate) fn native_string(bytes: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes)
}

/// A request may contain a guest environment, so only its owner may read the staged file.
#[cfg(not(feature = "compiler"))]
pub(crate) fn private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// exec preserves an inheritable descriptor's open file description and advisory locks.
#[cfg(not(feature = "compiler"))]
pub(crate) fn inherit_file(file: &std::fs::File) -> std::io::Result<i32> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

/// # Safety
/// `fd` must belong exclusively to the handoff protocol; no live Rust owner may hold it.
pub(crate) unsafe fn take_inherited_file(fd: i32) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    if fd < 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "input lease cannot own a standard stream",
        ));
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}
