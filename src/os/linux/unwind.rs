//! Handing the unwinder a frame description that was not in the image at load time.
//!
//! See the subsystem file one level up for what this is for. Here it is one call: this platform's
//! `libgcc`-compatible entry point registers a whole record list at once, which is why the section
//! is the unit a caller hands over and nothing has to be walked.

unsafe extern "C" {
    fn __register_frame(begin: *const u8);
    fn __deregister_frame(begin: *const u8);
}

pub(crate) fn register_frame(fde: *const u8) {
    unsafe { __register_frame(fde) };
}

pub(crate) fn deregister_frame(fde: *const u8) {
    unsafe { __deregister_frame(fde) };
}

/// One record list, CIE and its FDEs together, terminated by a zero-length record.
pub(crate) fn register_section(section: *const u8) {
    unsafe { __register_frame(section) };
}
