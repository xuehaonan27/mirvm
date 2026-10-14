//! Engine-local execution credit and compile-cost calibration for experimental tier admission.
//! Counters describe completed own-body work, never predicted remaining lifetime or callee time.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use super::Regions;
use crate::vm::ctx::{Ctx, Shared};
use crate::vm::jit::FAIL_SENTINEL;

const PERIOD: u64 = 64;
const LIMIT: u64 = 4096;

fn add(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Relaxed, Relaxed, |old| Some(old.saturating_add(value)));
}

#[derive(Default)]
struct Work {
    entries: AtomicU64,
    samples: AtomicU64,
    ns: AtomicU64,
    units: AtomicU64,
}

#[derive(Default)]
struct Instance {
    interpreted: Work,
    compiled: Work,
    pending: AtomicBool,
    iterations: AtomicU64,
}

struct Group {
    first: AtomicU32,
    diverse: AtomicBool,
    ns: AtomicU64,
    pending: AtomicBool,
}

impl Default for Group {
    fn default() -> Self {
        Self {
            first: AtomicU32::new(u32::MAX),
            diverse: AtomicBool::new(false),
            ns: AtomicU64::new(0),
            pending: AtomicBool::new(false),
        }
    }
}

impl Group {
    fn observe(&self, func: u32) {
        if self.first.load(Relaxed) == func {
            return;
        }
        let first = match self
            .first
            .compare_exchange(u32::MAX, func, Relaxed, Relaxed)
        {
            Ok(_) => func,
            Err(first) => first,
        };
        if first != func {
            self.diverse.store(true, Relaxed);
        }
    }
}

#[derive(Default)]
struct Cost {
    ns: AtomicU64,
    units: AtomicU64,
    spent: AtomicU64,
    credit: AtomicU64,
    deferred: AtomicU64,
    transitions: AtomicU64,
}

impl Cost {
    fn estimate(&self, units: u64) -> u64 {
        (u128::from(self.ns.load(Relaxed)) * u128::from(units)
            / u128::from(self.units.load(Relaxed).max(1)))
        .min(u128::from(u64::MAX)) as u64
    }

    fn admits(&self, own: u64, units: u64) -> bool {
        let estimate = self.estimate(units);
        own >= estimate
            && self
                .credit
                .load(Relaxed)
                .saturating_sub(self.spent.load(Relaxed))
                >= estimate
    }
}

pub(super) struct Policy {
    pub(super) enforce: bool,
    indices: Vec<usize>,
    instances: Vec<Instance>,
    groups: Vec<Group>,
    cost: Cost,
}

pub(crate) struct Ticket {
    start: Instant,
    units: u64,
    tracked: bool,
}

impl Policy {
    pub(super) fn new(regions: &Regions) -> Self {
        let mut instances = Vec::new();
        let indices = regions
            .targets
            .iter()
            .map(|target| {
                if target.is_some() {
                    instances.push(Instance::default());
                    instances.len() - 1
                } else {
                    usize::MAX
                }
            })
            .collect();
        Self {
            enforce: crate::options::shared_region_policy() && !crate::options::jit_sync(),
            indices,
            instances,
            groups: (0..regions.representatives.len())
                .map(|_| Group::default())
                .collect(),
            cost: Cost::default(),
        }
    }

    fn instance(&self, func: u32) -> &Instance {
        &self.instances[self.indices[func as usize]]
    }

    pub(super) fn nominate(&self, ctx: *mut Ctx, func: u32, target: u32, entry: u64) {
        let jit = unsafe { &(*ctx).shared.jit };
        let group = &self.groups[target as usize - self.indices.len()];
        let count = jit.counters[func as usize].fetch_add(1, Relaxed) + 1;
        let total = jit.counters[target as usize].fetch_add(1, Relaxed) + 1;
        let retry = |n: u32| {
            n >= jit.threshold
                && (n == jit.threshold || n.is_multiple_of(jit.threshold.max(PERIOD as u32)))
        };
        let request = if entry != 0 {
            retry(count).then_some((func, &self.instance(func).pending))
        } else if retry(count)
            && (!group.diverse.load(Relaxed) || u64::from(count) * 2 >= u64::from(total))
        {
            // A queued shared transition must finish before deciding to pay for a concrete one.
            (!group.pending.load(Relaxed)).then_some((func, &self.instance(func).pending))
        } else if group.diverse.load(Relaxed) && retry(total) {
            Some((target, &group.pending))
        } else {
            None
        };
        if let Some((id, pending)) = request
            && !pending.swap(true, Relaxed)
            && !jit.request_compile(id)
        {
            pending.store(false, Relaxed);
        }
    }

    pub(super) fn iterations(&self, ctx: *mut Ctx, func: u32, target: u32, count: u32) {
        let instance = self.instance(func);
        let before = instance.iterations.fetch_add(u64::from(count), Relaxed);
        let threshold = u64::from(crate::vm::jit::ITERATION_REQUEST);
        if before / threshold == before.saturating_add(u64::from(count)) / threshold {
            return;
        }
        let jit = unsafe { &(*ctx).shared.jit };
        let group = &self.groups[target as usize - self.indices.len()];
        let (id, pending) = if group.diverse.load(Relaxed) {
            (target, &group.pending)
        } else {
            (func, &instance.pending)
        };
        if !pending.swap(true, Relaxed) && !jit.request_compile(id) {
            pending.store(false, Relaxed);
        }
    }

    pub(super) fn sample(&self, func: u32, group: usize, compiled: bool) -> Option<Sample<'_>> {
        self.groups[group].observe(func);
        let instance = self.instance(func);
        let work = if compiled {
            &instance.compiled
        } else {
            &instance.interpreted
        };
        let entry = work.entries.fetch_add(1, Relaxed);
        if !entry.is_multiple_of(PERIOD) {
            return None;
        }
        Some(Sample {
            work,
            group: (!compiled).then_some(&self.groups[group]),
            cost: &self.cost,
            weight: if entry == 0 { 1 } else { PERIOD },
            start: Cell::new(Some(Instant::now())),
            elapsed: Cell::new(0),
            units: Cell::new(0),
            finished: Cell::new(false),
        })
    }

    pub(super) fn start(
        &self,
        regions: &Regions,
        shared: &Shared,
        func: u32,
    ) -> Result<Option<Ticket>, ()> {
        let entry = shared.jit.slots[func as usize].load(Relaxed);
        if entry != 0 {
            return Ok(None);
        }
        let start = Instant::now();
        let group = (func as usize)
            .checked_sub(self.indices.len())
            .filter(|&group| group < self.groups.len());
        let tracked = group.is_some() || regions.contains(func);
        let source = group.map_or(func, |group| regions.representatives[group]);
        let body = shared.module.funcs.execution(source as usize).unwrap();
        let units = body
            .code()
            .iter()
            .map(|block| block.stmts.len() as u64 + 1)
            .sum::<u64>()
            .max(1);
        if self.enforce && tracked {
            let (own, pending) = if let Some(group) = group {
                let group = &self.groups[group];
                (group.ns.load(Relaxed), &group.pending)
            } else {
                let instance = self.instance(func);
                let target = regions.targets[func as usize].unwrap();
                let compiled = shared.jit.slots[target as usize].load(Relaxed);
                let work = if compiled != 0 && compiled != FAIL_SENTINEL {
                    &instance.compiled
                } else {
                    &instance.interpreted
                };
                (work.ns.load(Relaxed), &instance.pending)
            };
            if !self.cost.admits(own, units) {
                add(&self.cost.deferred, 1);
                pending.store(false, Relaxed);
                if let Some(work) = &shared.jit.work {
                    work.release(func);
                }
                return Err(());
            }
        }
        Ok(Some(Ticket {
            start,
            units,
            tracked,
        }))
    }

    pub(super) fn complete(&self, shared: &Shared, func: u32, ticket: Ticket) {
        let ns = ticket.start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let entry = shared.jit.slots[func as usize].load(Relaxed);
        if entry != 0 && entry != FAIL_SENTINEL {
            add(&self.cost.ns, ns);
            add(&self.cost.units, ticket.units);
        }
        if ticket.tracked {
            add(&self.cost.spent, ns);
            add(&self.cost.transitions, 1);
        }
    }

    pub(super) fn report(&self, regions: &Regions, shared: &Shared) {
        let mut interpreted = 0u64;
        let mut compiled = 0u64;
        let mut samples = 0u64;
        for (func, &index) in self.indices.iter().enumerate() {
            if index == usize::MAX {
                continue;
            }
            let instance = &self.instances[index];
            let i = instance.interpreted.ns.load(Relaxed);
            let c = instance.compiled.ns.load(Relaxed);
            interpreted = interpreted.saturating_add(i);
            compiled = compiled.saturating_add(c);
            samples += instance.interpreted.samples.load(Relaxed)
                + instance.compiled.samples.load(Relaxed);
            if i != 0 || c != 0 {
                crate::diag::instrument(format_args!(
                    "mirvm-shared-work-row: func={func} group={} interpreted_entries={} interpreted_ns={i} interpreted_units={} compiled_entries={} compiled_ns={c} compiled_calls={} name={}",
                    regions.targets[func].unwrap(),
                    instance.interpreted.entries.load(Relaxed),
                    instance.interpreted.units.load(Relaxed),
                    instance.compiled.entries.load(Relaxed),
                    instance.compiled.units.load(Relaxed),
                    shared.module.funcs.header(func).unwrap().name(),
                ));
            }
        }
        crate::diag::instrument(format_args!(
            "mirvm-shared-policy: enforce={} diverse={} samples={samples} interpreted_ns={interpreted} compiled_ns={compiled} worker_ns={} credit_ns={} deferred={} transitions={} calibration_ns={} calibration_units={}",
            self.enforce,
            self.groups
                .iter()
                .filter(|group| group.diverse.load(Relaxed))
                .count(),
            self.cost.spent.load(Relaxed),
            self.cost.credit.load(Relaxed),
            self.cost.deferred.load(Relaxed),
            self.cost.transitions.load(Relaxed),
            self.cost.ns.load(Relaxed),
            self.cost.units.load(Relaxed),
        ));
    }
}

pub(crate) struct Sample<'a> {
    work: &'a Work,
    group: Option<&'a Group>,
    cost: &'a Cost,
    weight: u64,
    start: Cell<Option<Instant>>,
    elapsed: Cell<u64>,
    units: Cell<u64>,
    finished: Cell<bool>,
}

impl Sample<'_> {
    pub(crate) fn pause(&self) {
        if let Some(start) = self.start.take() {
            self.elapsed.set(
                self.elapsed
                    .get()
                    .saturating_add(start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64),
            );
        }
    }

    pub(crate) fn resume(&self) {
        if !self.finished.get() {
            self.start.set(Some(Instant::now()));
        }
    }

    pub(crate) fn statements(&self, count: usize) {
        self.units
            .set(self.units.get().saturating_add(count as u64 + 1));
        if self.units.get() >= LIMIT {
            self.finish();
        }
    }

    fn finish(&self) {
        if self.finished.replace(true) {
            return;
        }
        self.pause();
        let credit = self.elapsed.get().saturating_mul(self.weight);
        add(&self.work.ns, credit);
        add(&self.work.samples, 1);
        add(&self.work.units, self.units.get());
        add(&self.cost.credit, credit);
        if let Some(group) = self.group {
            add(&group.ns, credit);
        }
    }
}

impl Drop for Sample<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_entries_of_one_instance_do_not_fake_diversity() {
        let group = Group::default();
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| group.observe(7));
            }
        });
        assert!(!group.diverse.load(Relaxed));
        group.observe(8);
        assert!(group.diverse.load(Relaxed));
    }

    #[test]
    fn transition_debt_and_instance_credit_both_gate_compilation() {
        let cost = Cost::default();
        assert!(cost.admits(0, 10));
        cost.ns.store(1000, Relaxed);
        cost.units.store(10, Relaxed);
        cost.spent.store(1200, Relaxed);
        cost.credit.store(1500, Relaxed);
        assert!(!cost.admits(1000, 5));
        cost.credit.store(2000, Relaxed);
        assert!(!cost.admits(499, 5));
        assert!(cost.admits(500, 5));
        assert_eq!(cost.estimate(u64::MAX), u64::MAX);
    }

    #[test]
    fn paused_callee_and_unwind_do_not_earn_own_work_credit() {
        let work = Work::default();
        let cost = Cost::default();
        let sample = Sample {
            work: &work,
            group: None,
            cost: &cost,
            weight: 64,
            start: Cell::new(None),
            elapsed: Cell::new(123),
            units: Cell::new(0),
            finished: Cell::new(false),
        };
        sample.statements(LIMIT as usize);
        sample.resume();
        drop(sample);
        assert_eq!(work.ns.load(Relaxed), 123 * 64);
        assert_eq!(cost.credit.load(Relaxed), 123 * 64);
        assert_eq!(work.samples.load(Relaxed), 1);
    }
}
