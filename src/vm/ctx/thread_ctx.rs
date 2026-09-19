//! Per-thread execution state: `Ctx` (the vmctx), the `ThreadContexts` registry that owns one
//! `Ctx` per Engine on this host thread, the per-thread signal-mask and close-drain state, the
//! main-run bookkeeping, and the deferred pthread-key teardown rounds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::super::ffi::FfiState;
use super::super::frame::ByteRegion;
use super::engine::Shared;
use super::signals::{
    dispatch_signal_delivery, drain_current_thread_signal_deliveries, drain_pending_signals,
    start_pending_signal_finalizers,
};

/// Per-thread execution state (vmctx): one per guest thread, with the lifetime of that host
/// thread's thread-local storage.
pub struct Ctx {
    pub shared: *const Shared,
    shared_owner: Arc<Shared>,
    pub region: ByteRegion,
    /// Interpreted-frame recursion depth, kept for diagnostics only: guest stack overflow is
    /// detected from the real `stack_floor` guard below, not from this count.
    pub depth: u32,
    /// Ordinary execution state is delivering mailbox messages; nested safepoints traversed inside
    /// a handler must not recursively drain.
    pub(crate) signal_draining: bool,
    /// Host execution stack safety floor: the low end of this thread's stack plus a safety margin.
    /// When an interp_frame stack pointer approximation falls below this, it is a guest stack
    /// overflow (diagnostic exit rather than host SIGSEGV). Because the guard is derived from the
    /// thread's real stack bounds, the main thread's 1 GiB stack, amplified guest thread stacks and
    /// foreign native thread thunk re-entry all work. 0 means the probe failed and no guard is
    /// applied; getattr_np is available for all threads including the main thread on glibc.
    pub stack_floor: usize,
    /// Foreign passthrough state (dlsym cache + dlopen handles; dlsym is idempotent, per-thread
    /// independent caches are harmless).
    pub ffi: FfiState,
    /// Guest TLS instance table: TlsId -> real address of this thread's instance
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
pub(super) struct MainRunState {
    pub(super) boundary_activation: Option<u64>,
    pub(super) catcher_claimed: bool,
    pub(super) catcher_active: bool,
    pub(super) panicked: bool,
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
    pub(super) ctx: *mut Ctx,
    pub(super) index: usize,
    pub(super) finished: bool,
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

pub(super) struct MainBoundaryGuard {
    pub(super) ctx: *mut Ctx,
    pub(super) index: usize,
    pub(super) finished: bool,
}

impl MainBoundaryGuard {
    fn finish(mut self) {
        let state = unsafe { &mut (&mut (*self.ctx).main_runs)[self.index] };
        if !state.catcher_claimed || state.catcher_active {
            super::super::interp::engine_abort(
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
        super::super::interp::engine_abort("main panic catch call appeared outside run_main");
    };
    let state = &mut states[index];
    if state.boundary_activation.is_some() {
        super::super::interp::engine_abort(
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
    pub(super) ctx: *mut Ctx,
    pub(super) index: usize,
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
    role: super::super::ir::BuiltinCallRole,
) -> Option<MainCatchGuard> {
    if role != super::super::ir::BuiltinCallRole::MainPanicCatcher {
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
            super::super::heap::dealloc(addr, slot.size.max(1), slot.align as u64);
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

pub(super) struct SignalDrainGuard {
    pub(super) ctx: *mut Ctx,
}

pub(super) fn current_activation(ctx: *mut Ctx) -> u64 {
    let Some(key) = CTX_KEY.get().copied() else {
        super::super::interp::engine_abort("main panic catch happened outside Engine activation");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null()
        || unsafe { (*contexts).current != ctx || (*contexts).current_activation == 0 }
    {
        super::super::interp::engine_abort(
            "main panic catch does not belong to the current Engine activation",
        );
    }
    unsafe { (*contexts).current_activation }
}

pub(super) struct SignalMaskGuard {
    pub(super) contexts: *mut ThreadContexts,
    pub(super) previous: u64,
    pub(super) restored: bool,
}

impl SignalMaskGuard {
    pub(super) fn restore(&mut self) {
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
    pub(super) fn finish(
        mut self,
        ctx: *mut Ctx,
        host_mask: crate::os::signal::ThreadSignalMaskGuard,
    ) {
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

pub(super) struct CloseSignalDrainGuard {
    contexts: *mut ThreadContexts,
    previous: bool,
}

impl CloseSignalDrainGuard {
    pub(super) fn enter(contexts: *mut ThreadContexts, closing: bool) -> Self {
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

pub(super) fn current_thread_contexts(ctx: *mut Ctx) -> *mut ThreadContexts {
    let Some(key) = CTX_KEY.get().copied() else {
        super::super::interp::engine_abort("signal delivery happened outside an Engine activation");
    };
    let contexts = unsafe { crate::os::thread::tls_get(key) } as *mut ThreadContexts;
    if contexts.is_null() || unsafe { (*contexts).current != ctx } {
        super::super::interp::engine_abort("signal delivery does not belong to the current Engine");
    }
    contexts
}

pub(super) struct CtxSlot {
    pub(super) ctx: Mutex<Option<Box<Ctx>>>,
}

impl CtxSlot {
    fn new(shared: &Arc<Shared>) -> Self {
        Self {
            ctx: Mutex::new(Some(Box::new(Ctx::new(shared)))),
        }
    }
}

pub(super) struct ThreadContexts {
    by_engine: HashMap<usize, Arc<CtxSlot>>,
    pub(super) current: *mut Ctx,
    /// Serial number for the current embedded entry. A new value is allocated on each `activate`
    /// and restored to the outer value on exit; therefore signal/thunk re-entries into the same
    /// Engine are strictly distinguished from the interrupted execution.
    pub(super) current_activation: u64,
    pub(super) next_activation: u64,
    /// Code domain of the innermost activation on this thread, saved and restored
    /// like `current_activation`. Dispatch reads it to pick the domain's slot set,
    /// so a guest -> native -> guest chain keeps the domain it entered from and
    /// never mixes slot sets mid-chain.
    pub(super) domain: super::super::jit::CodeDomain,
    /// Engine ids for every active entry on this host thread. Looking only at
    /// `current` loses an outer Engine across A -> native -> B nesting, which
    /// can make `wait_closed(A)` deadlock on its own lease and can reject a
    /// legitimate A reentry while A is Closing.
    pub(super) active_engines: Vec<u64>,
    /// Deferred handlers execute at VM safe points rather than in the kernel
    /// frame. Mirror the kernel's temporary standard-signal mask per host
    /// thread, including across nested calls into another Engine.
    pub(super) signal_mask: u64,
    /// True only while a finalizer drains already accepted signal inbox work.
    /// A raise must not be left pending on that temporary pthread.
    pub(super) close_signal_drain: bool,
    /// Engines whose close was requested under a deferred handler's temporary
    /// mask. Starting their worker here would make pthread inherit that mask.
    pub(super) pending_finalizers: Vec<Arc<Shared>>,
    /// EngineFaults that have been raised on this thread and have not yet been
    /// consumed by their owning execution boundaries. A native catch may
    /// suspend one unwind and re-enter an Engine, so this is a token stack,
    /// not a thread-wide boolean.
    pub(super) in_flight_faults: Vec<EngineFaultToken>,
    pub(super) next_fault_nonce: u64,
    teardown_rounds: u8,
    pub(super) final_tsd_cursor: Option<libc::pthread_key_t>,
    pub(super) final_tsd_active: bool,
    /// Avoid touching telemetry from final TSD on threads that never entered a
    /// capture-capable Engine.
    pub(super) telemetry_touched: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EngineFaultToken {
    pub(super) owner_id: u64,
    pub(super) raising_ctx: *mut Ctx,
    pub(super) nonce: u64,
}

impl ThreadContexts {
    pub(super) fn new() -> Self {
        Self {
            by_engine: HashMap::new(),
            current: std::ptr::null_mut(),
            current_activation: 0,
            next_activation: 1,
            // Plain until an activation says otherwise: the default must not
            // reserve a register or carry collection state.
            domain: super::super::jit::CodeDomain::Plain,
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

    pub(super) fn attach(&mut self, shared: &Arc<Shared>) -> *mut Ctx {
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
pub(super) static CTX_KEY: OnceLock<crate::os::thread::TlsKey> = OnceLock::new();

#[thread_local]
pub(super) static THREAD_CONTEXT_EXITING: std::sync::atomic::AtomicBool =
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
/// consulting plain entries. Plain is the answer outside any
/// activation, so the default path never reserves a register.
pub(crate) fn current_code_domain() -> super::super::jit::CodeDomain {
    let Some(ctx_key) = CTX_KEY.get().copied() else {
        return super::super::jit::CodeDomain::Plain;
    };
    let contexts = unsafe { crate::os::thread::tls_get(ctx_key) } as *mut ThreadContexts;
    if contexts.is_null() {
        return super::super::jit::CodeDomain::Plain;
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

/// Fork guard baseline: count of threads attributable to the guest when guest main
/// starts. At this moment = mirvm internal threads (main-in-join, guest-exec, allocator) plus any
/// MIRVM service thread already running (capture writer), and 0 guest-spawned threads.
/// **Use the real OS thread count, not the Ctx count**: after pthread_create returns the new thread
/// already exists, but its Ctx is not created until trampoline attach—Ctx counting has a TOCTOU
/// window and would miss it. MIRVM's own service threads are subtracted because the guest can never
/// have created them.
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
pub(super) fn guest_threads_from(raw: usize, service: usize) -> usize {
    raw.saturating_sub(service)
}

/// Guest-attributable thread count for `shared`, repairing a baseline that a `fork` child inherited.
/// `fork` keeps only the calling thread, so the child's service-thread count is zero while the
/// inherited baseline still counted the parent's service threads. Without this repair a child that
/// forks again would compare 1 thread against the parent's inflated baseline and see a
/// multi-threaded guest. Both values are only written at pin points (guest main start) and here, so
/// the read/compare/write below is confined to the single-threaded child.
pub(super) fn guest_thread_count_for(shared: &Shared) -> usize {
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
    for signum in 1..super::super::signal::SIGNAL_SLOTS as i32 {
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
    while let Some((delivery, signum)) = super::super::signal::take_current_thread_delivery(0) {
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
pub(super) fn run_thread_exit_inbox_empty_hook() {
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
pub(super) unsafe extern "C" fn ctx_key_dtor(p: *mut std::ffi::c_void) {
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
            super::super::unwind::guard_native_teardown(drain_thread_signal_inbox_for_exit);
            #[cfg(test)]
            run_thread_exit_inbox_empty_hook();
            let cursor = (*contexts)
                .final_tsd_cursor
                .expect("pthread final TSD cursor disappeared");
            if let Some(callback) =
                super::super::deferred::take_next_current_thread_tsd_for_exit(cursor)
            {
                (*contexts).final_tsd_cursor = Some(callback.key());
                super::super::unwind::guard_native_teardown(|| callback.invoke());
                continue;
            }
            original_mask = block_thread_signals_for_exit();
            if !super::super::signal::current_thread_has_pending() {
                super::super::signal::deactivate_current_thread_inbox();
                break;
            }
        }
        super::super::deferred::abandon_current_thread_tsd_for_exit();
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
/// region continues nesting on the disciplined stack.
///
/// Returns a raw pointer (Box pins the address, and the raw-ptr vmctx is passed across native
/// stacks). The main thread's Ctx is reclaimed together with process exit (glibc exit
/// does not go through the TSD phase, same as native).
// Used by the separately compiled TSan harness; product entries go through
// `activate`, which also establishes an activation identity.
#[cfg_attr(not(test), allow(dead_code))]
pub fn attach(shared: &Arc<Shared>) -> *mut Ctx {
    super::super::signal::initialize_current_thread_inbox();
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
