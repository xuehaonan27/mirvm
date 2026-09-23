//! The compiler/tooling cold path: the perf-map file, the registry that owns it, and the symbol
//! ranges a compiled function publishes for it.
//!
//! Nothing on the guest call path reads any of this; `JitState` reaches it only when a compile
//! finishes or a tool asks. Its own module keeps that separation visible, and keeps the locking and
//! file writes out of the file a reader opens to understand the slot table.

use super::*;

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

/// The map's own state. Its fields are `pub(super)` because the slot table in the parent and its
/// tests are what drive the registry; nothing outside `state` reads them.
pub(crate) struct PerfMapRegistry {
    pub(super) ranges: Vec<JitSymbolRange>,
    pub(super) sink: Option<Box<dyn Write + Send>>,
    pub(super) health: PerfMapHealth,
    pub(super) path: Option<PathBuf>,
    pub(super) written_ranges: usize,
    pub(super) error: Option<String>,
    pub(super) control: Option<std::sync::Arc<ControlOperation>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlKind {
    Install,
    Stop,
}

/// A synchronous perf-map install or stop, as the caller that asked for it waits for it.
pub(super) struct ControlOperation {
    kind: ControlKind,
    result: Mutex<Option<PerfMapStatus>>,
    completed: std::sync::Condvar,
    #[cfg(test)]
    pub(super) waiters: std::sync::atomic::AtomicUsize,
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
    pub(super) fn status(&self) -> PerfMapStatus {
        PerfMapStatus {
            health: self.health,
            path: self.path.clone(),
            registered_ranges: self.ranges.len(),
            written_ranges: self.written_ranges,
            error: self.error.clone(),
        }
    }

    pub(super) fn mark_incomplete(&mut self, error: &io::Error) {
        self.sink = None;
        self.health = PerfMapHealth::Incomplete;
        self.error = Some(error.to_string());
    }

    pub(super) fn register(&mut self, range: JitSymbolRange) {
        self.ranges.push(range);
    }
}

pub(super) fn perf_registry() -> &'static Mutex<PerfMapRegistry> {
    static REGISTRY: OnceLock<Mutex<PerfMapRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(PerfMapRegistry::default()))
}

fn with_perf_registry<T>(f: impl FnOnce(&mut PerfMapRegistry) -> T) -> T {
    with_registry(perf_registry(), f)
}

pub(super) fn with_registry<T>(
    registry: &Mutex<PerfMapRegistry>,
    f: impl FnOnce(&mut PerfMapRegistry) -> T,
) -> T {
    let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut registry)
}

pub(super) fn install_registry(
    registry: &Mutex<PerfMapRegistry>,
    path: &Path,
) -> io::Result<PerfMapStatus> {
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

pub(super) fn stop_registry(registry: &Mutex<PerfMapRegistry>) -> PerfMapStatus {
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
