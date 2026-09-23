//! The guest's exit-handler registry: `atexit`, `__cxa_atexit` and `on_exit` callbacks, owned
//! per Engine and run in LIFO order at virtual process teardown.
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
