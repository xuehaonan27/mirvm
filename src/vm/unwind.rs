//! Engine-owned exception class, raw unwind classification, and returning an uncaught guest
//! panic to the guest standard library.

use std::ptr::NonNull;
use std::sync::Arc;

use crate::os::unwind::RawException;

const CLASS: u64 = u64::from_ne_bytes(*b"MIRVM\0RS");
const RUST_CLASS: u64 = u64::from_ne_bytes(*b"MOZ\0RUST");
const ABI_COOKIE: u64 = 1;
static CANARY: u8 = 0;

#[repr(C, align(16))]
struct MirvmException {
    header: RawException,
    abi_cookie: u64,
    canary: *const u8,
    payload: StoredPayload,
    control: Arc<super::ctx::EngineControl>,
    /// A guest exception may be retained by a native catch after the thunk's
    /// execution lease has unwound. Keep teardown in Closing until the
    /// unwinder consumes or deletes that exception.
    _hold: Option<super::deferred::DeferredHold>,
}

enum StoredPayload {
    GuestPanic {
        inner: u64,
        shared: Arc<super::ctx::Shared>,
    },
    EngineFault {
        message: String,
        code: i32,
        token: super::ctx::EngineFaultToken,
        shared: Arc<super::ctx::Shared>,
    },
    EngineClosed,
    Empty,
}

extern "C" fn cleanup(_: i32, exception: *mut RawException) {
    let exception = exception.cast::<MirvmException>();
    let payload = unsafe { &(*exception).payload };
    if matches!(payload, StoredPayload::Empty) {
        unsafe { drop(Box::from_raw(exception)) };
        return;
    }

    match payload {
        StoredPayload::GuestPanic { .. } => {
            eprintln!("mirvm[m4-engine]: guest panic was caught but not rethrown")
        }
        StoredPayload::EngineFault { .. } => {
            eprintln!("mirvm[m4-engine]: EngineFault was caught but not consumed by its owner")
        }
        StoredPayload::EngineClosed => {
            eprintln!("mirvm[m4-engine]: EngineClosed was caught but not consumed")
        }
        StoredPayload::Empty => unreachable!(),
    }
    std::process::abort()
}

fn raise_payload(
    control: Arc<super::ctx::EngineControl>,
    payload: StoredPayload,
    hold: Option<super::deferred::DeferredHold>,
    description: &str,
) -> ! {
    let exception = Box::new(MirvmException {
        header: RawException {
            class: CLASS,
            cleanup: Some(cleanup),
            private: [std::ptr::null(); 2],
        },
        abi_cookie: ABI_COOKIE,
        canary: &CANARY,
        payload,
        control,
        _hold: hold,
    });
    let exception = Box::into_raw(exception);
    let reason = unsafe { crate::os::unwind::raise(&raw mut (*exception).header) };
    eprintln!("mirvm[m4-engine]: {description} reached the end of the host stack ({reason})");
    std::process::abort()
}

pub fn raise_guest(shared: Arc<super::ctx::Shared>, inner: u64) -> ! {
    if inner == 0 {
        eprintln!("mirvm[m4-engine]: attempted to raise a null guest panic exception");
        std::process::abort();
    }
    let control = Arc::clone(shared.control());
    let hold = super::deferred::DeferredHold::acquire(&control, true).unwrap_or_else(|_| {
        eprintln!("mirvm[m4-engine]: guest panic was raised after its Engine began finalizing");
        std::process::abort()
    });
    raise_payload(
        control,
        StoredPayload::GuestPanic { inner, shared },
        Some(hold),
        "guest panic",
    )
}

pub(crate) fn raise_engine_fault(ctx: *mut super::ctx::Ctx, message: String, code: i32) -> ! {
    let (shared, token) = super::ctx::begin_engine_fault(ctx);
    let control = Arc::clone(shared.control());
    let hold = super::deferred::DeferredHold::acquire(&control, true).unwrap_or_else(|_| {
        eprintln!("mirvm[m4-engine]: EngineFault was raised after its Engine began finalizing");
        std::process::abort()
    });
    raise_payload(
        control,
        StoredPayload::EngineFault {
            message,
            code,
            token,
            shared,
        },
        Some(hold),
        "EngineFault",
    )
}

/// Runs `f` under an unwind action: a panic escaping a `Terminate` edge aborts (a double panic,
/// or a guest `extern "C"` boundary) instead of resuming.
pub(crate) fn guarding_terminate<R>(unwind: &super::ir::UnwindAction, f: impl FnOnce() -> R) -> R {
    if let super::ir::UnwindAction::Terminate = unwind {
        guard_terminate(f)
    } else {
        f()
    }
}

/// Aborts the running activation with a diagnostic: an unrecoverable violation of a MIRVM
/// invariant (never a guest fault), raised as an EngineFault so it surfaces on the engine's own
/// error path instead of as a bare host abort. Every layer that can detect such a violation --
/// the interpreter, the JIT helpers, the signal path, the thunk factory -- calls this one.
pub(crate) fn engine_abort(what: &str) -> ! {
    let ctx = super::ctx::current();
    raise_engine_fault(ctx, what.to_owned(), crate::diag::exit::SOFTWARE.into())
}

/// Raises a guest panic from the running activation: the inner pointer stays entirely owned by
/// the guest standard library, while the outer wrapper only tags the MIRVM exception identity
/// and the Engine whose `Ctx` is current.
pub(crate) fn raise_guest_in_current_engine(inner: u64) -> ! {
    let shared = unsafe { (*super::ctx::current()).shared_arc() };
    raise_guest(shared, inner)
}

pub(crate) fn raise_engine_closed(control: Arc<super::ctx::EngineControl>) -> ! {
    // This exception is created precisely because no execution lease can be
    // acquired. It never returns to guest code and therefore owns no hold.
    raise_payload(control, StoredPayload::EngineClosed, None, "EngineClosed")
}

struct CatchState<F, R> {
    f: std::mem::ManuallyDrop<F>,
    result: std::mem::ManuallyDrop<Option<R>>,
    exception: *mut RawException,
}

#[derive(Debug)]
pub struct CaughtException {
    raw: NonNull<RawException>,
}

unsafe fn call<F: FnOnce() -> R, R>(state: *mut CatchState<F, R>) {
    let f = unsafe { std::mem::ManuallyDrop::take(&mut (*state).f) };
    unsafe { *(*state).result = Some(f()) };
}

#[rustc_nounwind]
unsafe fn caught<F, R>(state: *mut CatchState<F, R>, exception: *mut u8) {
    unsafe { (*state).exception = exception.cast() };
}

pub fn catch_raw<F: FnOnce() -> R, R>(f: F) -> Result<R, CaughtException> {
    let mut state = CatchState {
        f: std::mem::ManuallyDrop::new(f),
        result: std::mem::ManuallyDrop::new(None),
        exception: std::ptr::null_mut(),
    };
    let did_catch = unsafe { std::intrinsics::catch_unwind(call, &raw mut state, caught) };
    if did_catch {
        Err(CaughtException {
            raw: NonNull::new(state.exception).expect("raw catch returned a null exception"),
        })
    } else {
        Ok(unsafe { std::mem::ManuallyDrop::take(&mut state.result) }
            .expect("raw catch result missing"))
    }
}

impl CaughtException {
    fn mirvm(&self) -> Option<&MirvmException> {
        let raw = self.raw.as_ptr();
        let own_cleanup: extern "C" fn(i32, *mut RawException) = cleanup;
        if unsafe { (*raw).class } != CLASS
            || !(raw as usize).is_multiple_of(std::mem::align_of::<MirvmException>())
            || !unsafe { (*raw).cleanup }
                .is_some_and(|candidate| std::ptr::fn_addr_eq(candidate, own_cleanup))
        {
            return None;
        }
        let exception = raw.cast::<MirvmException>();
        if unsafe {
            (*exception).abi_cookie != ABI_COOKIE || !std::ptr::eq((*exception).canary, &CANARY)
        } {
            return None;
        }
        Some(unsafe { &*exception })
    }

    pub fn take_mirvm(
        self,
        expected_owner: &Arc<super::ctx::Shared>,
    ) -> Result<MirvmPayload, Self> {
        let Some(exception) = self.mirvm() else {
            return Err(self);
        };
        if !Arc::ptr_eq(&exception.control, expected_owner.control()) {
            return Err(self);
        }

        let raw = self.raw.as_ptr();
        let exception = raw.cast::<MirvmException>();
        let stored = std::mem::replace(unsafe { &mut (*exception).payload }, StoredPayload::Empty);
        std::mem::forget(self);
        unsafe { crate::os::unwind::delete(raw) };

        match stored {
            StoredPayload::GuestPanic { inner, shared } => {
                Ok(MirvmPayload::Guest(GuestPanicPayload { inner, shared }))
            }
            StoredPayload::EngineFault {
                message,
                code,
                token,
                shared,
            } => Ok(MirvmPayload::EngineFault(EngineFaultPayload {
                message: Some(message),
                code,
                token,
                shared,
                handled: false,
            })),
            StoredPayload::EngineClosed => Ok(MirvmPayload::EngineClosed),
            StoredPayload::Empty => {
                eprintln!("mirvm[m4-engine]: MIRVM exception payload was consumed twice");
                std::process::abort()
            }
        }
    }

    pub(crate) fn take_engine_closed(
        self,
        expected_owner: &Arc<super::ctx::EngineControl>,
    ) -> Result<(), Self> {
        let Some(exception) = self.mirvm() else {
            return Err(self);
        };
        if !Arc::ptr_eq(&exception.control, expected_owner)
            || !matches!(exception.payload, StoredPayload::EngineClosed)
        {
            return Err(self);
        }
        let raw = self.raw.as_ptr();
        let exception = raw.cast::<MirvmException>();
        unsafe { (*exception).payload = StoredPayload::Empty };
        std::mem::forget(self);
        unsafe { crate::os::unwind::delete(raw) };
        Ok(())
    }

    /// Consume only an EngineFault owned by `expected_owner`.
    ///
    /// Signal callbacks can run under an Engine other than the one whose
    /// export initiated the synchronous delivery. The callback owner must
    /// finish its token while that activation is still current, then hand a
    /// plain report back to the initiating Engine. This narrower operation
    /// must not consume guest panics or EngineClosed payloads.
    pub(crate) fn take_engine_fault(
        self,
        expected_owner: &Arc<super::ctx::Shared>,
    ) -> Result<EngineFaultPayload, Self> {
        let Some(exception) = self.mirvm() else {
            return Err(self);
        };
        if !Arc::ptr_eq(&exception.control, expected_owner.control())
            || !matches!(exception.payload, StoredPayload::EngineFault { .. })
        {
            return Err(self);
        }

        let payload = self
            .take_mirvm(expected_owner)
            .expect("classified owner EngineFault must remain consumable");
        let MirvmPayload::EngineFault(fault) = payload else {
            unreachable!("EngineFault classification changed while consuming payload")
        };
        Ok(fault)
    }

    pub fn is_rust(&self) -> bool {
        unsafe { (*self.raw.as_ptr()).class == RUST_CLASS }
    }

    pub(crate) fn delete_foreign_for_startup(self) -> Result<(), Self> {
        if self.mirvm().is_some() || self.is_rust() {
            return Err(self);
        }
        let raw = self.raw.as_ptr();
        std::mem::forget(self);
        unsafe { crate::os::unwind::delete(raw) };
        Ok(())
    }

    pub(crate) fn is_engine_fault(&self) -> bool {
        self.mirvm()
            .is_some_and(|exception| matches!(exception.payload, StoredPayload::EngineFault { .. }))
    }

    /// Apply the exact native `std::panic::catch_unwind` boundary policy.
    /// Only this Engine's guest panic is caught. Engine faults, another
    /// Engine's payload and host Rust panics keep unwinding; C++ and other
    /// foreign exceptions are destroyed and then abort the process.
    pub(crate) fn at_guest_catch(
        self,
        expected_owner: &Arc<super::ctx::Shared>,
    ) -> GuestCatchDisposition {
        if let Some(exception) = self.mirvm() {
            let owned_guest = Arc::ptr_eq(&exception.control, expected_owner.control())
                && matches!(exception.payload, StoredPayload::GuestPanic { .. });
            if owned_guest {
                let payload = self
                    .take_mirvm(expected_owner)
                    .expect("classified MIRVM exception must remain MIRVM");
                let MirvmPayload::Guest(payload) = payload else {
                    unreachable!("guest classification changed while consuming exception")
                };
                return GuestCatchDisposition::Guest(payload);
            }
            return GuestCatchDisposition::Resume(self);
        }
        if self.is_rust() {
            return GuestCatchDisposition::Resume(self);
        }
        self.abort_foreign_at_guest_catch()
    }

    pub fn resume_or_rethrow(self) -> ! {
        let raw = self.raw.as_ptr();
        std::mem::forget(self);
        unsafe { crate::os::unwind::resume_or_rethrow(raw) }
    }

    /// Apply a guest MIR `UnwindAction::Terminate` boundary.
    ///
    /// An EngineFault is an interpreter/JIT failure rather than guest ABI
    /// unwind, so it must keep travelling to the Engine that owns it. Every
    /// other exception reaching this boundary follows native Rust's terminate
    /// rule and ends the process.
    pub(crate) fn terminate_or_resume_engine_fault(self) -> ! {
        if self.is_engine_fault() {
            self.resume_or_rethrow()
        }

        std::mem::forget(self);
        eprintln!("mirvm[m4-engine]: unwind reached Terminate boundary (double panic/ABI) — abort");
        std::process::abort()
    }

    /// A second exception while disposing an uncaught panic payload follows
    /// `lang_start`'s double-panic rule. The process is about to abort, so the
    /// newly caught exception must not run its normal "swallowed" cleanup.
    pub(crate) fn abort_during_panic_cleanup(self) -> ! {
        std::mem::forget(self);
        eprintln!("mirvm[m4-engine]: drop of the panic payload panicked");
        std::process::abort()
    }

    fn abort_native_teardown(self) -> ! {
        // Engine teardown has no caller that can own or resume an exception.
        // In particular, resuming an EngineFault would strand the lifecycle in
        // Closing after `finalizer_started` was already published.
        std::mem::forget(self);
        eprintln!("mirvm[m4-engine]: native finalizer unwound during Engine teardown");
        std::process::abort()
    }

    fn abort_foreign_at_guest_catch(self) -> ! {
        let raw = self.raw.as_ptr();
        std::mem::forget(self);
        unsafe { crate::os::unwind::delete(raw) };
        eprintln!("Rust cannot catch foreign exceptions, aborting");
        std::process::abort()
    }
}

/// Classify the exception pointer delivered by a native landing pad.
///
/// The pointer is inspected, never consumed. Ownership remains with the
/// unwinder and the landing pad must eventually resume it.
///
/// # Safety
///
/// `exception` must be the live exception pointer supplied to that landing
/// pad by the platform unwinder.
#[cfg(feature = "cranelift")]
pub(crate) unsafe fn raw_is_engine_fault(exception: *mut u8) -> bool {
    let Some(raw) = NonNull::new(exception.cast::<RawException>()) else {
        return false;
    };
    let exception = CaughtException { raw };
    let result = exception.is_engine_fault();
    std::mem::forget(exception);
    result
}

/// Run a call protected by a guest MIR `Terminate` edge. This is shared by
/// interpreter calls and JIT helpers so both engines classify exceptions in
/// exactly the same place.
pub(crate) fn guard_terminate<R>(f: impl FnOnce() -> R) -> R {
    match catch_raw(f) {
        Ok(result) => result,
        Err(exception) => exception.terminate_or_resume_engine_fault(),
    }
}

/// Native finalizers run after the Engine has committed to one-way teardown.
/// No exception kind may leave this boundary: doing so would skip the rest of
/// finalization and leave every waiter stuck in `Closing`.
pub(crate) fn guard_native_teardown<R>(f: impl FnOnce() -> R) -> R {
    match catch_raw(f) {
        Ok(result) => result,
        Err(exception) => exception.abort_native_teardown(),
    }
}

impl Drop for CaughtException {
    fn drop(&mut self) {
        eprintln!("mirvm[m4-engine]: caught exception was dropped without a disposition");
        std::process::abort()
    }
}

pub(crate) enum GuestCatchDisposition {
    Guest(GuestPanicPayload),
    Resume(CaughtException),
}

pub enum MirvmPayload {
    Guest(GuestPanicPayload),
    EngineFault(EngineFaultPayload),
    EngineClosed,
}

impl std::fmt::Debug for MirvmPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Guest(payload) => payload.fmt(f),
            Self::EngineFault(payload) => payload.fmt(f),
            Self::EngineClosed => f.write_str("EngineClosed"),
        }
    }
}

pub struct GuestPanicPayload {
    inner: u64,
    shared: Arc<super::ctx::Shared>,
}

impl std::fmt::Debug for GuestPanicPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestPanicPayload")
            .field("owner", &self.shared.id)
            .field("inner", &self.inner)
            .finish()
    }
}

impl GuestPanicPayload {
    /// Transfer the guest exception pointer to a guest-side consumer. The
    /// pointer remains live until the callback returns successfully; if that
    /// callback unwinds, Drop aborts instead of silently leaking the payload.
    pub fn transfer<R>(mut self, f: impl FnOnce(&Arc<super::ctx::Shared>, u64) -> R) -> R {
        let result = f(&self.shared, self.inner);
        self.inner = 0;
        result
    }
}

impl Drop for GuestPanicPayload {
    fn drop(&mut self) {
        if self.inner == 0 {
            return;
        }
        eprintln!("mirvm[m4-engine]: guest panic payload was dropped without a disposition");
        std::process::abort()
    }
}

/// Return an uncaught guest panic to the guest standard library before the
/// Engine reports it. `cleanup` lowers the guest panic counter and yields the
/// opaque two-word panic Box; its own guest drop glue then runs the payload's
/// destructor and allocator route. No host-side Rust layout is assumed here.
pub(crate) fn dispose_uncaught_guest_panic(ctx: *mut super::ctx::Ctx, payload: GuestPanicPayload) {
    payload.transfer(|shared, inner| {
        let Some(plan) = shared.module.guest_panic_cleanup else {
            eprintln!("mirvm[m4-engine]: executable Module has no guest panic cleanup plan");
            std::process::abort()
        };
        match catch_raw(|| {
            let (data, vtable) = super::dispatch::call_guest(ctx, plan.cleanup, &[inner]);
            let mut opaque_box = [data, vtable];
            super::dispatch::call_guest(ctx, plan.drop_payload, &[opaque_box.as_mut_ptr() as u64]);
        }) {
            Ok(()) => {}
            Err(exception) => exception.abort_during_panic_cleanup(),
        }
    });
}

pub(crate) fn dispose_guest_panic_during_startup(
    shared: &Arc<super::ctx::Shared>,
    payload: GuestPanicPayload,
) {
    let activation = super::ctx::activate(shared);
    dispose_uncaught_guest_panic(activation.ctx(), payload);
}

pub struct EngineFaultPayload {
    message: Option<String>,
    code: i32,
    token: super::ctx::EngineFaultToken,
    shared: Arc<super::ctx::Shared>,
    handled: bool,
}

impl std::fmt::Debug for EngineFaultPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineFaultPayload")
            .field("owner", &self.shared.id)
            .field("message", &self.message)
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct EngineFaultReport {
    pub message: String,
    pub code: i32,
}

impl EngineFaultPayload {
    pub fn finish(mut self) -> EngineFaultReport {
        super::ctx::finish_engine_fault(self.token);
        self.handled = true;
        EngineFaultReport {
            message: self
                .message
                .take()
                .expect("unhandled EngineFault must retain its message"),
            code: self.code,
        }
    }
}

impl Drop for EngineFaultPayload {
    fn drop(&mut self) {
        if self.handled {
            return;
        }
        eprintln!("mirvm[m4-engine]: EngineFault payload was dropped without owner handling");
        std::process::abort()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_catch_returns_normally() {
        assert_eq!(catch_raw(|| 42).unwrap(), 42);
    }

    extern "C" fn foreign_cleanup(_: i32, _: *mut RawException) {}

    #[repr(C, align(16))]
    struct ShortForeignException(RawException);

    #[test]
    fn colliding_short_foreign_header_is_not_cast_as_a_mirvm_exception() {
        let short = Box::into_raw(Box::new(ShortForeignException(RawException {
            class: CLASS,
            cleanup: Some(foreign_cleanup),
            private: [std::ptr::null(); 2],
        })));
        let raw = unsafe { &raw mut (*short).0 };
        let exception = CaughtException {
            raw: NonNull::new(raw).unwrap(),
        };
        assert!(exception.mirvm().is_none());
        std::mem::forget(exception);
        unsafe { drop(Box::from_raw(short)) };
    }

    #[test]
    fn mirvm_exception_requires_matching_owner() {
        let shared = Arc::new(super::super::ctx::Shared::new(Default::default()));
        let other = Arc::new(super::super::ctx::Shared::new(Default::default()));
        let exception = catch_raw(|| raise_guest(Arc::clone(&shared), 0x1234)).unwrap_err();
        let exception = exception.take_mirvm(&other).unwrap_err();
        let MirvmPayload::Guest(payload) = exception.take_mirvm(&shared).unwrap() else {
            panic!("guest exception changed kind")
        };
        assert_eq!(
            payload.transfer(|owner, inner| (owner.id, inner)),
            (shared.id, 0x1234)
        );
    }

    #[test]
    fn engine_fault_requires_owner_and_clears_thread_state() {
        let shared = Arc::new(super::super::ctx::Shared::new(Default::default()));
        let other = Arc::new(super::super::ctx::Shared::new(Default::default()));
        let activation = super::super::ctx::activate(&shared);
        let exception =
            catch_raw(|| raise_engine_fault(activation.ctx(), "broken bytecode".into(), 70))
                .unwrap_err();
        assert!(super::super::ctx::engine_fault_in_flight());
        let exception = exception.take_mirvm(&other).unwrap_err();
        let payload = exception.take_mirvm(&shared).unwrap();
        let MirvmPayload::EngineFault(fault) = payload else {
            panic!("engine fault changed kind")
        };
        let report = fault.finish();
        assert_eq!(report.message, "broken bytecode");
        assert_eq!(report.code, 70);
        assert!(!super::super::ctx::engine_fault_in_flight());
    }

    #[test]
    fn guest_catch_resumes_other_engine_payload() {
        let shared = Arc::new(super::super::ctx::Shared::new(Default::default()));
        let other = Arc::new(super::super::ctx::Shared::new(Default::default()));
        let exception = catch_raw(|| raise_guest(Arc::clone(&shared), 0x1234)).unwrap_err();
        let GuestCatchDisposition::Resume(exception) = exception.at_guest_catch(&other) else {
            panic!("another Engine's panic must not be caught")
        };
        let MirvmPayload::Guest(payload) = exception.take_mirvm(&shared).unwrap() else {
            panic!("guest exception changed kind")
        };
        payload.transfer(|_, inner| assert_eq!(inner, 0x1234));
    }

    #[test]
    fn rust_exception_can_be_rethrown_to_std() {
        let exception = catch_raw(|| std::panic::panic_any(73_u8)).unwrap_err();
        assert!(exception.is_rust());
        let payload = std::panic::catch_unwind(|| exception.resume_or_rethrow()).unwrap_err();
        assert_eq!(payload.downcast_ref::<u8>(), Some(&73));
    }
}
