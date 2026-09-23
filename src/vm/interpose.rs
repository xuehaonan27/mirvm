//! The runtime calls a self-produced image must not reach directly.
//!
//! An image built from a guest's native objects borrows the host's C library for everything it does
//! not define itself. A few of those calls the engine has to own rather than let through: a thread
//! the guest creates must be one the engine accounts for before it can be waited on, and a signal
//! disposition the guest installs must be one the engine's own handler can still route. Such a call
//! reaches a replacement the engine installs, and *which* calls those are is engine policy -- it is
//! the set `crate::vm::deferred` and `crate::vm::signal` carry replacements for, not a fact about an
//! object format or a CPU.
//!
//! Two layers below turn this list into a link, and neither is this one: `crate::os::linker` says
//! how a platform's linker is told to redirect a call, and `crate::arch::asmstub` holds the machine
//! code that receives it.

/// The C symbols an image must not reach directly, by the name it would call them by.
///
/// A platform's linker redirects each `<name>` to a bridge entry `__wrap_<name>`, which passes the
/// owning engine on to the replacement through the slot `__mirvm_<name>_target`;
/// `crate::vm::native_instance::wire` fills those slots before any constructor runs.
pub(crate) const INTERPOSED_CALLS: &[&str] = &[
    "pthread_create",
    "pthread_key_create",
    "pthread_setspecific",
    "pthread_key_delete",
    "signal",
    "sigaction",
    "raise",
];
