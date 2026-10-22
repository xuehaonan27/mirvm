//! OSR demand belongs to interpreted activations; code and queue state belong to their engine.

use super::*;

#[derive(Default)]
pub(crate) struct OsrState {
    pub(crate) entry: AtomicU64,
    waiting: AtomicU32,
}

impl OsrState {
    pub(crate) fn waiting(&self) -> u32 {
        self.waiting.load(Ordering::Acquire)
    }
}

/// Registered only after an invocation reaches an eligible OSR poll. Holding this across guest
/// calls keeps a suspended caller eligible; return, unwind and transfer release it exactly once.
pub(crate) struct OsrInterest<'a>(&'a AtomicU32);

impl Drop for OsrInterest<'_> {
    fn drop(&mut self) {
        let previous = self.0.fetch_sub(1, Ordering::Release);
        debug_assert_ne!(previous, 0);
    }
}

impl JitState {
    /// Deduplicate requests while retaining the caller's demand until its activation ends.
    pub(crate) fn request_osr<'a>(
        &'a self,
        domain: CodeDomain,
        func: u32,
        interest: &mut Option<OsrInterest<'a>>,
    ) {
        if !self.enabled
            || !self.osr_enabled
            || self.stopping.load(Ordering::Acquire)
            || self.iterations[func as usize].load(Ordering::Relaxed) < ITERATION_REQUEST
        {
            return;
        }
        let state = &self.slots_for(domain).osr[func as usize];
        if !matches!(state.entry.load(Ordering::Acquire), 0 | OSR_QUEUED) {
            return;
        }
        if interest.is_none() {
            state.waiting.fetch_add(1, Ordering::AcqRel);
            *interest = Some(OsrInterest(&state.waiting));
        }
        if state
            .entry
            .compare_exchange(0, OSR_QUEUED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let sent = self.queue.lock().unwrap().as_ref().is_some_and(|queue| {
            queue
                .send(Request {
                    func,
                    tier: Tier::Optimized,
                    osr: true,
                    queued: crate::options::jit_debug().then(std::time::Instant::now),
                })
                .is_ok()
        });
        if !sent {
            state.entry.store(FAIL_SENTINEL, Ordering::Release);
        }
    }

    /// The worker alone cancels, after dequeuing: there is never an old queued request left behind
    /// a reset slot. Demand racing with this check retries at its next sparse poll if needed.
    pub(crate) fn begin_osr(&self, domain: CodeDomain, func: u32) -> bool {
        let state = &self.slots_for(domain).osr[func as usize];
        if state.waiting() != 0 {
            return true;
        }
        let _ = state
            .entry
            .compare_exchange(OSR_QUEUED, 0, Ordering::AcqRel, Ordering::Acquire);
        false
    }

    pub(crate) fn publish_osr(
        &self,
        domain: CodeDomain,
        func: u32,
        entry: u64,
        ranges: Vec<JitSymbolRange>,
    ) {
        for range in ranges {
            self.register_symbol_range_with(range, perf_registry());
        }
        self.slots_for(domain).osr[func as usize]
            .entry
            .store(entry, Ordering::Release);
    }
}
