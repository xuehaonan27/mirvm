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
use crate::os::thread::{
    INVALID_ARGUMENT, TLS_KEY_GONE, ThreadId, TlsKey, current_thread, spawn_raw,
    tls_key_create_raw, tls_key_delete, tls_set,
};

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
    key: Option<TlsKey>,
    code: u64,
    values: HashMap<ThreadId, (u64, usize)>,
    active_rounds: HashMap<ThreadId, Vec<usize>>,
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

type TsdKey = (u64, TlsKey);
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

    pub(crate) fn commit(self: &Arc<Self>, key: TlsKey) {
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
        let thread = current_thread();
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

    fn finish_callback(self: &Arc<Self>, thread: ThreadId, round: usize, final_pthread_pass: bool) {
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
            let _ = unsafe { tls_set(key, std::ptr::null()) };
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

    fn current_value(&self) -> Option<(TlsKey, u64, u64)> {
        let thread = current_thread();
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
        let thread = current_thread();
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

    fn abandon_current_value_for_exit(&self) -> Option<TlsKey> {
        let thread = current_thread();
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
            let thread = current_thread();
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
            tls_key_delete(key);
        }
        drop(cleanup.1);
    }
}

fn remove_registration(registration: &TsdRegistration, key: TlsKey) {
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
    thread: ThreadId,
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
            TlsKey::from_raw(args[0] as libc::pthread_key_t),
            TsdOperationKind::Set(args[1]),
        ),
        "pthread_key_delete" if !args.is_empty() => (
            TlsKey::from_raw(args[0] as libc::pthread_key_t),
            TsdOperationKind::Delete,
        ),
        _ => return None,
    };
    let registration = { TSD_KEYS.lock().unwrap().get(&(shared.id, key)).cloned() };
    registration.and_then(|registration| registration.begin_operation(kind))
}

fn registration_for_native_key(owner: u64, key: TlsKey) -> Option<Arc<TsdRegistration>> {
    TSD_KEYS.lock().unwrap().get(&(owner, key)).cloned()
}

pub(crate) unsafe extern "C" fn native_pthread_setspecific(
    key: libc::pthread_key_t,
    value: *const c_void,
    owner: u64,
) -> libc::c_int {
    let key = TlsKey::from_raw(key);
    let operation = registration_for_native_key(owner, key)
        .and_then(|registration| registration.begin_operation(TsdOperationKind::Set(value as u64)));
    let result = unsafe { tls_set(key, value) };
    if let Some(operation) = operation {
        operation.complete(result as u64);
    }
    result
}

pub(crate) unsafe extern "C" fn native_pthread_key_delete(
    key: libc::pthread_key_t,
    owner: u64,
) -> libc::c_int {
    let key = TlsKey::from_raw(key);
    let operation = registration_for_native_key(owner, key)
        .and_then(|registration| registration.begin_operation(TsdOperationKind::Delete));
    let result = tls_key_delete(key);
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
        return unsafe { tls_key_create_raw(key, None) };
    };
    let Some((registration, proxy)) =
        super::thunks::wrap_tsd_destructor(owner, destructor as usize as u64)
    else {
        return INVALID_ARGUMENT;
    };
    let proxy: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(proxy as usize) };
    let result = unsafe { tls_key_create_raw(key, Some(proxy)) };
    if result == 0 {
        registration.commit(TlsKey::from_raw(unsafe { key.read() }));
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
        return INVALID_ARGUMENT;
    };
    let proxy: extern "C" fn(*mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(proxy as usize) };
    let result = unsafe { spawn_raw(thread, attr, proxy, value) };
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
    key: TlsKey,
    value: u64,
    code: u64,
}

impl ThreadExitTsdCallback {
    pub(crate) fn key(&self) -> TlsKey {
        self.key
    }

    pub(crate) fn invoke(self) {
        let error = unsafe { tls_set(self.key, std::ptr::null()) };
        if error != 0 && error != TLS_KEY_GONE {
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
    cursor: TlsKey,
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
            let error = unsafe { tls_set(key, std::ptr::null()) };
            if error != 0 && error != TLS_KEY_GONE {
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
                let _ = tls_set(key, std::ptr::null());
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
mod tests;
