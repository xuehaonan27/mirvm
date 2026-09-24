//! Linux dynamic loading primitives.
//! This layer is only for single issue primitives.
//! Handles and return values are passed as [`usize`] (leaf type discipline).
//! Business semantics like binding priority and symbol existence are left to
//! the caller.

use crate::os::dll::{Mode, ObjectFormat, PrivateImage};
use std::ffi::CStr;

/// The format this platform's toolchain writes and its loader accepts, which is what a caller that
/// has to publish an object, or read one back, has to know.
pub const OBJECT_FORMAT: ObjectFormat = ObjectFormat::Elf;

/// Publishes `bytes` as an object this process's loader can load, and answers the handle, the base
/// its contents were mapped at, and the file that keeps them alive.
///
/// The bytes reach the loader through an unnamed file: this kernel's `memfd_create` gives a
/// descriptor with no directory entry at all, and the loader reads it back through
/// `/proc/self/fd`, so nothing is written to a filesystem and nothing has to be cleaned up
/// afterwards. The descriptor stays open for as long as the image is loaded.
pub fn load_private_image(bytes: &[u8], name: &CStr) -> Result<PrivateImage, crate::os::Error> {
    use std::io::{Seek, Write};

    let failure = |detail: String| crate::os::Error::PrivateImage { detail };
    let fd = crate::os::mem::anonymous_file(name).ok_or_else(|| {
        failure(format!(
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        ))
    })?;
    // SAFETY: `fd` is a fresh descriptor this call owns, and nothing else closes it.
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    file.write_all(bytes)
        .map_err(|error| failure(format!("writing the private object failed: {error}")))?;
    file.rewind()
        .map_err(|error| failure(format!("rewinding the private object failed: {error}")))?;
    let path = std::ffi::CString::new(format!("/proc/self/fd/{fd}"))
        .map_err(|_| failure("the descriptor path unexpectedly contains NUL".to_string()))?;
    let handle = open_with_flags(&path, libc::RTLD_NOW | libc::RTLD_LOCAL)?;
    let Some(bias) = load_bias(handle, &path) else {
        unsafe { close(handle) };
        return Err(failure(
            "reading the private object's load base failed".to_string(),
        ));
    };
    Ok(PrivateImage::new(handle, bias, Some(file), None))
}

/// `dlopen`.
/// # Return value
///
/// - Success: handle(usize, non-zero).
/// - Failure: Err with dlerror details. Message string is cleared and copied
///   within this function, the caller gets owned failure message.
pub fn open(path: &CStr, mode: Mode) -> Result<usize, crate::os::Error> {
    let flag = match mode {
        Mode::Now => libc::RTLD_NOW,
        Mode::Lazy => libc::RTLD_LAZY,
    } | libc::RTLD_GLOBAL;
    open_with_flags(path, flag)
}

/// Nothing: this platform's loader reads an ELF's bytes as they are, so a private copy mirvm
/// rewrote is one it still loads.
///
/// The name is the ladder's, so a caller that rewrites a copy names one function on every target
/// and the question of whether anything has to happen stays here.
pub fn reseal(_path: &std::path::Path) -> Result<(), crate::os::Error> {
    Ok(())
}

/// `dlopen` with explicit flag form.
/// Used for testing/partial visibility; use [`open`] for product paths.
pub fn open_with_flags(path: &CStr, flag: i32) -> Result<usize, crate::os::Error> {
    unsafe { libc::dlerror() }; // clear dlerror.
    let h = unsafe { libc::dlopen(path.as_ptr(), flag) };
    if h.is_null() {
        return Err(crate::os::Error::Dlopen {
            detail: error_string(),
        });
    }
    Ok(h as usize)
}

/// Commonly used flag constants for `dlopen`.
pub const RTLD_NOW: i32 = libc::RTLD_NOW;
pub const RTLD_LOCAL: i32 = libc::RTLD_LOCAL;

/// `dlclose`.
/// Process-level native library handles remain intentionally persistent with
/// their caller.
/// The symbolic image which is private to the Engine and with non-escapeable
/// addresses is released using this entry point during Module destruction.
///
/// # Safety
/// Handles must originate from [`open`] in this module and should not be
/// [`sym`]ed again afterward.
pub unsafe fn close(handle: usize) {
    unsafe { libc::dlclose(handle as *mut libc::c_void) };
}

/// dlsym (single issued)
/// # Return value
/// - hit: address (non-zero)
/// - miss: 0.
///
/// A zero `handle` is the global scope. This loader spells that with a null handle, which is the
/// same value `RTLD_DEFAULT` has here; the mapping is named rather than passed through so that a
/// reader of either half sees which platform is doing what.
pub fn sym(handle: usize, name: &CStr) -> usize {
    let scope = if handle == 0 {
        libc::RTLD_DEFAULT
    } else {
        handle as *mut libc::c_void
    };
    // SAFETY: `scope` is a handle this process's loader returned, or the global scope, and `name`
    // is a NUL-terminated C string.
    unsafe { libc::dlsym(scope, name.as_ptr()) as usize }
}

/// The current value of dlerror.
/// When dlerror message is empty, [`error_string`] returns fixed message
/// `[dlerror without value]`. It's caller's duty to take care of this. Only
/// used internally by [`open`] for correct timing.
pub fn error_string() -> String {
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "[dlerror without value]".into()
    } else {
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

// Minimal field layout of glibc link_map (returned by dlinfo RTLD_DI_LINKMAP).
#[repr(C)]
struct LinkMap {
    l_addr: usize,
    l_name: *const std::ffi::c_char,
    l_ld: *mut std::ffi::c_void,
    l_next: *mut LinkMap,
    l_prev: *mut LinkMap,
}

/// The load base address of the dlopen handle (dlinfo => link_map.l_addr).
/// If returns [`None`], it means [`dlinfo`] fails, and that usually means an
/// invalid handle is passed to [`load_bias`].
/// A handle just opened by [`dlopen`] should not be invalid.
///
/// `path` is the name the library was opened under. This platform answers from the handle alone, so
/// it is not consulted here; it is part of the signature because the platform on the other side of
/// the ladder has nowhere else to get it, and a call site must be able to name one function.
pub fn load_bias(handle: usize, _path: &CStr) -> Option<usize> {
    let mut lm: *mut LinkMap = std::ptr::null_mut();
    let r = unsafe {
        libc::dlinfo(
            handle as *mut libc::c_void,
            libc::RTLD_DI_LINKMAP,
            &mut lm as *mut *mut LinkMap as *mut libc::c_void,
        )
    };
    if r == 0 && !lm.is_null() {
        Some(unsafe { (*lm).l_addr })
    } else {
        None
    }
}
