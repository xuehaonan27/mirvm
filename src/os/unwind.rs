//! The Itanium C++ ABI unwinder.
//!
//! Mirvm raises and catches guest exceptions by speaking the ABI the C++ runtime above it already
//! speaks, which makes the unwinder a library interface rather than a kernel one: `_Unwind_*` are
//! the same entry points with the same meanings on every platform this build supports (libgcc_s on
//! Linux, libSystem on macOS). Raising, resuming, deleting and walking are all shared, and so is the
//! record layout a section of frame descriptions uses.
//!
//! Handing the unwinder a frame description that was *not* in the image at load time is not shared,
//! because the entry point that does it is not the same one everywhere: the `libgcc`-compatible
//! `__register_frame` exists on both platforms but only one of them acts on it. Each platform's half
//! is in its own directory and is reached through the pair of names below.

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

#[cfg(target_os = "linux")]
use super::linux::unwind::register_section;
/// One frame description entry, and the way to take it back: what a loader that maps an image's
/// `.eh_frame` itself has in hand. Each platform's half decides how that is spelled, and a caller
/// names one pair of names either way.
#[cfg(target_os = "linux")]
pub(crate) use super::linux::unwind::{deregister_frame, register_frame};
#[cfg(target_os = "macos")]
use super::macos::unwind::register_section;
#[cfg(target_os = "macos")]
pub(crate) use super::macos::unwind::{deregister_frame, register_frame};

/// Register one complete `.eh_frame` section with the process unwinder.
///
/// The CIE records at the start of the section are shared by its FDE records, so the section is the
/// unit a caller has in hand; what the platform does with it -- one call, or one per FDE -- is its
/// own half's business. The unwinder retains the bytes for the process lifetime, which is why they
/// are leaked rather than owned.
pub fn register_frame_section(mut bytes: Vec<u8>) {
    let _gate = register_gate_exclusive();
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    // The registration entry point mutates the unwinder's own record list, which is not reentrant:
    // mirvm registers from every Engine's compile worker, and two sections handed over at once can
    // interleave inside that list. One process lock makes the handover atomic in time, which is all
    // the list needs; the bytes themselves are already process-lifetime.
    static REGISTER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = REGISTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    register_section(bytes.as_ptr());
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

/// The registration/walk gate: a thread that is inside an unwinder walk holds it shared, and a
/// `.eh_frame` registration holds it exclusively.
///
/// libgcc mutates its object list under a lock it does not take on the lookup path, so registering a
/// batch while another thread walks that list can end as `_URC_END_OF_STACK` instead of a caught
/// exception. The walk itself is the whole window: `raise` and `resume_or_rethrow` return once the
/// unwinder has found its context, so a guard around either call covers exactly the lookups, and a
/// landing pad that runs afterwards (a cleanup that asks for a compilation, for instance) no longer
/// holds it -- which is what keeps this gate free of any ordering against the JIT compile service.
static WALK_GATE: std::sync::atomic::AtomicPtr<std::sync::RwLock<()>> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// The gate, allocating it on first use. The pointer is only ever replaced by a forked child
/// resetting the lock it may have inherited in a held state (see [`reset_gate_after_fork`]).
fn gate() -> &'static std::sync::RwLock<()> {
    use std::sync::atomic::Ordering;
    loop {
        let current = WALK_GATE.load(Ordering::Acquire);
        if !current.is_null() {
            return unsafe { &*current };
        }
        let fresh = Box::into_raw(Box::new(std::sync::RwLock::new(())));
        if WALK_GATE
            .compare_exchange(
                std::ptr::null_mut(),
                fresh,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            return unsafe { &*fresh };
        }
        drop(unsafe { Box::from_raw(fresh) });
    }
}

/// Held for the extent of one unwinder walk.
fn walk_gate_shared() -> std::sync::RwLockReadGuard<'static, ()> {
    gate().read().unwrap_or_else(|error| error.into_inner())
}

/// Held while a `.eh_frame` section is registered.
fn register_gate_exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
    gate().write().unwrap_or_else(|error| error.into_inner())
}

/// A forked child inherits the lock but not the thread that held it, so it must not keep it: a new
/// lock is installed and the inherited one is left to the child's own memory, exactly as the
/// service-thread counter is reset next door.
pub(crate) fn reset_gate_after_fork() {
    use std::sync::atomic::Ordering;
    let fresh = Box::into_raw(Box::new(std::sync::RwLock::new(())));
    let old = WALK_GATE.swap(fresh, Ordering::AcqRel);
    if !old.is_null() {
        // SAFETY: the child is single-threaded and no reader or writer can be holding the old lock,
        // because the only thread that could have is not in the child.
        drop(unsafe { Box::from_raw(old) });
    }
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
    let _walk = walk_gate_shared();
    unsafe { _Unwind_RaiseException(exception) }
}

pub unsafe fn resume_or_rethrow(exception: *mut RawException) -> ! {
    let _walk = walk_gate_shared();
    let reason = unsafe { _Unwind_Resume_or_Rethrow(exception) };
    eprintln!("mirvm: _Unwind_Resume_or_Rethrow unexpectedly returned {reason}");
    std::process::abort()
}

pub unsafe fn delete(exception: *mut RawException) {
    unsafe { _Unwind_DeleteException(exception) }
}
