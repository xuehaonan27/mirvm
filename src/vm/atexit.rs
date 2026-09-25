//! The exit-handler registry: `atexit`, `__cxa_atexit` and `on_exit` callbacks, owned per Engine and
//! run in LIFO order at virtual process teardown.
//!
//! Two kinds of callback arrive here and they are run by different machinery: a *guest* one, which
//! is a guest function the engine calls through its dispatcher, and a *native* one, which is a
//! function inside an image the engine linked and is therefore invoked directly. The second kind
//! exists because a native destructor reaches the registry through the interposed `__cxa_atexit`
//! (`crate::vm::interpose`), which is how an image's teardown becomes the Engine's to run rather
//! than the process's.
//!
//! glibc does not export `atexit` for a guest `dlsym`, so the engine keeps its own registry and
//! one native trampoline, mounted through the libc `atexit` the engine itself links against
//! (not through `dlsym`). At process teardown libc calls the trampoline on the main thread,
//! which runs the guest callbacks one by one under a fresh `Ctx` attach.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::vm::ctx::Ctx;
use crate::vm::dispatch::call_fn_addr;
use crate::vm::unwind::engine_abort;

/// What a registered callback is called with.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    /// `atexit`: `fn()`.
    Plain,
    /// `__cxa_atexit`: `fn(arg)`.
    CxaArg,
    /// `on_exit`: `fn(status, arg)`.
    OnExit,
}

struct Entry {
    func: u64,
    kind: Kind,
    arg: u64,
}

static REGISTRY: LazyLock<Mutex<HashMap<usize, Vec<Entry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Registers a callback for the Engine that owns `ctx`. `func` must be a known guest entry; a
/// non-guest callback is refused, not silently dropped.
pub(crate) fn register(ctx: *mut Ctx, func: u64, kind: Kind, arg: u64) -> u64 {
    let shared = unsafe { &*(*ctx).shared };
    if !shared.instance.fn_addrs.contains_key(&func) {
        engine_abort(&format!(
            "atexit callback {func:#x} is not a known guest fn entry"
        ));
    }
    REGISTRY
        .lock()
        .unwrap()
        .entry(shared.id as usize)
        .or_default()
        .push(Entry { func, kind, arg });
    0
}

/// Drops the callbacks of an Engine that has finished.
pub(crate) fn discard(engine_id: u64) {
    REGISTRY.lock().unwrap().remove(&(engine_id as usize));
    NATIVE_REGISTRY
        .lock()
        .unwrap()
        .remove(&(engine_id as usize));
}

/// One native exit handler: a function inside an image the Engine linked, and its argument.
struct NativeEntry {
    func: u64,
    arg: u64,
}

static NATIVE_REGISTRY: LazyLock<Mutex<HashMap<usize, Vec<NativeEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The replacement an interposed `__cxa_atexit` reaches, with the Engine the bridge loaded for it.
///
/// The caller's `__dso_handle` is deliberately not consulted: the engine owns every registration
/// its images make, runs them all at its own teardown, and never asks the platform to finalize one
/// image's handlers early, which is the only thing that argument is for.
pub(crate) unsafe extern "C-unwind" fn native_cxa_atexit(
    func: u64,
    arg: u64,
    _dso: u64,
    owner: u64,
) -> i32 {
    let Some(control) = super::ctx::control_for_engine(owner) else {
        crate::os::process::set_errno(crate::os::process::ESRCH);
        return -1;
    };
    if super::ctx::ExecutionLease::for_thunk(&control).is_err() {
        crate::os::process::set_errno(crate::os::process::ESRCH);
        return -1;
    }
    NATIVE_REGISTRY
        .lock()
        .unwrap()
        .entry(owner as usize)
        .or_default()
        .push(NativeEntry { func, arg });
    0
}

/// Runs an Engine's native exit handlers in LIFO order.
///
/// A fault escaping one aborts, exactly as it does out of an image's finalizer list: teardown has
/// no caller that could own the exception.
pub(crate) fn run_native_handlers(engine_id: u64) {
    loop {
        let entry = {
            let mut registry = NATIVE_REGISTRY.lock().unwrap();
            let entry = registry.get_mut(&(engine_id as usize)).and_then(Vec::pop);
            if registry
                .get(&(engine_id as usize))
                .is_some_and(Vec::is_empty)
            {
                registry.remove(&(engine_id as usize));
            }
            entry
        };
        let Some(entry) = entry else { break };
        let callback: unsafe extern "C-unwind" fn(u64) = unsafe { std::mem::transmute(entry.func) };
        crate::vm::unwind::guard_native_teardown(|| unsafe { callback(entry.arg) });
    }
}

#[cfg(test)]
pub(crate) fn seed_callback(engine_id: u64) {
    REGISTRY
        .lock()
        .unwrap()
        .entry(engine_id as usize)
        .or_default()
        .push(Entry {
            func: 0,
            kind: Kind::Plain,
            arg: 0,
        });
}

#[cfg(test)]
pub(crate) fn has_callbacks(engine_id: u64) -> bool {
    REGISTRY.lock().unwrap().contains_key(&(engine_id as usize))
}

/// Virtual process teardown for this Engine: runs its own guest callbacks in LIFO order, with
/// `status` as `on_exit` sees it.
pub(crate) fn run_callbacks(ctx: *mut Ctx, status: i32) {
    let shared = unsafe { &*(*ctx).shared };
    let key = shared.id as usize;
    // LIFO: the last registered callback runs first (C semantics).
    loop {
        let entry = {
            let mut reg = REGISTRY.lock().unwrap();
            let entry = reg.get_mut(&key).and_then(Vec::pop);
            if reg.get(&key).is_some_and(Vec::is_empty) {
                reg.remove(&key);
            }
            entry
        };
        let Some(entry) = entry else { break };
        let args: &[u64] = match entry.kind {
            Kind::Plain => &[],
            Kind::CxaArg => &[entry.arg],
            Kind::OnExit => &[status as u64, entry.arg],
        };
        // A guest callback panic escaping into the C exit path aborts, as under native.
        let _ = call_fn_addr(ctx, entry.func, args, "atexit");
    }
}
