//! Handing the unwinder a frame description that was not in the image at load time.
//!
//! See the subsystem file one level up for what this is for, and for why this half exists at all.
//!
//! The difference from the other platform is the *unit*, and it is the whole of it: the
//! `libgcc`-compatible entry point is here too, but it takes **one FDE**, where the other platform's
//! reads a whole record list from wherever it is pointed. So handing it a section -- which starts
//! with a CIE -- registers a record that describes nothing, and the frame stays invisible.
//!
//! Measured on a hand-built CIE and FDE, four registrations of the same bytes and the frames
//! `_Unwind_Backtrace` collected from a function reached through them:
//!
//! | call                             | frames |
//! |----------------------------------|--------|
//! | `__register_frame(section start)` | 1      |
//! | `__register_frame(fde)`           | 4      |
//! | `__unw_add_dynamic_fde(fde)`      | 4      |
//!
//! So this half walks the list and hands over its FDEs, through this platform's own entry point
//! because that one says what it takes. Each FDE names the CIE it shares by address, which is why
//! the bytes have to stay where the section put them: a caller that hands over a section keeps it
//! alive for the process, and nothing here copies anything.

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

/// One record list, CIE and its FDEs together, terminated by a zero-length record. What is handed
/// over is the FDEs: this platform's registration takes one, and the list's first record is a CIE.
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
