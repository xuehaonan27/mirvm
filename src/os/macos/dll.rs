//! The dynamic loader, as this platform's C library exposes it.
//!
//! Everything here is the `dlfcn` interface, which is the same on both platforms this build
//! supports: handles and return values travel as [`usize`] (leaf type discipline), and business
//! semantics like binding priority and symbol existence stay with the caller.
//!
//! `load_bias` is the one item that is not a straight `dlfcn` call, and three measurements say what
//! it has to be rather than the obvious guess.
//!
//! `dlopen` returns an opaque handle here rather than the image header — treating it as one
//! dereferences an invalid pointer — and `dladdr(handle)` answers nothing either, so Linux's
//! link-map read has no counterpart. What every caller does have is the path it just opened, and
//! the path is what the loader keys its own image list by, so the base is found by looking the
//! image up in that list. The loader reports the *resolved* path (`/private/tmp/…` for `/tmp/…`),
//! which is why the comparison goes through `realpath`.
//!
//! The base is then `slide + __TEXT.vmaddr`, not the slide: a locally built image has
//! `vmaddr == 0`, where the slide alone looks right, while a system library does not and the slide
//! is short by exactly that. That is read out of the loaded image's own load commands, which is the
//! same object layout `reloc` and the loader read.

use crate::os::dll::Mode;
use std::ffi::{CStr, CString};

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

// The loader's image list, declared here rather than taken from `libc`, all of whose bindings for
// it carry a deprecation notice pointing at a crate mirvm does not depend on. The structures are
// this platform's object layout, which is also what `reloc` and the loader read.

/// A 64-bit Mach-O header: what a loaded image starts with.
#[repr(C)]
struct MachHeader64 {
    magic: u32,
    cputype: i32,
    cpusubtype: i32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

/// The head of every load command, whose `cmdsize` walks to the next one.
#[repr(C)]
struct LoadCommand {
    cmd: u32,
    cmdsize: u32,
}

/// A 64-bit segment command, whose `vmaddr` is the address the segment was linked at.
#[repr(C)]
struct SegmentCommand64 {
    cmd: u32,
    cmdsize: u32,
    segname: [u8; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: i32,
    initprot: i32,
    nsects: u32,
    flags: u32,
}

unsafe extern "C" {
    fn dyld_image_count() -> u32;
    fn dyld_get_image_header(index: u32) -> *const MachHeader64;
    fn dyld_get_image_name(index: u32) -> *const libc::c_char;
    fn dyld_get_image_vmaddr_slide(index: u32) -> libc::intptr_t;
}

/// The load base of a handle: the address the image's `__TEXT` segment was mapped at.
///
/// `handle` is not consulted — see the module header for why it cannot be — so the image is found
/// by the path it was opened under, resolved the way the loader resolves it.
pub fn load_bias(_handle: usize, path: &CStr) -> Option<usize> {
    let resolved = resolve(path)?;
    let index = (0..unsafe { dyld_image_count() }).find(|&index| {
        let name = unsafe { dyld_get_image_name(index) };
        !name.is_null() && unsafe { CStr::from_ptr(name) } == resolved.as_c_str()
    })?;
    let header = unsafe { dyld_get_image_header(index) };
    if header.is_null() {
        return None;
    }
    let text = text_segment_vmaddr(header)?;
    let slide = unsafe { dyld_get_image_vmaddr_slide(index) } as usize;
    slide.checked_add(text)
}

/// The path the loader would report for `path`, which is what its image list is keyed by.
fn resolve(path: &CStr) -> Option<CString> {
    let mut buffer = [0 as libc::c_char; libc::PATH_MAX as usize];
    let result = unsafe { libc::realpath(path.as_ptr(), buffer.as_mut_ptr()) };
    if result.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_owned())
}

/// `__TEXT`'s `vmaddr`, read from the loaded image's own load commands.
///
/// The image is this build's, so a header that is not 64-bit is refused rather than read with the
/// wrong command offset.
fn text_segment_vmaddr(header: *const MachHeader64) -> Option<usize> {
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const LC_SEGMENT_64: u32 = 0x19;

    if unsafe { (*header).magic } != MH_MAGIC_64 {
        return None;
    }
    let mut cursor = unsafe { header.cast::<u8>().add(size_of::<MachHeader64>()) };
    let commands = unsafe { (*header).ncmds };
    for _ in 0..commands {
        let command = unsafe { &*cursor.cast::<LoadCommand>() };
        if command.cmd == LC_SEGMENT_64 {
            let segment = unsafe { &*cursor.cast::<SegmentCommand64>() };
            if segment.segname == *b"__TEXT\0\0\0\0\0\0\0\0\0\0" {
                return Some(segment.vmaddr as usize);
            }
        }
        cursor = unsafe { cursor.add(command.cmdsize as usize) };
    }
    None
}
