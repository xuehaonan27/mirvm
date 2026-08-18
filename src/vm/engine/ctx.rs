//! 执行环境：Shared（发布后只读）+ Ctx（每线程执行态，vmctx）。
//!
//! 状态三分（concurrency-arch §2）在真引擎的落地；raw-ptr ctx + 字段级瞬态借用
//! 纪律沿用 spike2/3/4。Shared 由 Engine 以 Arc 持有；每线程 Ctx 也持一份 Arc，
//! 因而执行态结束前模块不会被释放。Ctx 落 **自管 pthread key**——
//! **边界 TLS attach**（vmctx-passing §1，JNI 同款）是所有入口
//! （run_main / run_export / thunk）进入引擎的唯一门。
//!
//! 为什么不用宿主 `thread_local!`：guest 的 TLS dtor（std run_dtors 经 pthread_key
//! 注册，thunk 化）跑在 pthread TSD 相位，而宿主 C++ TLS 析构**先于** TSD 相位
//! （glibc start_thread：__call_tls_dtors → __nptl_deallocate_tsd）——彼时 Ctx 已亡，
//! dtor thunk 内 attach 撞已销毁宿主 TLS。自管 pthread key + **迟退 N 轮**（dtor 里
//! 重新 setspecific 挂回，glibc 上限 4 轮）让 Ctx 存活到 guest key dtor 之后。

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use super::ffi::FfiState;
use super::frame::ByteRegion;
use super::ir::Module;

/// 发布后只读：加载相建好、执行相 lock-free 共享读（引擎 Sync 的根基，spike4）。
/// 例外 = thunks（M4.4 D1）：执行期按需物化的 thunk 缓存——Mutex 显式同步，
/// 状态三分（concurrency-arch §2）的第三格，合法。
pub struct Shared {
    pub id: u64,
    pub module: Module,
    pub thunks: super::thunks::ThunkCache,
    /// Frozen when this Engine is created. Capture-capable Engines keep their
    /// trace IR after a session stops; later activations may bind a new session.
    pub(crate) trace_capable: bool,
    /// J1 分层基座（M5.3a）：PLT 槽 + 计数，按合并后 FuncId 空间建。
    /// 槽的写入者是 M5.3b 编译线程（单原子交换发布），此外发布后只读纪律不变。
    pub jit: super::jit::JitState,
    control: Arc<EngineControl>,
    /// Every host thread owns its slot; Shared keeps only weak discovery links
    /// so an exited thread cannot form a cycle. Finalization clears the live
    /// slots after the execution count reaches zero.
    ctx_slots: Mutex<Vec<Weak<CtxSlot>>>,
    fork_baseline_threads: std::sync::atomic::AtomicUsize,
}

impl Shared {
    pub(crate) fn new(module: Module) -> Self {
        Self::try_from_module(module)
            .unwrap_or_else(|error| panic!("Engine Shared initialization failed: {error}"))
    }

    pub(crate) fn try_from_module(mut module: Module) -> Result<Self, String> {
        static NEXT_ENGINE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let trace_capable = crate::telemetry::capture::is_armed();
        if trace_capable {
            module.rewrite_host_syscalls_for_capture();
        }
        module.ensure_function_names();
        super::backtrace::materialize_symbols(&mut module)?;
        let jit = super::jit::JitState::new(module.funcs.len());
        let id = NEXT_ENGINE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Shared {
            id,
            module,
            thunks: super::thunks::ThunkCache::default(),
            trace_capable,
            jit,
            control: Arc::new(EngineControl::new(id)),
            ctx_slots: Mutex::new(Vec::new()),
            fork_baseline_threads: std::sync::atomic::AtomicUsize::new(0),
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

/// 每线程执行态（vmctx）。M4.4 起每 guest 线程一份，生命周期 = 宿主 thread_local。
pub struct Ctx {
    pub shared: *const Shared,
    shared_owner: Arc<Shared>,
    pub region: ByteRegion,
    /// 解释帧递归深度（诊断计数；溢出判定改用 stack_floor 真栈守卫，M5.2 D8a）
    pub depth: u32,
    /// 普通执行态正在派送 mailbox；handler 内经过的嵌套安全点不得递归 drain。
    signal_draining: bool,
    /// 宿主执行栈安全下界（M5.2 D8a）：本线程栈低端 + 安全边距。interp_frame 的
    /// 栈指针近似值低于此 = guest 栈溢出（诊断退出而非宿主 SIGSEGV）。真栈字节
    /// 守卫替代旧的固定帧数上限（8000）：随线程真实栈自适应（主执行线程 1 GiB、
    /// guest 线程放大后的栈、外来 native 线程 thunk 再入均正确）。0 = 探测失败，
    /// 不守卫（与旧世界的裸奔等价，getattr_np 在 glibc 上对含主线程的所有线程可用）。
    pub stack_floor: usize,
    /// foreign 直通状态（dlsym 缓存 + dlopen 句柄；dlsym 幂等，每线程独立缓存无碍）
    pub ffi: FfiState,
    /// guest TLS 实例表（M4.4 D3）：TlsId → 本线程实例真地址（0 = 未物化，首访
    /// heap 分配 + 模板拷贝）。guest dtor 先由 pthread-key thunk 执行，Ctx 最后
    /// 一轮析构时再释放实例内存。
    pub tls: Vec<u64>,
    /// 活动解释帧。IP 供 guest unwinder 消费，CFA 是该解释调用在宿主栈上的位置；
    /// backtrace 用 CFA 把解释帧与系统展开器读出的 JIT 真机器帧恢复成一个调用序列。
    /// enter 时 push、FrameGuard::drop 时 pop（与 depth 同生命周期，unwind 安全）。
    pub shadow: Vec<ShadowFrame>,
    /// 当前线程在本 Engine 上嵌套执行的 `run_main` 状态。每次运行独立记录，避免
    /// 正常返回 101 与 guest std 把 main panic 转成 101 后无法区分。
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
                "固定 std 的 main panic 捕获调用未经过预期 catch_unwind intrinsic",
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

/// 执行 IR 标出的标准 main 捕获调用。边界记住当前 Engine 入口的激活编号；signal
/// handler 或 native thunk 重入会获得另一编号，不能借用外层边界认领 main panic。
pub(crate) fn call_main_panic_boundary<R>(ctx: *mut Ctx, f: impl FnOnce() -> R) -> R {
    let states = unsafe { &mut (*ctx).main_runs };
    let Some(index) = states.len().checked_sub(1) else {
        super::interp::engine_abort("main panic 捕获调用出现在 run_main 之外");
    };
    let state = &mut states[index];
    if state.boundary_activation.is_some() {
        super::interp::engine_abort("同一次 main 执行重复进入 panic 捕获边界");
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

/// 只有降低阶段精确标出的 intrinsic 且仍在同一次 Engine 激活中，才能认领 main
/// 捕获。普通 catch、signal handler 和 native thunk 重入都返回 `None`。
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

/// 在普通 VM 状态派送已登记信号。每轮先拿走一个传统 signal pending set；handler
/// 产生的新信号进入下一轮。轮数有界，剩余事件留给下一个块入口/返回安全点。
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
    /// 当前嵌入入口的编号。每次 `activate` 分配新值，退出时恢复外层值；因此同一
    /// Engine 的 signal/thunk 重入也与被打断的执行严格区分。
    current_activation: u64,
    next_activation: u64,
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

/// 本线程栈安全下界：os::thread 取 [lo, lo+size)，下界加安全边距。
/// 边距覆盖单次 interp_frame 的宿主最坏用量 + 最深处的 FFI/unwind/诊断路径；
/// 小栈取 1/8 防止边距吃光可用区。仅 Ctx 创建时调一次（getattr 对主线程读
/// /proc，非热路径）。
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

/// Ctx 的 pthread key（进程唯一；dtor = ctx_key_dtor）。
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

/// fork 守卫基线（M5.2 D8f）：guest main 启动时的 OS 线程数（`/proc/self/task`）。
/// 此刻 = mirvm 内部线程（main-in-join、guest-exec、分配器）+ 0 个 guest 派生线程。
/// **用真 OS 线程数而非 Ctx 计数**：pthread_create 返回后新线程即存在，但其 Ctx
/// 要到 trampoline attach 才建——Ctx 计数有 TOCTOU 窗口会漏计。fork 只在当前线程数
/// == 基线（guest 未派生任何线程）时放行。
/// guest main 启动点调用（run_main/run_export）：钉住单 guest 线程的基线。
pub fn set_fork_baseline(shared: &Shared) {
    shared.fork_baseline_threads.store(
        crate::os::thread::os_thread_count(),
        std::sync::atomic::Ordering::SeqCst,
    );
}

/// guest 是否已派生额外线程（HostFork 守卫）：当前 OS 线程数 > 基线 = 是。
/// 基线未设（0）或读取失败时保守判"多线程"（拒绝 fork）。
/// # Safety
///
/// `ctx` 必须是当前线程仍处于激活范围内的 `Ctx`。
pub unsafe fn guest_spawned_threads(ctx: *mut Ctx) -> bool {
    let base = unsafe { &*(*ctx).shared }
        .fork_baseline_threads
        .load(std::sync::atomic::Ordering::SeqCst);
    base == 0 || crate::os::thread::os_thread_count() > base
}

/// TSD 相位的 Ctx 收尾：迟退 3 轮（重新挂回 → glibc 追加轮次，上限 4）——guest 的
/// pthread-key dtor（std run_dtors thunk，键序不可控）总能在存活的 Ctx 上执行；
/// 末轮真正销毁（ByteRegion munmap 等）。
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

/// 边界 TLS attach：本线程首次进入引擎时创建 Ctx（新 guest 线程执行态的诞生点），
/// 之后幂等返回同一实例——再入（guest→native→thunk→guest）天然拿到同一 vmctx，
/// 操作数区按纪律化栈继续嵌套（spike2 形状）。
///
/// 返回裸指针（Box 钉地址，raw-ptr vmctx 在 native 栈间传递——借用纪律 §9）。
/// 主线程的 Ctx 随进程 exit 一并回收（glibc exit 不走 TSD 相位，与 native 同）。
// Used by the separately compiled TSan harness; product entries go through
// `activate`, which also establishes an activation identity.
#[cfg_attr(not(test), allow(dead_code))]
pub fn attach(shared: &Arc<Shared>) -> *mut Ctx {
    super::signal::initialize_current_thread_inbox();
    let key = *CTX_KEY.get_or_init(|| {
        // TSan 配置：不注册 dtor——TSan 的线程态在 TSD 相位前已析构，插桩代码
        // 不可在彼时运行（Ctx 每线程泄漏，仅测试配置；dtor 链由 threads_panic
        // 差分在真配置验证）。
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
        panic!("JIT 助手在 Engine 激活前被调用");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    assert!(!contexts.is_null(), "JIT 助手在线程 attach 前被调用");
    let ctx = unsafe { (*contexts).current };
    assert!(!ctx.is_null(), "JIT 助手在 Engine 激活范围外被调用");
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
        super::interp::engine_abort("main panic 捕获发生在 Engine 激活之外");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null()
        || unsafe { (*contexts).current != ctx || (*contexts).current_activation == 0 }
    {
        super::interp::engine_abort("main panic 捕获不属于当前 Engine 激活");
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
        let telemetry = if shared.trace_capable {
            (*contexts).telemetry_touched = true;
            Some(crate::telemetry::capture::activation_enter(shared.id))
        } else {
            None
        };
        ActivationGuard {
            contexts,
            previous,
            previous_activation,
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
}
