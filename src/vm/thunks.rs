//! thunk factory (M4.4 D1, the only new mechanism this cycle): delivering the opposite FFI
//! direction.
//!
//! DESIGN §5: std already wraps pthread well (thread_start is std's extern "C" Rust fn); the
//! engine's only gap = when an interpreted function pointer escapes to native, materialize it
//! into real machine code. libffi Closure builds a trampoline from the frozen signature; the entry
//! does a boundary TLS attach (vmctx-passing §1, same as JNI) — the birth point of a new guest
//! thread execution state (Ctx).
//!
//! Lifecycle: thunk code and small ThunkData live for process lifetime, guaranteeing escaped
//! addresses remain callable; ThunkData only holds an EngineControl tombstone, not a Module. After
//! Engine closes, the C-unwind thunk raises EngineClosed; ordinary C thunks terminate with a
//! non-unwind ABI, while the Module can still be normally reclaimed.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

use libffi::low::ffi_cif;
use libffi::middle::{Cif, Closure};

use super::ctx::Shared;
use super::ffi::inbound::{marshal_args, repack_ret};
use super::ir::{FfiKind, ForeignSig, FuncId};

/// (fn entry address, escaped-bit signature) → thunk real code address. Mutex = "explicit
/// synchronization" lattice of the three-state split; creation is a cold path (once per
/// (fn, signature)), lock held for the whole duration, simplicity/correctness first.
#[derive(Default)]
pub struct ThunkCache {
    map: Mutex<HashMap<(u64, ForeignSig), u64>>,
}

/// Frozen data per thunk / entry stub (leaked for process lifetime; shared across threads —
/// Shared: Sync, rest is plain data).
struct ThunkData {
    control: Arc<super::ctx::EngineControl>,
    engine_id: u64,
    func: FuncId,
    args: Box<[FfiKind]>,
    ret: FfiKind,
    kind: ThunkKind,
}

static THUNK_DATA: LazyLock<Mutex<HashMap<u64, &'static ThunkData>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

enum ThunkKind {
    Ordinary,
    PthreadStart(Arc<super::deferred::PthreadStart>),
    TsdDestructor(Arc<super::deferred::TsdRegistration>),
    NativePthreadStart {
        registration: Arc<super::deferred::PthreadStart>,
        callback: usize,
    },
    NativeTsdDestructor {
        registration: Arc<super::deferred::TsdRegistration>,
        callback: usize,
    },
}

/// Common execution body shared by trampoline and P1 entry stub: attach → move args by signature
/// → interpret → write back return value. Return buffer always aligned (integers promoted to
/// ffi_arg / F32 bits in low 32, LE).
/// C1: when ret = Agg, branch — callee RetAbi::Indirect → result passed as hidden first arg
/// through `ffi::inbound::call_guest_ffi` (sret passed directly, callee memcpy's to that address); others →
/// (lo,hi) then repack_ret to struct bytes. Whether the ABI boundary allows unwind is decided by
/// the outer wrapper; the body does not duplicate two semantics.
unsafe fn trampoline_body(
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
    lease: super::ctx::ExecutionLease,
) {
    let shared = lease.shared();
    // A same-Engine native callback reuses its thread's Ctx, but it is still
    // a distinct Engine entry. Always assign a fresh activation nonce so the
    // callback cannot claim an outer run_main panic catcher.
    let activation = super::ctx::activate(shared);
    let ctx = activation.ctx();
    let av = unsafe { marshal_args(&data.args, args) };
    match &data.ret {
        FfiKind::Agg(agg) => {
            let (lo, hi) = crate::vm::ffi::inbound::call_guest_ffi(
                ctx,
                data.func,
                &data.args,
                &av,
                Some(result as *mut u64 as u64),
            );
            if !matches!(
                crate::vm::dispatch::ret_abi_of(ctx, data.func),
                super::ir::RetAbi::Indirect { .. }
            ) {
                unsafe { repack_ret(result as *mut u64 as *mut u8, agg, lo, hi) };
            }
        }
        _ => {
            let (lo, _hi) =
                crate::vm::ffi::inbound::call_guest_ffi(ctx, data.func, &data.args, &av, None);
            if data.ret != FfiKind::Void {
                *result = lo;
            }
        }
    }
}

/// Ordinary `extern "C"` boundary: guest panic or foreign exception must not cross out; Rust
/// preserves native abort semantics at this boundary.
unsafe extern "C" fn trampoline_c(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
) {
    let mut tsd_callback = None;
    let lease = match &data.kind {
        ThunkKind::PthreadStart(start)
        | ThunkKind::NativePthreadStart {
            registration: start,
            ..
        } => start.enter(&data.control),
        ThunkKind::TsdDestructor(registration)
        | ThunkKind::NativeTsdDestructor { registration, .. } => {
            match registration.enter_callback() {
                Ok((lease, callback)) => {
                    tsd_callback = Some(callback);
                    Ok(lease)
                }
                // pthread_key_delete can race a destructor already fetched by
                // libc. The leaked closure is a stable tombstone after revoke;
                // an obsolete void destructor becomes a no-op instead of calling
                // through a closed Engine.
                Err(_) => return,
            }
        }
        ThunkKind::Ordinary => super::ctx::ExecutionLease::for_thunk(&data.control),
    };
    let lease = match lease {
        Ok(lease) => lease,
        Err(_) => {
            eprintln!("mirvm[m4-engine]: plain C thunk called after its Engine was closed");
            std::process::abort()
        }
    };
    match data.kind {
        ThunkKind::NativePthreadStart { callback, .. } => {
            let activation = super::ctx::activate(lease.shared());
            let argument = unsafe { *(*args).cast::<*mut c_void>() };
            let callback: extern "C" fn(*mut c_void) -> *mut c_void =
                unsafe { std::mem::transmute(callback) };
            *result = callback(argument) as u64;
            drop(activation);
            drop(lease);
            return;
        }
        ThunkKind::NativeTsdDestructor { callback, .. } => {
            let activation = super::ctx::activate(lease.shared());
            let argument = unsafe { *(*args).cast::<*mut c_void>() };
            let callback: unsafe extern "C" fn(*mut c_void) =
                unsafe { std::mem::transmute(callback) };
            unsafe { callback(argument) };
            drop(activation);
            drop(lease);
            drop(tsd_callback);
            return;
        }
        _ => {}
    }
    unsafe { trampoline_body(result, args, data, lease) };
    drop(tsd_callback);
}

/// `extern "C-unwind"` boundary: allows guest panic or foreign exception to continue through
/// libffi closure, handed to the outer Rust/C++ handler.
unsafe extern "C-unwind" fn trampoline_c_unwind(
    _cif: &ffi_cif,
    result: &mut u64,
    args: *const *const c_void,
    data: &ThunkData,
) {
    let lease = match super::ctx::ExecutionLease::for_thunk(&data.control) {
        Ok(lease) => lease,
        Err(_) => super::unwind::raise_engine_closed(Arc::clone(&data.control)),
    };
    unsafe { trampoline_body(result, args, data, lease) }
}

type ThunkCallback = libffi::low::Callback<ThunkData, u64>;

struct PendingEntryClosure {
    closure: Option<Closure<'static>>,
    data: *mut ThunkData,
    code: u64,
}

impl Drop for PendingEntryClosure {
    fn drop(&mut self) {
        if let Some(closure) = self.closure.take() {
            drop(closure);
            unsafe { drop(Box::from_raw(self.data)) };
        }
    }
}

/// P1 closures are reclaimable until Engine publication. Once constructors
/// may run, their addresses can escape through arbitrary native code and are
/// converted into process-lifetime tombstones by `commit`.
pub(crate) struct EntryClosures {
    pending: Vec<PendingEntryClosure>,
}

impl EntryClosures {
    pub(crate) fn commit(mut self) {
        for entry in &mut self.pending {
            let closure = entry
                .closure
                .take()
                .expect("uncommitted P1 closure missing");
            let data = unsafe { &*entry.data };
            THUNK_DATA.lock().unwrap().insert(entry.code, data);
            std::mem::forget(closure);
        }
    }
}

/// libffi 5.x hard-codes the closure callback type as `extern "C"` in the Rust API, but C and
/// C-unwind have identical machine calling conventions; the difference is only whether Rust allows
/// the unwinder to cross that function boundary. libffi only stores and indirectly calls this
/// address from the native closure trampoline, not through the transmuted Rust `extern "C" type.
/// So here we only erase the type-level difference; the actual entry remains the C-unwind wrapper.
fn callback_for(unwind: bool) -> ThunkCallback {
    if unwind {
        let callback: unsafe extern "C-unwind" fn(
            &ffi_cif,
            &mut u64,
            *const *const c_void,
            &ThunkData,
        ) = trampoline_c_unwind;
        // SAFETY: both ABIs have identical machine signatures; the transmuted value is only given
        // to libffi as an opaque callback address, not executed through a Rust `extern "C"` call
        // site.
        unsafe {
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(&ffi_cif, &mut u64, *const *const c_void, &ThunkData),
                ThunkCallback,
            >(callback)
        }
    } else {
        trampoline_c
    }
}

/// Get or create: same (entry address, signature) always yields the same real code address (fn ptr
/// equality semantics).
pub(crate) fn get_or_create(shared: &Shared, entry: u64, func: FuncId, sig: &ForeignSig) -> u64 {
    let key = (entry, sig.clone());
    let mut map = shared.thunks.map.lock().unwrap();
    if let Some(&code) = map.get(&key) {
        return code;
    }
    let cif = Cif::new(
        sig.args.iter().map(crate::vm::ffi::ffi_type),
        crate::vm::ffi::ffi_type(&sig.ret),
    );
    let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
        control: Arc::clone(shared.control()),
        engine_id: shared.id,
        func,
        args: sig.args.clone().into(),
        ret: sig.ret.clone(),
        kind: ThunkKind::Ordinary,
    }));
    let closure = Closure::new(cif, callback_for(sig.unwind), data);
    let code = *closure.code_ptr() as usize as u64;
    std::mem::forget(closure); // process-lifetime immortal (executable page not reclaimed — guest holds code address)
    THUNK_DATA.lock().unwrap().insert(code, data);
    map.insert(key, code);
    code
}

/// A signal handler address is either unrelated native code, a callable guest
/// function, or a MIRVM-owned address that must not fall through to the kernel.
/// The last case includes stale Engine tombstones and callbacks with another
/// ABI: treating either as native code would execute an invalid MIRVM entry in
/// a kernel signal frame.
pub(crate) enum SignalHandlerResolution {
    Unknown,
    Valid {
        control: Arc<super::ctx::EngineControl>,
        func: FuncId,
    },
    KnownInvalid,
}

fn has_signal_body_shape(shared: &Shared, func: FuncId) -> bool {
    let Some(body) = shared.module.funcs.get(func as usize) else {
        return false;
    };
    matches!(body.ret, super::ir::RetAbi::Zst)
        && matches!(body.params.as_slice(), [super::ir::ParamAbi::Scalar(_)])
        && body.caller_loc_off.is_none()
}

fn has_signal_thunk_shape(data: &ThunkData) -> bool {
    matches!(&data.kind, ThunkKind::Ordinary)
        && data.args.as_ref() == [FfiKind::I32]
        && data.ret == FfiKind::Void
}

/// Resolve both the current Engine's guest function addresses and escaped
/// process-lifetime thunk/P1 addresses. `Unknown` alone may be considered a
/// genuine native address by the signal layer.
pub(crate) fn resolve_signal_handler(shared: &Shared, code: u64) -> SignalHandlerResolution {
    if let Some(&func) = shared.instance.fn_addrs.get(&code) {
        if !has_signal_body_shape(shared, func) {
            return SignalHandlerResolution::KnownInvalid;
        }

        // A materialized P1 entry is real executable code and therefore must
        // also match its preserved C ABI metadata. Hand-built Modules used by
        // focused tests have only a logical fn_addrs entry, so their strongest
        // available check is the FuncBody shape above.
        if shared.instance.is_executable_entry(code) {
            let thunks = THUNK_DATA.lock().unwrap();
            let Some(data) = thunks.get(&code).copied() else {
                return SignalHandlerResolution::KnownInvalid;
            };
            if !Arc::ptr_eq(&data.control, shared.control())
                || data.func != func
                || !has_signal_thunk_shape(data)
                || !data.control.accepts_signal_install()
            {
                return SignalHandlerResolution::KnownInvalid;
            }
        }

        return SignalHandlerResolution::Valid {
            control: Arc::clone(shared.control()),
            func,
        };
    }

    let thunks = THUNK_DATA.lock().unwrap();
    let Some(data) = thunks.get(&code).copied() else {
        return SignalHandlerResolution::Unknown;
    };
    if !has_signal_thunk_shape(data) || !data.control.accepts_signal_install() {
        return SignalHandlerResolution::KnownInvalid;
    }
    SignalHandlerResolution::Valid {
        control: Arc::clone(&data.control),
        func: data.func,
    }
}

fn create_registered(shared: &Shared, func: FuncId, sig: &ForeignSig, kind: ThunkKind) -> u64 {
    create_registered_data(
        Arc::clone(shared.control()),
        shared.id,
        func,
        sig.args.clone().into(),
        sig.ret.clone(),
        sig.unwind,
        kind,
    )
}

fn create_registered_data(
    control: Arc<super::ctx::EngineControl>,
    engine_id: u64,
    func: FuncId,
    args: Box<[FfiKind]>,
    ret: FfiKind,
    unwind: bool,
    kind: ThunkKind,
) -> u64 {
    let cif = Cif::new(
        args.iter().map(crate::vm::ffi::ffi_type),
        crate::vm::ffi::ffi_type(&ret),
    );
    let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
        control,
        engine_id,
        func,
        args,
        ret,
        kind,
    }));
    let closure = Closure::new(cif, callback_for(unwind), data);
    let code = *closure.code_ptr() as usize as u64;
    std::mem::forget(closure);
    THUNK_DATA.lock().unwrap().insert(code, data);
    code
}

pub(crate) fn wrap_pthread_start(
    owner: u64,
    code: u64,
) -> Option<(Arc<super::deferred::PthreadStart>, u64)> {
    let data = THUNK_DATA.lock().unwrap().get(&code).copied();
    let control = data
        .filter(|data| data.engine_id == owner)
        .map(|data| Arc::clone(&data.control))
        .or_else(|| super::ctx::control_for_engine(owner))?;
    let start = super::deferred::PthreadStart::new(&control).ok()?;
    let (func, kind) = match data {
        Some(data) if data.engine_id == owner && matches!(data.kind, ThunkKind::Ordinary) => {
            (data.func, ThunkKind::PthreadStart(Arc::clone(&start)))
        }
        _ => (
            0,
            ThunkKind::NativePthreadStart {
                registration: Arc::clone(&start),
                callback: code as usize,
            },
        ),
    };
    let proxy = create_registered_data(
        control,
        owner,
        func,
        vec![FfiKind::Ptr].into(),
        FfiKind::Ptr,
        false,
        kind,
    );
    Some((start, proxy))
}

pub(crate) fn wrap_tsd_destructor(
    owner: u64,
    code: u64,
) -> Option<(Arc<super::deferred::TsdRegistration>, u64)> {
    let data = THUNK_DATA.lock().unwrap().get(&code).copied();
    let control = data
        .filter(|data| data.engine_id == owner)
        .map(|data| Arc::clone(&data.control))
        .or_else(|| super::ctx::control_for_engine(owner))?;
    let registration = super::deferred::TsdRegistration::pending(&control).ok()?;
    let (func, kind) = match data {
        Some(data) if data.engine_id == owner && matches!(data.kind, ThunkKind::Ordinary) => (
            data.func,
            ThunkKind::TsdDestructor(Arc::clone(&registration)),
        ),
        _ => (
            0,
            ThunkKind::NativeTsdDestructor {
                registration: Arc::clone(&registration),
                callback: code as usize,
            },
        ),
    };
    let proxy = create_registered_data(
        control,
        owner,
        func,
        vec![FfiKind::Ptr].into(),
        FfiKind::Void,
        false,
        kind,
    );
    registration.set_code(proxy);
    Some((registration, proxy))
}

enum PendingCallback {
    PthreadStart(Arc<super::deferred::PthreadStart>),
    PthreadKey {
        registration: Arc<super::deferred::TsdRegistration>,
        key_out: u64,
    },
}

/// Per-foreign-call callback registrations. Recognized pthread APIs get a
/// unique closure and a concrete lifecycle contract. All other callback
/// arguments keep the ordinary cached tombstone behavior.
pub(crate) struct ForeignCallbacks {
    pending: Vec<PendingCallback>,
    operation: Option<super::deferred::TsdOperation>,
    completed: bool,
}

impl ForeignCallbacks {
    pub(crate) fn complete(mut self, result: u64) {
        let succeeded = result == 0;
        for pending in &self.pending {
            match pending {
                PendingCallback::PthreadStart(start) => {
                    if !succeeded {
                        start.cancel();
                    }
                }
                PendingCallback::PthreadKey {
                    registration,
                    key_out,
                } => {
                    if succeeded {
                        let key = unsafe {
                            (*key_out as *const crate::os::thread::TlsKey).read_unaligned()
                        };
                        registration.commit(key);
                    } else {
                        registration.cancel();
                    }
                }
            }
        }
        if let Some(operation) = self.operation.take() {
            operation.complete(result);
        }
        self.completed = true;
    }
}

impl Drop for ForeignCallbacks {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        for pending in &self.pending {
            match pending {
                PendingCallback::PthreadStart(start) => start.cancel(),
                PendingCallback::PthreadKey { registration, .. } => registration.cancel(),
            }
        }
    }
}

pub(crate) fn prepare_foreign_callbacks(
    shared: &Shared,
    sym: &str,
    sig: &ForeignSig,
    args: &mut [u64],
) -> ForeignCallbacks {
    let mut pending = Vec::new();
    for (pos, inner) in &sig.thunk_args {
        let value = args[*pos];
        if value == 0 {
            continue;
        }
        let Some(&func) = shared.instance.fn_addrs.get(&value) else {
            continue;
        };
        match (sym, *pos) {
            ("pthread_create", 2) => {
                let start =
                    super::deferred::PthreadStart::new(shared.control()).unwrap_or_else(|_| {
                        crate::vm::unwind::engine_abort(
                            "pthread_create callback registered while Engine was finalizing",
                        )
                    });
                args[*pos] = create_registered(
                    shared,
                    func,
                    inner,
                    ThunkKind::PthreadStart(Arc::clone(&start)),
                );
                pending.push(PendingCallback::PthreadStart(start));
            }
            ("pthread_key_create", 1) => {
                let registration = super::deferred::TsdRegistration::pending(shared.control())
                    .unwrap_or_else(|_| {
                        crate::vm::unwind::engine_abort(
                            "pthread TSD callback registered while Engine was finalizing",
                        )
                    });
                let code = create_registered(
                    shared,
                    func,
                    inner,
                    ThunkKind::TsdDestructor(Arc::clone(&registration)),
                );
                registration.set_code(code);
                args[*pos] = code;
                pending.push(PendingCallback::PthreadKey {
                    registration,
                    key_out: args[0],
                });
            }
            _ if !shared.instance.is_executable_entry(value) => {
                args[*pos] = get_or_create(shared, value, func, inner);
            }
            _ => {}
        }
    }
    ForeignCallbacks {
        pending,
        operation: super::deferred::prepare_pthread_operation(shared, sym, args),
        completed: false,
    }
}

// ===== P1 entry executable (decision-history §7.6) =====

/// Materialize an Engine-private closure per recipe. site.link_addr is artifact identity; closure
/// code is this Engine's runtime identity; closure and tombstone live for process lifetime, old
/// addresses never reused, so after close it still stably belongs to the original Engine.
fn materialize_domain(
    instance: &mut super::instance::Instance,
    control: &Arc<super::ctx::EngineControl>,
    sites: &[super::ir::EntryStubSite],
    closures: &mut EntryClosures,
) -> Result<(), String> {
    for site in sites {
        let cif = Cif::new(
            site.sig.args.iter().map(crate::vm::ffi::ffi_type),
            crate::vm::ffi::ffi_type(&site.sig.ret),
        );
        let data: &'static ThunkData = Box::leak(Box::new(ThunkData {
            control: Arc::clone(control),
            engine_id: control.id(),
            func: site.func,
            args: site.sig.args.clone().into(),
            ret: site.sig.ret.clone(),
            kind: ThunkKind::Ordinary,
        }));
        let closure = Closure::new(cif, callback_for(site.sig.unwind), data);
        let code = *closure.code_ptr() as usize as u64;
        closures.pending.push(PendingEntryClosure {
            closure: Some(closure),
            data: std::ptr::from_ref(data).cast_mut(),
            code,
        });
        instance.load_map.add_exact(site.link_addr, code)?;
        instance.executable_entry_addrs.insert(code);
    }
    Ok(())
}

/// P1 startup-phase materializer (third full-phase step alongside run_vm_engine and argv
/// finalization / GOT refill): own domain + absorb-mounted image / base domains, recipe →
/// per-Engine closure → LoadMap exact mapping. After all mappings ready, rebuild fn_addrs uniformly
/// and apply frozen pointer relocs.
pub(crate) fn materialize_all_entry_stubs(
    module: &mut super::ir::Module,
    instance: &mut super::instance::Instance,
    control: &Arc<super::ctx::EngineControl>,
) -> Result<EntryClosures, String> {
    let mut closures = EntryClosures {
        pending: Vec::new(),
    };
    let own_sites = std::mem::take(&mut module.entry_stub_sites);
    materialize_domain(instance, control, &own_sites, &mut closures)?;
    module.entry_stub_sites = own_sites;
    let image_stubs = std::mem::take(&mut instance.image_entry_stubs);
    for (home, sites, arena) in image_stubs {
        materialize_domain(instance, control, &sites, &mut closures)?;
        instance.image_entry_stubs.push((home, sites, arena));
    }
    // The fixed arenas are link-time address allocators only. Runtime executes the direct
    // per-Engine closures above, so retaining those mappings would reintroduce conflicts.
    instance.entry_stubs = Default::default();
    for (_, _, arena) in &mut instance.image_entry_stubs {
        *arena = Default::default();
    }
    instance.rebuild_fn_addrs();
    instance.apply_frozen_relocs(module)?;
    Ok(closures)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PanicProbe;

    unsafe extern "C-unwind" fn panic_probe(
        _cif: &ffi_cif,
        _result: &mut u64,
        _args: *const *const c_void,
        _data: &PanicProbe,
    ) {
        std::panic::panic_any(0x18u8);
    }

    #[test]
    fn callback_selector_keeps_distinct_abi_wrappers() {
        assert_eq!(callback_for(false) as *const (), trampoline_c as *const ());
        assert_eq!(
            callback_for(true) as *const (),
            trampoline_c_unwind as *const ()
        );
    }

    #[test]
    fn libffi_closure_preserves_c_unwind_callback() {
        let callback: unsafe extern "C-unwind" fn(
            &ffi_cif,
            &mut u64,
            *const *const c_void,
            &PanicProbe,
        ) = panic_probe;
        // SAFETY: same as callback_for: only pass the address to libffi; actual entry is still
        // C-unwind; the test below also calls the closure code through the C-unwind type.
        let callback: libffi::low::Callback<PanicProbe, u64> = unsafe {
            std::mem::transmute::<
                unsafe extern "C-unwind" fn(&ffi_cif, &mut u64, *const *const c_void, &PanicProbe),
                libffi::low::Callback<PanicProbe, u64>,
            >(callback)
        };
        let data = PanicProbe;
        let closure = Closure::new(
            Cif::new(
                std::iter::empty::<libffi::middle::Type>(),
                libffi::middle::Type::u64(),
            ),
            callback,
            &data,
        );
        let code: &unsafe extern "C-unwind" fn() -> u64 = unsafe { closure.instantiate_code_ptr() };

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { code() }));
        let payload = caught.expect_err("C-unwind callback should not swallow panic");
        assert_eq!(payload.downcast_ref::<u8>(), Some(&0x18));
    }
}
