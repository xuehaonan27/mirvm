//! Sampled own-body interpretation cost and a serial compile worker's work budget.
//!
//! Credit is observed work, not a prediction of future lifetime or a promised speedup.
//! Calls suspend the clock so a cheap wrapper cannot spend its callee's execution time.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

const SAMPLE_PERIOD: u64 = 64;
const SAMPLE_WORK: u64 = 4096;

fn add(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_add(value))
    });
}

fn nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

#[derive(Default)]
struct Instance {
    entries: AtomicU64,
    own_ns: AtomicU64,
    samples: AtomicU64,
    work: AtomicU64,
    pending: AtomicBool,
}

#[derive(Default)]
struct Budget {
    spent_ns: u64,
    units: u64,
    transitions: u64,
    deferred: u64,
}

impl Budget {
    fn estimate(&self, units: u64) -> u64 {
        (u128::from(self.spent_ns) * u128::from(units) / u128::from(self.units.max(1)))
            .min(u128::from(u64::MAX)) as u64
    }

    fn admit(&mut self, units: u64, instance_ns: u64, total_ns: u64) -> (bool, u64) {
        if self.transitions == 0 {
            return (true, 0);
        }
        let estimate = self.estimate(units);
        let admitted =
            instance_ns >= estimate && total_ns.saturating_sub(self.spent_ns) >= estimate;
        self.deferred += u64::from(!admitted);
        (admitted, estimate)
    }

    fn charge(&mut self, units: u64, elapsed_ns: u64) {
        self.spent_ns = self.spent_ns.saturating_add(elapsed_ns);
        self.units = self.units.saturating_add(units);
        self.transitions += 1;
    }
}

pub(crate) struct WorkLedger {
    instances: Vec<Instance>,
    total_ns: AtomicU64,
    budget: Mutex<Budget>,
    pub(crate) enforce: bool,
    report: bool,
}

impl WorkLedger {
    pub(crate) fn new(count: usize, enforce: bool, report: bool) -> Self {
        Self {
            instances: (0..count).map(|_| Instance::default()).collect(),
            total_ns: AtomicU64::new(0),
            budget: Mutex::new(Budget::default()),
            enforce,
            report,
        }
    }

    pub(crate) fn sample(&self, func: u32) -> Option<WorkSample<'_>> {
        let entry = self.instances[func as usize]
            .entries
            .fetch_add(1, Ordering::Relaxed);
        if !entry.is_multiple_of(SAMPLE_PERIOD) {
            return None;
        }
        Some(WorkSample {
            ledger: self,
            func,
            weight: if entry == 0 { 1 } else { SAMPLE_PERIOD },
            start: Some(Instant::now()),
            elapsed_ns: 0,
            work: 0,
            finished: false,
        })
    }

    /// The channel may contain at most one request for an instance. A deferred request releases
    /// this bit; a completed or rejected compilation keeps it set for the rest of the Engine.
    pub(crate) fn request(&self, func: u32) -> bool {
        !self.instances[func as usize]
            .pending
            .swap(true, Ordering::Relaxed)
    }

    pub(crate) fn release(&self, func: u32) {
        self.instances[func as usize]
            .pending
            .store(false, Ordering::Relaxed);
    }

    pub(crate) fn admit(&self, func: u32, units: u64) -> bool {
        if !self.enforce {
            return true;
        }
        let own_ns = self.instances[func as usize].own_ns.load(Ordering::Relaxed);
        let total_ns = self.total_ns.load(Ordering::Relaxed);
        let (admitted, estimate_ns) = self.budget.lock().unwrap().admit(units, own_ns, total_ns);
        if !admitted {
            self.release(func);
            if self.report {
                crate::diag::instrument(format_args!(
                    "mirvm-generic-tier-deferred: func={func} own_ns={own_ns} estimate_ns={estimate_ns}"
                ));
            }
        }
        admitted
    }

    pub(crate) fn transition(
        &self,
        func: u32,
        units: u64,
        queue_ns: u64,
        specialize_ns: u64,
        worker_ns: u64,
        published: bool,
    ) {
        let own_ns = self.instances[func as usize].own_ns.load(Ordering::Relaxed);
        let mut budget = self.budget.lock().unwrap();
        let estimate_ns = budget.estimate(units);
        budget.charge(units, worker_ns);
        if self.report {
            crate::diag::instrument(format_args!(
                "mirvm-generic-tier-row: func={func} units={units} own_ns={own_ns} \
                 estimate_ns={estimate_ns} queue_ns={queue_ns} specialize_ns={specialize_ns} \
                 worker_ns={worker_ns} published={published}"
            ));
        }
    }

    pub(crate) fn report(&self, final_publish_ns: u64) {
        if !self.report {
            return;
        }
        let budget = self.budget.lock().unwrap();
        let own_ns = self.total_ns.load(Ordering::Relaxed);
        let debt_ns = budget.spent_ns.saturating_sub(own_ns);
        let samples: u64 = self
            .instances
            .iter()
            .map(|v| v.samples.load(Ordering::Relaxed))
            .sum();
        let work: u64 = self
            .instances
            .iter()
            .map(|v| v.work.load(Ordering::Relaxed))
            .sum();
        crate::diag::instrument(format_args!(
            "mirvm-generic-tier: policy={} samples={samples} sample_work={work} own_ns={own_ns} \
             worker_ns={} final_publish_ns={final_publish_ns} debt_ns={debt_ns} transitions={} deferred={}",
            if self.enforce { "budget" } else { "direct" },
            budget.spent_ns,
            budget.transitions,
            budget.deferred,
        ));
    }
}

pub(crate) struct WorkSample<'a> {
    ledger: &'a WorkLedger,
    func: u32,
    weight: u64,
    start: Option<Instant>,
    elapsed_ns: u64,
    work: u64,
    finished: bool,
}

impl WorkSample<'_> {
    pub(crate) fn statements(&mut self, count: usize) -> bool {
        if self.finished {
            return true;
        }
        self.work = self.work.saturating_add(count as u64 + 1);
        if self.work >= SAMPLE_WORK {
            self.finish();
        }
        self.finished
    }

    pub(crate) fn pause(&mut self) {
        if let Some(start) = self.start.take() {
            self.elapsed_ns = self.elapsed_ns.saturating_add(nanos(start.elapsed()));
        }
    }

    pub(crate) fn resume(&mut self) {
        if !self.finished {
            self.start = Some(Instant::now());
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.pause();
        self.finished = true;
        let instance = &self.ledger.instances[self.func as usize];
        let credit = self.elapsed_ns.saturating_mul(self.weight);
        add(&instance.own_ns, credit);
        add(&self.ledger.total_ns, credit);
        add(&instance.work, self.work);
        instance.samples.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for WorkSample<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_debt_blocks_work_until_it_is_repaid() {
        let mut budget = Budget::default();
        assert_eq!(budget.admit(10, 0, 0), (true, 0));
        budget.charge(10, 1000);
        assert_eq!(budget.admit(5, 1000, 1200), (false, 500));
        assert_eq!(budget.admit(5, 400, 2000), (false, 500));
        assert_eq!(budget.admit(5, 500, 1500), (true, 500));
        budget.charge(5, 1500);
        assert_eq!(budget.admit(3, 1000, 2000), (false, 500));
        assert_eq!(budget.admit(3, 1000, 3000), (true, 500));
    }

    #[test]
    fn request_is_unique_until_deferred_or_disconnected() {
        let ledger = WorkLedger::new(1, true, false);
        std::thread::scope(|scope| {
            let ledger = &ledger;
            let handles: Vec<_> = (0..16).map(|_| scope.spawn(|| ledger.request(0))).collect();
            assert_eq!(
                handles
                    .into_iter()
                    .map(|handle| handle.join().unwrap())
                    .filter(|won| *won)
                    .count(),
                1
            );
        });
        ledger.release(0);
        assert!(ledger.request(0));
        assert!(!ledger.request(0));
    }

    #[test]
    fn deferred_requests_retry_without_flooding_the_compile_queue() {
        let mut jit = super::super::JitState::new(2);
        jit.threshold = 1000;
        jit.work = None;
        assert!(jit.call_requests_compile(1000));
        assert!(!jit.call_requests_compile(2000));
        jit.work = Some(WorkLedger::new(2, true, false));
        assert!(!jit.call_requests_compile(1001));
        assert!(jit.call_requests_compile(2000));
        let (sender, receiver) = std::sync::mpsc::channel();
        *jit.queue.lock().unwrap() = Some(sender);
        for _ in 0..16 {
            assert!(jit.request_compile(1));
        }
        assert_eq!(receiver.try_iter().count(), 1);
        let ledger = jit.work.as_ref().unwrap();
        ledger.budget.lock().unwrap().charge(10, 1000);
        assert!(!ledger.admit(1, 5));
        assert!(jit.request_compile(1));
        assert_eq!(receiver.try_iter().count(), 1);
        ledger.release(1);
        drop(receiver);
        assert!(!jit.request_compile(1));
        assert!(!jit.request_compile(1));
    }

    #[test]
    fn sampling_is_sparse_and_bounded_and_excludes_paused_work() {
        let ledger = WorkLedger::new(1, false, false);
        let mut sample = ledger.sample(0).unwrap();
        sample.pause();
        sample.elapsed_ns = 123;
        sample.statements(SAMPLE_WORK as usize);
        sample.resume();
        sample.statements(5000);
        drop(sample);
        assert_eq!(ledger.total_ns.load(Ordering::Relaxed), 123);
        assert_eq!(ledger.instances[0].samples.load(Ordering::Relaxed), 1);
        for _ in 1..SAMPLE_PERIOD {
            assert!(ledger.sample(0).is_none());
        }
        let mut sample = ledger.sample(0).unwrap();
        sample.pause();
        sample.elapsed_ns = 10;
        drop(sample);
        assert_eq!(ledger.total_ns.load(Ordering::Relaxed), 763);
    }

    #[test]
    fn cost_estimate_saturates_instead_of_wrapping() {
        let budget = Budget {
            spent_ns: u64::MAX,
            units: 1,
            ..Budget::default()
        };
        assert_eq!(budget.estimate(u64::MAX), u64::MAX);
    }
}
