//! Unit tests for the signal module: the registry chain, installation commit
//! and rollback, kernel normalization, deferred inbox delivery and Engine
//! close.
//!
//! The siblings below hold one question each; shared fixtures live here.

mod chain;
mod close;
mod delivery;
mod normalization;
mod rollback;

use super::inbox::current_thread_inbox;
use super::*;
use crate::os::process::{exit_now, getpid};
use crate::os::signal::{
    MaskOp, SA_EXPOSE_TAGBITS, SA_NOCLDSTOP, SA_RESTART, SA_SIGINFO, SA_UNSUPPORTED, SIGURG,
    SIGUSR1, SIGUSR2, SIGWINCH, SignalMask, kill, send_to_thread, set_thread_mask,
};
use crate::os::thread::current_thread;
use crate::os_arch::signal::RESTORER_FLAG;
use crate::vm::ctx::Shared;
use crate::vm::ir::Module;

use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

static DETACHED_CLOSE_NATIVE_RAN: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn detached_close_native_handler(_signum: i32) {
    DETACHED_CLOSE_NATIVE_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn detached_close_external_handler(_signum: i32) {}

extern "C" fn first_test_restorer() {}

fn run_signal_test_child(test_name: &str, child_env: &str) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(child_env, "1")
        .output()
        .expect("failed to start isolated signal test child")
}

struct RestoreSignal {
    signum: i32,
    action: Sigaction,
}

impl Drop for RestoreSignal {
    fn drop(&mut self) {
        let _ = self.action.replace_exact(self.signum);
    }
}

fn control() -> Arc<EngineControl> {
    Arc::clone(Shared::new(Module::default()).control())
}

fn registration(
    control: &Arc<EngineControl>,
    signum: i32,
    func: FuncId,
    visible: Sigaction,
) -> &'static SignalRegistration {
    SignalRegistration::new(
        Arc::clone(control),
        DeferredSignalCallback::Guest(func),
        signum,
        visible,
    )
}

fn descriptor(
    control: &Arc<EngineControl>,
    registration: &'static SignalRegistration,
    visible: Sigaction,
    kernel: Sigaction,
    fallback: SignalChain,
) -> StubDescriptor {
    StubDescriptor {
        install_control: Arc::clone(control),
        install_owner: control.id(),
        callback_owner: registration.control().id(),
        registration,
        visible,
        kernel,
        accepted_kernel: Some(kernel),
        fallback,
    }
}

fn node(
    control: &Arc<EngineControl>,
    guest: Sigaction,
    kernel: Sigaction,
    registration: Option<&'static SignalRegistration>,
) -> DispositionNode {
    DispositionNode {
        install_control: Arc::clone(control),
        install_owner: control.id(),
        callback_owner: registration.map(|registration| registration.control().id()),
        guest,
        kernel,
        accepted_kernel: Some(kernel),
        registration,
    }
}
