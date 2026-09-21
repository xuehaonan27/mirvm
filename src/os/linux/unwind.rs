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
