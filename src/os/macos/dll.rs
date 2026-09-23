//! The dynamic loader, as this platform's C library exposes it.
//!
//! Everything here is the `dlfcn` interface, which is the same on both platforms this build
//! supports: handles and return values travel as [`usize`] (leaf type discipline), and business
//! semantics like binding priority and symbol existence stay with the caller.
//!
//! One surface item is deliberately absent: `load_bias`. Linux reads a handle's load base out of
//! the loader's own link map; this platform has no such call, and its `dlopen` returns an opaque
//! handle rather than the image header, so `dladdr(handle)` answers nothing — measured, for both a
//! shared-cache library and one built locally. The base is reachable only from an address *inside*
//! the image (`dladdr(dlsym(handle, name))` gives it), which the current signature has no way to
//! supply. Until that surface question is settled, a caller that needs the base gets the
//! unresolved name rather than a wrong number.

use crate::os::dll::Mode;
use std::ffi::CStr;

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
/// Handle = 0 indicates the global search, which is what this loader treats a null handle as on
/// both platforms this build supports.
pub fn sym(handle: usize, name: &CStr) -> usize {
    unsafe { libc::dlsym(handle as *mut libc::c_void, name.as_ptr()) as usize }
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
