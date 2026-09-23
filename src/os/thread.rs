//! The thread bookkeeping that is mirvm's own, and the platform's thread primitives.
//!
//! A count of mirvm's service threads is not a kernel fact. Mirvm starts threads the guest cannot
//! see, and its fork guard has to subtract them from whatever the platform reports; the counter and
//! the guard are the same atomic on every platform, so they are declared here.
//!
//! What is the platform's stays in the platform directory: pthread keys and spawn, the `attr` stack
//! bounds and the sentinel a missing one leaves behind, and how that platform counts the threads in
//! its own process.
//!
//! The platform half must provide, under the same names a caller already uses: `ThreadId`,
//! `TlsKey`, `current_thread`, `tls_key_create`, `tls_key_delete`, `tls_get`, `tls_set`,
//! `spawn_raw`, `current_stack_bounds`, `attr_stack_bounds`, `attr_set_stack_size`,
//! `os_thread_count`, and the raw futex and error-code leaves.

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
/// gone. Also re-pin the fork baseline via the caller, which must read the
/// platform's own thread count in the child.
///
/// Only an atomic store runs here, so this is safe to call from the post-fork
/// child before it touches any inherited lock.
pub fn reset_service_threads_after_fork() {
    SERVICE_THREADS.store(0, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(target_os = "linux")]
pub(crate) use super::linux::thread::*;
#[cfg(target_os = "macos")]
pub(crate) use super::macos::thread::*;
