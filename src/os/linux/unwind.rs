//! Linux/x86_64 Itanium unwinder primitives.

#[repr(C)]
pub struct RawException {
    pub class: u64,
    pub cleanup: Option<extern "C" fn(i32, *mut RawException)>,
    pub private: [*const u8; 2],
}

unsafe extern "C-unwind" {
    fn _Unwind_RaiseException(exception: *mut RawException) -> i32;
    fn _Unwind_Resume_or_Rethrow(exception: *mut RawException) -> i32;
}

unsafe extern "C" {
    fn _Unwind_DeleteException(exception: *mut RawException);
    fn __register_frame(begin: *const u8);
    fn __deregister_frame(begin: *const u8);
}

/// Register one frame description entry with the process unwinder.
///
/// A whole section goes through [`register_frame_section`]; this one takes a single record, which
/// is what a loader that maps an image's `.eh_frame` itself has in hand.
pub fn register_frame(fde: *const u8) {
    unsafe { __register_frame(fde) };
}

/// Undo [`register_frame`] for an image that is about to be unmapped.
pub fn deregister_frame(fde: *const u8) {
    unsafe { __deregister_frame(fde) };
}

/// Register one complete `.eh_frame` section with the process unwinder.
///
/// The CIE records at the start of the section are shared by its FDE records, so the registration
/// unit has to be the complete section, not a single FDE. The unwinder retains the bytes for the
/// process lifetime, which is why they are leaked rather than owned.
pub fn register_frame_section(mut bytes: Vec<u8>) {
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    unsafe { __register_frame(bytes.as_ptr()) };
}

/// One frame's context, as the unwinder hands it to a trace callback.
pub type Context = *mut std::ffi::c_void;

unsafe extern "C" {
    #[link_name = "_Unwind_Backtrace"]
    fn unwind_backtrace(trace: extern "C" fn(Context, Context) -> i32, arg: Context) -> i32;
    #[link_name = "_Unwind_GetIP"]
    fn unwind_get_ip(context: Context) -> usize;
    #[link_name = "_Unwind_GetCFA"]
    fn unwind_get_cfa(context: Context) -> usize;
}

/// Walk the live machine stack, calling `trace` once per frame until it returns non-zero.
///
/// The frames are real machine frames. A caller that has its own shadow stack merges the two; this
/// layer only reports what the unwinder sees.
///
/// # Safety
/// `trace` must be a valid callback, and `arg` must stay valid for the whole walk.
pub unsafe fn backtrace(trace: extern "C" fn(Context, Context) -> i32, arg: Context) {
    unsafe { unwind_backtrace(trace, arg) };
}

/// The instruction pointer of a frame the callback was handed.
///
/// # Safety
/// `context` must be a context the unwinder passed to a trace callback.
pub unsafe fn frame_ip(context: Context) -> usize {
    unsafe { unwind_get_ip(context) }
}

/// The canonical frame address of a frame the callback was handed.
///
/// # Safety
/// Same requirement as [`frame_ip`].
pub unsafe fn frame_cfa(context: Context) -> usize {
    unsafe { unwind_get_cfa(context) }
}

pub unsafe fn raise(exception: *mut RawException) -> i32 {
    unsafe { _Unwind_RaiseException(exception) }
}

pub unsafe fn resume_or_rethrow(exception: *mut RawException) -> ! {
    let reason = unsafe { _Unwind_Resume_or_Rethrow(exception) };
    eprintln!("mirvm: _Unwind_Resume_or_Rethrow unexpectedly returned {reason}");
    std::process::abort()
}

pub unsafe fn delete(exception: *mut RawException) {
    unsafe { _Unwind_DeleteException(exception) }
}
