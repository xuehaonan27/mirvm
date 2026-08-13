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
