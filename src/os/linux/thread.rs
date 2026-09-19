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

use std::ffi::c_void;

#[cfg(target_arch = "x86_64")]
std::arch::global_asm!(
    ".globl mirvm_futex_wait_raw",
    ".hidden mirvm_futex_wait_raw",
    ".type mirvm_futex_wait_raw,@function",
    "mirvm_futex_wait_raw:",
    "mov edx, esi",
    "mov eax, 202",
    "mov esi, 128",
    "mov r10d, 0",
    "syscall",
    "ret",
    ".size mirvm_futex_wait_raw, .-mirvm_futex_wait_raw",
    ".globl mirvm_futex_wake_one_raw",
    ".hidden mirvm_futex_wake_one_raw",
    ".type mirvm_futex_wake_one_raw,@function",
    "mirvm_futex_wake_one_raw:",
    "mov eax, 202",
    "mov esi, 129",
    "mov edx, 1",
    "syscall",
    "ret",
    ".size mirvm_futex_wake_one_raw, .-mirvm_futex_wake_one_raw",
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn mirvm_futex_wait_raw(addr: *const u32, expected: u32) -> i64;
    fn mirvm_futex_wake_one_raw(addr: *const u32) -> i64;
}

/// pthread TLS key.
/// Unique within the process after creation (`dtor` determined by the caller).
#[derive(Clone, Copy)]
pub struct TlsKey(libc::pthread_key_t);

impl TlsKey {
    pub fn as_raw(self) -> libc::pthread_key_t {
        self.0
    }
}

/// Thin wrapper of `pthread_key_create`.
/// Panic on failure. The engine does not have a keyless downgrade path.
pub fn tls_key_create(dtor: Option<unsafe extern "C" fn(*mut c_void)>) -> TlsKey {
    let mut k: libc::pthread_key_t = 0;
    let rc = unsafe { libc::pthread_key_create(&mut k, dtor) };
    assert_eq!(rc, 0, "pthread_key_create failed: {rc}");
    TlsKey(k)
}

/// Thin wrapper of `pthread_getspecific`.
///
/// # Safety
/// Consistent with libc semantics.
pub unsafe fn tls_get(key: TlsKey) -> *mut c_void {
    unsafe { libc::pthread_getspecific(key.0) }
}

/// Thin wrapper of `pthread_setspecific`
///
/// # Safety
/// Consistent with libc semantics.
pub unsafe fn tls_set(key: TlsKey, p: *mut c_void) {
    unsafe { libc::pthread_setspecific(key.0, p) };
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

/// glibc detail: an attr that was never setstack'd holds stackaddr=NULL
/// internally, so getstack returns `NULL - stacksize` (a bogus address near the
/// top of u64) instead of NULL. x86_64 user addresses fit in 47 bits, so
/// anything above that range means "unset"; a real user stack address (a
/// guest-provided stack) falls inside it.
pub fn stack_addr_is_unset(lo: usize) -> bool {
    lo == 0 || lo >= 1 << 48
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
/// `crate::vm::engine::ctx::guest_spawned_threads`).
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

/// Wait while `*addr == expected`, returning the kernel's raw result. This
/// leaf never writes libc `errno`; callers use it for telemetry wakeups that
/// must be invisible to the guest syscall contract.
#[cfg(target_arch = "x86_64")]
pub fn futex_wait_raw(addr: *const u32, expected: u32) -> i64 {
    // SAFETY: the caller keeps the aligned atomic word alive for the wait.
    unsafe { mirvm_futex_wait_raw(addr, expected) }
}

/// Wake at most one waiter, returning the kernel's raw result without touching
/// libc `errno`.
#[cfg(target_arch = "x86_64")]
pub fn futex_wake_one_raw(addr: *const u32) -> i64 {
    // SAFETY: the caller keeps the aligned atomic word alive for the syscall.
    unsafe { mirvm_futex_wake_one_raw(addr) }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn raw_futex_wait_passes_expected_without_touching_errno() {
        let word = AtomicU32::new(1);
        unsafe { *libc::__errno_location() = 73 };

        let rc = futex_wait_raw(word.as_ptr(), 0);

        assert_eq!(rc, -(libc::EAGAIN as i64));
        assert_eq!(unsafe { *libc::__errno_location() }, 73);
    }
}
