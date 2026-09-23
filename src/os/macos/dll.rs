//! The dynamic loader, as this platform's C library exposes it.
//!
//! Everything here is the `dlfcn` interface, which is the same on both platforms this build
//! supports: handles and return values travel as [`usize`] (leaf type discipline), and business
//! semantics like binding priority and symbol existence stay with the caller.
//!
//! One surface item is deliberately absent: `load_bias`, because three measurements say what it has
//! to be and none of them is the obvious guess.
//!
//! `dlopen` returns an opaque handle here rather than the image header, so `dladdr(handle)` answers
//! nothing — measured for both a shared-cache library and one built locally — which rules out
//! translating Linux's link-map read directly. An address *inside* the image does work
//! (`dladdr(dlsym(handle, name))`), but the product caller that needs the base has no symbol at
//! that point: it reads its symbol table from the file afterwards. What every caller does have is
//! the path it just opened, and the path is what the loader itself keys its image list by — with
//! the trap that it reports the *resolved* path (`/private/tmp/…` for `/tmp/…`), so a comparison
//! against the caller's spelling misses silently and always.
//!
//! The base is then `slide + __TEXT.vmaddr`, not the slide: a locally built image has
//! `vmaddr == 0`, where the slide alone looks right, while a system library does not and the slide
//! is short by exactly that. Reading it means reading this platform's object layout, so this is
//! one piece of the same question that `reloc` and the loader face.
//!
//! Until that is settled a caller gets an unresolved name rather than a wrong number: the base
//! feeds both the executable ranges and the lifecycle materialization, so a base short by a
//! preferred address would corrupt native-instance addressing quietly.

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
