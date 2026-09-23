//! The guest unwinder's builtins: raising and deleting an exception, walking the backtrace,
//! reading an unwind context, and `rust_try`'s raw catch.
//!
//! Only a guest panic owned by the current Engine is consumed here; a foreign or host exception
//! keeps unwinding, which is what stops a guest `catch_unwind` from swallowing a native
//! exception.

use crate::vm::ctx::Ctx;
use crate::vm::dispatch::call_fn_addr;
use crate::vm::ir::{Builtin, BuiltinCallRole, FfiKind, Module, Width};
use crate::vm::semantics::memory::{mem_read, mem_write};
use crate::vm::unwind::raise_guest_in_current_engine;

pub(super) fn exec(
    ctx: *mut Ctx,
    module: &Module,
    builtin: &Builtin,
    av: &[u64],
    role: BuiltinCallRole,
) -> u64 {
    let a = |i: usize| av[i];
    match builtin {
        // Unwind primitive (raise): the host unwinder carries the guest exception pointer.
        Builtin::UnwindRaise => raise_guest_in_current_engine(a(0)),
        Builtin::UnwindDeleteException => {
            // Itanium `_Unwind_Exception`: exception_class @0, cleanup fn @8. A guest panic's
            // cleanup is a frozen fn entry, but a foreign exception may carry a native cleanup
            // too, so the address domain picks interpretation or native FFI.
            let exc = a(0);
            let cleanup = mem_read(exc + 8, Width::W64);
            if cleanup != 0 {
                let cav = [1, exc]; // _URC_FOREIGN_EXCEPTION_CAUGHT
                if module.fn_addrs.contains_key(&cleanup) {
                    call_fn_addr(ctx, cleanup, &cav, "_Unwind_DeleteException");
                } else {
                    let sig = crate::vm::ir::ForeignSig {
                        args: vec![FfiKind::I32, FfiKind::Ptr],
                        ret: FfiKind::Void,
                        fixed: None,
                        thunk_args: vec![],
                        unwind: false,
                    };
                    crate::vm::ffi::call_addr(cleanup as usize, &sig, &cav, None);
                }
            }
            0
        }
        // backtrace shadow frames
        Builtin::UnwindBacktrace => crate::vm::backtrace::unwind_backtrace(ctx, a(0), a(1)),
        Builtin::UnwindGetIp => mem_read(a(0), Width::W64),
        Builtin::UnwindGetIpInfo => {
            // (ctx, *ip_before_insn) -> IP; *ip_before_insn = 0 (a synthetic frame has no
            // such distinction)
            if a(1) != 0 {
                mem_write(a(1), Width::W32, 0);
            }
            mem_read(a(0), Width::W64)
        }
        Builtin::UnwindGetCfa => mem_read(a(0) + 8, Width::W64),
        // A synthetic IP is the function entry, so return ip itself (the enclosing fn start).
        Builtin::UnwindFindEnclosing => a(0),
        // rust_try: a raw unwinder catch. Only a guest panic owned by the current Engine
        // calls catch_fn(data, exc) and returns 1; foreign or host exceptions keep unwinding.
        Builtin::CatchUnwind => {
            let (try_fn, data, catch_fn) = (a(0), a(1), a(2));
            let shared = unsafe { (*ctx).shared_arc() };
            let mut main_catch = crate::vm::ctx::claim_main_panic_catch(ctx, role);
            match crate::vm::unwind::catch_raw(|| {
                call_fn_addr(ctx, try_fn, &[data], "catch_unwind.try")
            }) {
                Ok(_) => 0,
                Err(exception) => match exception.at_guest_catch(&shared) {
                    crate::vm::unwind::GuestCatchDisposition::Guest(payload) => {
                        if let Some(main_catch) = &mut main_catch {
                            main_catch.mark_panicked();
                        }
                        payload.transfer(|_, inner| {
                            call_fn_addr(ctx, catch_fn, &[data, inner], "catch_unwind.catch");
                            1
                        })
                    }
                    crate::vm::unwind::GuestCatchDisposition::Resume(exception) => {
                        exception.resume_or_rethrow()
                    }
                },
            }
        }
        _ => unreachable!("non-unwind builtin reached the unwinder family"),
    }
}
