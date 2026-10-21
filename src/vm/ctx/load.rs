//! The load step: publish a `Module` plus its loaded instance as `Shared`.
//!
//! Everything `Shared` holds is fixed here, once, before the first thread can read it. This
//! Engine's code domain is decided from the telemetry session -- and only a recording domain gets
//! the rewritable host-syscall form -- function names and JIT tiering state are built over the final
//! function list. Backtrace symbol publication is deferred until a stack capture needs it.
//! The instance is either materialized from the artifact or handed over by the command-line load
//! phase, which owns the process state an artifact cannot describe.
//!
//! The constructors stay `Shared::new*` because that is the name every embedding and test call site
//! uses; what the load step hands over to is `engine`'s lifetime.

use std::sync::{Arc, Mutex};

use super::super::instance::Instance;
use super::super::ir::Module;
use super::engine::{EngineControl, Shared};

impl Shared {
    /// Build the instance an artifact supports and publish both. Raw and test callers use this; the
    /// command-line load phase has state an artifact cannot describe and calls [`Shared::new_loaded`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(module: Module) -> Self {
        let instance = Instance::materialize(&module)
            .unwrap_or_else(|error| panic!("Engine Shared initialization failed: {error}"));
        Self::new_loaded(module, instance)
    }

    /// Publish a Module together with the instance the load phase built for it.
    pub(crate) fn new_loaded(mut module: Module, instance: Instance) -> Self {
        static NEXT_ENGINE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        // One fact, decided once: whether a session is armed now fixes this
        // Engine's code domain for its whole lifetime.
        let domain = if crate::telemetry::capture::is_armed() {
            // Only the trace domain records, so only it needs the rewritable
            // syscall form; plain keeps the untouched IR.
            module.rewrite_host_syscalls_for_capture();
            super::super::jit::CodeDomain::Trace
        } else {
            super::super::jit::CodeDomain::Plain
        };
        module.ensure_function_names();
        let jit = super::super::jit::JitState::new(module.funcs.len());
        let id = NEXT_ENGINE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Shared {
            id,
            module,
            instance,
            symbols: std::sync::OnceLock::new(),
            thunks: super::super::thunks::ThunkCache::default(),
            domain,
            jit,
            control: Arc::new(EngineControl::new(id)),
            heat: std::sync::OnceLock::new(),
            ctx_slots: Mutex::new(Vec::new()),
            fork_baseline_threads: std::sync::atomic::AtomicUsize::new(0),
            fork_baseline_pid: std::sync::atomic::AtomicI32::new(0),
        }
    }

    pub(crate) fn backtrace_symbols(
        &self,
    ) -> Result<&super::super::backtrace::Symbols, &crate::error::Error> {
        self.symbols
            .get_or_init(|| super::super::backtrace::Symbols::materialize(&self.module))
            .as_ref()
    }
}
