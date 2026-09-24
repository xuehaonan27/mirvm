//! Handing the unwinder a frame description that was not in the image at load time.
//!
//! See the subsystem file one level up for what this is for, and for why this half exists at all:
//! the `libgcc`-compatible entry point the other platform uses is present here too, but it does not
//! put a frame in front of this platform's unwinder.
//!
//! What does is this platform's own registration, and it takes one FDE at a time, so a record list
//! has to be walked and its FDEs handed over individually. Each names the CIE it shares by address,
//! which is why the bytes have to stay where the section put them: a caller that hands over a
//! section keeps it alive for the process, and nothing here copies anything.
//!
//! Measured, because the difference is invisible otherwise: a hand-built CIE and FDE registered
//! through `__register_frame` leaves `_Unwind_Backtrace` stopping at the frame, while the same
//! bytes registered through `__unw_add_dynamic_fde` walk straight out of it.

unsafe extern "C" {
    fn __unw_add_dynamic_fde(fde: *const u8) -> bool;
    fn __unw_remove_dynamic_fde(fde: *const u8) -> bool;
}

pub(crate) fn register_frame(fde: *const u8) {
    unsafe { __unw_add_dynamic_fde(fde) };
}

pub(crate) fn deregister_frame(fde: *const u8) {
    unsafe { __unw_remove_dynamic_fde(fde) };
}

/// One record list, CIE and its FDEs together, terminated by a zero-length record.
pub(crate) fn register_section(section: *const u8) {
    for fde in record_list(section) {
        register_frame(fde);
    }
}

/// The FDE records of a `.eh_frame` record list, in order, with the CIEs left out.
///
/// Each record is a length and then that many bytes; a zero length ends the list. A record whose
/// first four bytes are zero is a CIE, and one whose are not is an FDE naming the CIE it shares as
/// the distance back to it.
fn record_list(section: *const u8) -> Vec<*const u8> {
    let mut out = Vec::new();
    let mut cursor = section;
    loop {
        let length = unsafe { std::ptr::read_unaligned(cursor.cast::<u32>()) };
        if length == 0 {
            break;
        }
        let identifier = unsafe { std::ptr::read_unaligned(cursor.add(4).cast::<u32>()) };
        if identifier != 0 {
            out.push(cursor);
        }
        cursor = unsafe { cursor.add(4 + length as usize) };
    }
    out
}
