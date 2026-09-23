//! The counters a session claims for its process: the session id its file header records, and the
//! fork generation that makes a child's capture distinguishable from its parent's.

use super::*;

pub(crate) static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Fork generation of this process. `fork` duplicates it, so the child sees the
/// parent's value and must not reuse it: the first session started after the
/// fork takes the next number (`reset_generation_after_fork` marks that). The
/// header value identifies which process's records a file holds, so a parent and
/// child writing concurrently cannot be mistaken for one stream.
static PROCESS_GENERATION: AtomicU64 = AtomicU64::new(0);
static GENERATION_PENDING: AtomicBool = AtomicBool::new(false);

/// Generation this process's next capture session belongs to.
pub(crate) fn claim_process_generation() -> u64 {
    if GENERATION_PENDING.swap(false, Ordering::SeqCst) {
        PROCESS_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
    } else {
        PROCESS_GENERATION.load(Ordering::SeqCst)
    }
}

/// Mark that this process is a `fork` child: its inherited generation belongs to
/// the parent, so the first session it starts must take the next one.
pub(super) fn reset_generation_after_fork() {
    GENERATION_PENDING.store(true, Ordering::SeqCst);
}
