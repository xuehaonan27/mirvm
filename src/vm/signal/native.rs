//! The interposed entries a self-produced native image calls, and the SA_SIGINFO adapter the
//! kernel frame reaches: the fixed-stub materialization, the three `native_*` bridges, and the
//! `errno` they post.

use super::*;

/// Runtime bridge used by self-produced native archive images. The hidden
/// owner argument identifies the Engine whose P1 entry address may appear as
/// `handler`; handlers inside a MIRVM-produced image are deferred too.
pub(crate) unsafe extern "C-unwind" fn native_signal(
    signum: i32,
    handler: usize,
    owner: u64,
) -> usize {
    let Some(control) = super::super::ctx::control_for_engine(owner) else {
        set_errno(ESRCH);
        return crate::os::signal::SIG_ERR;
    };
    let Ok(lease) = super::super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(ESRCH);
        return crate::os::signal::SIG_ERR;
    };
    let _activation = super::super::ctx::activate(lease.shared());
    let resolution = super::super::thunks::resolve_signal_handler(lease.shared(), handler as u64);
    match install_sigaction_value(
        &control,
        signum,
        Some(Sigaction::for_signal(handler)),
        Some(resolution),
    ) {
        Ok(old) => old.handler(),
        Err(SignalError::Libc { errno, .. }) => {
            set_errno(errno);
            crate::os::signal::SIG_ERR
        }
        Err(SignalError::Contract(message)) => crate::vm::unwind::engine_abort(&message),
    }
}

/// The interposed `sigaction`: `action` and `oldact` are the caller's own structures,
/// which is why this entry point names them rather than a guest address.
pub(crate) unsafe extern "C-unwind" fn native_sigaction(
    signum: i32,
    action: *const Sigaction,
    oldact: *mut Sigaction,
    owner: u64,
) -> i32 {
    let Some(control) = super::super::ctx::control_for_engine(owner) else {
        set_errno(ESRCH);
        return -1;
    };
    let Ok(lease) = super::super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(ESRCH);
        return -1;
    };
    let _activation = super::super::ctx::activate(lease.shared());
    let action = unsafe { action.as_ref() }.copied();
    let resolution = action.as_ref().map(|action| {
        super::super::thunks::resolve_signal_handler(lease.shared(), action.handler() as u64)
    });
    match install_sigaction_value(&control, signum, action, resolution) {
        Ok(old) => {
            old.write_to(oldact as u64);
            0
        }
        Err(SignalError::Libc { errno, .. }) => {
            set_errno(errno);
            -1
        }
        Err(SignalError::Contract(message)) => crate::vm::unwind::engine_abort(&message),
    }
}

pub(crate) unsafe extern "C-unwind" fn native_raise(signum: i32, owner: u64) -> i32 {
    let Some(control) = super::super::ctx::control_for_engine(owner) else {
        set_errno(ESRCH);
        return -1;
    };
    let Ok(lease) = super::super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(ESRCH);
        return -1;
    };
    let activation = super::super::ctx::activate(lease.shared());
    super::super::ctx::raise_signal(activation.ctx(), signum)
}

pub(crate) fn set_errno(value: i32) {
    crate::os::process::set_errno(value);
}

pub(crate) fn materialize_signal_stub(
    registration: &'static SignalRegistration,
) -> Result<usize, String> {
    // The entry stub's bytes are the pair's (the kernel's SA_SIGINFO entry contract as the CPU
    // encodes it); where they go and that they must end up read-execute is the engine's.
    let page_size = crate::os::mem::page_size();
    let page = crate::os::mem::map_anon(page_size, crate::os::mem::Prot::RW, false);
    if page.is_null() {
        return Err("mmap for fixed signal stub failed".into());
    }
    let code = crate::os_arch::signal::entry_stub_bytes(
        ptr::from_ref(registration) as usize,
        signal_adapter as *const () as usize,
    );
    unsafe { ptr::copy_nonoverlapping(code.as_ptr(), page, code.len()) };
    if let Err(error) = crate::os::mem::protect(page, page_size, crate::os::mem::Prot::RX) {
        unsafe { crate::os::mem::unmap(page, page_size) };
        return Err(format!("failed to seal fixed signal stub RX: {error}"));
    }
    Ok(page as usize)
}

unsafe extern "C" fn signal_adapter(
    signum: i32,
    info: SignalInfo,
    _context: *mut std::ffi::c_void,
    registration: *mut SignalRegistration,
) {
    unsafe { record_async_signal(registration, signum, info) };
}
