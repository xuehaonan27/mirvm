//! Execution environment: Shared (read-only after publication) + Ctx (per-thread execution state, vmctx).
//!
//! Concrete implementation of the three-way state split (concurrency-arch §2) in the real engine;
//! raw-ptr ctx + field-level transient borrows follow the spike2/3/4 discipline. Shared is held
//! by Engine via Arc; each per-thread Ctx also keeps an Arc, so the module is not released while
//! execution state lives. Ctx lives on a **self-managed pthread key**—
//! **boundary TLS attach** (vmctx-passing §1, same as JNI) is the only gate through which every
//! entry point (run_main / run_export / thunk) enters the engine.
//!
//! Why not use host `thread_local!`: guest TLS dtors (std run_dtors registered via pthread_key,
//! thunkified) run in the pthread TSD phase, while host C++ TLS destructors run **before** the
//! TSD phase (glibc start_thread: __call_tls_dtors → __nptl_deallocate_tsd)—at that point Ctx is
//! already gone, and attach inside the dtor thunk would hit destroyed host TLS. Self-managed
//! pthread key + **deferred teardown for N rounds** (re-setspecific in the dtor, glibc max 4
//! rounds) keeps Ctx alive until after the guest key dtor runs.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use super::ffi::FfiState;
use super::frame::ByteRegion;
use super::ir::Module;

/// Read-only after publication: built during load phase, lock-free shared reads during execution
/// phase (foundation of Engine Sync, spike4). Exception = thunks (M4.4 D1): thunk cache
/// materialized on demand during execution—Mutex provides explicit synchronization, the third
/// cell of the three-way state split (concurrency-arch §2), legitimate.
pub struct Shared {
    pub id: u64,
    pub module: Module,
    pub thunks: super::thunks::ThunkCache,
    /// Code domain this Engine's activations use (design §5.2.3). Chosen once,
    /// when the Engine is created, and then held for every activation of this
    /// Engine: guest calls native and native callbacks back into guest stay in
    /// the chain they entered from, so a running activation never migrates.
    /// Plain must stay the default and must not reserve a register or carry
    /// collection state.
    pub(crate) domain: super::jit::CodeDomain,
    /// J1 tiering base (M5.3a): PLT slots + counters, built over the merged FuncId space.
    /// Slot writers are M5.3b compiler threads (published via single atomic swap); otherwise the
    /// read-only-after-publication discipline holds.
    pub jit: super::jit::JitState,
    control: Arc<EngineControl>,
    /// Every host thread owns its slot; Shared keeps only weak discovery links
    /// so an exited thread cannot form a cycle. Finalization clears the live
    /// slots after the execution count reaches zero.
    ctx_slots: Mutex<Vec<Weak<CtxSlot>>>,
    fork_baseline_threads: std::sync::atomic::AtomicUsize,
    /// Process id that pinned `fork_baseline_threads`. A `fork` child keeps the
    /// value but not the parent's service threads, so a mismatching pid marks a
    /// baseline that must be recomputed before the guard can trust it.
    fork_baseline_pid: std::sync::atomic::AtomicI32,
}

impl Shared {
    pub(crate) fn new(module: Module) -> Self {
        Self::try_from_module(module)
            .unwrap_or_else(|error| panic!("Engine Shared initialization failed: {error}"))
    }

    pub(crate) fn try_from_module(mut module: Module) -> Result<Self, String> {
        static NEXT_ENGINE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        // One fact, decided once: whether a session is armed now fixes this
        // Engine's code domain for its whole lifetime.
        let domain = if crate::telemetry::capture::is_armed() {
            // Only the trace domain records, so only it needs the rewritable
            // syscall form; plain keeps the untouched IR.
            module.rewrite_host_syscalls_for_capture();
            super::jit::CodeDomain::Trace
        } else {
            super::jit::CodeDomain::Plain
        };
        module.ensure_function_names();
        super::backtrace::materialize_symbols(&mut module)?;
        let jit = super::jit::JitState::new(module.funcs.len());
        let id = NEXT_ENGINE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Shared {
            id,
            module,
            thunks: super::thunks::ThunkCache::default(),
            domain,
            jit,
            control: Arc::new(EngineControl::new(id)),
            ctx_slots: Mutex::new(Vec::new()),
            fork_baseline_threads: std::sync::atomic::AtomicUsize::new(0),
            fork_baseline_pid: std::sync::atomic::AtomicI32::new(0),
        })
    }

    pub(crate) fn control(&self) -> &Arc<EngineControl> {
        &self.control
    }

    fn register_ctx_slot(&self, slot: &Arc<CtxSlot>) {
        let mut slots = self.ctx_slots.lock().unwrap();
        slots.retain(|slot| slot.strong_count() != 0);
        slots.push(Arc::downgrade(slot));
    }

    fn release_thread_contexts(&self) {
        let slots = std::mem::take(&mut *self.ctx_slots.lock().unwrap());
        let contexts = slots
            .into_iter()
            .filter_map(|slot| slot.upgrade())
            .filter_map(|slot| slot.ctx.lock().unwrap().take())
            .collect::<Vec<_>>();
        drop(contexts);
    }
}

static ENGINES: std::sync::LazyLock<std::sync::RwLock<HashMap<u64, Weak<Shared>>>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(HashMap::new()));

#[cfg(test)]
pub(crate) fn engine(id: u64) -> Option<Arc<Shared>> {
    ENGINES.read().unwrap().get(&id).and_then(Weak::upgrade)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineState {
    Running,
    Closing,
    Closed,
}

const PHASE_SHIFT: u32 = usize::BITS - 2;
const COUNT_MASK: usize = (1usize << PHASE_SHIFT) - 1;
const PHASE_RUNNING: usize = 0 << PHASE_SHIFT;
const PHASE_CLOSING: usize = 1 << PHASE_SHIFT;
const PHASE_FINALIZING: usize = 2 << PHASE_SHIFT;
const PHASE_CLOSED: usize = 3 << PHASE_SHIFT;

/// Small process-lifetime tombstone shared by handles and native callbacks.
/// Its self-owner keeps Shared alive until the last execution lease leaves,
/// then finalization breaks the cycle. Escaped callbacks retain only this
/// small tombstone after close, not the Module.
pub(crate) struct EngineControl {
    id: u64,
    lifecycle: std::sync::atomic::AtomicUsize,
    active_executions: std::sync::atomic::AtomicUsize,
    finalizer_started: std::sync::atomic::AtomicBool,
    shared: std::sync::atomic::AtomicPtr<Shared>,
    owner: Mutex<Option<Arc<Shared>>>,
    wait_lock: Mutex<()>,
    closed: Condvar,
    pub(crate) signal_inbox: super::signal::SignalInbox,
}

#[cfg(test)]
type WaitClosedCheckHook = Box<dyn FnOnce() + Send + 'static>;

#[cfg(test)]
static WAIT_CLOSED_CHECK_HOOK: OnceLock<Mutex<Option<WaitClosedCheckHook>>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn set_wait_closed_check_hook(hook: WaitClosedCheckHook) {
    *WAIT_CLOSED_CHECK_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = Some(hook);
}

#[cfg(test)]
fn run_wait_closed_check_hook() {
    let hook = WAIT_CLOSED_CHECK_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some(hook) = hook {
        hook();
    }
}

impl EngineControl {
    fn new(id: u64) -> Self {
        Self {
            id,
            lifecycle: std::sync::atomic::AtomicUsize::new(PHASE_RUNNING),
            active_executions: std::sync::atomic::AtomicUsize::new(0),
            finalizer_started: std::sync::atomic::AtomicBool::new(false),
            shared: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            owner: Mutex::new(None),
            wait_lock: Mutex::new(()),
            closed: Condvar::new(),
            signal_inbox: super::signal::SignalInbox::new(),
        }
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn install_owner(&self, shared: Arc<Shared>) {
        self.shared.store(
            Arc::as_ptr(&shared).cast_mut(),
            std::sync::atomic::Ordering::Release,
        );
        assert!(self.owner.lock().unwrap().replace(shared).is_none());
    }

    fn state(&self) -> EngineState {
        match self.phase() {
            PHASE_RUNNING => EngineState::Running,
            PHASE_CLOSING | PHASE_FINALIZING => EngineState::Closing,
            PHASE_CLOSED => EngineState::Closed,
            _ => unreachable!("invalid Engine lifecycle phase"),
        }
    }

    fn phase(&self) -> usize {
        self.lifecycle.load(std::sync::atomic::Ordering::Acquire) & !COUNT_MASK
    }

    fn begin_execution(&self, allow_closing_reentry: bool, allow_finalizing: bool) -> bool {
        loop {
            let state = self.lifecycle.load(std::sync::atomic::Ordering::Acquire);
            let phase = state & !COUNT_MASK;
            if phase != PHASE_RUNNING
                && !(allow_closing_reentry && phase == PHASE_CLOSING)
                && !(allow_finalizing && phase == PHASE_FINALIZING)
            {
                return false;
            }
            if state & COUNT_MASK == COUNT_MASK {
                eprintln!("mirvm[m4-engine]: Engine execution lease count overflowed");
                std::process::abort();
            }
            if self
                .lifecycle
                .compare_exchange_weak(
                    state,
                    state + 1,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return true;
            }
        }
    }

    fn begin_active_execution(&self, allow_closing_reentry: bool) -> bool {
        // Publish the active-operation guard before acquiring the lifecycle
        // count. Close-side pthread revocation reads this counter, so it must
        // never observe an admitted execution in the gap between two atomics.
        if self
            .active_executions
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            == usize::MAX
        {
            eprintln!("mirvm[m4-engine]: Engine active execution count overflowed");
            std::process::abort();
        }
        if self.begin_execution(allow_closing_reentry, false) {
            return true;
        }
        let last = self
            .active_executions
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1;
        if last && self.is_closing() {
            super::deferred::executions_idle(self);
        }
        false
    }

    fn begin_finalizer_execution(&self) -> bool {
        if self
            .active_executions
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            == usize::MAX
        {
            eprintln!("mirvm[m4-engine]: Engine active execution count overflowed");
            std::process::abort();
        }
        if self
            .lifecycle
            .compare_exchange(
                PHASE_CLOSING,
                PHASE_CLOSING | 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            return true;
        }
        let last = self
            .active_executions
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1;
        if last && self.is_closing() {
            super::deferred::executions_idle(self);
        }
        false
    }

    fn begin_finalizer_permit(&self) -> bool {
        self.lifecycle
            .compare_exchange(
                PHASE_CLOSING,
                PHASE_CLOSING | 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    fn request_close(&self) -> bool {
        loop {
            let state = self.lifecycle.load(std::sync::atomic::Ordering::Acquire);
            if state & !COUNT_MASK != PHASE_RUNNING {
                return false;
            }
            let next = PHASE_CLOSING | (state & COUNT_MASK);
            if self
                .lifecycle
                .compare_exchange_weak(
                    state,
                    next,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return true;
            }
        }
    }

    pub(crate) fn begin_finalizing_with_permit(&self) -> bool {
        self.lifecycle
            .compare_exchange(
                PHASE_CLOSING | 1,
                PHASE_FINALIZING | 1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    #[cfg(test)]
    pub(crate) fn prepare_signal_seal_test(&self) {
        assert!(self.request_close());
        assert!(self.begin_finalizer_permit());
    }

    fn finish_execution(&self) {
        loop {
            let state = self.lifecycle.load(std::sync::atomic::Ordering::Acquire);
            let count = state & COUNT_MASK;
            if count == 0 {
                eprintln!("mirvm[m4-engine]: Engine execution lease count underflowed");
                std::process::abort();
            }
            let next = state - 1;
            if self
                .lifecycle
                .compare_exchange_weak(
                    state,
                    next,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return;
            }
        }
    }

    fn finish_close(&self) {
        let _wait = self.wait_lock.lock().unwrap();
        assert_eq!(
            self.lifecycle
                .swap(PHASE_CLOSED, std::sync::atomic::Ordering::AcqRel),
            PHASE_FINALIZING
        );
        self.shared
            .store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
        self.owner.lock().unwrap().take();
        self.closed.notify_all();
    }

    fn wait_closed(&self) {
        let mut wait = self.wait_lock.lock().unwrap();
        while self.lifecycle.load(std::sync::atomic::Ordering::Acquire) != PHASE_CLOSED {
            wait = self.closed.wait(wait).unwrap();
        }
    }

    fn wait_closed_or_current_thread_signal(&self) -> bool {
        let mut wait = self.wait_lock.lock().unwrap();
        loop {
            if self.lifecycle.load(std::sync::atomic::Ordering::Acquire) == PHASE_CLOSED {
                return true;
            }
            if super::signal::current_thread_has_pending_for_engine(self.id) {
                return false;
            }
            #[cfg(test)]
            run_wait_closed_check_hook();
            let (next, _) = self
                .closed
                .wait_timeout(wait, std::time::Duration::from_millis(10))
                .unwrap();
            wait = next;
        }
    }

    fn shared_after_lease(&self) -> Arc<Shared> {
        let shared = self.shared.load(std::sync::atomic::Ordering::Acquire);
        if shared.is_null() {
            eprintln!("mirvm[m4-engine]: active Engine lease lost its Shared state");
            std::process::abort();
        }
        unsafe { Arc::increment_strong_count(shared) };
        unsafe { Arc::from_raw(shared) }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.phase() == PHASE_RUNNING
    }

    /// Signal installation always starts under an execution lease, but close
    /// permits already-registered callbacks to finish while the Engine is
    /// Closing. Recheck the phase while holding the process signal registry
    /// lock so no registration can cross the final Closing -> Finalizing seal.
    pub(crate) fn accepts_signal_install(&self) -> bool {
        matches!(self.phase(), PHASE_RUNNING | PHASE_CLOSING)
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.phase() == PHASE_CLOSING
    }

    pub(crate) fn executions_idle(&self) -> bool {
        self.active_executions
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
    }
}

struct EngineInner {
    shared: Arc<Shared>,
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        close_shared(&self.shared);
    }
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

impl Engine {
    // Used by the separately compiled TSan harness, which includes the VM
    // sources directly instead of linking this library target.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(shared: Shared) -> Self {
        Self::try_new(shared)
            .unwrap_or_else(|error| panic!("Engine initialization failed: {error}"))
    }

    pub(crate) fn try_new(mut shared: Shared) -> Result<Self, String> {
        super::native_instance::isolate_required_libraries(&mut shared.module, shared.id)?;
        let mut shared = Arc::new(shared);
        let control = Arc::clone(shared.control());
        let module = &mut Arc::get_mut(&mut shared)
            .ok_or("new Engine Shared unexpectedly aliased")?
            .module;
        let entry_closures = super::thunks::materialize_all_entry_stubs(module, &control)?;
        super::native_instance::prepare_required_libraries(module)?;
        super::native_instance::patch_entry_slots(module)?;
        super::native_instance::patch_pthread_slots(module, control.id())?;
        super::ffi::resolve_got_fixups(module)?;
        ENGINES
            .write()
            .unwrap()
            .insert(shared.id, Arc::downgrade(&shared));
        control.install_owner(Arc::clone(&shared));
        #[cfg(feature = "cranelift")]
        super::jit::start(&shared);
        super::native_instance::commit_images(&shared.module, &control);
        entry_closures.commit();

        enum StartupFailure {
            Error(String),
            Resume(super::unwind::CaughtException),
        }
        let startup = ExecutionLease::acquire(Arc::clone(&shared), false)
            .map_err(|_| "Engine closed before native constructors started".to_string())?;
        let startup_failure = {
            let activation = activate(startup.shared());
            let failure = match super::unwind::catch_raw(|| {
                super::native_instance::run_initializers(&shared.module)
            }) {
                Ok(Ok(())) if shared.control.is_running() => None,
                Ok(Ok(())) => Some(StartupFailure::Error(
                    "native constructor closed its Engine".to_string(),
                )),
                Ok(Err(error)) => Some(StartupFailure::Error(error)),
                Err(exception) => Some(match exception.take_mirvm(&shared) {
                    Ok(super::unwind::MirvmPayload::Guest(payload)) => {
                        super::interp::dispose_guest_panic_during_startup(&shared, payload);
                        StartupFailure::Error("native constructor raised a guest panic".into())
                    }
                    Ok(super::unwind::MirvmPayload::EngineFault(fault)) => {
                        let fault = drain_current_thread_signal_deliveries_after_fault(
                            activation.ctx(),
                            fault.finish(),
                        );
                        StartupFailure::Error(format!(
                            "native constructor hit an Engine fault: {}",
                            fault.message
                        ))
                    }
                    Ok(super::unwind::MirvmPayload::EngineClosed) => {
                        StartupFailure::Error("native constructor called a closed Engine".into())
                    }
                    Err(exception) if exception.is_rust() => StartupFailure::Resume(exception),
                    Err(exception) => {
                        if let Err(exception) = exception.delete_foreign_for_startup() {
                            StartupFailure::Resume(exception)
                        } else {
                            StartupFailure::Error(
                                "native constructor raised a foreign exception".into(),
                            )
                        }
                    }
                }),
            };
            drop(activation);
            failure
        };
        drop(startup);
        if let Some(failure) = startup_failure {
            close_shared(&shared);
            shared.control.wait_closed();
            return match failure {
                StartupFailure::Error(error) => Err(error),
                StartupFailure::Resume(exception) => exception.resume_or_rethrow(),
            };
        }
        Ok(Self {
            inner: Arc::new(EngineInner { shared }),
        })
    }

    /// Construct an Engine from raw executable IR.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that every embedded native address, memory
    /// operand and ABI description is valid for this process. Bytecode shape
    /// verification alone cannot prove those host-pointer obligations.
    pub unsafe fn from_module_unchecked(module: Module) -> Result<Self, String> {
        // Structural verification is still mandatory. `unsafe` covers only
        // facts it cannot prove: validity/lifetime of embedded host pointers
        // and agreement of native ABI descriptions with their real callees.
        super::verify::module(&module)?;
        Self::try_new(Shared::try_from_module(module)?)
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.inner.shared
    }

    pub(crate) fn control(&self) -> &Arc<EngineControl> {
        self.inner.shared.control()
    }

    pub fn state(&self) -> EngineState {
        self.control().state()
    }

    /// Start closing without waiting for active guest calls to return.
    pub fn close(&self) {
        close_shared(self.shared());
    }

    /// Close and wait until all active calls have left and teardown is done.
    ///
    /// Waiting anywhere inside this Engine's current native call chain would
    /// wait for an outer execution lease owned by the same host thread. That
    /// case closes the Engine but returns immediately so an outer host frame
    /// can wait after the call chain has unwound.
    pub fn wait_closed(&self) -> Result<(), WaitClosedError> {
        self.close();
        if current_thread_has_engine(self.control().id())
            || current_thread_has_pending_finalizer(self.control().id())
        {
            return Err(WaitClosedError::ActiveOnCurrentThread);
        }
        self.control()
            .wait_closed_or_current_thread_signal()
            .then_some(())
            .ok_or(WaitClosedError::ActiveOnCurrentThread)
    }

    /// Run an embedding-side `C-unwind` callback invocation and classify closure of this
    /// callback's owning Engine. Panics and exceptions not owned by this Engine are resumed.
    pub fn catch_callback_unwind<R>(
        &self,
        callback: impl FnOnce() -> R,
    ) -> Result<R, EngineClosed> {
        match super::unwind::catch_raw(callback) {
            Ok(value) => Ok(value),
            Err(exception) => match exception.take_engine_closed(self.control()) {
                Ok(()) => Err(EngineClosed),
                Err(exception) => exception.resume_or_rethrow(),
            },
        }
    }

    pub(crate) fn execution_lease(&self) -> Result<ExecutionLease, EngineClosed> {
        ExecutionLease::acquire(Arc::clone(self.shared()), false)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EngineClosed;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitClosedError {
    ActiveOnCurrentThread,
}

pub(crate) struct ExecutionLease {
    shared: Arc<Shared>,
}

/// Keeps the lifecycle count nonzero while close drains signals, without
/// claiming an active execution. TSD cleanup must still see the Engine as idle;
/// actual signal handlers take short `ExecutionLease`s of their own.
struct FinalizerPermit {
    shared: Arc<Shared>,
}

impl FinalizerPermit {
    fn acquire(shared: &Shared) -> Option<Self> {
        if !shared.control.begin_finalizer_permit() {
            return None;
        }
        Some(Self {
            shared: shared.control.shared_after_lease(),
        })
    }

    fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

impl Drop for FinalizerPermit {
    fn drop(&mut self) {
        self.shared.control.finish_execution();
    }
}

impl ExecutionLease {
    fn acquire(shared: Arc<Shared>, allow_closing_reentry: bool) -> Result<Self, EngineClosed> {
        if shared.control.begin_active_execution(allow_closing_reentry) {
            Ok(Self { shared })
        } else {
            Err(EngineClosed)
        }
    }

    pub(crate) fn for_thunk(control: &Arc<EngineControl>) -> Result<Self, EngineClosed> {
        // Reentry belongs to the whole native call chain, not just its
        // innermost Engine. For example A -> native -> B -> native -> A still
        // has an active A lease even while B is the current context.
        let allow_closing_reentry = current_thread_has_engine(control.id());
        if !control.begin_active_execution(allow_closing_reentry) {
            return Err(EngineClosed);
        }
        let shared = control.shared_after_lease();
        Ok(Self { shared })
    }

    /// A callback registration already owns a deferred hold, so it may enter
    /// while the Engine is Closing. The hold prevents Finalizing until this
    /// execution lease has been acquired.
    pub(crate) fn for_registered_callback(
        control: &Arc<EngineControl>,
    ) -> Result<Self, EngineClosed> {
        if !control.begin_active_execution(true) {
            return Err(EngineClosed);
        }
        let shared = control.shared_after_lease();
        Ok(Self { shared })
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }
}

/// A native subsystem has accepted a guest callback but has not yet either
/// invoked or revoked it. It uses the same atomic count as executions so the
/// Running -> Closing transition cannot race past the registration.
pub(crate) struct DeferredHold {
    control: Arc<EngineControl>,
}

impl DeferredHold {
    pub(crate) fn acquire(
        control: &Arc<EngineControl>,
        allow_closing: bool,
    ) -> Result<Self, EngineClosed> {
        if control.begin_execution(allow_closing, false) {
            Ok(Self {
                control: Arc::clone(control),
            })
        } else {
            Err(EngineClosed)
        }
    }
}

impl Drop for DeferredHold {
    fn drop(&mut self) {
        self.control.finish_execution();
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        let last = self
            .shared
            .control
            .active_executions
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1;
        self.shared.control.finish_execution();
        if last && self.shared.control.is_closing() {
            super::deferred::executions_idle(self.shared.control());
        }
    }
}

fn close_shared(shared: &Arc<Shared>) {
    let initiated = shared.control.request_close();
    // This runs after the phase transition: registered callbacks may enter in
    // Closing, while no new unregistered public entry can start. It is
    // idempotent because close() and Engine handle Drop can race.
    super::deferred::begin_engine_close(shared);
    if !initiated {
        return;
    }
    if !shared
        .control
        .finalizer_started
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        // pthread_create inherits the caller's real signal mask. Do not create
        // a finalizer while a deferred handler's temporary mask is active.
        // The outermost handler restores both masks before starting this work.
        if defer_finalizer_while_signal_masked(shared) {
            return;
        }
        start_finalizer(Arc::clone(shared));
    }
}

fn start_finalizer(shared: Arc<Shared>) {
    let physical_mask =
        crate::os::signal::Sigaction::current_standard_mask_bits().unwrap_or_else(|error| {
            eprintln!("mirvm[m4-engine]: failed to query finalizer caller signal mask: {error}");
            std::process::abort();
        });
    // Running inline would make close callbacks execute on a host thread that
    // explicitly blocked their signal. A worker gives the already-recorded
    // process event an independent execution context. It still inherits the
    // mask, which the close-only inbox drain handles without changing either
    // pthread's real mask.
    if physical_mask == 0 && shared.control.begin_finalizer_execution() {
        finalize_shared(&shared);
        return;
    }
    if let Err(error) = std::thread::Builder::new()
        .name("mirvm-close".into())
        .spawn(move || {
            while !shared.control.begin_finalizer_execution() {
                if shared.control.phase() != PHASE_CLOSING {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            finalize_shared(&shared);
        })
    {
        eprintln!("mirvm[m4-engine]: failed to start Engine finalizer: {error}");
        std::process::abort();
    }
}

fn finalize_shared(shared: &Shared) {
    let finalizer = ExecutionLease {
        shared: shared.control.shared_after_lease(),
    };
    let activation = activate(finalizer.shared());
    // A close worker can inherit a physically blocked signal mask. Cover its
    // whole lifetime, including native fini, so a successful wrapped raise is
    // owned by ordinary VM pending state and cannot die with this pthread.
    let _close_signal_drain = CloseSignalDrainGuard::enter(activation.contexts, true);
    super::unwind::guard_native_teardown(|| super::native_instance::run_finalizers(&shared.module));
    drop(activation);
    drop(finalizer);

    // Native finalizers and signal handlers may create tracked pthread/TSD
    // work. Wait for that work to release every hold, then take the unique
    // Closing lease before touching dispositions. A drained handler may create
    // another hold or disposition, in which case the loop deliberately goes
    // back through the same quiescence point. A lifecycle-only permit keeps
    // CLOSING/1 while active_executions remains zero for TSD cleanup. It then
    // changes CLOSING/1 -> FINALIZING/1 in place, so no registered callback can
    // enter between signal cleanup and the phase transition.
    loop {
        super::deferred::begin_engine_close(shared);
        let signal_permit = loop {
            if let Some(permit) = FinalizerPermit::acquire(shared) {
                break permit;
            }
            if shared.control.phase() != PHASE_CLOSING {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        let signal_activation = activate(signal_permit.shared());
        super::unwind::guard_native_teardown(|| {
            loop {
                super::signal::deactivate_engine(shared.control()).unwrap_or_else(|error| {
                    eprintln!(
                        "mirvm[m4-engine]: failed to close guest signal disposition: {error}"
                    );
                    std::process::abort();
                });
                drain_pending_signals_for_close(signal_activation.ctx());
                if !super::signal::has_engine_registrations(shared.control())
                    && !super::signal::has_engine_pending(shared.control())
                {
                    break;
                }
            }
        });
        let finalizing = super::signal::try_seal_engine(shared.control());
        drop(signal_activation);
        drop(signal_permit);

        if finalizing {
            break;
        }
    }
    super::deferred::finish_engine_close(shared);
    shared.release_thread_contexts();
    shared.module.funcs.flush_heat_order();
    #[cfg(feature = "cranelift")]
    super::jit::stop(shared);
    super::interp::discard_engine_state(shared.id);
    ENGINES.write().unwrap().remove(&shared.id);
    shared.control.finish_close();
}

const SIGNAL_DRAIN_ROUNDS: usize = 8;

/// Per-thread execution state (vmctx). Since M4.4 one per guest thread, lifetime = host thread_local.
pub struct Ctx {
    pub shared: *const Shared,
    shared_owner: Arc<Shared>,
    pub region: ByteRegion,
    /// Interpreted-frame recursion depth (diagnostic count; overflow detection now uses the real
    /// stack guard `stack_floor`, M5.2 D8a).
    pub depth: u32,
    /// Ordinary execution state is delivering mailbox messages; nested safepoints traversed inside
    /// a handler must not recursively drain.
    signal_draining: bool,
    /// Host execution stack safety floor (M5.2 D8a): low end of this thread's stack + safety margin.
    /// When an interp_frame stack pointer approximation falls below this, it is a guest stack
    /// overflow (diagnostic exit rather than host SIGSEGV). The real byte guard replaces the old
    /// fixed frame limit (8000): it adapts to the thread's real stack (main execution thread 1 GiB,
    /// amplified guest thread stacks, and foreign native thread thunk re-entry all work). 0 means
    /// probe failed and no guard is applied (equivalent to the old unguarded world; getattr_np is
    /// available for all threads including the main thread on glibc).
    pub stack_floor: usize,
    /// Foreign passthrough state (dlsym cache + dlopen handles; dlsym is idempotent, per-thread
    /// independent caches are harmless).
    pub ffi: FfiState,
    /// Guest TLS instance table (M4.4 D3): TlsId → real address of this thread's instance
    /// (0 = not materialized; first visit heap-allocates + copies the template). Guest dtors run
    /// first via the pthread-key thunk; instance memory is freed only in Ctx's final teardown round.
    pub tls: Vec<u64>,
    /// Active interpreted frames. IP is consumed by the guest unwinder; CFA is the location of this
    /// interpreted call on the host stack. The backtrace uses CFA to merge interpreted frames with
    /// JIT real-machine frames read by the system unwinder into a single call sequence. Pushed on
    /// enter and popped on FrameGuard::drop (same lifetime as depth, unwind-safe).
    pub shadow: Vec<ShadowFrame>,
    /// Nested `run_main` states for this thread on this Engine. Each run is recorded independently
    /// so that a normal return 101 cannot be confused with guest std converting a main panic into
    /// 101.
    main_runs: Vec<MainRunState>,
}

// A Ctx is used only by its owning host thread while an ExecutionLease is
// live. Finalization can move it to the closing thread solely after the global
// execution count reaches zero; its drops (mimalloc free and munmap) are
// thread-independent.
unsafe impl Send for Ctx {}

#[derive(Default)]
struct MainRunState {
    boundary_activation: Option<u64>,
    catcher_claimed: bool,
    catcher_active: bool,
    panicked: bool,
}

#[derive(Clone, Copy)]
pub struct ShadowFrame {
    pub ip: u64,
    pub cfa: u64,
}

impl Ctx {
    pub fn new(shared: &Arc<Shared>) -> Self {
        Ctx {
            shared: Arc::as_ptr(shared),
            shared_owner: Arc::clone(shared),
            region: ByteRegion::new(),
            depth: 0,
            signal_draining: false,
            stack_floor: thread_stack_floor(),
            ffi: FfiState::default(),
            tls: Vec::new(),
            shadow: Vec::new(),
            main_runs: Vec::new(),
        }
    }
}

pub(crate) struct MainRunGuard {
    ctx: *mut Ctx,
    index: usize,
    finished: bool,
}

impl MainRunGuard {
    pub(crate) fn finish(mut self) -> bool {
        let states = unsafe { &mut (*self.ctx).main_runs };
        if states.len() != self.index + 1 {
            eprintln!("mirvm[m4-engine]: nested main execution state was finished out of order");
            std::process::abort();
        }
        let state = states.pop().unwrap();
        self.finished = true;
        state.panicked
    }
}

impl Drop for MainRunGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let states = unsafe { &mut (*self.ctx).main_runs };
        if states.len() != self.index + 1 {
            eprintln!("mirvm[m4-engine]: nested main execution state unwound out of order");
            std::process::abort();
        }
        states.pop();
    }
}

pub(crate) fn begin_main_run(ctx: *mut Ctx) -> MainRunGuard {
    let states = unsafe { &mut (*ctx).main_runs };
    let index = states.len();
    states.push(MainRunState::default());
    MainRunGuard {
        ctx,
        index,
        finished: false,
    }
}

struct MainBoundaryGuard {
    ctx: *mut Ctx,
    index: usize,
    finished: bool,
}

impl MainBoundaryGuard {
    fn finish(mut self) {
        let state = unsafe { &mut (&mut (*self.ctx).main_runs)[self.index] };
        if !state.catcher_claimed || state.catcher_active {
            super::interp::engine_abort(
                "fixed std main panic catch call did not pass the expected catch_unwind intrinsic",
            );
        }
        state.boundary_activation = None;
        self.finished = true;
    }
}

impl Drop for MainBoundaryGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let states = unsafe { &mut (*self.ctx).main_runs };
        if let Some(state) = states.get_mut(self.index) {
            state.boundary_activation = None;
            state.catcher_active = false;
        }
    }
}

/// Execute the standard main catch call marked by the IR. The boundary records the activation
/// number of the current Engine entry; signal handlers or native thunk re-entries receive a
/// different number and cannot borrow the outer boundary to claim a main panic.
pub(crate) fn call_main_panic_boundary<R>(ctx: *mut Ctx, f: impl FnOnce() -> R) -> R {
    let states = unsafe { &mut (*ctx).main_runs };
    let Some(index) = states.len().checked_sub(1) else {
        super::interp::engine_abort("main panic catch call appeared outside run_main");
    };
    let state = &mut states[index];
    if state.boundary_activation.is_some() {
        super::interp::engine_abort(
            "duplicate entry into main panic catch boundary during the same main execution",
        );
    }
    state.boundary_activation = Some(current_activation(ctx));
    state.catcher_claimed = false;
    let guard = MainBoundaryGuard {
        ctx,
        index,
        finished: false,
    };
    let result = f();
    guard.finish();
    result
}

pub(crate) struct MainCatchGuard {
    ctx: *mut Ctx,
    index: usize,
}

impl MainCatchGuard {
    pub(crate) fn mark_panicked(&mut self) {
        unsafe { (&mut (*self.ctx).main_runs)[self.index].panicked = true };
    }
}

impl Drop for MainCatchGuard {
    fn drop(&mut self) {
        let states = unsafe { &mut (*self.ctx).main_runs };
        let Some(state) = states.get_mut(self.index) else {
            eprintln!("mirvm[m4-engine]: main catch state disappeared while active");
            std::process::abort();
        };
        state.catcher_active = false;
    }
}

/// Only the intrinsic precisely marked by the lowering phase and still within the same Engine
/// activation may claim the main catch. Ordinary catches, signal handlers, and native thunk
/// re-entries all return `None`.
pub(crate) fn claim_main_panic_catch(
    ctx: *mut Ctx,
    role: super::ir::BuiltinCallRole,
) -> Option<MainCatchGuard> {
    if role != super::ir::BuiltinCallRole::MainPanicCatcher {
        return None;
    }
    let activation = current_activation(ctx);
    let states = unsafe { &mut (*ctx).main_runs };
    let index = states.len().checked_sub(1)?;
    let state = &mut states[index];
    if state.boundary_activation != Some(activation) || state.catcher_claimed {
        return None;
    }
    state.catcher_claimed = true;
    state.catcher_active = true;
    Some(MainCatchGuard { ctx, index })
}

impl Drop for Ctx {
    fn drop(&mut self) {
        for (id, addr) in self.tls.iter().copied().enumerate() {
            if addr == 0 {
                continue;
            }
            let slot = self.shared().module.tls[id];
            super::heap::dealloc(addr, slot.size.max(1), slot.align as u64);
        }
    }
}

impl Ctx {
    pub fn shared(&self) -> &Shared {
        &self.shared_owner
    }

    pub fn shared_arc(&self) -> Arc<Shared> {
        Arc::clone(&self.shared_owner)
    }
}

struct SignalDrainGuard {
    ctx: *mut Ctx,
}

struct SignalMaskGuard {
    contexts: *mut ThreadContexts,
    previous: u64,
    restored: bool,
}

impl SignalMaskGuard {
    fn restore(&mut self) {
        if self.restored {
            return;
        }
        unsafe { (*self.contexts).signal_mask = self.previous };
        self.restored = true;
    }

    /// Restore the interrupted mask and deliver synchronous raises against
    /// the disposition that is current now. This method is reached only after
    /// the guest handler returned normally; `Drop` deliberately does not run
    /// guest code while an EngineFault is unwinding.
    fn finish(mut self, ctx: *mut Ctx, host_mask: crate::os::signal::ThreadSignalMaskGuard) {
        self.restore();
        drop(host_mask);
        start_pending_signal_finalizers(self.contexts);
        drain_current_thread_signal_deliveries(ctx);
        drain_pending_signals(ctx);
    }
}

impl Drop for SignalMaskGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

struct CloseSignalDrainGuard {
    contexts: *mut ThreadContexts,
    previous: bool,
}

impl CloseSignalDrainGuard {
    fn enter(contexts: *mut ThreadContexts, closing: bool) -> Self {
        let previous = unsafe { (*contexts).close_signal_drain };
        unsafe { (*contexts).close_signal_drain = previous || closing };
        Self { contexts, previous }
    }
}

impl Drop for CloseSignalDrainGuard {
    fn drop(&mut self) {
        unsafe { (*self.contexts).close_signal_drain = self.previous };
    }
}

struct CloseSignalUnblockGuard {
    previous: libc::sigset_t,
}

impl Drop for CloseSignalUnblockGuard {
    fn drop(&mut self) {
        let error = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if error != 0 {
            eprintln!("mirvm[m4-engine]: failed to restore close finalizer signal mask: {error}");
            std::process::abort();
        }
    }
}

fn raise_from_close_drain(ctx: *mut Ctx, signum: i32) -> i32 {
    let mut signal: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut signal);
        libc::sigaddset(&mut signal, signum);
    }
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    let error = unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &signal, &mut previous) };
    if error != 0 {
        super::interp::engine_abort(&format!(
            "failed to unblock signal {signum} on the close finalizer thread: {error}"
        ));
    }
    let _restore = CloseSignalUnblockGuard { previous };
    real_raise_and_drain(ctx, signum)
}

fn dispatch_signal_delivery(
    delivery: super::signal::SignalDeliveryGuard,
    signum: i32,
) -> Result<(), super::unwind::EngineFaultReport> {
    let registration = delivery.registration();
    let lease = super::ctx::ExecutionLease::for_registered_callback(registration.control())
        .unwrap_or_else(|_| {
            super::interp::engine_abort("signal handler belongs to a closed Engine")
        });
    let activation = activate(lease.shared());
    let handler_ctx = activation.ctx();
    let bit = 1u64 << signum;
    let previous = unsafe { (*activation.contexts).signal_mask };
    if previous & bit != 0 {
        drop(activation);
        drop(lease);
        registration.defer(signum);
        return Ok(());
    }
    // Declare the physical guard before the logical guard. If an EngineFault
    // leaves the callback, reverse-order unwinding restores the logical mask
    // first and the real pthread mask second; ActivationGuard may only start a
    // queued finalizer after both have been restored.
    let host_mask = registration
        .action()
        .block_for_handler(signum)
        .unwrap_or_else(|error| {
            super::interp::engine_abort(&format!(
                "failed to apply signal {signum} handler mask to the host thread: {error}"
            ))
        });
    unsafe {
        (*activation.contexts).signal_mask = previous | bit | registration.mask_bits();
    }
    let mask = SignalMaskGuard {
        contexts: activation.contexts,
        previous,
        restored: false,
    };
    let callback = super::unwind::catch_raw(|| {
        crate::vm::engine::unwind::guard_terminate(|| match registration.callback() {
            super::signal::DeferredSignalCallback::Guest(func) => {
                super::interp::call_guest(handler_ctx, func, &[signum as u64]);
            }
            super::signal::DeferredSignalCallback::ImageNative(address) => {
                let callback: unsafe extern "C-unwind" fn(i32) =
                    unsafe { std::mem::transmute(address) };
                unsafe { callback(signum) };
            }
        });
        // Restoring the mask can synchronously dispatch a raise made by this
        // handler. Keep that nested work inside the callback owner's catch:
        // its EngineFault token must be consumed while this activation is
        // still current, then re-raised for the initiating Engine below.
        mask.finish(handler_ctx, host_mask);
    });
    match callback {
        Ok(()) => Ok(()),
        Err(exception) => {
            let fault = match exception.take_engine_fault(lease.shared()) {
                Ok(fault) => fault.finish(),
                Err(exception) => exception.resume_or_rethrow(),
            };
            // The caught frame already dropped both mask guards while
            // unwinding. Keep the callback owner's activation current until
            // its fault token has been consumed, then re-raise for the
            // initiating Engine below.
            drop(activation);
            drop(lease);
            Err(fault)
        }
    }
}

fn dispatch_signal_delivery_for_ctx(
    ctx: *mut Ctx,
    delivery: super::signal::SignalDeliveryGuard,
    signum: i32,
) {
    if let Err(fault) = dispatch_signal_delivery(delivery, signum) {
        super::unwind::raise_engine_fault(ctx, fault.message, fault.code);
    }
}

/// Complete target-pthread callbacks without entering the owner process inbox.
/// This path deliberately ignores `Ctx::signal_draining`: a real unblocked
/// `raise(3)` must finish the handler selected by the kernel before it returns,
/// including when called by an already deferred handler.
fn drain_current_thread_signal_deliveries(ctx: *mut Ctx) {
    let contexts = current_thread_contexts(ctx);
    loop {
        let blocked = current_thread_signal_mask(contexts);
        let Some((delivery, signum)) = super::signal::take_current_thread_delivery(blocked) else {
            return;
        };
        dispatch_signal_delivery_for_ctx(ctx, delivery, signum);
    }
}

/// Finish target-pthread work accepted before an owning execution boundary
/// reports an EngineFault. A handler fault may restore its mask and thereby
/// publish another delivery for this pthread. Consume each nested fault in its
/// callback owner's catch, keep draining, and report the last fault observed.
pub(crate) fn drain_current_thread_signal_deliveries_after_fault(
    ctx: *mut Ctx,
    mut fault: super::unwind::EngineFaultReport,
) -> super::unwind::EngineFaultReport {
    let contexts = current_thread_contexts(ctx);
    loop {
        let blocked = current_thread_signal_mask(contexts);
        let Some((delivery, signum)) = super::signal::take_current_thread_delivery(blocked) else {
            return fault;
        };
        if let Err(nested) = dispatch_signal_delivery(delivery, signum) {
            fault = nested;
        }
    }
}

fn real_raise_and_drain(ctx: *mut Ctx, signum: i32) -> i32 {
    loop {
        let attempt = super::signal::HostRaiseAttempt::begin(signum);
        let result = crate::os::process::raise(signum);
        let retry = attempt.finish();
        if result != 0 {
            return result;
        }
        if retry {
            match super::signal::host_raise_retry_is_stale(signum) {
                Ok(true) => super::unwind::raise_engine_fault(
                    ctx,
                    format!(
                        "HostRaise for signal {signum} reached a raw-restored closed MIRVM fixed stub"
                    ),
                    70,
                ),
                Ok(false) => {}
                Err(error) => super::unwind::raise_engine_fault(
                    ctx,
                    format!("failed to validate HostRaise retry for signal {signum}: {error}"),
                    70,
                ),
            }
            continue;
        }
        drain_current_thread_signal_deliveries(ctx);
        drain_pending_signals(ctx);
        return 0;
    }
}

/// Preserve libc `raise(3)` itself as the signal's linearization point. A
/// managed fixed stub records an SI_TKILL delivery in this pthread's mailbox;
/// the ordinary VM boundary below then runs that callback synchronously when
/// the signal was not blocked.
pub(crate) fn raise_signal(ctx: *mut Ctx, signum: i32) -> i32 {
    if signum > 0 && (signum as usize) < super::signal::SIGNAL_SLOTS {
        let contexts = current_thread_contexts(ctx);
        let bit = 1u64 << signum;
        if unsafe { (*contexts).close_signal_drain } && current_physical_signal_mask() & bit != 0 {
            return raise_from_close_drain(ctx, signum);
        }
    }
    real_raise_and_drain(ctx, signum)
}

impl Drop for SignalDrainGuard {
    fn drop(&mut self) {
        unsafe { (*self.ctx).signal_draining = false };
    }
}

/// Deliver registered signals in ordinary VM state. Each round first takes one traditional signal
/// pending set; new signals produced by the handler enter the next round. Rounds are bounded, and
/// remaining events are left for the next block entry or return safepoint.
pub(crate) fn drain_pending_signals(ctx: *mut Ctx) {
    drain_pending_signals_with_mode(ctx, false);
}

/// Close has already detached these registrations from the kernel. Events in
/// the owner inbox were accepted by an earlier fixed-stub frame, so a worker's
/// inherited pthread mask must not strand them. This changes only MIRVM's
/// choice of an inbox event; it never unblocks a kernel signal or mutates the
/// caller's real mask.
fn drain_pending_signals_for_close(ctx: *mut Ctx) {
    drain_pending_signals_with_mode(ctx, true);
}

fn drain_pending_signals_with_mode(ctx: *mut Ctx, closing: bool) {
    if unsafe { (*ctx).signal_draining } {
        return;
    }
    unsafe { (*ctx).signal_draining = true };
    let _draining = SignalDrainGuard { ctx };
    let contexts = current_thread_contexts(ctx);
    let _close_drain = CloseSignalDrainGuard::enter(contexts, closing);
    let shared = unsafe { (*ctx).shared_arc() };
    for _ in 0..SIGNAL_DRAIN_ROUNDS {
        let mut progressed = false;
        let blocked = if closing {
            unsafe { (*contexts).signal_mask }
        } else {
            current_thread_signal_mask(contexts)
        };
        if let Some((delivery, signum)) = super::signal::take_current_thread_delivery(blocked) {
            progressed = true;
            dispatch_signal_delivery_for_ctx(ctx, delivery, signum);
        }
        for signum in 1..super::signal::SIGNAL_SLOTS {
            if blocked & (1u64 << signum) != 0 {
                continue;
            }
            let Some(delivery) = shared.control.signal_inbox.take_delivery(signum) else {
                continue;
            };
            progressed = true;
            dispatch_signal_delivery_for_ctx(ctx, delivery, signum as i32);
        }
        if !progressed {
            break;
        }
    }
}

fn current_thread_contexts(ctx: *mut Ctx) -> *mut ThreadContexts {
    let Some(key) = CTX_KEY.get().copied() else {
        super::interp::engine_abort("signal delivery happened outside an Engine activation");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() || unsafe { (*contexts).current != ctx } {
        super::interp::engine_abort("signal delivery does not belong to the current Engine");
    }
    contexts
}

struct CtxSlot {
    ctx: Mutex<Option<Box<Ctx>>>,
}

impl CtxSlot {
    fn new(shared: &Arc<Shared>) -> Self {
        Self {
            ctx: Mutex::new(Some(Box::new(Ctx::new(shared)))),
        }
    }
}

struct ThreadContexts {
    by_engine: HashMap<usize, Arc<CtxSlot>>,
    current: *mut Ctx,
    /// Serial number for the current embedded entry. A new value is allocated on each `activate`
    /// and restored to the outer value on exit; therefore signal/thunk re-entries into the same
    /// Engine are strictly distinguished from the interrupted execution.
    current_activation: u64,
    next_activation: u64,
    /// Code domain of the innermost activation on this thread, saved and restored
    /// like `current_activation`. Dispatch reads it to pick the domain's slot set,
    /// so a guest -> native -> guest chain keeps the domain it entered from and
    /// never mixes slot sets mid-chain (design §5.2.3).
    domain: super::jit::CodeDomain,
    /// Engine ids for every active entry on this host thread. Looking only at
    /// `current` loses an outer Engine across A -> native -> B nesting, which
    /// can make `wait_closed(A)` deadlock on its own lease and can reject a
    /// legitimate A reentry while A is Closing.
    active_engines: Vec<u64>,
    /// Deferred handlers execute at VM safe points rather than in the kernel
    /// frame. Mirror the kernel's temporary standard-signal mask per host
    /// thread, including across nested calls into another Engine.
    signal_mask: u64,
    /// True only while a finalizer drains already accepted signal inbox work.
    /// A raise must not be left pending on that temporary pthread.
    close_signal_drain: bool,
    /// Engines whose close was requested under a deferred handler's temporary
    /// mask. Starting their worker here would make pthread inherit that mask.
    pending_finalizers: Vec<Arc<Shared>>,
    /// EngineFaults that have been raised on this thread and have not yet been
    /// consumed by their owning execution boundaries. A native catch may
    /// suspend one unwind and re-enter an Engine, so this is a token stack,
    /// not a thread-wide boolean.
    in_flight_faults: Vec<EngineFaultToken>,
    next_fault_nonce: u64,
    teardown_rounds: u8,
    final_tsd_cursor: Option<libc::pthread_key_t>,
    final_tsd_active: bool,
    /// Avoid touching telemetry from final TSD on threads that never entered a
    /// capture-capable Engine.
    telemetry_touched: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EngineFaultToken {
    owner_id: u64,
    raising_ctx: *mut Ctx,
    nonce: u64,
}

impl ThreadContexts {
    fn new() -> Self {
        Self {
            by_engine: HashMap::new(),
            current: std::ptr::null_mut(),
            current_activation: 0,
            next_activation: 1,
            // Plain until an activation says otherwise: the default must not
            // reserve a register or carry collection state.
            domain: super::jit::CodeDomain::Plain,
            active_engines: Vec::new(),
            signal_mask: 0,
            close_signal_drain: false,
            pending_finalizers: Vec::new(),
            in_flight_faults: Vec::new(),
            next_fault_nonce: 1,
            teardown_rounds: 0,
            final_tsd_cursor: None,
            final_tsd_active: false,
            telemetry_touched: false,
        }
    }

    fn attach(&mut self, shared: &Arc<Shared>) -> *mut Ctx {
        let key = shared.id as usize;
        let slot = match self.by_engine.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => Arc::clone(entry.get()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let slot = Arc::new(CtxSlot::new(shared));
                shared.register_ctx_slot(&slot);
                entry.insert(Arc::clone(&slot));
                slot
            }
        };
        let mut ctx = slot.ctx.lock().unwrap();
        let Some(ctx) = ctx.as_deref_mut() else {
            eprintln!("mirvm[m4-engine]: attempted to attach a finalized Engine context");
            std::process::abort();
        };
        let ptr = ctx as *mut Ctx;
        self.current = ptr;
        ptr
    }
}

/// This thread's stack safety floor: os::thread returns [lo, lo+size); the floor adds a safety
/// margin. The margin covers the worst-case host usage of a single interp_frame plus the deepest
/// FFI/unwind/diagnostic path; for small stacks use 1/8 so the margin does not consume the usable
/// area. Called only once per Ctx creation (getattr reads /proc for the main thread, not hot path).
fn thread_stack_floor() -> usize {
    let Some((lo, size)) = crate::os::thread::current_stack_bounds() else {
        return 0;
    };
    if lo == 0 {
        return 0;
    }
    let margin = (size / 8).clamp(256 << 10, 4 << 20);
    lo + margin
}

/// Ctx's pthread key (process-wide; dtor = ctx_key_dtor).
static CTX_KEY: OnceLock<crate::os::thread::TlsKey> = OnceLock::new();

#[thread_local]
static THREAD_CONTEXT_EXITING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn current_thread_is_in_final_tsd_pass(key: libc::pthread_key_t) -> bool {
    let Some(ctx_key) = CTX_KEY.get().copied() else {
        return false;
    };
    let contexts = unsafe { crate::os::thread::tls_get(ctx_key) } as *mut ThreadContexts;
    if contexts.is_null() {
        return false;
    }
    unsafe {
        (*contexts)
            .final_tsd_cursor
            .is_some_and(|cursor| (*contexts).final_tsd_active || key < cursor)
    }
}

/// Code domain of the innermost activation on this host thread. Dispatch uses it
/// to select the publish slots, which is the single point where a trace run stops
/// consulting plain entries (design §5.2.3). Plain is the answer outside any
/// activation, so the default path never reserves a register.
pub(crate) fn current_code_domain() -> super::jit::CodeDomain {
    let Some(ctx_key) = CTX_KEY.get().copied() else {
        return super::jit::CodeDomain::Plain;
    };
    let contexts = unsafe { crate::os::thread::tls_get(ctx_key) } as *mut ThreadContexts;
    if contexts.is_null() {
        return super::jit::CodeDomain::Plain;
    }
    unsafe { (*contexts).domain }
}

pub(crate) fn current_thread_final_tsd_pass_is_armed() -> bool {
    let Some(ctx_key) = CTX_KEY.get().copied() else {
        return false;
    };
    let contexts = unsafe { crate::os::thread::tls_get(ctx_key) } as *mut ThreadContexts;
    !contexts.is_null() && unsafe { (*contexts).final_tsd_cursor.is_some() }
}

#[cfg(test)]
pub(crate) fn test_ctx_key() -> libc::pthread_key_t {
    CTX_KEY.get().copied().unwrap().as_raw()
}

/// Fork guard baseline (M5.2 D8f): count of threads attributable to the guest when guest main
/// starts. At this moment = mirvm internal threads (main-in-join, guest-exec, allocator) plus any
/// MIRVM service thread already running (capture writer), and 0 guest-spawned threads.
/// **Use the real OS thread count, not the Ctx count**: after pthread_create returns the new thread
/// already exists, but its Ctx is not created until trampoline attach—Ctx counting has a TOCTOU
/// window and would miss it. MIRVM's own service threads are subtracted because the guest can never
/// have created them (design: mirvm_high_performance_log.md §5.5/§6.3).
/// Fork is allowed only when the current guest-attributable count equals the baseline.
/// Called at the guest main start point (run_main/run_export): pins the baseline for a single
/// guest thread.
pub fn set_fork_baseline(shared: &Shared) {
    shared
        .fork_baseline_threads
        .store(guest_thread_count(), std::sync::atomic::Ordering::SeqCst);
    shared.fork_baseline_pid.store(
        unsafe { libc::getpid() },
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// Threads the guest is accountable for: real OS threads minus MIRVM service threads, saturating so
/// an accounting error under-counts instead of wrapping. Returns 0 when `/proc/self/task` is
/// unreadable, which the guard already treats as "cannot judge".
pub(crate) fn guest_thread_count() -> usize {
    guest_threads_from(
        crate::os::thread::os_thread_count(),
        crate::os::thread::service_thread_count(),
    )
}

/// Pure form of the guest thread accounting, so the fork-guard arithmetic is
/// testable without depending on how many threads the whole test process
/// happens to have.
fn guest_threads_from(raw: usize, service: usize) -> usize {
    raw.saturating_sub(service)
}

/// Guest-attributable thread count for `shared`, repairing a baseline that a `fork` child inherited.
/// `fork` keeps only the calling thread, so the child's service-thread count is zero while the
/// inherited baseline still counted the parent's service threads. Without this repair a child that
/// forks again would compare 1 thread against the parent's inflated baseline and see a
/// multi-threaded guest. Both values are only written at pin points (guest main start) and here, so
/// the read/compare/write below is confined to the single-threaded child.
fn guest_thread_count_for(shared: &Shared) -> usize {
    let raw = crate::os::thread::os_thread_count();
    let service = crate::os::thread::service_thread_count();
    let pid = unsafe { libc::getpid() };
    if shared
        .fork_baseline_pid
        .load(std::sync::atomic::Ordering::SeqCst)
        != pid
    {
        let base = guest_threads_from(raw, service);
        shared
            .fork_baseline_threads
            .store(base, std::sync::atomic::Ordering::SeqCst);
        shared
            .fork_baseline_pid
            .store(pid, std::sync::atomic::Ordering::SeqCst);
        return base;
    }
    guest_threads_from(raw, service)
}

/// Whether the guest has spawned extra threads (HostFork guard): current guest-attributable thread
/// count > baseline means yes. If the baseline is unset (0) or reading failed, conservatively treat
/// it as "multi-threaded" (reject fork).
/// # Safety
///
/// `ctx` must be a `Ctx` whose host thread is still inside its active scope.
pub unsafe fn guest_spawned_threads(ctx: *mut Ctx) -> bool {
    let shared = unsafe { &*(*ctx).shared };
    let base = shared
        .fork_baseline_threads
        .load(std::sync::atomic::Ordering::SeqCst);
    base == 0 || guest_thread_count_for(shared) > base
}

/// Ctx teardown during the TSD phase: deferred for 3 rounds (re-hang → glibc appends rounds, max
/// 4)—guest pthread-key dtors (std run_dtors thunk, key order uncontrollable) can always execute
/// on a living Ctx; the final round actually destroys it (ByteRegion munmap, etc.).
fn block_thread_signals_for_exit() -> libc::sigset_t {
    let mut signals: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe { libc::sigemptyset(&mut signals) };
    for signum in 1..super::signal::SIGNAL_SLOTS as i32 {
        if matches!(signum, libc::SIGKILL | libc::SIGSTOP) {
            continue;
        }
        unsafe { libc::sigaddset(&mut signals, signum) };
    }
    let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
    let error = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signals, &mut previous) };
    if error != 0 {
        eprintln!("mirvm[m4-engine]: failed to block signals before pthread exit: {error}");
        std::process::abort();
    }
    previous
}

fn restore_thread_signal_mask_for_exit(mask: &libc::sigset_t) {
    let error = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
    if error != 0 {
        eprintln!(
            "mirvm[m4-engine]: failed to restore signals while draining pthread exit: {error}"
        );
        std::process::abort();
    }
}

fn drain_thread_signal_inbox_for_exit() {
    while let Some((delivery, signum)) = super::signal::take_current_thread_delivery(0) {
        if let Err(fault) = dispatch_signal_delivery(delivery, signum) {
            eprintln!(
                "mirvm[m4-engine]: target pthread signal handler faulted during thread exit: {}",
                fault.message
            );
            std::process::abort();
        }
    }
}

#[cfg(test)]
type ThreadExitInboxEmptyHook = Box<dyn FnOnce() + Send + 'static>;

#[cfg(test)]
static THREAD_EXIT_INBOX_EMPTY_HOOK: OnceLock<Mutex<Option<ThreadExitInboxEmptyHook>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn set_thread_exit_inbox_empty_hook(hook: ThreadExitInboxEmptyHook) {
    *THREAD_EXIT_INBOX_EMPTY_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = Some(hook);
}

#[cfg(test)]
fn run_thread_exit_inbox_empty_hook() {
    let hook = THREAD_EXIT_INBOX_EMPTY_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(not(sanitize = "thread"))]
unsafe extern "C" fn ctx_key_dtor(p: *mut std::ffi::c_void) {
    let contexts = p as *mut ThreadContexts;
    unsafe {
        if (*contexts).teardown_rounds < 3 {
            (*contexts).teardown_rounds += 1;
            let key = *CTX_KEY.get().unwrap();
            if (*contexts).teardown_rounds == 3 {
                // The next libc pass is globally the last one. Managed keys
                // below this raw slot will run before Ctx; keys above it are
                // completed here after Ctx receives control.
                (*contexts).final_tsd_cursor = Some(key.as_raw());
                THREAD_CONTEXT_EXITING.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            crate::os::thread::tls_set(key, p);
            return;
        }
        // `current` also remembers an attach-only context. An active call is
        // represented by the activation id and stack, which the guard clears.
        if (*contexts).current_activation != 0 || !(*contexts).active_engines.is_empty() {
            eprintln!("mirvm[m4-engine]: pthread exited inside an active Engine call");
            std::process::abort();
        }
        let key = *CTX_KEY.get().unwrap();
        (*contexts).final_tsd_active = true;
        (*contexts).final_tsd_cursor = Some(key.as_raw());
        // pthread clears a key before invoking its destructor. Reinstall this
        // process-lifetime context only while target-thread callbacks drain so
        // their ordinary Engine activations reuse the valid final-round state.
        crate::os::thread::tls_set(key, p);
        // Preserve the pthread's original mask while callbacks run. Each empty
        // observation is then checked again with every catchable traditional
        // signal blocked. Only that physical cutoff closes the inbox gate.
        let mut original_mask = block_thread_signals_for_exit();
        loop {
            restore_thread_signal_mask_for_exit(&original_mask);
            super::unwind::guard_native_teardown(drain_thread_signal_inbox_for_exit);
            #[cfg(test)]
            run_thread_exit_inbox_empty_hook();
            let cursor = (*contexts)
                .final_tsd_cursor
                .expect("pthread final TSD cursor disappeared");
            if let Some(callback) = super::deferred::take_next_current_thread_tsd_for_exit(cursor) {
                (*contexts).final_tsd_cursor = Some(callback.key());
                super::unwind::guard_native_teardown(|| callback.invoke());
                continue;
            }
            original_mask = block_thread_signals_for_exit();
            if !super::signal::current_thread_has_pending() {
                super::signal::deactivate_current_thread_inbox();
                break;
            }
        }
        super::deferred::abandon_current_thread_tsd_for_exit();
        if (*contexts).telemetry_touched {
            crate::telemetry::capture::retire_current_thread();
        }
        crate::os::thread::tls_set(key, std::ptr::null_mut());
        drop(Box::from_raw(contexts));
    }
}

/// Boundary TLS attach: creates the Ctx on this thread's first entry into the engine (birth point
/// of a new guest thread's execution state), then idempotently returns the same instance on later
/// entries—re-entries (guest→native→thunk→guest) naturally get the same vmctx, and the operand
/// region continues nesting on the disciplined stack (spike2 shape).
///
/// Returns a raw pointer (Box pins the address; raw-ptr vmctx is passed across native stacks—
/// borrow discipline §9). The main thread's Ctx is reclaimed together with process exit (glibc exit
/// does not go through the TSD phase, same as native).
// Used by the separately compiled TSan harness; product entries go through
// `activate`, which also establishes an activation identity.
#[cfg_attr(not(test), allow(dead_code))]
pub fn attach(shared: &Arc<Shared>) -> *mut Ctx {
    super::signal::initialize_current_thread_inbox();
    let key = *CTX_KEY.get_or_init(|| {
        // TSan config: do not register a dtor—TSan's thread state is destroyed before the TSD
        // phase, so instrumented code must not run then (Ctx leaks per thread, test-only config;
        // the dtor chain is validated by the threads_panic differential test in real config).
        #[cfg(sanitize = "thread")]
        let dtor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)> = None;
        #[cfg(not(sanitize = "thread"))]
        let dtor = Some(ctx_key_dtor as unsafe extern "C" fn(*mut std::ffi::c_void));
        crate::os::thread::tls_key_create(dtor)
    });
    unsafe {
        let p = crate::os::thread::tls_get(key);
        let contexts = if p.is_null() {
            if THREAD_CONTEXT_EXITING.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("mirvm[m4-engine]: Engine callback entered after pthread teardown");
                std::process::abort();
            }
            let contexts = Box::into_raw(Box::new(ThreadContexts::new()));
            crate::os::thread::tls_set(key, contexts as *mut std::ffi::c_void);
            contexts
        } else {
            p as *mut ThreadContexts
        };
        (*contexts).attach(shared)
    }
}

pub fn current() -> *mut Ctx {
    let Some(key) = CTX_KEY.get().copied() else {
        panic!("JIT helper called before Engine activation");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    assert!(
        !contexts.is_null(),
        "JIT helper called before thread attach"
    );
    let ctx = unsafe { (*contexts).current };
    assert!(
        !ctx.is_null(),
        "JIT helper called outside Engine activation scope"
    );
    ctx
}

pub(crate) fn current_thread_has_engine(id: u64) -> bool {
    let Some(key) = CTX_KEY.get().copied() else {
        return false;
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() {
        return false;
    }
    unsafe { (*contexts).active_engines.contains(&id) }
}

fn current_thread_has_pending_finalizer(id: u64) -> bool {
    let Some(key) = CTX_KEY.get().copied() else {
        return false;
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() {
        return false;
    }
    unsafe {
        (*contexts)
            .pending_finalizers
            .iter()
            .any(|shared| shared.control.id() == id)
    }
}

fn current_thread_signal_mask(contexts: *mut ThreadContexts) -> u64 {
    unsafe { (*contexts).signal_mask | current_physical_signal_mask() }
}

fn current_physical_signal_mask() -> u64 {
    crate::os::signal::Sigaction::current_standard_mask_bits().unwrap_or_else(|error| {
        super::interp::engine_abort(&format!(
            "failed to query the host thread signal mask: {error}"
        ))
    })
}

fn defer_finalizer_while_signal_masked(shared: &Arc<Shared>) -> bool {
    let Some(key) = CTX_KEY.get().copied() else {
        return false;
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() || unsafe { (*contexts).signal_mask } == 0 {
        return false;
    }
    let pending = unsafe { &mut (*contexts).pending_finalizers };
    if pending.try_reserve(1).is_err() {
        eprintln!("mirvm[m4-engine]: failed to queue a masked Engine finalizer");
        std::process::abort();
    }
    pending.push(Arc::clone(shared));
    true
}

/// Called only after the outermost deferred handler restored the real pthread
/// mask. A nested handler leaves the queue for its outer caller.
fn start_pending_signal_finalizers(contexts: *mut ThreadContexts) {
    if unsafe { (*contexts).signal_mask } != 0 {
        return;
    }
    let pending = std::mem::take(unsafe { &mut (*contexts).pending_finalizers });
    for shared in pending {
        start_finalizer(shared);
    }
}

pub(crate) fn control_for_engine(id: u64) -> Option<Arc<EngineControl>> {
    ENGINES
        .read()
        .unwrap()
        .get(&id)
        .and_then(std::sync::Weak::upgrade)
        .map(|shared| Arc::clone(shared.control()))
}

/// Start an EngineFault on this host thread.
///
/// The returned token is stored in the MIRVM exception and must come back to
/// [`finish_engine_fault`] at the matching Engine boundary. A native catch may
/// suspend this exception and re-enter an Engine; each nested fault therefore
/// receives an independent token.
pub(crate) fn begin_engine_fault(ctx: *mut Ctx) -> (Arc<Shared>, EngineFaultToken) {
    if ctx.is_null() {
        eprintln!("mirvm[m4-engine]: EngineFault started without an active Engine context");
        std::process::abort();
    }
    let Some(key) = CTX_KEY.get().copied() else {
        eprintln!("mirvm[m4-engine]: EngineFault started before thread context initialization");
        std::process::abort();
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() || unsafe { (*contexts).current } != ctx {
        eprintln!("mirvm[m4-engine]: EngineFault started outside its active Engine context");
        std::process::abort();
    }
    let owner = unsafe { (*ctx).shared_arc() };
    let nonce = unsafe { (*contexts).next_fault_nonce };
    if nonce == 0 {
        eprintln!("mirvm[m4-engine]: EngineFault token counter exhausted");
        std::process::abort();
    }
    let token = EngineFaultToken {
        owner_id: owner.id,
        raising_ctx: ctx,
        nonce,
    };
    unsafe {
        (*contexts).next_fault_nonce = nonce.wrapping_add(1);
        (*contexts).in_flight_faults.push(token);
    }
    (owner, token)
}

/// Whether this test thread owns any EngineFault that has not been consumed.
#[cfg(test)]
pub(crate) fn engine_fault_in_flight() -> bool {
    let Some(key) = CTX_KEY.get().copied() else {
        return false;
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    !contexts.is_null() && unsafe { !(*contexts).in_flight_faults.is_empty() }
}

/// Consume an EngineFault at its owning execution boundary.
pub(crate) fn finish_engine_fault(token: EngineFaultToken) {
    let Some(key) = CTX_KEY.get().copied() else {
        eprintln!("mirvm[m4-engine]: EngineFault finished after thread context teardown");
        std::process::abort();
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() || unsafe { (*contexts).in_flight_faults.last().copied() } != Some(token)
    {
        eprintln!("mirvm[m4-engine]: EngineFault was consumed by a non-owning Engine boundary");
        std::process::abort();
    }
    let current = unsafe { (*contexts).current };
    if current.is_null()
        || unsafe { (*current).shared().id } != token.owner_id
        || token.raising_ctx.is_null()
        || unsafe { (*token.raising_ctx).shared().id } != token.owner_id
    {
        eprintln!("mirvm[m4-engine]: EngineFault owner context no longer matches its exception");
        std::process::abort();
    }

    unsafe {
        (*contexts).in_flight_faults.pop();
    }
    if unsafe { (*contexts).signal_mask == 0 && (*contexts).in_flight_faults.is_empty() }
        && !std::thread::panicking()
    {
        // The handler activation may already have unwound before an embedding
        // boundary consumes the EngineFault, so there need not be another
        // ActivationGuard drop to release work queued under its signal mask.
        start_pending_signal_finalizers(contexts);
    }
}

pub struct ActivationGuard {
    contexts: *mut ThreadContexts,
    previous: *mut Ctx,
    previous_activation: u64,
    /// Domain of the activation this one nests inside, restored on exit so a
    /// native callback returning to an outer guest chain resumes that chain's
    /// domain rather than the callee's.
    previous_domain: super::jit::CodeDomain,
    previous_signal_owner: u64,
    ctx: *mut Ctx,
    engine_id: u64,
    activation: u64,
    telemetry: Option<crate::telemetry::capture::ActivationToken>,
}

impl ActivationGuard {
    pub fn ctx(&self) -> *mut Ctx {
        self.ctx
    }
}

impl Drop for ActivationGuard {
    fn drop(&mut self) {
        let shared = unsafe { (*self.ctx).shared_arc() };
        let drain_deferred;
        let start_signal_finalizers;
        unsafe {
            if (*self.contexts).current != self.ctx
                || (*self.contexts).current_activation != self.activation
                || (*self.contexts).active_engines.last().copied() != Some(self.engine_id)
            {
                eprintln!("mirvm[m4-engine]: Engine activation exited out of order");
                std::process::abort();
            }
            super::signal::restore_owner(self.previous_signal_owner);
            (*self.contexts).active_engines.pop();
            (*self.contexts).current = self.previous;
            (*self.contexts).current_activation = self.previous_activation;
            (*self.contexts).domain = self.previous_domain;
            drain_deferred = shared.control().is_closing()
                && !(*self.contexts).active_engines.contains(&self.engine_id);
            // An EngineFault unwinds the handler activation before its outer
            // boundary consumes the fault token. Only that ordinary boundary,
            // after the token is gone, may start work queued under the mask.
            start_signal_finalizers =
                (*self.contexts).signal_mask == 0 && (*self.contexts).in_flight_faults.is_empty();
        }
        if let Some(token) = self.telemetry {
            let restored_engine_id = if self.previous.is_null() {
                0
            } else {
                unsafe { (*self.previous).shared().id }
            };
            crate::telemetry::capture::activation_exit(token, restored_engine_id);
        }
        if drain_deferred && !std::thread::panicking() {
            super::deferred::drain_current_thread(&shared);
        }
        if start_signal_finalizers && !std::thread::panicking() {
            start_pending_signal_finalizers(self.contexts);
        }
    }
}

fn current_activation(ctx: *mut Ctx) -> u64 {
    let Some(key) = CTX_KEY.get().copied() else {
        super::interp::engine_abort("main panic catch happened outside Engine activation");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null()
        || unsafe { (*contexts).current != ctx || (*contexts).current_activation == 0 }
    {
        super::interp::engine_abort(
            "main panic catch does not belong to the current Engine activation",
        );
    }
    unsafe { (*contexts).current_activation }
}

pub fn activate(shared: &Arc<Shared>) -> ActivationGuard {
    super::signal::initialize_current_thread_inbox();
    let key = *CTX_KEY.get_or_init(|| {
        #[cfg(sanitize = "thread")]
        let dtor: Option<unsafe extern "C" fn(*mut std::ffi::c_void)> = None;
        #[cfg(not(sanitize = "thread"))]
        let dtor = Some(ctx_key_dtor as unsafe extern "C" fn(*mut std::ffi::c_void));
        crate::os::thread::tls_key_create(dtor)
    });
    unsafe {
        let p = crate::os::thread::tls_get(key);
        let contexts = if p.is_null() {
            if THREAD_CONTEXT_EXITING.load(std::sync::atomic::Ordering::Relaxed) {
                eprintln!("mirvm[m4-engine]: Engine callback entered after pthread teardown");
                std::process::abort();
            }
            let contexts = Box::into_raw(Box::new(ThreadContexts::new()));
            crate::os::thread::tls_set(key, contexts as *mut std::ffi::c_void);
            contexts
        } else {
            p as *mut ThreadContexts
        };
        if (*contexts).active_engines.try_reserve(1).is_err() {
            eprintln!("mirvm[m4-engine]: failed to grow the Engine activation stack");
            std::process::abort();
        }
        let previous = (*contexts).current;
        let previous_activation = (*contexts).current_activation;
        let previous_domain = (*contexts).domain;
        let activation = (*contexts).next_activation;
        if activation == 0 {
            eprintln!("mirvm[m4-engine]: Engine activation counter exhausted");
            std::process::abort();
        }
        (*contexts).next_activation = activation.wrapping_add(1);
        let ctx = (*contexts).attach(shared);
        (*contexts).current_activation = activation;
        (*contexts).active_engines.push(shared.id);
        let previous_signal_owner = super::signal::activate_owner(shared.id);
        // The code domain of this activation is the Engine's frozen choice. It is
        // installed for the duration of the activation so dispatch reads one
        // value; a nested activation saves and restores it, which keeps a
        // guest -> native -> guest chain in the domain it entered from.
        let domain = shared.domain;
        (*contexts).domain = domain;
        let telemetry = if domain == super::jit::CodeDomain::Trace {
            (*contexts).telemetry_touched = true;
            Some(crate::telemetry::capture::activation_enter(shared.id))
        } else {
            None
        };
        ActivationGuard {
            contexts,
            previous,
            previous_activation,
            previous_domain,
            previous_signal_owner,
            ctx,
            engine_id: shared.id,
            activation,
            telemetry,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Weak, mpsc};

    use super::{Engine, Shared, attach};
    use crate::vm::engine::ir::{Block, FuncBody, Module, RetAbi, Terminator};

    fn context_export_module() -> Module {
        let mut module = Module {
            funcs: vec![FuncBody {
                frame_size: 0,
                frame_align: 1,
                ret: RetAbi::Zst,
                params: Vec::new(),
                caller_loc_off: None,
                blocks: vec![Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                }],
                name: "context_export".into(),
            }]
            .into(),
            ..Module::default()
        };
        module.exports.insert("context_export".into(), 0);
        module
    }

    #[test]
    fn one_host_thread_keeps_distinct_contexts_for_distinct_engines() {
        let first = Arc::new(Shared::new(Module::default()));
        let second = Arc::new(Shared::new(Module::default()));

        let first_ctx = attach(&first);
        let second_ctx = attach(&second);

        assert_ne!(first_ctx, second_ctx);
        assert_eq!(unsafe { (*first_ctx).shared }, Arc::as_ptr(&first));
        assert_eq!(unsafe { (*second_ctx).shared }, Arc::as_ptr(&second));
    }

    #[test]
    fn first_attach_prunes_slots_from_threads_that_already_exited() {
        let shared = Arc::new(Shared::new(Module::default()));
        for _ in 0..2 {
            let worker_shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _ = attach(&worker_shared);
            })
            .join()
            .unwrap();
            let slots = shared.ctx_slots.lock().unwrap();
            assert_eq!(slots.len(), 1);
            assert_eq!(slots[0].strong_count(), 0);
        }

        let _ = attach(&shared);
        let slots = shared.ctx_slots.lock().unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].strong_count(), 1);
    }

    #[test]
    fn closing_releases_context_owned_by_a_long_lived_thread() {
        let engine = Engine::new(Shared::new(context_export_module()));
        let weak = Arc::downgrade(engine.shared());
        let (attached_tx, attached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_engine = engine.clone();
        let worker = std::thread::spawn(move || {
            let outcome =
                unsafe { super::super::interp::run_export(&worker_engine, "context_export", &[]) }
                    .unwrap();
            assert!(matches!(
                outcome,
                super::super::interp::RunOutcome::Returned(_)
            ));
            drop(worker_engine);
            attached_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        attached_rx.recv().unwrap();
        engine.wait_closed().unwrap();
        drop(engine);
        assert!(
            weak.upgrade().is_none(),
            "an inactive worker Ctx retained Shared after Engine close"
        );

        release_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn nested_engine_activation_restores_the_outer_context() {
        let first = Arc::new(Shared::new(Module::default()));
        let second = Arc::new(Shared::new(Module::default()));
        let outer = super::activate(&first);
        assert_eq!(super::current(), outer.ctx());
        assert!(super::current_thread_has_engine(first.id));
        {
            let inner = super::activate(&second);
            assert_eq!(super::current(), inner.ctx());
            assert!(super::current_thread_has_engine(first.id));
            assert!(super::current_thread_has_engine(second.id));
        }
        assert_eq!(super::current(), outer.ctx());
        assert!(super::current_thread_has_engine(first.id));
        assert!(!super::current_thread_has_engine(second.id));
    }

    #[test]
    fn closing_reentry_and_wait_see_an_outer_engine_activation() {
        let first = Engine::new(Shared::new(Module::default()));
        let second = Engine::new(Shared::new(Module::default()));
        let outer_lease = first.execution_lease().unwrap();
        let outer = super::activate(first.shared());
        let inner = super::activate(second.shared());

        first.close();
        assert_eq!(first.state(), super::EngineState::Closing);
        assert_eq!(
            first.wait_closed(),
            Err(super::WaitClosedError::ActiveOnCurrentThread)
        );
        let reentry = super::ExecutionLease::for_thunk(first.control())
            .expect("an outer active Engine may re-enter while Closing");

        drop(reentry);
        drop(inner);
        drop(outer);
        drop(outer_lease);
        first.wait_closed().unwrap();
        second.wait_closed().unwrap();
    }

    #[test]
    fn nested_main_runs_keep_panic_classification_separate() {
        let shared = Arc::new(Shared::new(Module::default()));
        let activation = super::activate(&shared);
        let ctx = activation.ctx();
        let outer = super::begin_main_run(ctx);

        let inner = super::begin_main_run(ctx);
        super::call_main_panic_boundary(ctx, || {
            let mut catch = super::claim_main_panic_catch(
                ctx,
                crate::vm::engine::ir::BuiltinCallRole::MainPanicCatcher,
            )
            .unwrap();
            assert!(
                super::claim_main_panic_catch(
                    ctx,
                    crate::vm::engine::ir::BuiltinCallRole::MainPanicCatcher,
                )
                .is_none()
            );
            catch.mark_panicked();
        });
        assert!(inner.finish());

        super::call_main_panic_boundary(ctx, || {
            let _catch = super::claim_main_panic_catch(
                ctx,
                crate::vm::engine::ir::BuiltinCallRole::MainPanicCatcher,
            )
            .unwrap();
        });
        assert!(!outer.finish());
    }

    #[test]
    fn main_catcher_requires_exact_role_and_same_activation() {
        use crate::vm::engine::ir::BuiltinCallRole;

        let shared = Arc::new(Shared::new(Module::default()));
        let activation = super::activate(&shared);
        let ctx = activation.ctx();
        let run = super::begin_main_run(ctx);
        super::call_main_panic_boundary(ctx, || {
            assert!(super::claim_main_panic_catch(ctx, BuiltinCallRole::Normal).is_none());
            {
                let reentrant = super::activate(&shared);
                assert!(
                    super::claim_main_panic_catch(
                        reentrant.ctx(),
                        BuiltinCallRole::MainPanicCatcher,
                    )
                    .is_none()
                );
            }
            let _catch =
                super::claim_main_panic_catch(ctx, BuiltinCallRole::MainPanicCatcher).unwrap();
        });
        assert!(!run.finish());
    }

    #[test]
    fn engine_unregisters_and_releases_shared_state_on_drop() {
        let (id, weak): (u64, Weak<Shared>) = {
            let mut shared = Shared::new(Module::default());
            shared.jit.enabled = true;
            let engine = Engine::new(shared);
            let id = engine.shared().id;
            assert!(super::engine(id).is_some());
            super::super::interp::seed_engine_state_for_test(id);
            assert!(super::super::interp::has_engine_state_for_test(id));
            (id, Arc::downgrade(engine.shared()))
        };
        assert!(super::engine(id).is_none());
        assert!(!super::super::interp::has_engine_state_for_test(id));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn simultaneous_idle_close_has_one_synchronous_finalizer() {
        for _ in 0..256 {
            let engine = Engine::new(Shared::new(Module::default()));
            let weak = Arc::downgrade(engine.shared());
            let start = Arc::new(Barrier::new(3));

            let first_engine = engine.clone();
            let first_start = Arc::clone(&start);
            let first = std::thread::spawn(move || {
                first_start.wait();
                first_engine.close();
            });
            let second_engine = engine.clone();
            let second_start = Arc::clone(&start);
            let second = std::thread::spawn(move || {
                second_start.wait();
                second_engine.close();
            });

            start.wait();
            first.join().expect("first close thread panicked");
            second.join().expect("second close thread panicked");
            engine.wait_closed().unwrap();
            drop(engine);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn finalizing_transition_keeps_an_idle_lifecycle_permit() {
        let control = super::EngineControl::new(u64::MAX);
        control
            .lifecycle
            .store(super::PHASE_CLOSING, std::sync::atomic::Ordering::Release);

        assert!(control.begin_finalizer_permit());
        assert_eq!(
            control.lifecycle.load(std::sync::atomic::Ordering::Acquire),
            super::PHASE_CLOSING | 1
        );
        assert_eq!(
            control
                .active_executions
                .load(std::sync::atomic::Ordering::Acquire),
            0,
            "the close permit must not hide an idle Engine from TSD cleanup"
        );
        assert!(control.begin_finalizing_with_permit());
        assert_eq!(
            control.lifecycle.load(std::sync::atomic::Ordering::Acquire),
            super::PHASE_FINALIZING | 1
        );
        assert!(!control.begin_active_execution(true));

        control.finish_execution();
        assert_eq!(
            control.lifecycle.load(std::sync::atomic::Ordering::Acquire),
            super::PHASE_FINALIZING
        );
    }

    #[test]
    fn failed_finalizer_execution_rolls_back_the_active_count() {
        let control = super::EngineControl::new(u64::MAX - 1);
        control.lifecycle.store(
            super::PHASE_CLOSING | 1,
            std::sync::atomic::Ordering::Release,
        );

        assert!(!control.begin_finalizer_execution());
        assert_eq!(
            control
                .active_executions
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
    }

    /// L3: the code domain is frozen when an Engine is created, and arming a
    /// session later must NOT migrate it. That is the design's confirmed first
    /// version (§5.2.3): plain activations keep running plain, and only threads
    /// entering guest after arming pick up trace. Getting this backwards would
    /// silently move a running guest into a domain whose bodies expect recorder
    /// state the running frame does not have.
    #[test]
    fn code_domain_is_frozen_at_engine_creation() {
        use crate::vm::engine::jit::CodeDomain;

        // No session armed here, so this Engine is born plain and stays plain.
        let plain = Shared::new(context_export_module());
        assert_eq!(plain.domain, CodeDomain::Plain);
        assert_eq!(plain.domain, CodeDomain::Plain);

        let path = std::env::temp_dir().join(format!(
            "mirvm-domain-{}-{}.mlog",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let session = crate::telemetry::capture::CaptureSession::start(
            crate::telemetry::capture::StartOptions::new(
                &path,
                crate::telemetry::capture::STARTER_BYTES,
            ),
        );
        let Ok(session) = session else {
            // Another parallel test owns the process-wide session; the frozen
            // domain of an already-created Engine is what matters and is asserted
            // above, so skip the arming half rather than fight the global.
            return;
        };
        // Arming after creation cannot migrate the Engine that already exists.
        assert_eq!(
            plain.domain,
            CodeDomain::Plain,
            "an Engine created before arming must not migrate to trace"
        );

        // An Engine created while the session is armed is trace for its lifetime.
        let trace = Shared::new(context_export_module());
        assert_eq!(trace.domain, CodeDomain::Trace);
        assert_eq!(trace.domain, CodeDomain::Trace);
        drop(session);
        std::fs::remove_file(&path).ok();
    }

    /// L2 prerequisite: MIRVM's own service threads are invisible to the fork
    /// guard. A baseline pinned while one is live must stay valid after it
    /// exits, and a baseline inherited across a pid change must be repaired
    /// instead of rejecting the child's later forks.
    ///
    /// Assertions are deliberately independent of the absolute OS thread count:
    /// `cargo test` runs tests in parallel threads, so only differences within
    /// one thread of control and self-consistency between the two functions are
    /// meaningful.
    #[test]
    fn service_threads_are_excluded_from_the_guest_fork_baseline() {
        use crate::os::thread::ServiceThreadGuard;

        // The accounting itself, independent of the process thread count.
        assert_eq!(super::guest_threads_from(5, 0), 5);
        assert_eq!(super::guest_threads_from(5, 2), 3);
        assert_eq!(
            super::guest_threads_from(1, 4),
            0,
            "an over-counted service total must saturate, never wrap"
        );

        // The fork guard's subtraction must be the only consumer of the service
        // count; nothing here asserts an absolute value because other tests run
        // capture writers (hence service threads) in parallel.
        let service = ServiceThreadGuard::register();

        // A baseline pinned with a service thread live stays pinned afterwards,
        // so the writer exiting cannot retroactively reject a fork.
        let shared = Shared::new(context_export_module());
        super::set_fork_baseline(&shared);
        let pinned = shared
            .fork_baseline_threads
            .load(std::sync::atomic::Ordering::SeqCst);
        drop(service);
        assert_eq!(
            shared
                .fork_baseline_threads
                .load(std::sync::atomic::Ordering::SeqCst),
            pinned,
            "service thread exit must not invalidate the pinned baseline"
        );

        shared
            .fork_baseline_pid
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let repaired = super::guest_thread_count_for(&shared);
        let stamped = shared
            .fork_baseline_pid
            .load(std::sync::atomic::Ordering::SeqCst);
        // Compare the stamp against the repair's own result rather than a second
        // `getpid()`: a parallel test may `fork`, which would change the answer
        // between the two reads.
        assert_ne!(stamped, 1, "the repair must re-stamp the current pid");
        assert_eq!(
            repaired,
            shared
                .fork_baseline_threads
                .load(std::sync::atomic::Ordering::SeqCst),
            "the repair must pin what it returned"
        );
    }
}
