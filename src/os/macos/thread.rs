//! macOS pthread primitives.
//! - TLS key family
//! - stack boundary detection
//! - `attr` stack size
//! - Mach thread count
//!
//! Primitives are not adjudicated. Detailed behaviours like `ctx` attach,
//! dtor semantics, fork baseline determination, stack amplification policies,
//! safety rules, are all left to the engine. The module only do honest pthread
//! read/write.
//! Type discipline: `attr` pointers are entered and exited as `c_void` (libc
//!  pthread type signatures are not leaked).
//!
//! The raw wait/wake and the user-address width test are this pair's and live in
//! [`crate::os_arch::thread`]; they are re-exported below so a caller keeps one name on this
//! platform.
//!
//! The service-thread accounting is not this platform's at all and lives in [`crate::os::thread`].

use std::ffi::c_void;

unsafe extern "C" {
    /// This platform's C library declares the function but the `libc` crate binds only
    /// `pthread_attr_getstackaddr`, which answers a different question (the caller-supplied base
    /// rather than the thread's own range).
    fn pthread_attr_getstack(
        attr: *const libc::pthread_attr_t,
        stackaddr: *mut *mut libc::c_void,
        stacksize: *mut libc::size_t,
    ) -> libc::c_int;
}

/// The pair's half of this module: raw futex operations that must not touch libc `errno`, and
/// the architecture's user-address bound that makes glibc's "no stack set" sentinel
/// recognizable.
pub use crate::os_arch::thread::{futex_wait_raw, futex_wake_one_raw, stack_addr_is_unset};

/// A host thread, as the C library names it.
///
/// Kept opaque: the engine keys its per-thread tables by thread identity and never inspects one.
/// `repr(transparent)` because an interposed `pthread_create` hands the caller's own
/// `pthread_t *` straight through, so the wrapper has to be the library's type.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThreadId(libc::pthread_t);

impl ThreadId {
    /// The library's own handle, for a call inside this layer that has to forward it.
    #[cfg(test)]
    pub(crate) fn raw(self) -> libc::pthread_t {
        self.0
    }

    /// The identity as a plain integer, which is the form a test keeps in an atomic or sends
    /// through a channel.
    ///
    /// `pthread_t` is the library's own opaque integer type, so flattening it is this file's
    /// business and not a caller's.
    #[cfg(test)]
    pub fn as_u64(self) -> u64 {
        self.0 as u64
    }

    /// Wrap a handle this process did not create, from the integer form the engine carries it in.
    ///
    /// The engine carries a thread identity as the `u64` a caller hands around, while each
    /// platform's `pthread_t` is an integer of its own width (`uintptr_t` here, `c_ulong` on
    /// Linux), so neither width is the parameter's type.
    #[cfg(test)]
    pub fn from_raw(raw: u64) -> Self {
        ThreadId(raw as usize as libc::pthread_t)
    }
}

/// The calling thread's identity.
pub fn current_thread() -> ThreadId {
    ThreadId(unsafe { libc::pthread_self() })
}

/// The library's `EINVAL`, which is what a thread-specific-data call returns for a key that is
/// already gone: the state POSIX puts a destructor in, having cleared the thread's values before
/// running them.
pub const TLS_KEY_GONE: i32 = libc::EINVAL;

/// The library's `EINVAL` as the create/spawn calls use it: an argument they refuse, which for an
/// interposed call is a callback this process cannot wrap rather than anything the caller can fix.
pub const INVALID_ARGUMENT: i32 = libc::EINVAL;

/// pthread TLS key.
///
/// Unique within the process after creation (`dtor` determined by the caller). Ordered by the
/// library's own key number, which is what the final pthread-destructor pass walks to visit the
/// keys above the process-lifetime one in the order that pass uses. `repr(transparent)` for the
/// same reason as [`ThreadId`]: an interposed call passes the caller's own `pthread_key_t *`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TlsKey(libc::pthread_key_t);

impl TlsKey {
    /// The key as the plain integer a test drives the raw ABI with.
    #[cfg(test)]
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// Wrap a key this process did not create, which is the shape an interposed
    /// `pthread_setspecific`/`pthread_key_delete` receives from its caller.
    pub fn from_raw(raw: libc::pthread_key_t) -> Self {
        TlsKey(raw)
    }
}

/// `pthread_key_create` into a caller-owned slot, returning the library's code.
///
/// The slot is raw because an interposed call writes the key into its caller's own storage;
/// [`tls_key_create`] is the shape the engine uses for its own keys.
///
/// # Safety
/// `out` must be valid for one [`TlsKey`] (the library's `pthread_key_t`), and `dtor` a valid
/// destructor.
pub unsafe fn tls_key_create_raw(
    out: *mut TlsKey,
    dtor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> i32 {
    unsafe { libc::pthread_key_create(out.cast(), dtor) }
}

/// Thin wrapper of `pthread_key_create`.
/// Panic on failure. The engine does not have a keyless downgrade path.
pub fn tls_key_create(dtor: Option<unsafe extern "C" fn(*mut c_void)>) -> TlsKey {
    let mut key = TlsKey(0);
    let rc = unsafe { tls_key_create_raw(&mut key, dtor) };
    assert_eq!(rc, 0, "pthread_key_create failed: {rc}");
    key
}

/// Thin wrapper of `pthread_key_delete`, returning the library's code.
pub fn tls_key_delete(key: TlsKey) -> i32 {
    unsafe { libc::pthread_key_delete(key.0) }
}

/// Thin wrapper of `pthread_getspecific`.
///
/// # Safety
/// Consistent with libc semantics.
pub unsafe fn tls_get(key: TlsKey) -> *mut c_void {
    unsafe { libc::pthread_getspecific(key.0) }
}

/// Thin wrapper of `pthread_setspecific`.
///
/// The code comes back instead of being judged here: a caller clearing a managed key tolerates
/// [`TLS_KEY_GONE`] and one installing a value does not, and only the caller knows which it is.
///
/// # Safety
/// Consistent with libc semantics.
pub unsafe fn tls_set(key: TlsKey, p: *const c_void) -> i32 {
    unsafe { libc::pthread_setspecific(key.0, p) }
}

/// `pthread_create` with the caller's own start routine, returning the library's code.
///
/// `attr` is entered as [`c_void`] like every other use of an attribute in this file: the attribute
/// is the caller's, and only [`attr_stack_bounds`]/[`attr_set_stack_size`] look inside it.
///
/// # Safety
/// The pointers must satisfy `pthread_create`'s contract.
pub unsafe fn spawn_raw(
    thread: *mut ThreadId,
    attr: *const c_void,
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    value: *mut c_void,
) -> i32 {
    unsafe { libc::pthread_create(thread.cast(), attr.cast(), start, value) }
}

/// `pthread_join`: wait for `thread` and return the library's code.
///
/// The return value the thread produced is discarded: the only callers are this crate's tests
/// draining a thread they started, and none of them carries a result out.
#[cfg(test)]
pub fn join_raw(thread: ThreadId) -> i32 {
    unsafe { libc::pthread_join(thread.0, std::ptr::null_mut()) }
}

/// This thread's stack [lo, lo+size).
///
/// The library exposes the top and the size rather than a base, and both are real for every thread
/// on this platform, so there is no "never set" sentinel to recognize here.
pub fn current_stack_bounds() -> Option<(usize, usize)> {
    let top = unsafe { libc::pthread_get_stackaddr_np(libc::pthread_self()) } as usize;
    let size = unsafe { libc::pthread_get_stacksize_np(libc::pthread_self()) };
    if top == 0 || size == 0 || top < size {
        return None;
    }
    Some((top - size, size))
}

/// The attr's stack setting (a `pthread_attr_getstack` passthrough):
/// Some((lo, size)), or None on failure. An attribute that never had a stack setting may still
/// report an address outside the user range, which `stack_addr_is_unset` recognizes; callers must
/// not interpret `lo` themselves.
pub fn attr_stack_bounds(attr: *mut c_void) -> Option<(usize, usize)> {
    let mut lo: *mut libc::c_void = std::ptr::null_mut();
    let mut size: libc::size_t = 0;
    let rc =
        unsafe { pthread_attr_getstack(attr as *const libc::pthread_attr_t, &mut lo, &mut size) };
    if rc != 0 || size == 0 {
        return None;
    }
    Some((lo as usize, size))
}

/// Thin wrapper of `pthread_attr_setstacksize`.
/// Returns `true` on success.
pub fn attr_set_stack_size(attr: *mut c_void, size: usize) -> bool {
    unsafe { libc::pthread_attr_setstacksize(attr as *mut libc::pthread_attr_t, size) == 0 }
}

/// Get OS thread count.
/// The task's own thread list, which is the only reliable source of fork guard baseline: new
/// threads exist immediately after pthread_create returns. Application-level counters have a
/// TOCTOU window. Returns 0 on failure.
pub fn os_thread_count() -> usize {
    let mut list: libc::thread_act_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;
    let rc = unsafe { libc::task_threads(super::task_self(), &mut list, &mut count) };
    if rc != libc::KERN_SUCCESS || list.is_null() {
        return 0;
    }
    let size = count as usize * std::mem::size_of::<libc::thread_t>();
    unsafe { libc::vm_deallocate(super::task_self(), list as libc::vm_address_t, size) };
    count as usize
}
