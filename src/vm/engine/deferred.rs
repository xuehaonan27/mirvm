//! Lifetimes for callbacks accepted and retained by native pthread APIs.
//!
//! An execution lease covers code that is running now. A deferred hold covers
//! the gap after native code accepted a callback and before that callback
//! starts or is revoked. Only APIs with a concrete completion/revocation
//! contract get a hold; an arbitrary library-retained pointer remains a stable
//! process-lifetime tombstone and never makes `wait_closed` wait forever.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, LazyLock, Mutex};

use super::ctx::{DeferredHold, EngineClosed, EngineControl, ExecutionLease, Shared};

const TSD_DTOR_ROUNDS: usize = 4;

pub(crate) struct PthreadStart {
    hold: Mutex<Option<DeferredHold>>,
}

impl PthreadStart {
    pub(crate) fn new(control: &Arc<EngineControl>) -> Result<Arc<Self>, EngineClosed> {
        Ok(Arc::new(Self {
            hold: Mutex::new(Some(DeferredHold::acquire(control, true)?)),
        }))
    }

    pub(crate) fn enter(
        &self,
        control: &Arc<EngineControl>,
    ) -> Result<ExecutionLease, EngineClosed> {
        let hold = self.hold.lock().unwrap().take().ok_or(EngineClosed)?;
        let lease = ExecutionLease::for_registered_callback(control)?;
        drop(hold);
        Ok(lease)
    }

    pub(crate) fn cancel(&self) {
        self.hold.lock().unwrap().take();
    }
}

struct TsdState {
    key: Option<libc::pthread_key_t>,
    code: u64,
    values: HashMap<libc::pthread_t, (u64, usize)>,
    active_rounds: HashMap<libc::pthread_t, Vec<usize>>,
    callbacks: usize,
    operations: usize,
    deleting: bool,
    closing: bool,
    deleted: bool,
    hold: Option<DeferredHold>,
}

pub(crate) struct TsdRegistration {
    control: Arc<EngineControl>,
    state: Mutex<TsdState>,
}

type TsdKey = (u64, libc::pthread_key_t);
type TsdKeyRegistry = HashMap<TsdKey, Arc<TsdRegistration>>;

static TSD_KEYS: LazyLock<Mutex<TsdKeyRegistry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

impl TsdRegistration {
    pub(crate) fn pending(control: &Arc<EngineControl>) -> Result<Arc<Self>, EngineClosed> {
        Ok(Arc::new(Self {
            control: Arc::clone(control),
            state: Mutex::new(TsdState {
                key: None,
                code: 0,
                values: HashMap::new(),
                active_rounds: HashMap::new(),
                callbacks: 0,
                operations: 0,
                deleting: false,
                closing: false,
                deleted: false,
                hold: Some(DeferredHold::acquire(control, true)?),
            }),
        }))
    }

    pub(crate) fn set_code(&self, code: u64) {
        let mut state = self.state.lock().unwrap();
        assert_eq!(state.code, 0);
        state.code = code;
    }

    pub(crate) fn commit(self: &Arc<Self>, key: libc::pthread_key_t) {
        let closing = {
            // Publishing a key and close's registry scan share this lock. If
            // close scanned first, the phase check makes this registration
            // responsible for its own cleanup; if commit publishes first,
            // close must observe and mark it before releasing the lock.
            let mut keys = TSD_KEYS.lock().unwrap();
            let slot = (self.control.id(), key);
            let replaceable = keys.get(&slot).is_some_and(|old| {
                let state = old.state.lock().unwrap();
                state.deleting || state.deleted
            });
            if keys.contains_key(&slot) && !replaceable {
                eprintln!("mirvm[m4-engine]: duplicate live pthread TSD key registration");
                std::process::abort();
            }
            keys.insert(slot, Arc::clone(self));
            let mut state = self.state.lock().unwrap();
            state.key = Some(key);
            state.closing = !self.control.is_running();
            state.closing
        };
        if closing {
            self.cleanup_if_idle();
        }
    }

    pub(crate) fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.deleted = true;
        state.hold.take();
    }

    pub(crate) fn enter_callback(
        self: &Arc<Self>,
    ) -> Result<(ExecutionLease, TsdCallback), EngineClosed> {
        let lease = ExecutionLease::for_registered_callback(&self.control)?;
        let thread = unsafe { libc::pthread_self() };
        let (round, final_pthread_pass) = {
            let mut state = self.state.lock().unwrap();
            if state.deleted {
                return Err(EngineClosed);
            }
            let key = state
                .key
                .expect("pthread TSD callback entered before its key was committed");
            let round = state
                .values
                .remove(&thread)
                .map_or(1, |(_, previous)| previous + 1);
            state.active_rounds.entry(thread).or_default().push(round);
            state.callbacks += 1;
            (round, super::ctx::current_thread_is_in_final_tsd_pass(key))
        };
        Ok((
            lease,
            TsdCallback {
                registration: Arc::clone(self),
                thread,
                round,
                final_pthread_pass,
            },
        ))
    }

    fn finish_callback(
        self: &Arc<Self>,
        thread: libc::pthread_t,
        round: usize,
        final_pthread_pass: bool,
    ) {
        let (clear_native, drain_after_return) = {
            let mut state = self.state.lock().unwrap();
            let rounds = state
                .active_rounds
                .get_mut(&thread)
                .expect("pthread TSD callback round disappeared");
            assert_eq!(rounds.pop(), Some(round));
            if rounds.is_empty() {
                state.active_rounds.remove(&thread);
            }
            state.callbacks = state
                .callbacks
                .checked_sub(1)
                .expect("pthread TSD callback count underflowed");
            let clear_native = if round >= TSD_DTOR_ROUNDS && state.values.remove(&thread).is_some()
            {
                state.key
            } else {
                None
            };
            let drain_after_return = state.closing
                && !final_pthread_pass
                && !state
                    .active_rounds
                    .get(&thread)
                    .is_some_and(|rounds| !rounds.is_empty())
                && state.values.contains_key(&thread);
            (clear_native, drain_after_return)
        };
        if let Some(key) = clear_native {
            // POSIX abandons a destructor value that is reinstalled in every
            // one of the implementation's bounded rounds.
            unsafe { libc::pthread_setspecific(key, std::ptr::null()) };
        }
        if drain_after_return {
            // ActivationGuard could not drain while this destructor round was
            // active. Continue only after that round has completely returned,
            // preserving POSIX's bounded reinstallation behavior.
            drain_registrations_on_current_thread(vec![Arc::clone(self)]);
        } else {
            self.cleanup_if_idle();
        }
    }

    fn mark_closing(&self) {
        self.state.lock().unwrap().closing = true;
    }

    fn current_value(&self) -> Option<(libc::pthread_key_t, u64, u64)> {
        let thread = unsafe { libc::pthread_self() };
        let state = self.state.lock().unwrap();
        if state.deleted
            || state.deleting
            || state
                .active_rounds
                .get(&thread)
                .is_some_and(|rounds| !rounds.is_empty())
        {
            return None;
        }
        let key = state.key?;
        let (value, _) = *state.values.get(&thread)?;
        Some((key, value, state.code))
    }

    fn abandon_current_value(&self) {
        let thread = unsafe { libc::pthread_self() };
        let mut state = self.state.lock().unwrap();
        if state
            .active_rounds
            .get(&thread)
            .is_some_and(|rounds| !rounds.is_empty())
        {
            return;
        }
        state.values.remove(&thread);
        drop(state);
        self.cleanup_if_idle();
    }

    fn abandon_current_value_for_exit(&self) -> Option<libc::pthread_key_t> {
        let thread = unsafe { libc::pthread_self() };
        let mut state = self.state.lock().unwrap();
        if state
            .active_rounds
            .get(&thread)
            .is_some_and(|rounds| !rounds.is_empty())
        {
            eprintln!("mirvm[m4-engine]: pthread exited inside a managed TSD callback");
            std::process::abort();
        }
        let key = state.key?;
        state.values.remove(&thread)?;
        Some(key)
    }

    fn begin_operation(self: &Arc<Self>, kind: TsdOperationKind) -> Option<TsdOperation> {
        let mut state = self.state.lock().unwrap();
        if state.deleted || state.deleting {
            return None;
        }
        if matches!(kind, TsdOperationKind::Delete) {
            // libc may reuse the numeric key immediately after delete returns.
            // Mark this generation before the native call so lookups never
            // attach a later operation to the retired generation.
            state.deleting = true;
        }
        state.operations += 1;
        Some(TsdOperation {
            registration: Arc::clone(self),
            kind,
            completed: false,
        })
    }

    fn finish_operation(&self, kind: TsdOperationKind, succeeded: bool) {
        let (remove_key, hold) = {
            let thread = unsafe { libc::pthread_self() };
            let mut state = self.state.lock().unwrap();
            if succeeded {
                match kind {
                    TsdOperationKind::Set(value) => {
                        if value == 0 {
                            state.values.remove(&thread);
                        } else {
                            let round = state
                                .active_rounds
                                .get(&thread)
                                .and_then(|rounds| rounds.last())
                                .copied()
                                .unwrap_or(0);
                            state.values.insert(thread, (value, round));
                        }
                    }
                    TsdOperationKind::Delete => {
                        state.deleted = true;
                        state.values.clear();
                    }
                }
            } else if matches!(kind, TsdOperationKind::Delete) {
                state.deleting = false;
            }
            state.operations = state
                .operations
                .checked_sub(1)
                .expect("pthread TSD operation count underflowed");
            if succeeded && matches!(kind, TsdOperationKind::Delete) {
                (state.key, state.hold.take())
            } else {
                (None, None)
            }
        };
        if let Some(key) = remove_key {
            remove_registration(self, key);
        }
        drop(hold);
        self.cleanup_if_idle();
    }

    fn cleanup_if_idle(&self) {
        let cleanup = {
            let mut state = self.state.lock().unwrap();
            if !state.closing
                || state.deleted
                || state.callbacks != 0
                || state.operations != 0
                || !state.values.is_empty()
                || !self.control.executions_idle()
            {
                return;
            }
            state.deleted = true;
            state.values.clear();
            let key = state.key;
            let hold = state.hold.take();
            (key, hold)
        };
        if let Some(key) = cleanup.0 {
            remove_registration(self, key);
            unsafe { libc::pthread_key_delete(key) };
        }
        drop(cleanup.1);
    }
}

fn remove_registration(registration: &TsdRegistration, key: libc::pthread_key_t) {
    let slot = (registration.control.id(), key);
    let mut keys = TSD_KEYS.lock().unwrap();
    if keys
        .get(&slot)
        .is_some_and(|current| std::ptr::eq(Arc::as_ptr(current), registration))
    {
        keys.remove(&slot);
    }
}

pub(crate) struct TsdCallback {
    registration: Arc<TsdRegistration>,
    thread: libc::pthread_t,
    round: usize,
    final_pthread_pass: bool,
}

impl Drop for TsdCallback {
    fn drop(&mut self) {
        self.registration
            .finish_callback(self.thread, self.round, self.final_pthread_pass);
    }
}

#[derive(Clone, Copy)]
enum TsdOperationKind {
    Set(u64),
    Delete,
}

pub(crate) struct TsdOperation {
    registration: Arc<TsdRegistration>,
    kind: TsdOperationKind,
    completed: bool,
}

impl TsdOperation {
    pub(crate) fn complete(mut self, result: u64) {
        self.registration.finish_operation(self.kind, result == 0);
        self.completed = true;
    }
}

impl Drop for TsdOperation {
    fn drop(&mut self) {
        if !self.completed {
            self.registration.finish_operation(self.kind, false);
        }
    }
}

pub(crate) fn prepare_pthread_operation(
    shared: &Shared,
    sym: &str,
    args: &[u64],
) -> Option<TsdOperation> {
    let (key, kind) = match sym {
        "pthread_setspecific" if args.len() >= 2 => (
            args[0] as libc::pthread_key_t,
            TsdOperationKind::Set(args[1]),
        ),
        "pthread_key_delete" if !args.is_empty() => {
            (args[0] as libc::pthread_key_t, TsdOperationKind::Delete)
        }
        _ => return None,
    };
    let registration = { TSD_KEYS.lock().unwrap().get(&(shared.id, key)).cloned() };
    registration.and_then(|registration| registration.begin_operation(kind))
}

fn registration_for_native_key(
    owner: u64,
    key: libc::pthread_key_t,
) -> Option<Arc<TsdRegistration>> {
    TSD_KEYS.lock().unwrap().get(&(owner, key)).cloned()
}

pub(crate) unsafe extern "C" fn native_pthread_setspecific(
    key: libc::pthread_key_t,
    value: *const c_void,
    owner: u64,
) -> libc::c_int {
    let operation = registration_for_native_key(owner, key)
        .and_then(|registration| registration.begin_operation(TsdOperationKind::Set(value as u64)));
    let result = unsafe { libc::pthread_setspecific(key, value) };
    if let Some(operation) = operation {
        operation.complete(result as u64);
    }
    result
}

pub(crate) unsafe extern "C" fn native_pthread_key_delete(
    key: libc::pthread_key_t,
    owner: u64,
) -> libc::c_int {
    let operation = registration_for_native_key(owner, key)
        .and_then(|registration| registration.begin_operation(TsdOperationKind::Delete));
    let result = unsafe { libc::pthread_key_delete(key) };
    if let Some(operation) = operation {
        operation.complete(result as u64);
    }
    result
}

pub(crate) unsafe extern "C" fn native_pthread_key_create(
    key: *mut libc::pthread_key_t,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
    owner: u64,
) -> libc::c_int {
    let Some(destructor) = destructor else {
        return unsafe { libc::pthread_key_create(key, None) };
    };
    let Some((registration, proxy)) =
        super::thunks::wrap_tsd_destructor(owner, destructor as usize as u64)
    else {
        return libc::EINVAL;
    };
    let proxy: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(proxy as usize) };
    let result = unsafe { libc::pthread_key_create(key, Some(proxy)) };
    if result == 0 {
        registration.commit(unsafe { key.read() });
    } else {
        registration.cancel();
    }
    result
}

pub(crate) unsafe extern "C" fn native_pthread_create(
    thread: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    value: *mut c_void,
    owner: u64,
) -> libc::c_int {
    let Some((registration, proxy)) =
        super::thunks::wrap_pthread_start(owner, start as usize as u64)
    else {
        return libc::EINVAL;
    };
    let proxy: extern "C" fn(*mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(proxy as usize) };
    let result = unsafe { libc::pthread_create(thread, attr, proxy, value) };
    if result != 0 {
        registration.cancel();
    }
    result
}

pub(crate) fn begin_engine_close(shared: &Shared) {
    let registrations: Vec<_> = {
        let keys = TSD_KEYS.lock().unwrap();
        let registrations: Vec<_> = keys
            .iter()
            .filter(|((engine, _), _)| *engine == shared.id)
            .map(|(_, registration)| Arc::clone(registration))
            .collect();
        for registration in &registrations {
            registration.mark_closing();
        }
        registrations
    };

    if !super::ctx::current_thread_has_engine(shared.id) {
        drain_registrations_on_current_thread(registrations);
    }
}

pub(crate) fn drain_current_thread(shared: &Shared) {
    let registrations: Vec<_> = TSD_KEYS
        .lock()
        .unwrap()
        .iter()
        .filter(|((engine, _), _)| *engine == shared.id)
        .map(|(_, registration)| Arc::clone(registration))
        .collect();
    drain_registrations_on_current_thread(registrations);
}

pub(crate) struct ThreadExitTsdCallback {
    registration: Arc<TsdRegistration>,
    key: libc::pthread_key_t,
    value: u64,
    code: u64,
}

impl ThreadExitTsdCallback {
    pub(crate) fn key(&self) -> libc::pthread_key_t {
        self.key
    }

    pub(crate) fn invoke(self) {
        let error = unsafe { libc::pthread_setspecific(self.key, std::ptr::null()) };
        if error != 0 && error != libc::EINVAL {
            eprintln!(
                "mirvm[m4-engine]: failed to clear managed TSD before pthread exit callback: {error}"
            );
            std::process::abort();
        }
        let callback: unsafe extern "C" fn(*mut c_void) =
            unsafe { std::mem::transmute(self.code as usize) };
        unsafe { callback(self.value as *mut c_void) };
        drop(self.registration);
    }
}

/// Select the next managed TSD value that glibc has not yet visited in its
/// fourth and final pthread-destructor pass.
pub(crate) fn take_next_current_thread_tsd_for_exit(
    cursor: libc::pthread_key_t,
) -> Option<ThreadExitTsdCallback> {
    let registrations = TSD_KEYS
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    registrations
        .into_iter()
        .filter_map(|registration| {
            let (key, value, code) = registration.current_value()?;
            (key > cursor).then_some(ThreadExitTsdCallback {
                registration,
                key,
                value,
                code,
            })
        })
        .min_by_key(ThreadExitTsdCallback::key)
}

/// No pthread destructor pass follows the Ctx key's fourth invocation. Clear
/// every managed value left on this pthread and release closing registrations.
pub(crate) fn abandon_current_thread_tsd_for_exit() {
    let registrations = TSD_KEYS
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for registration in registrations {
        if let Some(key) = registration.abandon_current_value_for_exit() {
            let error = unsafe { libc::pthread_setspecific(key, std::ptr::null()) };
            if error != 0 && error != libc::EINVAL {
                eprintln!(
                    "mirvm[m4-engine]: failed to abandon managed TSD at pthread exit: {error}"
                );
                std::process::abort();
            }
        }
        registration.cleanup_if_idle();
    }
}

fn drain_registrations_on_current_thread(registrations: Vec<Arc<TsdRegistration>>) {
    if super::ctx::current_thread_final_tsd_pass_is_armed() {
        // Ctx owns the one remaining global pthread-destructor pass. A local
        // four-round drain here would run raw key slots more than once.
        return;
    }
    // pthread would run at most PTHREAD_DESTRUCTOR_ITERATIONS rounds on thread
    // exit. Do the same synchronously for the thread that requested close, so
    // wait_closed() cannot wait for its own future TSD phase.
    for _ in 0..TSD_DTOR_ROUNDS {
        let mut progressed = false;
        for registration in &registrations {
            let Some((key, value, code)) = registration.current_value() else {
                continue;
            };
            progressed = true;
            unsafe {
                libc::pthread_setspecific(key, std::ptr::null());
                let callback: unsafe extern "C" fn(*mut c_void) =
                    std::mem::transmute(code as usize);
                callback(value as *mut c_void);
            }
        }
        if !progressed {
            break;
        }
    }
    for registration in registrations {
        // POSIX abandons a value that keeps reinstalling itself after four
        // destructor rounds. Mirroring that bound prevents self-deadlock.
        registration.abandon_current_value();
        registration.cleanup_if_idle();
    }
}

pub(crate) fn executions_idle(control: &EngineControl) {
    let registrations: Vec<_> = {
        TSD_KEYS
            .lock()
            .unwrap()
            .iter()
            .filter(|((engine, _), _)| *engine == control.id())
            .map(|(_, registration)| Arc::clone(registration))
            .collect()
    };
    for registration in registrations {
        registration.cleanup_if_idle();
    }
}

pub(crate) fn finish_engine_close(shared: &Shared) {
    // Reaching Finalizing proves every deferred hold is gone. This assertion is
    // intentionally structural: stale registry rows must never outlive Shared.
    let stale = TSD_KEYS
        .lock()
        .unwrap()
        .keys()
        .any(|(engine, _)| *engine == shared.id);
    if stale {
        eprintln!("mirvm[m4-engine]: Engine finalized with live pthread TSD registrations");
        std::process::abort();
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::super::ctx::{Engine, Shared, activate};
    use super::super::interp::{RunOutcome, call_guest_ffi, run_export};
    use super::super::ir::{
        Block, FfiKind, ForeignSig, FuncBody, MemOrd, Module, Operand, ParamAbi, PlaceBase,
        PlaceExpr, PlaceStep, RetAbi, RetDest, RmwOp, Rvalue, ScalarPlace, Slot, Stmt, Terminator,
        UnwindAction, Width,
    };
    use super::{TSD_DTOR_ROUNDS, TSD_KEYS, TsdRegistration, prepare_pthread_operation};

    const TSD_CHILD: &str = "MIRVM_TEST_TSD_DEFERRED_CHILD";
    const START_CHILD: &str = "MIRVM_TEST_PTHREAD_START_CHILD";
    const TSD_REMOTE_CHILD: &str = "MIRVM_TEST_TSD_REMOTE_CHILD";
    const TSD_TOMBSTONE_CHILD: &str = "MIRVM_TEST_TSD_TOMBSTONE_CHILD";
    const TSD_NATIVE_DELETE_CHILD: &str = "MIRVM_TEST_TSD_NATIVE_DELETE_CHILD";
    const TSD_NATIVE_SET_CHILD: &str = "MIRVM_TEST_TSD_NATIVE_SET_CHILD";
    const TSD_NATIVE_CREATE_CHILD: &str = "MIRVM_TEST_TSD_NATIVE_CREATE_CHILD";
    const START_NATIVE_CREATE_CHILD: &str = "MIRVM_TEST_PTHREAD_NATIVE_CREATE_CHILD";
    const EXIT_SIGNAL_TSD_CHILD: &str = "MIRVM_TEST_EXIT_SIGNAL_TSD_CHILD";
    const TSD_ENTRY: u64 = 0xde33_7000;
    const START_ENTRY: u64 = 0xde33_7100;
    const EXIT_SIGNAL_ENTRY: u64 = 0xde33_7200;

    static EXIT_SIGNAL_TSD_KEY: AtomicU64 = AtomicU64::new(u64::MAX);
    static EXIT_SIGNAL_TSD_OWNER: AtomicU64 = AtomicU64::new(0);
    static EXIT_SIGNAL_TSD_SET_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);

    unsafe extern "C-unwind" fn set_tsd_from_exit_signal() {
        let result = unsafe {
            super::native_pthread_setspecific(
                EXIT_SIGNAL_TSD_KEY.load(Ordering::SeqCst) as libc::pthread_key_t,
                std::ptr::dangling::<c_void>(),
                EXIT_SIGNAL_TSD_OWNER.load(Ordering::SeqCst),
            )
        };
        EXIT_SIGNAL_TSD_SET_RESULT.store(result as u64, Ordering::SeqCst);
    }

    fn sig(args: Vec<FfiKind>, ret: FfiKind, thunk_args: Vec<(usize, ForeignSig)>) -> ForeignSig {
        ForeignSig {
            args,
            ret,
            fixed: None,
            thunk_args,
            unwind: false,
        }
    }

    fn engine(module: Module, jit: bool) -> Engine {
        #[allow(unused_mut)]
        let mut shared = Shared::new(module);
        #[cfg(feature = "cranelift")]
        if jit {
            shared.jit.enabled = true;
            shared.jit.threshold = 1;
            shared.jit.sync = true;
        }
        #[cfg(not(feature = "cranelift"))]
        assert!(!jit);
        Engine::new(shared)
    }

    fn jit_modes() -> Vec<bool> {
        #[allow(unused_mut)]
        let mut modes = vec![false];
        #[cfg(feature = "cranelift")]
        modes.push(true);
        modes
    }

    fn tsd_module(marker: &AtomicU64, reinstall: bool) -> Module {
        let status = Slot {
            off: 0,
            width: Width::W32,
        };
        let key_ptr = Slot {
            off: 8,
            width: Width::W64,
        };
        let key_value = PlaceExpr {
            base: PlaceBase::Local(key_ptr.off),
            steps: Box::new([PlaceStep::Deref]),
        };
        let dtor_sig = sig(vec![FfiKind::Ptr], FfiKind::Void, Vec::new());
        let register = FuncBody {
            frame_size: 16,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: vec![ParamAbi::Scalar(key_ptr)],
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallForeign {
                        sym: "pthread_key_create".into(),
                        sig: sig(
                            vec![FfiKind::Ptr, FfiKind::Ptr],
                            FfiKind::I32,
                            vec![(1, dtor_sig)],
                        ),
                        args: vec![
                            Operand::Slot(key_ptr),
                            Operand::Imm {
                                bits: TSD_ENTRY,
                                width: Width::W64,
                            },
                        ],
                        ret: RetDest::Scalar(ScalarPlace::Slot(status)),
                        target: 1,
                        unwind: UnwindAction::Terminate,
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallForeign {
                        sym: "pthread_setspecific".into(),
                        sig: sig(vec![FfiKind::U32, FfiKind::Ptr], FfiKind::I32, Vec::new()),
                        args: vec![
                            Operand::Mem {
                                expr: key_value,
                                width: Width::W32,
                            },
                            Operand::Slot(key_ptr),
                        ],
                        ret: RetDest::Scalar(ScalarPlace::Slot(status)),
                        target: 2,
                        unwind: UnwindAction::Terminate,
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                },
            ],
            name: "register_tsd_dtor".into(),
        };
        let value = Slot {
            off: 0,
            width: Width::W64,
        };
        let scratch = Slot {
            off: 8,
            width: Width::W64,
        };
        let mut dtor_blocks = vec![Block {
            stmts: vec![Stmt::AtomicRmw {
                op: RmwOp::Add,
                addr: Operand::Imm {
                    bits: marker.as_ptr() as u64,
                    width: Width::W64,
                },
                val: Operand::Imm {
                    bits: 1,
                    width: Width::W64,
                },
                dst: ScalarPlace::Slot(scratch),
                order: MemOrd::SeqCst,
            }],
            term: if reinstall {
                Terminator::CallForeign {
                    sym: "pthread_setspecific".into(),
                    sig: sig(vec![FfiKind::U32, FfiKind::Ptr], FfiKind::I32, Vec::new()),
                    args: vec![
                        Operand::Mem {
                            expr: PlaceExpr {
                                base: PlaceBase::Local(value.off),
                                steps: Box::new([PlaceStep::Deref]),
                            },
                            width: Width::W32,
                        },
                        Operand::Slot(value),
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(Slot {
                        off: 16,
                        width: Width::W32,
                    })),
                    target: 1,
                    unwind: UnwindAction::Terminate,
                }
            } else {
                Terminator::Return
            },
        }];
        if reinstall {
            dtor_blocks.push(Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            });
        }
        let dtor = FuncBody {
            frame_size: 24,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: vec![ParamAbi::Scalar(value)],
            caller_loc_off: None,
            blocks: dtor_blocks,
            name: "guest_tsd_dtor".into(),
        };
        let mut module = Module {
            funcs: vec![register, dtor].into(),
            ..Module::default()
        };
        module.exports.insert("probe".into(), 0);
        module.fn_addrs.insert(TSD_ENTRY, 1);
        module
    }

    fn exit_signal_tsd_module(marker: &AtomicU64) -> Module {
        let mut module = tsd_module(marker, false);
        let attach = FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            }],
            name: "attach_before_reusing_low_tsd_key".into(),
        };
        let handler = FuncBody {
            frame_size: 16,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: vec![ParamAbi::Scalar(Slot {
                off: 8,
                width: Width::W32,
            })],
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallIndirect {
                        callee: Operand::Imm {
                            bits: set_tsd_from_exit_signal as *const () as usize as u64,
                            width: Width::W64,
                        },
                        args: Vec::new(),
                        ret: RetDest::Ignore,
                        target: 1,
                        unwind: UnwindAction::Continue,
                        null_ok: false,
                        native_sig: Some(sig(Vec::new(), FfiKind::Void, Vec::new())),
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                },
            ],
            name: "set_managed_tsd_from_exit_signal".into(),
        };
        let attach_id = module.funcs.len() as u32;
        module.funcs.push(attach);
        let handler_id = module.funcs.len() as u32;
        module.funcs.push(handler);
        module.exports.insert("attach".into(), attach_id);
        module.fn_addrs.insert(EXIT_SIGNAL_ENTRY, handler_id);
        module
    }

    fn native_delete_module(marker: &AtomicU64, library: &std::path::Path) -> Module {
        let mut module = tsd_module(marker, false);
        let key_ptr = Slot {
            off: 8,
            width: Width::W64,
        };
        let mut funcs = Vec::new();
        module.funcs.drain_into(&mut funcs);
        funcs[0].blocks[2].term = Terminator::CallForeign {
            sym: "mirvm_delete_key_in_native".into(),
            sig: sig(vec![FfiKind::U32], FfiKind::I32, Vec::new()),
            args: vec![Operand::Mem {
                expr: PlaceExpr {
                    base: PlaceBase::Local(key_ptr.off),
                    steps: Box::new([PlaceStep::Deref]),
                },
                width: Width::W32,
            }],
            ret: RetDest::Scalar(ScalarPlace::Slot(Slot {
                off: 0,
                width: Width::W32,
            })),
            target: 3,
            unwind: UnwindAction::Terminate,
        };
        funcs[0].blocks.push(Block {
            stmts: Vec::new(),
            term: Terminator::Return,
        });
        module.funcs = funcs.into();
        module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
        module
    }

    fn native_set_module(marker: &AtomicU64, library: &std::path::Path) -> Module {
        let mut module = tsd_module(marker, false);
        let mut funcs = Vec::new();
        module.funcs.drain_into(&mut funcs);
        let Terminator::CallForeign { sym, .. } = &mut funcs[0].blocks[1].term else {
            unreachable!()
        };
        *sym = "mirvm_set_key_in_native".into();
        module.funcs = funcs.into();
        module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
        module
    }

    fn native_create_key_module(marker: &AtomicU64, library: &std::path::Path) -> Module {
        let mut module = tsd_module(marker, false);
        let mut funcs = Vec::new();
        module.funcs.drain_into(&mut funcs);
        let Terminator::CallForeign { sym, .. } = &mut funcs[0].blocks[0].term else {
            unreachable!()
        };
        *sym = "mirvm_create_key_in_native".into();
        module.funcs = funcs.into();
        module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
        module
    }

    fn native_start_module(
        thread: *mut libc::pthread_t,
        marker: &AtomicU64,
        release: &AtomicU64,
        library: &std::path::Path,
    ) -> Module {
        let mut module = start_module(thread, marker);
        let mut funcs = Vec::new();
        module.funcs.drain_into(&mut funcs);
        let Terminator::CallForeign { sym, args, .. } = &mut funcs[0].blocks[0].term else {
            unreachable!()
        };
        *sym = "mirvm_create_thread_in_native".into();
        args[3] = Operand::Imm {
            bits: release.as_ptr() as u64,
            width: Width::W64,
        };
        module.funcs = funcs.into();
        module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
        module
    }

    fn build_native_key_delete_archive() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("mirvm-tsd-native-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("delete.c");
        let object = dir.join("delete.o");
        let archive = dir.join("libdelete.a");
        std::fs::write(
            &source,
            concat!(
                "#include <pthread.h>\n",
                "int mirvm_delete_key_in_native(pthread_key_t key) { return pthread_key_delete(key); }\n",
                "int mirvm_set_key_in_native(pthread_key_t key, const void *value) { return pthread_setspecific(key, value); }\n",
                "int mirvm_create_key_in_native(pthread_key_t *key, void (*dtor)(void *)) { return pthread_key_create(key, dtor); }\n",
                "static void *(*saved_start)(void *);\n",
                "static void *saved_arg;\n",
                "static void *delayed_start(void *gate) { while (!__atomic_load_n((unsigned long *)gate, __ATOMIC_ACQUIRE)) {} return saved_start(saved_arg); }\n",
                "int mirvm_create_thread_in_native(pthread_t *thread, const pthread_attr_t *attr, void *(*start)(void *), void *gate) { saved_start = start; saved_arg = gate; return pthread_create(thread, attr, delayed_start, gate); }\n",
            ),
        )
        .unwrap();
        assert!(
            std::process::Command::new("cc")
                .args(["-fPIC", "-c"])
                .arg(&source)
                .arg("-o")
                .arg(&object)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("ar")
                .arg("crs")
                .arg(&archive)
                .arg(&object)
                .status()
                .unwrap()
                .success()
        );
        let library = crate::native_archive::materialize_in(&archive, &dir.join("cache")).unwrap();
        (dir, library)
    }

    fn start_module(thread: *mut libc::pthread_t, marker: &AtomicU64) -> Module {
        let status = Slot {
            off: 0,
            width: Width::W32,
        };
        let start_sig = sig(vec![FfiKind::Ptr], FfiKind::Ptr, Vec::new());
        let spawn = FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallForeign {
                        sym: "pthread_create".into(),
                        sig: sig(
                            vec![FfiKind::Ptr, FfiKind::Ptr, FfiKind::Ptr, FfiKind::Ptr],
                            FfiKind::I32,
                            vec![(2, start_sig)],
                        ),
                        args: vec![
                            Operand::Imm {
                                bits: thread as u64,
                                width: Width::W64,
                            },
                            Operand::Imm {
                                bits: 0,
                                width: Width::W64,
                            },
                            Operand::Imm {
                                bits: START_ENTRY,
                                width: Width::W64,
                            },
                            Operand::Imm {
                                bits: 0,
                                width: Width::W64,
                            },
                        ],
                        ret: RetDest::Scalar(ScalarPlace::Slot(status)),
                        target: 1,
                        unwind: UnwindAction::Terminate,
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                },
            ],
            name: "spawn_guest_thread".into(),
        };
        let ret = Slot {
            off: 0,
            width: Width::W64,
        };
        let start = FuncBody {
            frame_size: 16,
            frame_align: 8,
            ret: RetAbi::Scalar(ret),
            params: vec![ParamAbi::Scalar(Slot {
                off: 8,
                width: Width::W64,
            })],
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: vec![
                    Stmt::AtomicStore {
                        addr: Operand::Imm {
                            bits: marker.as_ptr() as u64,
                            width: Width::W64,
                        },
                        val: Operand::Imm {
                            bits: 1,
                            width: Width::W64,
                        },
                        order: super::super::ir::MemOrd::SeqCst,
                    },
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(ret),
                        rv: Rvalue::Use(Operand::Imm {
                            bits: 0,
                            width: Width::W64,
                        }),
                    },
                ],
                term: Terminator::Return,
            }],
            name: "guest_thread_start".into(),
        };
        let mut module = Module {
            funcs: vec![spawn, start].into(),
            ..Module::default()
        };
        module.exports.insert("probe".into(), 0);
        module.fn_addrs.insert(START_ENTRY, 1);
        module
    }

    fn run_child(name: &str, env: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(env, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: status={:?}\nstdout={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn raw_tsd_registration(engine: &Engine) -> (Arc<TsdRegistration>, libc::pthread_key_t) {
        let registration = TsdRegistration::pending(engine.control()).unwrap();
        let mut key = 0;
        assert_eq!(unsafe { libc::pthread_key_create(&mut key, None) }, 0);
        registration.commit(key);
        (registration, key)
    }

    #[test]
    fn wait_closed_drains_current_thread_tsd_dtor() {
        const NAME: &str =
            "vm::engine::deferred::tests::wait_closed_drains_current_thread_tsd_dtor";
        if std::env::var_os(TSD_CHILD).is_none() {
            run_child(NAME, TSD_CHILD);
            return;
        }
        for jit in jit_modes() {
            let marker = AtomicU64::new(0);
            let mut key = 0;
            let engine = engine(tsd_module(&marker, false), jit);
            let outcome =
                unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
            assert_eq!(outcome, RunOutcome::Returned(Default::default()));
            engine.wait_closed().unwrap();
            assert_eq!(marker.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn close_inside_reinstalling_tsd_dtor_runs_four_rounds() {
        for jit in jit_modes() {
            let marker = AtomicU64::new(0);
            let mut key = 0;
            let engine = engine(tsd_module(&marker, true), jit);
            let outcome =
                unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
            assert_eq!(outcome, RunOutcome::Returned(Default::default()));

            let registration = {
                TSD_KEYS
                    .lock()
                    .unwrap()
                    .get(&(engine.shared().id, key))
                    .cloned()
                    .unwrap()
            };
            unsafe { libc::pthread_setspecific(key, std::ptr::null()) };
            let (lease, callback) = registration.enter_callback().unwrap();
            let activation = activate(lease.shared());
            call_guest_ffi(
                activation.ctx(),
                1,
                &[FfiKind::Ptr],
                &[(&mut key as *mut _) as u64],
                None,
            );
            engine.close();
            drop(activation);
            drop(lease);
            drop(callback);

            engine.wait_closed().unwrap();
            assert_eq!(marker.load(Ordering::SeqCst), TSD_DTOR_ROUNDS as u64);
        }
    }

    #[test]
    fn final_ctx_destructor_round_drains_tsd_reset_by_target_signal_callback() {
        const NAME: &str = "vm::engine::deferred::tests::final_ctx_destructor_round_drains_tsd_reset_by_target_signal_callback";
        if std::env::var_os(EXIT_SIGNAL_TSD_CHILD).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env(EXIT_SIGNAL_TSD_CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("thread-exit signal left a managed TSD hold behind");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let mut stdout = String::new();
            let mut stderr = String::new();
            std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
            std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
            assert!(
                status.success(),
                "thread-exit signal did not finish managed TSD teardown:\n{stdout}{stderr}"
            );
            return;
        }

        let signum = libc::SIGWINCH;
        let baseline = crate::os::signal::Sigaction::query(signum).unwrap();
        for jit in jit_modes() {
            for low_slot in [true, false] {
                let low_key = if low_slot {
                    let mut key = 0;
                    assert_eq!(unsafe { libc::pthread_key_create(&mut key, None) }, 0);
                    Some(key)
                } else {
                    None
                };

                let marker = AtomicU64::new(0);
                EXIT_SIGNAL_TSD_KEY.store(u64::MAX, Ordering::SeqCst);
                EXIT_SIGNAL_TSD_SET_RESULT.store(u64::MAX, Ordering::SeqCst);
                let engine = engine(exit_signal_tsd_module(&marker), jit);
                let ctx_key = super::super::ctx::test_ctx_key();
                EXIT_SIGNAL_TSD_OWNER.store(engine.control().id(), Ordering::SeqCst);
                super::super::signal::install_signal(
                    engine.control(),
                    signum,
                    EXIT_SIGNAL_ENTRY as usize,
                    Some((3, EXIT_SIGNAL_ENTRY)),
                )
                .unwrap();

                let (attached_tx, attached_rx) = std::sync::mpsc::channel();
                let (reuse_tx, reuse_rx) = std::sync::mpsc::channel();
                let (registered_tx, registered_rx) = std::sync::mpsc::channel();
                let (exit_tx, exit_rx) = std::sync::mpsc::channel();
                let worker_engine = engine.clone();
                let worker = std::thread::spawn(move || {
                    let mut key = libc::pthread_key_t::MAX;
                    assert!(matches!(
                        unsafe { run_export(&worker_engine, "attach", &[]) },
                        Ok(RunOutcome::Returned(_))
                    ));
                    attached_tx
                        .send(unsafe { libc::pthread_self() } as usize)
                        .unwrap();
                    reuse_rx.recv().unwrap();
                    let result = unsafe {
                        run_export(
                            &worker_engine,
                            "probe",
                            &[std::ptr::from_mut(&mut key) as u64],
                        )
                    };
                    EXIT_SIGNAL_TSD_KEY.store(key as u64, Ordering::SeqCst);
                    registered_tx.send((key, result)).unwrap();
                    exit_rx.recv().unwrap();
                });

                let target = attached_rx.recv().unwrap() as libc::pthread_t;
                let mut filler_keys = Vec::new();
                if let Some(low_key) = low_key {
                    assert!(low_key < ctx_key);
                    assert_eq!(unsafe { libc::pthread_key_delete(low_key) }, 0);
                } else {
                    loop {
                        let mut key = 0;
                        assert_eq!(unsafe { libc::pthread_key_create(&mut key, None) }, 0);
                        filler_keys.push(key);
                        if key > ctx_key {
                            break;
                        }
                    }
                }
                reuse_tx.send(()).unwrap();
                let (guest_key, registered) = registered_rx.recv().unwrap();
                if let Some(low_key) = low_key {
                    assert_eq!(guest_key, low_key, "guest TSD did not reuse the low key");
                } else {
                    assert!(guest_key > ctx_key, "guest TSD did not use a high key");
                }
                assert!(matches!(registered, Ok(RunOutcome::Returned(_))));

                let (empty_tx, empty_rx) = std::sync::mpsc::channel();
                let (publish_tx, publish_rx) = std::sync::mpsc::channel();
                super::super::ctx::set_thread_exit_inbox_empty_hook(Box::new(move || {
                    empty_tx.send(()).unwrap();
                    publish_rx.recv().unwrap();
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while !super::super::signal::current_thread_has_pending() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "target signal did not reach the final Ctx destructor round"
                        );
                        std::thread::yield_now();
                    }
                }));
                exit_tx.send(()).unwrap();
                empty_rx.recv().unwrap();
                assert_eq!(unsafe { libc::pthread_kill(target, signum) }, 0);
                publish_tx.send(()).unwrap();
                worker.join().unwrap();

                assert_eq!(EXIT_SIGNAL_TSD_SET_RESULT.load(Ordering::SeqCst), 0);
                engine.wait_closed().unwrap();
                assert_eq!(
                    marker.load(Ordering::SeqCst),
                    if low_slot { 1 } else { 2 },
                    "the final pthread pass did not honor its raw-key cursor"
                );
                assert!(
                    crate::os::signal::Sigaction::query(signum)
                        .unwrap()
                        .same_disposition(&baseline)
                );
                for key in filler_keys {
                    assert_eq!(unsafe { libc::pthread_key_delete(key) }, 0);
                }
            }
        }
    }

    #[test]
    fn pthread_key_create_commit_linearizes_with_close() {
        for iteration in 0..128 {
            let engine = engine(Module::default(), false);
            let registration = TsdRegistration::pending(engine.control()).unwrap();
            let mut key = 0;
            assert_eq!(unsafe { libc::pthread_key_create(&mut key, None) }, 0);
            let close = if iteration % 2 == 0 {
                // Deterministically cover close scanning an empty registry
                // before pthread_key_create publishes its successful result.
                engine.close();
                None
            } else {
                let gate = Arc::new(std::sync::Barrier::new(2));
                let closer = engine.clone();
                let close_gate = Arc::clone(&gate);
                let close = std::thread::spawn(move || {
                    close_gate.wait();
                    closer.close();
                });
                gate.wait();
                Some(close)
            };
            registration.commit(key);
            if let Some(close) = close {
                close.join().unwrap();
            }
            engine.wait_closed().unwrap();
            assert!(
                !TSD_KEYS
                    .lock()
                    .unwrap()
                    .contains_key(&(engine.shared().id, key))
            );
        }
    }

    #[test]
    fn pthread_setspecific_operation_blocks_close_revocation() {
        for iteration in 0..128 {
            let engine = engine(Module::default(), false);
            let (_registration, key) = raw_tsd_registration(&engine);
            let lease = engine.execution_lease().unwrap();
            let value = std::ptr::dangling::<c_void>();
            let operation = prepare_pthread_operation(
                engine.shared(),
                "pthread_setspecific",
                &[key as u64, value as u64],
            )
            .unwrap();
            if iteration % 2 == 0 {
                engine.close();
            } else {
                let gate = Arc::new(std::sync::Barrier::new(2));
                let closer = engine.clone();
                let close_gate = Arc::clone(&gate);
                let close = std::thread::spawn(move || {
                    close_gate.wait();
                    closer.close();
                });
                gate.wait();
                close.join().unwrap();
            }
            assert_eq!(engine.state(), super::super::ctx::EngineState::Closing);
            let result = unsafe { libc::pthread_setspecific(key, value) };
            assert_eq!(result, 0, "close revoked a key during pthread_setspecific");
            operation.complete(result as u64);

            let clear =
                prepare_pthread_operation(engine.shared(), "pthread_setspecific", &[key as u64, 0])
                    .unwrap();
            let result = unsafe { libc::pthread_setspecific(key, std::ptr::null()) };
            assert_eq!(result, 0, "tracked pthread key became invalid before clear");
            clear.complete(result as u64);
            drop(lease);
            engine.wait_closed().unwrap();
        }
    }

    #[test]
    fn pthread_key_delete_operation_blocks_close_revocation() {
        for iteration in 0..128 {
            let engine = engine(Module::default(), false);
            let (_registration, key) = raw_tsd_registration(&engine);
            let lease = engine.execution_lease().unwrap();
            let operation =
                prepare_pthread_operation(engine.shared(), "pthread_key_delete", &[key as u64])
                    .unwrap();
            if iteration % 2 == 0 {
                engine.close();
            } else {
                let gate = Arc::new(std::sync::Barrier::new(2));
                let closer = engine.clone();
                let close_gate = Arc::clone(&gate);
                let close = std::thread::spawn(move || {
                    close_gate.wait();
                    closer.close();
                });
                gate.wait();
                close.join().unwrap();
            }
            assert_eq!(engine.state(), super::super::ctx::EngineState::Closing);
            let result = unsafe { libc::pthread_key_delete(key) };
            operation.complete(result as u64);
            drop(lease);
            engine.wait_closed().unwrap();
        }
    }

    #[test]
    fn deleted_tsd_destructor_thunk_is_a_stable_noop() {
        const NAME: &str =
            "vm::engine::deferred::tests::deleted_tsd_destructor_thunk_is_a_stable_noop";
        if std::env::var_os(TSD_TOMBSTONE_CHILD).is_none() {
            run_child(NAME, TSD_TOMBSTONE_CHILD);
            return;
        }
        let marker = AtomicU64::new(0);
        let mut key = 0;
        let engine = engine(tsd_module(&marker, false), false);
        let outcome =
            unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
        assert_eq!(outcome, RunOutcome::Returned(Default::default()));
        let code = {
            let registration = TSD_KEYS
                .lock()
                .unwrap()
                .get(&(engine.shared().id, key))
                .cloned()
                .unwrap();
            registration.state.lock().unwrap().code
        };

        engine.wait_closed().unwrap();
        assert_eq!(marker.load(Ordering::SeqCst), 1);
        let callback: unsafe extern "C" fn(*mut c_void) =
            unsafe { std::mem::transmute(code as usize) };
        unsafe { callback((&mut key as *mut libc::pthread_key_t).cast()) };
        assert_eq!(marker.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn native_archive_key_delete_revokes_tracked_registration() {
        const NAME: &str =
            "vm::engine::deferred::tests::native_archive_key_delete_revokes_tracked_registration";
        if std::env::var_os(TSD_NATIVE_DELETE_CHILD).is_none() {
            run_child(NAME, TSD_NATIVE_DELETE_CHILD);
            return;
        }
        let (dir, library) = build_native_key_delete_archive();
        let marker = AtomicU64::new(0);
        let mut key = 0;
        let engine = engine(native_delete_module(&marker, &library), false);
        let outcome =
            unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
        assert_eq!(outcome, RunOutcome::Returned(Default::default()));
        assert_eq!(
            unsafe { libc::pthread_setspecific(key, std::ptr::dangling_mut()) },
            libc::EINVAL,
            "the native archive wrapper did not actually delete the libc key"
        );
        assert!(
            !TSD_KEYS
                .lock()
                .unwrap()
                .contains_key(&(engine.shared().id, key)),
            "native pthread_key_delete left a stale MIRVM registration"
        );
        engine.close();
        for _ in 0..100 {
            if engine.state() == super::super::ctx::EngineState::Closed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            engine.state(),
            super::super::ctx::EngineState::Closed,
            "native pthread_key_delete was invisible to the registration lifecycle"
        );
        assert_eq!(marker.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn native_archive_setspecific_is_tracked() {
        const NAME: &str = "vm::engine::deferred::tests::native_archive_setspecific_is_tracked";
        if std::env::var_os(TSD_NATIVE_SET_CHILD).is_none() {
            run_child(NAME, TSD_NATIVE_SET_CHILD);
            return;
        }
        let (dir, library) = build_native_key_delete_archive();
        let marker = Arc::new(AtomicU64::new(0));
        let engine = engine(native_set_module(&marker, &library), false);
        let worker_engine = engine.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut key = 0;
            let outcome =
                unsafe { run_export(&worker_engine, "probe", &[(&mut key as *mut _) as u64]) }
                    .unwrap();
            assert_eq!(outcome, RunOutcome::Returned(Default::default()));
            assert!(!unsafe { libc::pthread_getspecific(key) }.is_null());
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();
        engine.close();
        assert_eq!(engine.state(), super::super::ctx::EngineState::Closing);
        assert_eq!(marker.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        engine.wait_closed().unwrap();
        assert_eq!(marker.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn native_archive_key_create_registers_guest_destructor() {
        const NAME: &str =
            "vm::engine::deferred::tests::native_archive_key_create_registers_guest_destructor";
        if std::env::var_os(TSD_NATIVE_CREATE_CHILD).is_none() {
            run_child(NAME, TSD_NATIVE_CREATE_CHILD);
            return;
        }
        let (dir, library) = build_native_key_delete_archive();
        let marker = Arc::new(AtomicU64::new(0));
        let engine = engine(native_create_key_module(&marker, &library), false);
        let worker_engine = engine.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut key = 0;
            let outcome =
                unsafe { run_export(&worker_engine, "probe", &[(&mut key as *mut _) as u64]) }
                    .unwrap();
            assert_eq!(outcome, RunOutcome::Returned(Default::default()));
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();
        engine.close();
        assert_eq!(engine.state(), super::super::ctx::EngineState::Closing);
        assert_eq!(marker.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        engine.wait_closed().unwrap();
        assert_eq!(marker.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn native_archive_pthread_create_holds_until_delayed_start() {
        const NAME: &str =
            "vm::engine::deferred::tests::native_archive_pthread_create_holds_until_delayed_start";
        if std::env::var_os(START_NATIVE_CREATE_CHILD).is_none() {
            run_child(NAME, START_NATIVE_CREATE_CHILD);
            return;
        }
        let (dir, library) = build_native_key_delete_archive();
        let marker = AtomicU64::new(0);
        let release = AtomicU64::new(0);
        let mut thread = 0;
        let engine = engine(
            native_start_module(&mut thread, &marker, &release, &library),
            false,
        );
        let outcome = unsafe { run_export(&engine, "probe", &[]) }.unwrap();
        assert_eq!(outcome, RunOutcome::Returned(Default::default()));
        engine.close();
        assert_eq!(engine.state(), super::super::ctx::EngineState::Closing);
        assert_eq!(marker.load(Ordering::SeqCst), 0);
        release.store(1, Ordering::Release);
        unsafe { libc::pthread_join(thread, std::ptr::null_mut()) };
        engine.wait_closed().unwrap();
        assert_eq!(marker.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remote_tsd_value_keeps_engine_closing_until_thread_exit() {
        const NAME: &str =
            "vm::engine::deferred::tests::remote_tsd_value_keeps_engine_closing_until_thread_exit";
        if std::env::var_os(TSD_REMOTE_CHILD).is_none() {
            run_child(NAME, TSD_REMOTE_CHILD);
            return;
        }
        for jit in jit_modes() {
            let marker = Arc::new(AtomicU64::new(0));
            let engine = engine(tsd_module(&marker, false), jit);
            let worker_engine = engine.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut key = 0;
                let outcome =
                    unsafe { run_export(&worker_engine, "probe", &[(&mut key as *mut _) as u64]) }
                        .unwrap();
                assert_eq!(outcome, RunOutcome::Returned(Default::default()));
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });

            ready_rx.recv().unwrap();
            engine.close();
            assert_eq!(engine.state(), super::super::ctx::EngineState::Closing);
            assert_eq!(marker.load(Ordering::SeqCst), 0);
            release_tx.send(()).unwrap();
            worker.join().unwrap();
            engine.wait_closed().unwrap();
            assert_eq!(marker.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn pthread_create_start_hold_closes_the_start_window() {
        const NAME: &str =
            "vm::engine::deferred::tests::pthread_create_start_hold_closes_the_start_window";
        if std::env::var_os(START_CHILD).is_none() {
            run_child(NAME, START_CHILD);
            return;
        }
        for jit in jit_modes() {
            for _ in 0..64 {
                let marker = AtomicU64::new(0);
                let mut thread = 0;
                let engine = engine(start_module(&mut thread, &marker), jit);
                let outcome = unsafe { run_export(&engine, "probe", &[]) }.unwrap();
                assert_eq!(outcome, RunOutcome::Returned(Default::default()));
                engine.wait_closed().unwrap();
                unsafe { libc::pthread_join(thread, std::ptr::null_mut()) };
                assert_eq!(marker.load(Ordering::SeqCst), 1);
            }
        }
    }
}
