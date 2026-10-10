//! Shared per-function JIT state: the PLT slot table plus the call counters.
//!
//! A function's state moves through Bytecode -> (counter crosses the threshold, compile)
//! -> Machine: a slot value of 0 means "interpret", non-zero is the packed entry machine
//! address called directly by i2c. Publication is a single atomic pointer swap: the
//! compiler worker stores with Release, `dispatch::call_guest` loads with Acquire.
//!
//! This module does not depend on Cranelift: the guest call path only reads the atomic
//! slots, and machine-code range registration plus perf-map locking and file writes happen
//! only on the compiler/tooling cold path — which is why that half is [`perfmap`]. The TSan
//! harness compiles `src/vm` from these same sources through `#[path]`, so the tables are
//! sized to the merged FuncId space and base functions tier up like any other.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

pub use perfmap::*;

mod perfmap;
#[cfg(test)]
mod tests;

/// Which code domain a compiled body belongs to.
///
/// The domain is chosen once, at the outermost guest activation entry, and stays
/// fixed for that whole call chain. It decides two things: which ISA the module
/// was built with (the trace domain pins a register), and which publish slots
/// the body lands in. Plain code has no pinned register and no recorder state,
/// and costs nothing for a session that is not running.
///
/// Defined here rather than beside the compiler because the guest dispatch path
/// reads the domain even in builds without the code generator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CodeDomain {
    Plain,
    Trace,
}

/// Which optimization level a function is compiled at.
///
/// The level is a jit-key dimension, so one fragment may have an entry per tier and dispatch publishes
/// whichever it has; the policy that chooses between them is the heat ledger's (§2.6 of the JIT
/// design), and nothing about a program's semantics depends on the choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Tier {
    /// The cheapest code to produce: what a function gets before anything says it is hot.
    Baseline,
    /// The optimized level: what the heat order and the upgrade threshold ask for.
    Optimized,
}

impl Tier {
    /// The cranelift `opt_level` value this tier compiles with.
    pub(crate) fn opt_level(self) -> &'static str {
        match self {
            Tier::Baseline => "speed_and_size",
            Tier::Optimized => "speed",
        }
    }
}

/// One compilation request: which function, and the tier the ledger asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub func: u32,
    pub tier: Tier,
}

/// The function heat order a session starts with, and where it leaves its own.
///
/// A program's hot functions are not a mystery: the order its last run asked the compiler for them in
/// is close to the order the next run will want them, so one run learns it and the next pre-links it —
/// warmup becomes one link wave instead of a compile wave. Nothing here is semantics: an order that is
/// absent, stale or partly stale costs pre-linking and nothing else, because every entry is still
/// checked against the fragment and key it was written for.
#[derive(Clone, Debug, Default)]
pub struct Heat {
    /// Where this session writes the order it observed; `None` writes nothing.
    pub path: Option<std::path::PathBuf>,
    /// The ids a previous run asked for, hottest first.
    pub order: Vec<u32>,
}

impl Heat {
    /// Read the order a previous run left at `path`.
    pub fn read(path: std::path::PathBuf) -> Heat {
        let order = std::fs::read_to_string(&path)
            .map(|text| {
                text.split_ascii_whitespace()
                    .filter_map(|value| value.parse::<u32>().ok())
                    .collect()
            })
            .unwrap_or_default();
        Heat {
            path: Some(path),
            order,
        }
    }

    /// Record this session's observed order, hottest first. Best effort: a heat file that cannot be
    /// written only costs the next run its prediction.
    pub fn write(&self, observed: &[u32]) {
        let Some(path) = &self.path else {
            return;
        };
        if observed.is_empty() {
            return;
        }
        let mut seen = std::collections::BTreeSet::new();
        let body = observed
            .iter()
            .filter(|id| seen.insert(**id))
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::store::publish_bytes(path, body.as_bytes());
    }
}

/// Failure sentinel for `MIRVM_JIT_SYNC` verification mode: when compilation of an
/// eligible function fails, the worker stores this into the slots and the sync waiter
/// aborts loudly on it. It lies outside the normal value range (0 = not compiled, keep
/// interpreting), and non-strict mode never writes it.
pub const FAIL_SENTINEL: u64 = u64::MAX;

/// Interpreted loop iterations of one function before the compile policy asks for it. A compiled body
/// is worth some milliseconds of compilation, and interpreting a loop body pays that back after tens
/// of thousands of iterations; a body that never reaches this stays interpreted.
pub const ITERATION_REQUEST: u32 = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitCodeRange {
    pub start: u64,
    pub end: u64,
    pub func: u32,
}

/// One code domain's publish slots. Plain and trace code must never share slots: a trace
/// body assumes recorder state the plain domain does not have, so a slot written by one
/// domain must be invisible to the other.
///
/// TODO: multiple guest threads may access this structure; optimize access, e.g. cache
/// locality, or make it volatile.
pub(crate) struct DomainSlots {
    /// interp i2c face: FuncId -> packed entry address (0 = not compiled).
    pub(crate) slots: Vec<AtomicU64>,
    /// compiled cc->cc face: FuncId -> fast entry / c2i trampoline address.
    pub(crate) slots_fast: Vec<AtomicU64>,
}

impl DomainSlots {
    fn new(fn_count: usize) -> Self {
        Self {
            slots: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            slots_fast: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
        }
    }
}

/// The slot set dispatch serves for a domain. Exactly one place decides this, so
/// plain and trace runs cannot accidentally read each other's entries. It borrows
/// rather than copies, so the plain domain keeps using the `slots`/`slots_fast`
/// fields instead of gaining duplicates.
#[derive(Clone, Copy)]
pub(crate) struct DomainSlotSet<'a> {
    pub(crate) slots: &'a [AtomicU64],
    pub(crate) slots_fast: &'a [AtomicU64],
}

pub struct JitState {
    /// PLT slots for the interpreter's i2c face: FuncId -> packed entry machine address
    /// (0 = not compiled, interpret instead).
    pub slots: Vec<AtomicU64>,
    /// PLT slots for the compiled-code cc->cc face: FuncId -> fast entry / c2i trampoline
    /// address (0 = none yet). Only compiled call sites read it (load plus
    /// `call_indirect`); the interpreter does not consume it.
    pub slots_fast: Vec<AtomicU64>,
    /// The trace domain's own slot set. Kept beside the plain one so dispatch has
    /// exactly one selection point; a trace activation does not consult the plain
    /// slots and vice versa.
    pub(crate) trace: DomainSlots,
    /// The trace domain's boundary entry: pins the calling
    /// thread's recorder in `r15`, calls one packed trace body, and restores the
    /// register on both the normal and the unwinding path. Zero means the trace
    /// domain has no legal way in, so no trace body is compiled -- a pinned
    /// register is not an optimization a body may run without.
    pub(crate) trace_enter: AtomicU64,
    /// Call counters (Relaxed; a lost increment under a race is harmless -- it only shifts
    /// the trigger moment, not program semantics).
    pub counters: Vec<AtomicU32>,
    /// False under `--jit off` / `MIRVM_JIT=off`: pure interpretation, not even counters
    /// are bumped (the differential-comparison baseline).
    pub enabled: bool,
    /// Requests baseline compilation once a function's counter crosses this.
    pub threshold: u32,
    /// The functions a previous run learned are hot, by id. This is the adaptive policy's whole
    /// input: the tier a function is compiled at is `tier_for`'s answer, and the order it comes from
    /// is learned again every run — which is the hysteresis, since one run of evidence is not enough
    /// to rewrite it. The D16 measurement ledger owns any refinement (§2.6 of the JIT design).
    hot: std::sync::RwLock<std::collections::HashSet<u32>>,
    /// Interpreted loop iterations, by function. The call counter answers "how often is this called",
    /// which is the wrong question for a body whose work is a loop: one call can run a loop for the
    /// whole program, and a body entered twice with a long loop pays for interpretation once per
    /// iteration. The interpreter reports its back edges here in batches, and crossing the request
    /// point asks for the function to be compiled.
    iterations: Vec<AtomicU32>,
    /// `MIRVM_JIT_SYNC=1` verification mode: after queueing, wait for publication or the
    /// failure sentinel. With threshold 1 this turns "first call requests compilation"
    /// into "first call compiles and publishes synchronously", and a compilation failure
    /// of an eligible function becomes a loud abort instead of silently staying
    /// interpreted, so a gate can observe it.
    pub sync: bool,
    /// Compilation-request channel (`jit_compile::start` fills it; always None without the
    /// cranelift feature).
    pub queue: std::sync::Mutex<Option<std::sync::mpsc::Sender<Request>>>,
    /// The worker must be joined before process exit runs allocator cleanup; dropping the
    /// handle instead lets Cranelift race libc/Rust teardown and corrupt the heap in ways
    /// that drift across workloads.
    pub worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Once set during teardown, the worker drops unconsumed compile requests after it
    /// finishes the current function.
    pub stopping: AtomicBool,
    /// Machine-code ranges of published guest fast bodies. guarded/packed/c2i wrappers are
    /// not registered, so a backtrace shows exactly one function frame per guest call.
    pub guest_code: RwLock<Vec<JitCodeRange>>,
    /// All Cranelift code ranges of this engine, for profiling and integrity checks.
    symbol_ranges: RwLock<Vec<JitSymbolRange>>,
    /// What each compiled fast body bakes, and where each absolute landed: the table a stored entry
    /// replays, and what the completeness check compares the recorded sites against.
    sites: RwLock<Vec<JitSites>>,
}

/// One compiled body's absolutes ([`crate::vm::jit::reloc`]).
pub struct JitSites {
    pub domain: CodeDomain,
    pub func: u32,
    pub placed: Vec<super::reloc::Placed>,
}

impl JitState {
    pub fn new(fn_count: usize) -> Self {
        let enabled = crate::options::jit();
        let threshold = crate::options::jit_threshold();
        JitState {
            slots: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            slots_fast: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            trace: DomainSlots::new(fn_count),
            trace_enter: AtomicU64::new(0),
            counters: (0..fn_count).map(|_| AtomicU32::new(0)).collect(),
            enabled,
            threshold,
            hot: std::sync::RwLock::new(std::collections::HashSet::new()),
            iterations: (0..fn_count).map(|_| AtomicU32::new(0)).collect(),
            sync: crate::options::jit_sync(),
            queue: std::sync::Mutex::new(None),
            worker: std::sync::Mutex::new(None),
            stopping: AtomicBool::new(false),
            guest_code: RwLock::new(Vec::new()),
            symbol_ranges: RwLock::new(Vec::new()),
            sites: RwLock::new(Vec::new()),
        }
    }

    /// Record the order a previous run asked for functions in: the hot set this session's tier
    /// policy reads.
    pub(crate) fn set_hot(&self, order: &[u32]) {
        let Ok(mut hot) = self.hot.write() else {
            return;
        };
        hot.clear();
        hot.extend(order.iter().copied());
    }

    /// Record interpreted loop iterations for one function and ask for it to be compiled when the
    /// count first crosses [`ITERATION_REQUEST`]. Batching is the caller's: one atomic per batch keeps
    /// this off the interpreter's hot path. A request already answered is a no-op at the worker, so
    /// the crossing is the only filter this needs.
    pub fn note_iterations(&self, func: u32, count: u32) {
        let Some(slot) = self.iterations.get(func as usize) else {
            return;
        };
        let prev = slot.fetch_add(count, Ordering::Relaxed);
        if prev < ITERATION_REQUEST && prev.saturating_add(count) >= ITERATION_REQUEST {
            self.request_compile(func);
        }
    }

    /// Ask the compile service for one function, at the tier the heat ledger names. False when no
    /// service is listening: the JIT is off, the worker never started, died, or is a parent's that a
    /// fork did not bring across.
    pub fn request_compile(&self, func: u32) -> bool {
        let Some(q) = self.queue.lock().unwrap().as_ref().cloned() else {
            return false;
        };
        q.send(Request {
            func,
            tier: self.tier_for(func),
        })
        .is_ok()
    }

    /// Which tier a function's request should ask for: the optimized one exactly when a previous run
    /// found it hot, and the cheap one otherwise. A request is raised once per function while it is
    /// interpreted, so this is the only place the policy is consulted.
    pub fn tier_for(&self, func: u32) -> Tier {
        match self.hot.read() {
            Ok(hot) if hot.contains(&func) => Tier::Optimized,
            _ => Tier::Baseline,
        }
    }

    /// Record one compiled body's absolutes. A later compilation replaces an earlier one of the same
    /// function and domain: a body is compiled once per domain, and a recompile must not leave two
    /// tables for one entry.
    pub(crate) fn record_sites(
        &self,
        domain: CodeDomain,
        func: u32,
        placed: Vec<super::reloc::Placed>,
    ) {
        let Ok(mut sites) = self.sites.write() else {
            return;
        };
        sites.retain(|entry| !(entry.domain == domain && entry.func == func));
        sites.push(JitSites {
            domain,
            func,
            placed,
        });
    }

    /// How many sites one compiled body's table holds.
    pub(crate) fn recorded_sites(&self, domain: CodeDomain, func: u32) -> Option<usize> {
        let sites = self.sites.read().ok()?;
        sites
            .iter()
            .find(|entry| entry.domain == domain && entry.func == func)
            .map(|entry| entry.placed.len())
    }

    /// Publish slots for a code domain. The plain arm returns the `slots`/`slots_fast`
    /// fields, so the plain path keeps its exact shape and cost.
    pub(crate) fn slots_for(&self, domain: CodeDomain) -> DomainSlotSet<'_> {
        match domain {
            CodeDomain::Plain => DomainSlotSet {
                slots: &self.slots,
                slots_fast: &self.slots_fast,
            },
            CodeDomain::Trace => DomainSlotSet {
                slots: &self.trace.slots,
                slots_fast: &self.trace.slots_fast,
            },
        }
    }

    fn register_symbol_range_with(&self, range: JitSymbolRange, registry: &Mutex<PerfMapRegistry>) {
        debug_assert!(range.size != 0, "Cranelift produced an empty code range");
        if range.role == JitSymbolRole::FastBody {
            self.guest_code
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .push(JitCodeRange {
                    start: range.start,
                    end: range.start.saturating_add(range.size),
                    func: range.func_id,
                });
        }
        self.symbol_ranges
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(range.clone());
        with_registry(registry, |registry| registry.register(range));
    }

    fn publish_compiled_entries_with(
        &self,
        domain: CodeDomain,
        func: u32,
        guarded_entry: u64,
        packed_entry: u64,
        ranges: Vec<JitSymbolRange>,
        registry: &Mutex<PerfMapRegistry>,
    ) {
        debug_assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::FastBody)
        );
        debug_assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::Guarded)
        );
        debug_assert!(
            ranges
                .iter()
                .any(|range| range.role == JitSymbolRole::Packed)
        );
        for range in ranges {
            self.register_symbol_range_with(range, registry);
        }
        self.publish_entries_to(self.slots_for(domain), func, guarded_entry, packed_entry);
    }

    fn publish_entries_to(
        &self,
        slots: DomainSlotSet<'_>,
        func: u32,
        guarded_entry: u64,
        packed_entry: u64,
    ) {
        slots.slots_fast[func as usize].store(guarded_entry, Ordering::Release);
        slots.slots[func as usize].store(packed_entry, Ordering::Release);
    }

    /// Publish a compiled body into its domain's slots, registering ranges with
    /// the process registry. This is the production entry point; the `_with`
    /// cores take an injected registry so tests can observe registration.
    pub(crate) fn publish_compiled_entries_for(
        &self,
        domain: CodeDomain,
        func: u32,
        guarded_entry: u64,
        packed_entry: u64,
        ranges: Vec<JitSymbolRange>,
    ) {
        self.publish_compiled_entries_with(
            domain,
            func,
            guarded_entry,
            packed_entry,
            ranges,
            perf_registry(),
        );
    }

    fn publish_c2i_entry_with(
        &self,
        domain: CodeDomain,
        func: u32,
        entry: u64,
        ranges: Vec<JitSymbolRange>,
        registry: &Mutex<PerfMapRegistry>,
    ) {
        debug_assert!(ranges.iter().any(|range| {
            range.func_id == func && range.role == JitSymbolRole::C2i && range.start == entry
        }));
        for range in ranges {
            self.register_symbol_range_with(range, registry);
        }
        self.slots_for(domain).slots_fast[func as usize].store(entry, Ordering::Release);
    }

    /// c2i trampoline into its domain's slots (production entry point).
    pub(crate) fn publish_c2i_entry_for(
        &self,
        domain: CodeDomain,
        func: u32,
        entry: u64,
        ranges: Vec<JitSymbolRange>,
    ) {
        self.publish_c2i_entry_with(domain, func, entry, ranges, perf_registry());
    }

    // TODO: export this per-engine view alongside the process-level snapshot.
    #[allow(dead_code)]
    pub fn symbol_ranges(&self) -> Vec<JitSymbolRange> {
        self.symbol_ranges
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn guest_func_at(&self, ip: u64) -> Option<u32> {
        self.guest_code
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|range| range.start <= ip && ip < range.end)
            .map(|range| range.func)
    }
}
