//! Shared per-function JIT state: the PLT slot table plus the call counters.
//!
//! A function's state moves through Bytecode -> (counter crosses the threshold, compile)
//! -> Machine: a slot value of 0 means "interpret", non-zero is the packed entry machine
//! address called directly by i2c. Publication is a single atomic pointer swap: the
//! compiler worker stores with Release, `interp::call_guest` loads with Acquire.
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CodeDomain {
    Plain,
    Trace,
}

/// Failure sentinel for `MIRVM_JIT_SYNC` verification mode: when compilation of an
/// eligible function fails, the worker stores this into the slots and the sync waiter
/// aborts loudly on it. It lies outside the normal value range (0 = not compiled, keep
/// interpreting), and non-strict mode never writes it.
pub const FAIL_SENTINEL: u64 = u64::MAX;

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
    /// Requests compilation once a function's counter crosses this.
    pub threshold: u32,
    /// `MIRVM_JIT_SYNC=1` verification mode: after queueing, wait for publication or the
    /// failure sentinel. With threshold 1 this turns "first call requests compilation"
    /// into "first call compiles and publishes synchronously", and a compilation failure
    /// of an eligible function becomes a loud abort instead of silently staying
    /// interpreted, so a gate can observe it.
    pub sync: bool,
    /// Compilation-request channel (`jit_compile::start` fills it; always None without the
    /// cranelift feature).
    pub queue: std::sync::Mutex<Option<std::sync::mpsc::Sender<u32>>>,
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
}

impl JitState {
    pub fn new(fn_count: usize) -> Self {
        let enabled = crate::options::get().jit();
        let threshold = crate::options::get().jit_threshold;
        JitState {
            slots: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            slots_fast: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            trace: DomainSlots::new(fn_count),
            trace_enter: AtomicU64::new(0),
            counters: (0..fn_count).map(|_| AtomicU32::new(0)).collect(),
            enabled,
            threshold,
            sync: crate::options::get().jit_sync,
            queue: std::sync::Mutex::new(None),
            worker: std::sync::Mutex::new(None),
            stopping: AtomicBool::new(false),
            guest_code: RwLock::new(Vec::new()),
            symbol_ranges: RwLock::new(Vec::new()),
        }
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
