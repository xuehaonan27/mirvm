//! Linux pthread primitives.
//! - TLS key family
//! - stack boundary detection
//! - `attr` stack size
//! - `/proc/self/task` thread count
//!
//! Primitives are not adjudicated. Detailed behaviours like `ctx` attach,
//! dtor semantics, fork baseline determination, stack amplification policies,
//! safety rules, are all left to the engine. The module only do honest pthread
//! read/write.
//! Type discipline: `attr` pointers are entered and exited as `c_void` (libc
//!  pthread type signatures are not leaked).
//!
//! The raw futex wait/wake and the user-address-width test are x86_64's half of this file's
//! knowledge and live in [`crate::os_arch::thread`]; they are re-exported below so a caller
//! keeps one name on every Linux CPU.

use std::ffi::c_void;

/// The pair's half of this module: raw futex operations that must not touch libc `errno`, and
/// the architecture's user-address bound that makes glibc's "no stack set" sentinel
/// recognizable.
pub use crate::os_arch::thread::{futex_wait_raw, futex_wake_one_raw, stack_addr_is_unset};

/// A host thread, as the C library names it.
///
/// Kept opaque: the engine keys its per-thread tables by thread identity and never inspects one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThreadId(libc::pthread_t);

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
/// keys above the process-lifetime one in the order that pass uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TlsKey(libc::pthread_key_t);

impl TlsKey {
    /// The library's own key number. Only a test that drives the raw ABI needs it: product code
    /// keeps the key opaque and asks [`TlsKey::from_raw`] when a library hands it one.
    #[cfg(test)]
    pub fn as_raw(self) -> libc::pthread_key_t {
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
/// `out` must be valid for one `pthread_key_t`, and `dtor` a valid destructor.
pub unsafe fn tls_key_create_raw(
    out: *mut libc::pthread_key_t,
    dtor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> i32 {
    unsafe { libc::pthread_key_create(out, dtor) }
}

/// Thin wrapper of `pthread_key_create`.
/// Panic on failure. The engine does not have a keyless downgrade path.
pub fn tls_key_create(dtor: Option<unsafe extern "C" fn(*mut c_void)>) -> TlsKey {
    let mut k: libc::pthread_key_t = 0;
    let rc = unsafe { tls_key_create_raw(&mut k, dtor) };
    assert_eq!(rc, 0, "pthread_key_create failed: {rc}");
    TlsKey(k)
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
/// # Safety
/// The pointers must satisfy `pthread_create`'s contract.
pub unsafe fn spawn_raw(
    thread: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    value: *mut c_void,
) -> i32 {
    unsafe { libc::pthread_create(thread, attr, start, value) }
}

/// This thread's stack [lo, lo+size) (pthread_getattr_np + getstack + destroy).
/// The main thread's `getattr` function internally reads `/proc` (non-hot path
///  primitive). `None` on failure (conservative handled by the caller).
pub fn current_stack_bounds() -> Option<(usize, usize)> {
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let rc = attr_stack_bounds(&mut attr as *mut _ as *mut c_void);
        libc::pthread_attr_destroy(&mut attr);
        rc
    }
}

/// The attr's stack setting (a `pthread_attr_getstack` passthrough):
/// Some((lo, size)), or None on failure. Note that the bogus address glibc
/// produces when no stack size was set is recognized by `stack_addr_is_unset`;
/// callers must not interpret `lo` themselves.
pub fn attr_stack_bounds(attr: *mut c_void) -> Option<(usize, usize)> {
    let mut lo: *mut libc::c_void = std::ptr::null_mut();
    let mut size: libc::size_t = 0;
    let rc = unsafe {
        libc::pthread_attr_getstack(attr as *mut libc::pthread_attr_t, &mut lo, &mut size)
    };
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
/// `/proc/self/task` directory entry count. The only reliable source of fork
/// guard baseline—new threads exist immediately after pthread_create returns.
/// Application-level counters have a TOCTOU window.
/// Returns 0 on failure.
pub fn os_thread_count() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0)
}

/// Count of live MIRVM-owned service threads in this process (capture writer,
/// and later the profile/services family). These are invisible to the guest, so
/// they must not be attributed to guest `pthread_create` when the fork guard
/// compares `/proc/self/task` against its baseline (see
/// `crate::vm::ctx::guest_spawned_threads`).
static SERVICE_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Mark the calling thread as a MIRVM service thread for as long as the guard
/// lives. Registration happens before the new thread can be observed by the
/// fork guard, because `std::thread::spawn` only returns after the child has
/// started and run its first instructions.
#[must_use = "dropping the guard immediately would unregister the service thread"]
pub struct ServiceThreadGuard(());

impl ServiceThreadGuard {
    pub fn register() -> Self {
        SERVICE_THREADS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(())
    }
}

impl Drop for ServiceThreadGuard {
    fn drop(&mut self) {
        SERVICE_THREADS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Live MIRVM service threads. Saturating: a wrong value here must never wrap
/// into a huge subtraction.
pub fn service_thread_count() -> usize {
    SERVICE_THREADS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Reset service-thread accounting in a forked child. The child inherits the
/// counter but none of the threads it counted: `fork` duplicates only the
/// calling thread, so the parent's writer and any other service thread are
/// gone. Also re-pin the fork baseline via the caller, which must read
/// `/proc/self/task` in the child.
///
/// Only atomic stores plus a `/proc` read run here, so this is safe to call
/// from the post-fork child before it touches any inherited lock.
pub fn reset_service_threads_after_fork() {
    SERVICE_THREADS.store(0, std::sync::atomic::Ordering::SeqCst);
}
