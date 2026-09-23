//! The C library services the guest reaches through the engine itself rather than through a
//! foreign call: environment and writes, `fork`, the exit-handler family, signals, and the raw
//! syscall passthrough.
//!
//! Each one is a real `os::` call at the true addresses the guest passed, so none of them
//! marshals. A request the platform refuses hands back what libc would, errno included.

use crate::vm::atexit::{self, Kind as AtexitKind};
use crate::vm::ctx::Ctx;
use crate::vm::ir::Builtin;
use crate::vm::unwind::engine_abort;

/// Resolves a guest signal-handler address to the thunk the signal path must install.
fn resolve_signal_handler(
    ctx: *mut Ctx,
    handler: u64,
) -> crate::vm::thunks::SignalHandlerResolution {
    crate::vm::thunks::resolve_signal_handler(unsafe { (*ctx).shared() }, handler)
}

pub(super) fn exec(ctx: *mut Ctx, builtin: &Builtin, av: &[u64]) -> u64 {
    let a = |i: usize| av[i];
    match builtin {
        // Minimal `os::` passthroughs: real addresses, no marshalling.
        Builtin::HostGetenv => crate::os::process::getenv(a(0)),
        Builtin::HostWrite => crate::os::fs::write_fd(a(0) as i32, a(1), a(2) as usize) as u64,
        Builtin::HostStrlen => crate::os::process::c_strlen(a(0)),
        Builtin::HostAbort => std::process::abort(),
        // fork: allowed only when the guest is single-threaded (the child is a whole-process
        // copy, so interpreter state is consistent by construction, and with no other guest
        // threads there is no cross-thread lock to deadlock on). A multithreaded fork is
        // rejected loudly -- it is just as much a minefield under native. The exec family goes
        // through the foreign path, not here.
        Builtin::HostFork => {
            if unsafe { crate::vm::ctx::guest_spawned_threads(ctx) } {
                engine_abort(
                    "fork() with guest-spawned threads: after a multithreaded fork only the \
                     forking thread survives and locks held by other threads stay locked \
                     forever in the child (UB under native too). Only a single-threaded guest \
                     is allowed through",
                );
            }
            let pid = crate::os::process::fork();
            if pid == 0 {
                // The child has no writer thread and must never publish into
                // the copied parent generation. This hook is store-only and
                // runs before any JIT service is restarted.
                crate::telemetry::capture::after_fork_child();
                // In the child the compiler thread does not survive fork. The publish wait of
                // the SYNC verification mode (MIRVM_JIT_SYNC) depends on a live compilation
                // service, so restart it: inherited published code pages, slot tables and
                // eh_frames stay valid while the queue and worker are replaced. A non-sync
                // child keeps the unchanged interpret-as-fallback semantics.
                #[cfg(feature = "cranelift")]
                if unsafe { &*(*ctx).shared }.jit.sync {
                    let shared = unsafe { (*ctx).shared_arc() };
                    crate::vm::jit::start(&shared);
                }
            }
            pid as u64
        }
        // atexit family: registers a guest callback and returns 0 (success).
        // __cxa_atexit(fn, arg, dso) calls fn(arg); on_exit(fn, arg) calls fn(status, arg);
        // atexit(fn) calls fn with no arguments. All are stored uniformly as (fn, kind, arg).
        Builtin::HostAtexit => atexit::register(ctx, a(0), AtexitKind::Plain, 0),
        Builtin::HostCxaAtexit => atexit::register(ctx, a(0), AtexitKind::CxaArg, a(1)),
        Builtin::HostOnExit => atexit::register(ctx, a(0), AtexitKind::OnExit, a(1)),
        Builtin::HostSignal => {
            let (signum, handler) = (a(0) as i32, a(1) as usize);
            let resolution =
                if handler == crate::os::signal::SIG_DFL || handler == crate::os::signal::SIG_IGN {
                    crate::vm::thunks::SignalHandlerResolution::Unknown
                } else {
                    resolve_signal_handler(ctx, handler as u64)
                };
            let control = unsafe { (*ctx).shared().control() };
            match crate::vm::signal::install_signal_resolved(control, signum, handler, resolution) {
                Ok(old) => old as u64,
                Err(error) => {
                    if let Some(errno) = error.libc_errno() {
                        crate::os::process::set_errno(errno);
                        crate::os::signal::SIG_ERR as u64
                    } else {
                        engine_abort(&error.to_string())
                    }
                }
            }
        }
        Builtin::HostRaise => crate::vm::ctx::raise_signal(ctx, a(0) as i32) as u64,
        Builtin::HostSigaction => {
            let (signum, act, oldact) = (a(0) as i32, a(1), a(2));
            let action = unsafe { crate::os::signal::Sigaction::copy_from(act) };
            let resolution = action.as_ref().map(|action| {
                let handler = action.handler();
                if handler == crate::os::signal::SIG_DFL || handler == crate::os::signal::SIG_IGN {
                    crate::vm::thunks::SignalHandlerResolution::Unknown
                } else {
                    resolve_signal_handler(ctx, handler as u64)
                }
            });
            let control = unsafe { (*ctx).shared().control() };
            match crate::vm::signal::install_sigaction_resolved(
                control, signum, action, resolution, oldact,
            ) {
                Ok(result) => result as u64,
                Err(error) => {
                    if let Some(errno) = error.libc_errno() {
                        crate::os::process::set_errno(errno);
                        (-1i32) as u64
                    } else {
                        engine_abort(&error.to_string())
                    }
                }
            }
        }
        Builtin::HostSyscall => crate::os::process::syscall(a(0) as i64, &av[1..]) as u64,
        Builtin::HostSyscallTrace => {
            crate::telemetry::capture::host_syscall(a(0) as i64, &av[1..]) as u64
        }
        _ => unreachable!("non-host builtin reached the host-service family"),
    }
}
