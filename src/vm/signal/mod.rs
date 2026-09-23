//! Process-wide signal dispositions with per-Engine deferred delivery.
//!
//! A disposition is a chain: the guest's request, the fixed stub MIRVM installs in front of it,
//! and whatever the kernel accepted, for one signal and one owning Engine. The module is split so
//! that each question about that chain has one home: [`registry`] holds the chain and reconciles
//! it, [`install`] performs an installation and its rollback, [`kernel`] translates between the
//! request and the action the kernel holds, [`engine`] answers what an Engine's close must do, and
//! [`native`] is the interposed entry points a native image calls.

mod inbox;

use std::collections::{HashMap, HashSet};
use std::ptr;
use std::sync::{Arc, LazyLock, Mutex};

use super::ctx::EngineControl;
use super::ir::FuncId;
use crate::os::process::ESRCH;
use crate::os::signal::{Sigaction, SignalInfo};

pub(crate) use inbox::{
    HostRaiseAttempt, SignalDeliveryGuard, SignalInbox, SignalRegistration, activate_owner,
    current_thread_has_pending, current_thread_has_pending_for_engine,
    deactivate_current_thread_inbox, initialize_current_thread_inbox, record_async_signal,
    restore_owner, take_current_thread_delivery,
};

mod engine;
mod install;
mod kernel;
mod native;
mod registry;

pub(crate) use engine::*;
pub(crate) use install::*;
pub(crate) use kernel::*;
pub(crate) use native::*;
pub(crate) use registry::*;

pub(crate) const SIGNAL_SLOTS: usize = (crate::os::signal::STANDARD_SIGNAL_MAX as usize) + 1;

/// Code executed later at an ordinary VM safe point. Even handlers originating
/// in MIRVM-produced native images use this path: running them in the kernel
/// signal frame would let wrapped libc calls and P1 callbacks re-enter the VM
/// while it is interrupted at an arbitrary instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeferredSignalCallback {
    Guest(FuncId),
    ImageNative(usize),
}

#[cfg(test)]
type AfterInstallReplaceHook = Box<dyn FnOnce(i32, Sigaction, Sigaction) + Send>;

#[cfg(test)]
static AFTER_INSTALL_REPLACE_HOOK: LazyLock<Mutex<Option<AfterInstallReplaceHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
struct AfterInboxClearHook {
    registration: usize,
    callback: Box<dyn FnOnce() + Send>,
}

#[cfg(test)]
static AFTER_INBOX_CLEAR_HOOK: LazyLock<Mutex<Option<AfterInboxClearHook>>> =
    LazyLock::new(|| Mutex::new(None));

#[cfg(test)]
fn run_after_install_replace_hook(signum: i32, requested: Sigaction, actual_old: Sigaction) {
    let hook = AFTER_INSTALL_REPLACE_HOOK.lock().unwrap().take();
    if let Some(hook) = hook {
        hook(signum, requested, actual_old);
    }
}

#[cfg(test)]
fn run_after_inbox_clear_hook(registration: &'static SignalRegistration) {
    let registration = ptr::from_ref(registration) as usize;
    let hook = {
        let mut hook = AFTER_INBOX_CLEAR_HOOK.lock().unwrap();
        if hook
            .as_ref()
            .is_some_and(|hook| hook.registration == registration)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        (hook.callback)();
    }
}

#[derive(Debug)]
pub(crate) enum SignalError {
    /// libc rejected an otherwise ordinary signal operation. The embedding
    /// surface must return the libc sentinel and preserve this errno.
    Libc {
        operation: &'static str,
        signum: i32,
        errno: i32,
    },
    /// MIRVM cannot faithfully implement the requested semantics or detected
    /// corruption of its own disposition bookkeeping.
    Contract(String),
}

impl SignalError {
    fn libc(operation: &'static str, signum: i32, errno: i32) -> Self {
        Self::Libc {
            operation,
            signum,
            errno,
        }
    }

    fn contract(message: impl Into<String>) -> Self {
        Self::Contract(message.into())
    }

    pub(crate) fn libc_errno(&self) -> Option<i32> {
        match self {
            Self::Libc { errno, .. } => Some(*errno),
            Self::Contract(_) => None,
        }
    }
}

impl std::fmt::Display for SignalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Libc {
                operation,
                signum,
                errno,
            } => write!(
                f,
                "{operation} for signal {signum} failed: {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::Contract(message) => f.write_str(message),
        }
    }
}

type SignalResult<T> = Result<T, SignalError>;

#[cfg(test)]
mod tests;
