//! Shared per-function JIT state: the PLT slot table plus the call counters.
//!
//! A function's state moves through Bytecode -> (counter crosses the threshold, compile)
//! -> Machine: a slot value of 0 means "interpret", non-zero is the packed entry machine
//! address called directly by i2c. Publication is a single atomic pointer swap: the
//! compiler worker stores with Release, `interp::call_guest` loads with Acquire.
//!
//! This module does not depend on Cranelift: the guest call path only reads the atomic
//! slots, and machine-code range registration plus perf-map locking and file writes happen
//! only on the compiler/tooling cold path. The TSan harness compiles `src/vm` from these
//! same sources through `#[path]`, so the tables are sized to the merged FuncId space and
//! base functions tier up like any other.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

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

/// A machine-code range visible to a profiler. Wider than `guest_code`: wrappers must show
/// up in perf output, but they must not pose as an extra MIRVM guest backtrace frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitSymbolRole {
    FastBody,
    Guarded,
    Packed,
    C2i,
}

impl JitSymbolRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::FastBody => "fast-body",
            Self::Guarded => "guarded",
            Self::Packed => "packed",
            Self::C2i => "c2i",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitSymbolRange {
    pub engine_id: u64,
    pub func_id: u32,
    pub role: JitSymbolRole,
    pub start: u64,
    pub size: u64,
    pub display_name: String,
}

impl JitSymbolRange {
    pub fn new(
        engine_id: u64,
        func_id: u32,
        role: JitSymbolRole,
        start: u64,
        size: u64,
        guest_name: &str,
    ) -> Self {
        // One perf-map line per symbol; escaping keeps control characters and non-ASCII
        // names unambiguous.
        let guest_name: String = guest_name.chars().flat_map(char::escape_default).collect();
        let display_name = format!(
            "mirvm::engine-{engine_id}::func-{func_id}::{}::{guest_name}",
            role.as_str()
        );
        Self {
            engine_id,
            func_id,
            role,
            start,
            size,
            display_name,
        }
    }

    fn perf_map_line(&self) -> String {
        format!("{:x} {:x} {}\n", self.start, self.size, self.display_name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerfMapHealth {
    Inactive,
    Active,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerfMapStatus {
    pub health: PerfMapHealth,
    pub path: Option<PathBuf>,
    pub registered_ranges: usize,
    pub written_ranges: usize,
    pub error: Option<String>,
}

pub(crate) struct PerfMapRegistry {
    ranges: Vec<JitSymbolRange>,
    sink: Option<Box<dyn Write + Send>>,
    health: PerfMapHealth,
    path: Option<PathBuf>,
    written_ranges: usize,
    error: Option<String>,
    control: Option<std::sync::Arc<ControlOperation>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Install,
    Stop,
}

struct ControlOperation {
    kind: ControlKind,
    result: Mutex<Option<PerfMapStatus>>,
    completed: std::sync::Condvar,
    #[cfg(test)]
    waiters: std::sync::atomic::AtomicUsize,
}

impl ControlOperation {
    fn new(kind: ControlKind) -> Self {
        Self {
            kind,
            result: Mutex::new(None),
            completed: std::sync::Condvar::new(),
            #[cfg(test)]
            waiters: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn wait(&self) -> PerfMapStatus {
        #[cfg(test)]
        self.waiters.fetch_add(1, Ordering::SeqCst);
        let mut result = self.result.lock().unwrap_or_else(|e| e.into_inner());
        while result.is_none() {
            result = self
                .completed
                .wait(result)
                .unwrap_or_else(|e| e.into_inner());
        }
        let status = result.as_ref().expect("control result is complete").clone();
        #[cfg(test)]
        self.waiters.fetch_sub(1, Ordering::SeqCst);
        status
    }

    fn finish(&self, status: PerfMapStatus) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(status);
        self.completed.notify_all();
    }
}

impl Default for PerfMapRegistry {
    fn default() -> Self {
        Self {
            ranges: Vec::new(),
            sink: None,
            health: PerfMapHealth::Inactive,
            path: None,
            written_ranges: 0,
            error: None,
            control: None,
        }
    }
}

impl PerfMapRegistry {
    fn status(&self) -> PerfMapStatus {
        PerfMapStatus {
            health: self.health,
            path: self.path.clone(),
            registered_ranges: self.ranges.len(),
            written_ranges: self.written_ranges,
            error: self.error.clone(),
        }
    }

    fn mark_incomplete(&mut self, error: &io::Error) {
        self.sink = None;
        self.health = PerfMapHealth::Incomplete;
        self.error = Some(error.to_string());
    }

    fn register(&mut self, range: JitSymbolRange) {
        self.ranges.push(range);
    }
}

fn perf_registry() -> &'static Mutex<PerfMapRegistry> {
    static REGISTRY: OnceLock<Mutex<PerfMapRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(PerfMapRegistry::default()))
}

fn with_perf_registry<T>(f: impl FnOnce(&mut PerfMapRegistry) -> T) -> T {
    with_registry(perf_registry(), f)
}

fn with_registry<T>(
    registry: &Mutex<PerfMapRegistry>,
    f: impl FnOnce(&mut PerfMapRegistry) -> T,
) -> T {
    let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut registry)
}

fn install_registry(registry: &Mutex<PerfMapRegistry>, path: &Path) -> io::Result<PerfMapStatus> {
    enum Decision {
        Wait(std::sync::Arc<ControlOperation>),
        Start(std::sync::Arc<ControlOperation>),
        AlreadyActive,
    }

    let operation = loop {
        let decision = with_registry(registry, |registry| {
            if let Some(operation) = registry.control.clone() {
                Decision::Wait(operation)
            } else if registry.health == PerfMapHealth::Active {
                Decision::AlreadyActive
            } else {
                let operation = std::sync::Arc::new(ControlOperation::new(ControlKind::Install));
                registry.control = Some(std::sync::Arc::clone(&operation));
                Decision::Start(operation)
            }
        });
        match decision {
            Decision::Wait(operation) => {
                operation.wait();
            }
            Decision::Start(operation) => break operation,
            Decision::AlreadyActive => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a perf-map session is already active",
                ));
            }
        }
    };

    // Filesystem work must not hold the range lock, or buffered output would still stall
    // the JIT worker's in-memory registration. A file belongs to one profile only: an
    // existing path must fail to open instead of being truncated and mixed with another
    // process generation.
    let opened = OpenOptions::new().write(true).create_new(true).open(path);
    let finishing = std::sync::Arc::clone(&operation);
    let (status, error) = with_registry(registry, move |registry| {
        registry.sink = None;
        registry.path = Some(path.to_path_buf());
        registry.written_ranges = 0;
        registry.error = None;
        let error = match opened {
            Ok(file) => {
                registry.sink = Some(Box::new(file));
                registry.health = PerfMapHealth::Active;
                None
            }
            Err(error) => {
                registry.mark_incomplete(&error);
                Some(error)
            }
        };
        debug_assert!(
            registry
                .control
                .as_ref()
                .is_some_and(|active| std::sync::Arc::ptr_eq(active, &finishing))
        );
        registry.control = None;
        (registry.status(), error)
    });
    operation.finish(status.clone());
    match error {
        Some(error) => Err(error),
        None => Ok(status),
    }
}

fn stop_registry(registry: &Mutex<PerfMapRegistry>) -> PerfMapStatus {
    enum Decision {
        Join(std::sync::Arc<ControlOperation>),
        Wait(std::sync::Arc<ControlOperation>),
        Start {
            operation: std::sync::Arc<ControlOperation>,
            sink: Box<dyn Write + Send>,
            ranges: Vec<JitSymbolRange>,
        },
        Return(PerfMapStatus),
    }

    let (operation, mut sink, ranges) = loop {
        let decision = with_registry(registry, |registry| {
            if let Some(operation) = registry.control.clone() {
                if operation.kind == ControlKind::Stop {
                    Decision::Join(operation)
                } else {
                    Decision::Wait(operation)
                }
            } else if registry.health != PerfMapHealth::Active {
                Decision::Return(registry.status())
            } else {
                let operation = std::sync::Arc::new(ControlOperation::new(ControlKind::Stop));
                registry.control = Some(std::sync::Arc::clone(&operation));
                // This lock point is the end of the session: ranges registered or published
                // after it belong to the next session.
                registry.health = PerfMapHealth::Inactive;
                let sink = registry
                    .sink
                    .take()
                    .expect("active perf-map must own its sink");
                Decision::Start {
                    operation,
                    sink,
                    ranges: registry.ranges.clone(),
                }
            }
        });
        match decision {
            Decision::Join(operation) => return operation.wait(),
            Decision::Wait(operation) => {
                operation.wait();
            }
            Decision::Start {
                operation,
                sink,
                ranges,
            } => break (operation, sink, ranges),
            Decision::Return(status) => return status,
        }
    };

    // The thread calling `stop` performs all output; a JIT worker never owns the sink and
    // never calls `Write`.
    let mut written = 0;
    let result = (|| -> io::Result<()> {
        for range in &ranges {
            sink.write_all(range.perf_map_line().as_bytes())?;
            written += 1;
        }
        sink.flush()
    })();
    drop(sink);

    let status = with_registry(registry, |registry| {
        registry.written_ranges = written;
        match result {
            Ok(()) => {
                registry.health = PerfMapHealth::Inactive;
                registry.error = None;
            }
            Err(error) => registry.mark_incomplete(&error),
        }
        debug_assert!(
            registry
                .control
                .as_ref()
                .is_some_and(|active| std::sync::Arc::ptr_eq(active, &operation))
        );
        registry.control = None;
        registry.status()
    });
    operation.finish(status.clone());
    status
}

/// Start the process-level perf-map, creating only the empty file for this session; the
/// ranges are written by `stop_perf_map` on the controlling thread.
// TODO: wire this into the CLI.
#[allow(dead_code)]
pub fn install_perf_map(path: impl AsRef<Path>) -> io::Result<PerfMapStatus> {
    install_registry(perf_registry(), path.as_ref())
}

#[allow(dead_code)]
pub fn stop_perf_map() -> PerfMapStatus {
    stop_registry(perf_registry())
}

#[allow(dead_code)]
pub fn perf_map_status() -> PerfMapStatus {
    with_perf_registry(|registry| registry.status())
}

#[allow(dead_code)]
pub fn jit_symbol_ranges() -> Vec<JitSymbolRange> {
    with_perf_registry(|registry| registry.ranges.clone())
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
        let enabled = match std::env::var("MIRVM_JIT") {
            Ok(v) => !(v == "off" || v == "0"),
            Err(_) => true, // Default on.
        };
        let threshold = std::env::var("MIRVM_JIT_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&t| t > 0)
            .unwrap_or(1000);
        JitState {
            slots: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            slots_fast: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            trace: DomainSlots::new(fn_count),
            trace_enter: AtomicU64::new(0),
            counters: (0..fn_count).map(|_| AtomicU32::new(0)).collect(),
            enabled,
            threshold,
            sync: std::env::var_os("MIRVM_JIT_SYNC").is_some(),
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

#[cfg(test)]
mod tests {
    use super::{
        JitState, JitSymbolRange, JitSymbolRole, PerfMapHealth, PerfMapRegistry, install_registry,
        stop_registry, with_registry,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    fn test_map_path(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "mirvm-{name}-{}-{}.map",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn range(func: u32, role: JitSymbolRole, start: u64, size: u64) -> JitSymbolRange {
        JitSymbolRange::new(17, func, role, start, size, "guest\nname")
    }

    #[test]
    fn tables_sized_to_fn_count_and_zero_initialized() {
        let j = JitState::new(7);
        assert_eq!(j.slots.len(), 7);
        assert_eq!(j.counters.len(), 7);
        assert!(j.slots.iter().all(|s| s.load(Ordering::Acquire) == 0));
        assert!(j.worker.lock().unwrap().is_none());
        assert!(!j.stopping.load(Ordering::Acquire));
        assert!(j.threshold > 0);
        assert!(j.guest_code.read().unwrap().is_empty());
        assert!(j.symbol_ranges().is_empty());
    }

    #[test]
    fn compiled_entry_publication_registers_all_roles_before_slots_become_visible() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let path = test_map_path("jit-publish");
        install_registry(&registry, &path).unwrap();

        let j = JitState::new(4);
        j.publish_compiled_entries_with(
            super::CodeDomain::Plain,
            3,
            0x2000,
            0x3000,
            vec![
                range(3, JitSymbolRole::FastBody, 0x1000, 0x31),
                range(3, JitSymbolRole::Guarded, 0x2000, 0x17),
                range(3, JitSymbolRole::Packed, 0x3000, 0x29),
            ],
            &registry,
        );
        j.publish_c2i_entry_with(
            super::CodeDomain::Plain,
            2,
            0x4000,
            vec![range(2, JitSymbolRole::C2i, 0x4000, 0x1d)],
            &registry,
        );

        assert_eq!(j.slots_fast[3].load(Ordering::Acquire), 0x2000);
        assert_eq!(j.slots[3].load(Ordering::Acquire), 0x3000);
        assert_eq!(j.slots_fast[2].load(Ordering::Acquire), 0x4000);
        let ranges = j.symbol_ranges();
        assert_eq!(ranges.len(), 4);
        assert_eq!(
            with_registry(&registry, |registry| registry.ranges.len()),
            4
        );
        for role in [
            JitSymbolRole::FastBody,
            JitSymbolRole::Guarded,
            JitSymbolRole::Packed,
            JitSymbolRole::C2i,
        ] {
            assert!(ranges.iter().any(|range| range.role == role));
        }
        assert_eq!(j.guest_func_at(0x1010), Some(3));
        assert_eq!(j.guest_func_at(0x2010), None);
        assert_eq!(j.guest_func_at(0x3010), None);
        assert_eq!(j.guest_func_at(0x4010), None);

        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let active = with_registry(&registry, |registry| registry.status());
        assert_eq!(active.health, PerfMapHealth::Active);
        assert_eq!(active.registered_ranges, 4);
        assert_eq!(active.written_ranges, 0);

        let stopped = stop_registry(&registry);
        assert_eq!(stopped.health, PerfMapHealth::Inactive);
        assert_eq!(stopped.written_ranges, 4);
        let map = std::fs::read_to_string(&path).unwrap();
        assert!(map.contains("1000 31 mirvm::engine-17::func-3::fast-body::guest\\nname\n"));
        assert!(map.contains("2000 17 mirvm::engine-17::func-3::guarded::guest\\nname\n"));
        assert!(map.contains("3000 29 mirvm::engine-17::func-3::packed::guest\\nname\n"));
        assert!(map.contains("4000 1d mirvm::engine-17::func-2::c2i::guest\\nname\n"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn map_open_failure_is_incomplete_but_does_not_block_entry_publication() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let missing_parent = test_map_path("missing-parent");
        let path = missing_parent.join("perf.map");
        assert!(install_registry(&registry, &path).is_err());
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Incomplete
        );

        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            super::CodeDomain::Plain,
            0,
            0x5000,
            vec![range(0, JitSymbolRole::C2i, 0x5000, 0x20)],
            &registry,
        );
        assert_eq!(j.slots_fast[0].load(Ordering::Acquire), 0x5000);
        assert_eq!(j.symbol_ranges().len(), 1);
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Incomplete
        );
    }

    #[test]
    fn install_never_replaces_an_existing_or_active_map() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let existing = test_map_path("jit-existing");
        std::fs::write(&existing, b"existing\n").unwrap();
        let error = install_registry(&registry, &existing).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&existing).unwrap(), b"existing\n");
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Incomplete
        );
        std::fs::remove_file(existing).unwrap();

        let active = test_map_path("jit-active");
        install_registry(&registry, &active).unwrap();
        let other = test_map_path("jit-second-active");
        let error = install_registry(&registry, &other).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(!other.exists());
        assert_eq!(
            with_registry(&registry, |registry| registry.status().health),
            PerfMapHealth::Active
        );
        stop_registry(&registry);
        std::fs::remove_file(active).unwrap();
    }

    #[test]
    fn inactive_registration_is_written_only_by_explicit_stop() {
        let registry = Mutex::new(PerfMapRegistry::default());
        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            super::CodeDomain::Plain,
            0,
            0x6000,
            vec![range(0, JitSymbolRole::C2i, 0x6000, 0x21)],
            &registry,
        );
        let before = with_registry(&registry, |registry| registry.status());
        assert_eq!(before.health, PerfMapHealth::Inactive);
        assert_eq!(before.path, None);
        assert_eq!(before.registered_ranges, 1);
        assert_eq!(before.written_ranges, 0);

        let path = test_map_path("jit-backfill");
        let after = install_registry(&registry, &path).unwrap();
        assert_eq!(after.health, PerfMapHealth::Active);
        assert_eq!(after.written_ranges, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let stopped = stop_registry(&registry);
        assert_eq!(stopped.health, PerfMapHealth::Inactive);
        assert_eq!(stopped.written_ranges, 1);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "6000 21 mirvm::engine-17::func-0::c2i::guest\\nname\n"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_engines_append_complete_lines() {
        let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
        let path = test_map_path("jit-concurrent");
        install_registry(&registry, &path).unwrap();
        let mut workers = Vec::new();
        for engine_id in 1..=8 {
            let registry = std::sync::Arc::clone(&registry);
            workers.push(std::thread::spawn(move || {
                let j = JitState::new(1);
                let start = 0x7000 + engine_id * 0x100;
                let range =
                    JitSymbolRange::new(engine_id, 0, JitSymbolRole::C2i, start, 0x22, "target");
                j.publish_c2i_entry_with(
                    super::CodeDomain::Plain,
                    0,
                    start,
                    vec![range],
                    &registry,
                );
                assert_eq!(j.slots_fast[0].load(Ordering::Acquire), start);
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(
            with_registry(&registry, |registry| registry.status().written_ranges),
            0
        );
        assert_eq!(stop_registry(&registry).written_ranges, 8);

        let map = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = map.lines().collect();
        assert_eq!(lines.len(), 8);
        for engine_id in 1..=8 {
            let start = 0x7000 + engine_id * 0x100;
            assert!(lines.iter().any(|line| {
                *line == format!("{start:x} 22 mirvm::engine-{engine_id}::func-0::c2i::target")
            }));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stop_cutoff_excludes_ranges_published_after_its_linearization_point() {
        struct BlockingSink {
            gate: std::sync::Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
            bytes: std::sync::Arc<Mutex<Vec<u8>>>,
        }

        impl std::io::Write for BlockingSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let (state, changed) = &*self.gate;
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.0 = true;
                changed.notify_all();
                while !state.1 {
                    state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                drop(state);
                self.bytes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
        let j = JitState::new(2);
        j.publish_c2i_entry_with(
            super::CodeDomain::Plain,
            0,
            0x9000,
            vec![range(0, JitSymbolRole::C2i, 0x9000, 0x24)],
            &registry,
        );
        let gate = std::sync::Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
        let bytes = std::sync::Arc::new(Mutex::new(Vec::new()));
        with_registry(&registry, |registry| {
            registry.sink = Some(Box::new(BlockingSink {
                gate: std::sync::Arc::clone(&gate),
                bytes: std::sync::Arc::clone(&bytes),
            }));
            registry.health = PerfMapHealth::Active;
            registry.path = Some(PathBuf::from("blocking.map"));
        });

        let stop_registry_ref = std::sync::Arc::clone(&registry);
        let stop = std::thread::spawn(move || stop_registry(&stop_registry_ref));
        let (gate_state, changed) = &*gate;
        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        while !flags.0 {
            flags = changed.wait(flags).unwrap_or_else(|e| e.into_inner());
        }
        drop(flags);

        let operation = with_registry(&registry, |registry| {
            registry
                .control
                .clone()
                .expect("the first stop must remain in progress")
        });
        let second_registry = std::sync::Arc::clone(&registry);
        let second_stop = std::thread::spawn(move || stop_registry(&second_registry));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while operation.waiters.load(Ordering::SeqCst) == 0 {
            assert!(
                !second_stop.is_finished(),
                "a concurrent stop returned an intermediate status"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the concurrent stop did not join the active control operation"
            );
            std::thread::yield_now();
        }
        let next = test_map_path("jit-next-session");
        let next_for_install = next.clone();
        let install_registry_ref = std::sync::Arc::clone(&registry);
        let install =
            std::thread::spawn(move || install_registry(&install_registry_ref, &next_for_install));
        while operation.waiters.load(Ordering::SeqCst) < 2 {
            assert!(
                !install.is_finished(),
                "an install raced past the active stop operation"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the concurrent install did not wait for stop"
            );
            std::thread::yield_now();
        }

        j.publish_c2i_entry_with(
            super::CodeDomain::Plain,
            1,
            0xa000,
            vec![range(1, JitSymbolRole::C2i, 0xa000, 0x25)],
            &registry,
        );
        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        flags.1 = true;
        changed.notify_all();
        drop(flags);

        let status = stop.join().unwrap();
        let second_status = second_stop.join().unwrap();
        assert_eq!(second_status, status);
        assert_eq!(
            install.join().unwrap().unwrap().health,
            PerfMapHealth::Active
        );
        assert_eq!(status.health, PerfMapHealth::Inactive);
        assert_eq!(status.registered_ranges, 2);
        assert_eq!(status.written_ranges, 1);
        let first =
            String::from_utf8(bytes.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
        assert!(first.contains("9000 24 mirvm::engine-17::func-0::c2i"));
        assert!(!first.contains("func-1::c2i"));

        assert_eq!(stop_registry(&registry).written_ranges, 2);
        let next_map = std::fs::read_to_string(&next).unwrap();
        assert!(next_map.contains("func-0::c2i"));
        assert!(next_map.contains("func-1::c2i"));
        std::fs::remove_file(next).unwrap();
    }

    #[test]
    fn concurrent_stop_waiter_observes_the_same_write_failure() {
        struct BlockingFailingSink {
            gate: std::sync::Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
            writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl std::io::Write for BlockingFailingSink {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                self.writes.fetch_add(1, Ordering::SeqCst);
                let (state, changed) = &*self.gate;
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.0 = true;
                changed.notify_all();
                while !state.1 {
                    state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                Err(std::io::Error::other("injected blocked write failure"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
        let gate = std::sync::Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
        let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        with_registry(&registry, |registry| {
            registry.sink = Some(Box::new(BlockingFailingSink {
                gate: std::sync::Arc::clone(&gate),
                writes: std::sync::Arc::clone(&writes),
            }));
            registry.health = PerfMapHealth::Active;
            registry.path = Some(PathBuf::from("blocking-failure.map"));
        });
        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            super::CodeDomain::Plain,
            0,
            0xb000,
            vec![range(0, JitSymbolRole::C2i, 0xb000, 0x26)],
            &registry,
        );

        let first_registry = std::sync::Arc::clone(&registry);
        let first = std::thread::spawn(move || stop_registry(&first_registry));
        let (gate_state, changed) = &*gate;
        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        while !flags.0 {
            flags = changed.wait(flags).unwrap_or_else(|e| e.into_inner());
        }
        drop(flags);

        let operation = with_registry(&registry, |registry| {
            registry
                .control
                .clone()
                .expect("the failing stop must remain in progress")
        });
        let second_registry = std::sync::Arc::clone(&registry);
        let second = std::thread::spawn(move || stop_registry(&second_registry));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while operation.waiters.load(Ordering::SeqCst) == 0 {
            assert!(!second.is_finished(), "the second stop returned too early");
            assert!(
                std::time::Instant::now() < deadline,
                "the second stop did not join the failing operation"
            );
            std::thread::yield_now();
        }

        let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
        flags.1 = true;
        changed.notify_all();
        drop(flags);

        let first_status = first.join().unwrap();
        let second_status = second.join().unwrap();
        assert_eq!(second_status, first_status);
        assert_eq!(first_status.health, PerfMapHealth::Incomplete);
        assert!(
            first_status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("injected blocked write failure"))
        );
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stop_write_failure_is_incomplete_without_blocking_published_code() {
        struct FailingSink;

        impl std::io::Write for FailingSink {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected perf-map write failure"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let registry = Mutex::new(PerfMapRegistry::default());
        with_registry(&registry, |registry| {
            registry.sink = Some(Box::new(FailingSink));
            registry.health = PerfMapHealth::Active;
            registry.path = Some(PathBuf::from("injected.map"));
        });
        let j = JitState::new(1);
        j.publish_c2i_entry_with(
            super::CodeDomain::Plain,
            0,
            0x8000,
            vec![range(0, JitSymbolRole::C2i, 0x8000, 0x23)],
            &registry,
        );

        assert_eq!(j.slots_fast[0].load(Ordering::Acquire), 0x8000);
        let status = stop_registry(&registry);
        assert_eq!(status.health, PerfMapHealth::Incomplete);
        assert_eq!(status.registered_ranges, 1);
        assert_eq!(status.written_ranges, 0);
        assert!(
            status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("injected perf-map write failure"))
        );
    }
    /// Plain and trace publish slots must be fully independent. A trace body
    /// assumes recorder state the plain domain does not have, so an address
    /// published for one domain must never become visible to the other -- and the
    /// selection itself must be a view, not a copy, so the plain path keeps using
    /// the `slots`/`slots_fast` fields.
    #[test]
    fn plain_and_trace_publish_slots_are_independent() {
        let jit = super::JitState::new(3);
        let plain = jit.slots_for(super::CodeDomain::Plain);
        let trace = jit.slots_for(super::CodeDomain::Trace);
        assert_eq!(plain.slots.len(), 3);
        assert_eq!(trace.slots.len(), 3);
        assert!(
            !std::ptr::eq(plain.slots, trace.slots),
            "the two domains must not share a publish slot array"
        );

        // Publishing into the trace domain leaves the plain entries untouched.
        trace.slots[1].store(0xabcd, Ordering::Release);
        assert_eq!(jit.slots[1].load(Ordering::Acquire), 0);
        assert_eq!(jit.trace.slots[1].load(Ordering::Acquire), 0xabcd);

        // And the plain view still borrows those same fields.
        plain.slots[2].store(0x1234, Ordering::Release);
        assert_eq!(jit.slots[2].load(Ordering::Acquire), 0x1234);
    }
}
