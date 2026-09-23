//! Engine lifecycle: `Shared` (read-only after publication), the process-lifetime
//! `EngineControl` tombstone, and close/finalize.
//!
//! `EngineControl` outlives `Shared`: handles and native callbacks keep it alive, so an escaped
//! callback retains only a small tombstone after close, never the Module.

use std::collections::HashMap;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Condvar, Mutex, Weak};

use super::super::ir::Module;
use super::activation::activate;
use super::signals::{
    CloseSignalDrainGuard, defer_finalizer_while_signal_masked,
    drain_current_thread_signal_deliveries_after_fault, drain_pending_signals_for_close,
};
use super::thread_ctx::{CTX_KEY, CtxSlot, ThreadContexts};

/// Read-only after publication: built by `ctx::load` during the load phase, then read lock-free by
/// every thread during execution. `thunks` is the one exception -- its entries are materialized on
/// demand during execution, so that cache carries its own `Mutex`.
pub struct Shared {
    pub id: u64,
    pub module: Module,
    /// The Engine's loaded instance: the frozen mapping, the address tables, the stub arenas and the
    /// native images. Process state lives here rather than in the artifact, which stays a
    /// self-contained description.
    pub instance: super::super::instance::Instance,
    /// The Engine's symbol carriers, built from the artifact when it is loaded: process state
    /// lives here rather than in the artifact, which stays a self-contained description.
    pub symbols: super::super::backtrace::Symbols,
    pub thunks: super::super::thunks::ThunkCache,
    /// Code domain this Engine's activations use. Chosen once,
    /// when the Engine is created, and then held for every activation of this
    /// Engine: guest calls native and native callbacks back into guest stay in
    /// the chain they entered from, so a running activation never migrates.
    /// Plain must stay the default and must not reserve a register or carry
    /// collection state.
    pub(crate) domain: super::super::jit::CodeDomain,
    /// JIT tiering state: PLT slots and call counters over the merged FuncId space.
    /// Compiler threads are the only writers, publishing a slot with a single
    /// atomic swap; readers keep the read-only-after-publication discipline.
    pub jit: super::super::jit::JitState,
    pub(crate) control: Arc<EngineControl>,
    /// Every host thread owns its slot; Shared keeps only weak discovery links
    /// so an exited thread cannot form a cycle. Finalization clears the live
    /// slots after the execution count reaches zero.
    pub(super) ctx_slots: Mutex<Vec<Weak<CtxSlot>>>,
    pub(crate) fork_baseline_threads: std::sync::atomic::AtomicUsize,
    /// Process id that pinned `fork_baseline_threads`. A `fork` child keeps the
    /// value but not the parent's service threads, so a mismatching pid marks a
    /// baseline that must be recomputed before the guard can trust it.
    pub(crate) fork_baseline_pid: std::sync::atomic::AtomicI32,
}

impl Shared {
    pub(crate) fn control(&self) -> &Arc<EngineControl> {
        &self.control
    }

    pub(super) fn register_ctx_slot(&self, slot: &Arc<CtxSlot>) {
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
pub(crate) const PHASE_RUNNING: usize = 0 << PHASE_SHIFT;
pub(crate) const PHASE_CLOSING: usize = 1 << PHASE_SHIFT;
pub(crate) const PHASE_FINALIZING: usize = 2 << PHASE_SHIFT;
const PHASE_CLOSED: usize = 3 << PHASE_SHIFT;

/// Small process-lifetime tombstone shared by handles and native callbacks.
/// Its self-owner keeps Shared alive until the last execution lease leaves,
/// then finalization breaks the cycle. Escaped callbacks retain only this
/// small tombstone after close, not the Module.
pub(crate) struct EngineControl {
    pub(crate) id: u64,
    pub(crate) lifecycle: std::sync::atomic::AtomicUsize,
    pub(crate) active_executions: std::sync::atomic::AtomicUsize,
    finalizer_started: std::sync::atomic::AtomicBool,
    pub(crate) shared: std::sync::atomic::AtomicPtr<Shared>,
    owner: Mutex<Option<Arc<Shared>>>,
    wait_lock: Mutex<()>,
    closed: Condvar,
    pub(crate) signal_inbox: super::super::signal::SignalInbox,
}

impl EngineControl {
    pub(crate) fn new(id: u64) -> Self {
        Self {
            id,
            lifecycle: std::sync::atomic::AtomicUsize::new(PHASE_RUNNING),
            active_executions: std::sync::atomic::AtomicUsize::new(0),
            finalizer_started: std::sync::atomic::AtomicBool::new(false),
            shared: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            owner: Mutex::new(None),
            wait_lock: Mutex::new(()),
            closed: Condvar::new(),
            signal_inbox: super::super::signal::SignalInbox::new(),
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

    pub(crate) fn begin_execution(
        &self,
        allow_closing_reentry: bool,
        allow_finalizing: bool,
    ) -> bool {
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

    pub(super) fn begin_active_execution(&self, allow_closing_reentry: bool) -> bool {
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
            super::super::deferred::executions_idle(self);
        }
        false
    }

    pub(super) fn begin_finalizer_execution(&self) -> bool {
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
            super::super::deferred::executions_idle(self);
        }
        false
    }

    pub(super) fn begin_finalizer_permit(&self) -> bool {
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

    pub(crate) fn finish_execution(&self) {
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
            if super::super::signal::current_thread_has_pending_for_engine(self.id) {
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
    pub(crate) shared: Arc<Shared>,
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
        super::super::native_instance::isolate_required_libraries(&mut shared.module, shared.id)?;
        let mut shared = Arc::new(shared);
        let control = Arc::clone(shared.control());
        let entry_closures = {
            let shared =
                Arc::get_mut(&mut shared).ok_or("new Engine Shared unexpectedly aliased")?;
            let closures = super::super::thunks::materialize_all_entry_stubs(
                &mut shared.module,
                &mut shared.instance,
                &control,
            )?;
            super::super::native_instance::prepare_required_libraries(
                &shared.module,
                &mut shared.instance,
            )?;
            super::super::native_instance::patch_entry_slots(&shared.module, &shared.instance)?;
            super::super::native_instance::patch_pthread_slots(&shared.instance, control.id())?;
            super::super::ffi::resolve_got_fixups(&shared.module, &shared.instance)?;
            closures
        };
        ENGINES
            .write()
            .unwrap()
            .insert(shared.id, Arc::downgrade(&shared));
        control.install_owner(Arc::clone(&shared));
        #[cfg(feature = "cranelift")]
        super::super::jit::start(&shared);
        super::super::native_instance::commit_images(&shared.instance, &control);
        entry_closures.commit();

        enum StartupFailure {
            Error(String),
            Resume(super::super::unwind::CaughtException),
        }
        let startup = ExecutionLease::acquire(Arc::clone(&shared), false)
            .map_err(|_| "Engine closed before native constructors started".to_string())?;
        let startup_failure = {
            let activation = activate(startup.shared());
            let failure = match super::super::unwind::catch_raw(|| {
                super::super::native_instance::run_initializers(&shared.instance)
            }) {
                Ok(Ok(())) if shared.control.is_running() => None,
                Ok(Ok(())) => Some(StartupFailure::Error(
                    "native constructor closed its Engine".to_string(),
                )),
                Ok(Err(error)) => Some(StartupFailure::Error(error)),
                Err(exception) => Some(match exception.take_mirvm(&shared) {
                    Ok(super::super::unwind::MirvmPayload::Guest(payload)) => {
                        super::super::unwind::dispose_guest_panic_during_startup(&shared, payload);
                        StartupFailure::Error("native constructor raised a guest panic".into())
                    }
                    Ok(super::super::unwind::MirvmPayload::EngineFault(fault)) => {
                        let fault = drain_current_thread_signal_deliveries_after_fault(
                            activation.ctx(),
                            fault.finish(),
                        );
                        StartupFailure::Error(format!(
                            "native constructor hit an Engine fault: {}",
                            fault.message
                        ))
                    }
                    Ok(super::super::unwind::MirvmPayload::EngineClosed) => {
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
        let instance = super::super::instance::Instance::materialize(&module)?;
        unsafe { Self::from_artifact_unchecked(module, instance) }
    }

    /// Construct an Engine from a Module and the instance the load phase built for it.
    ///
    /// # Safety
    ///
    /// The same obligation as [`Engine::from_module_unchecked`]: every embedded native address,
    /// memory operand and ABI description must be valid for this process.
    pub(crate) unsafe fn from_artifact_unchecked(
        module: Module,
        instance: super::super::instance::Instance,
    ) -> Result<Self, String> {
        // Structural verification is still mandatory. `unsafe` covers only
        // facts it cannot prove: validity/lifetime of embedded host pointers
        // and agreement of native ABI descriptions with their real callees.
        super::super::verify::module(&module, &instance)?;
        Self::try_new(Shared::try_new(module, instance)?)
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
        match super::super::unwind::catch_raw(callback) {
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
    pub(crate) shared: Arc<Shared>,
}

/// Keeps the lifecycle count nonzero while close drains signals, without
/// claiming an active execution. TSD cleanup must still see the Engine as idle;
/// actual signal handlers take short `ExecutionLease`s of their own.
struct FinalizerPermit {
    pub(crate) shared: Arc<Shared>,
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
            super::super::deferred::executions_idle(self.shared.control());
        }
    }
}

fn close_shared(shared: &Arc<Shared>) {
    let initiated = shared.control.request_close();
    // This runs after the phase transition: registered callbacks may enter in
    // Closing, while no new unregistered public entry can start. It is
    // idempotent because close() and Engine handle Drop can race.
    super::super::deferred::begin_engine_close(shared);
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

pub(super) fn start_finalizer(shared: Arc<Shared>) {
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
    super::super::unwind::guard_native_teardown(|| {
        super::super::native_instance::run_finalizers(&shared.instance)
    });
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
        super::super::deferred::begin_engine_close(shared);
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
        super::super::unwind::guard_native_teardown(|| {
            loop {
                super::super::signal::deactivate_engine(shared.control()).unwrap_or_else(|error| {
                    eprintln!(
                        "mirvm[m4-engine]: failed to close guest signal disposition: {error}"
                    );
                    std::process::abort();
                });
                drain_pending_signals_for_close(signal_activation.ctx());
                if !super::super::signal::has_engine_registrations(shared.control())
                    && !super::super::signal::has_engine_pending(shared.control())
                {
                    break;
                }
            }
        });
        let finalizing = super::super::signal::try_seal_engine(shared.control());
        drop(signal_activation);
        drop(signal_permit);

        if finalizing {
            break;
        }
    }
    super::super::deferred::finish_engine_close(shared);
    shared.release_thread_contexts();
    shared.module.funcs.flush_heat_order();
    #[cfg(feature = "cranelift")]
    super::super::jit::stop(shared);
    crate::vm::atexit::discard(shared.id);
    ENGINES.write().unwrap().remove(&shared.id);
    shared.control.finish_close();
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
pub(super) fn run_wait_closed_check_hook() {
    let hook = WAIT_CLOSED_CHECK_HOOK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap()
        .take();
    if let Some(hook) = hook {
        hook();
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

// Host-thread lookups used by `Engine::wait_closed` and `ExecutionLease::for_thunk`. They read
// this thread's `ThreadContexts`, so they live beside their only callers.

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
