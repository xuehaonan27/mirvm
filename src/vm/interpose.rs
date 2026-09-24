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
//! code that receives it. What a bridge entry is *named*, which slot carries the engine and which
//! carries the replacement are all mirvm's own vocabulary rather than the platform's, which is why
//! they are derived here once instead of being written out in the bridge text and again in
//! `crate::vm::native_instance::wire`, which fills the slots.

/// The C symbols an image must not reach directly, by the name it would call them by.
pub(crate) const INTERPOSED_CALLS: &[&str] = &[
    "pthread_create",
    "pthread_key_create",
    "pthread_setspecific",
    "pthread_key_delete",
    "signal",
    "sigaction",
    "raise",
];

/// The slot holding the engine that owns `call`, or `None` when `call` is not one a bridge entry
/// routes through an engine-wide owner.
///
/// The engine is one value per family rather than one per call, because the replacements of a
/// family are all reached with the same Engine: a thread the guest creates and a thread-local key
/// it destroys belong to the same one.
pub(crate) fn owner_slot(call: &str) -> Option<&'static str> {
    match call {
        "pthread_create" | "pthread_key_create" | "pthread_setspecific" | "pthread_key_delete" => {
            Some("__mirvm_pthread_owner")
        }
        "signal" | "sigaction" | "raise" => Some("__mirvm_signal_owner"),
        _ => None,
    }
}

/// The slot holding the replacement `call` is routed to, which a bridge entry branches through and
/// `crate::vm::native_instance::wire` fills.
pub(crate) fn target_slot(call: &str) -> String {
    format!("__mirvm_{call}_target")
}

#[cfg(test)]
mod tests {
    use super::{INTERPOSED_CALLS, owner_slot, target_slot};

    /// The bridge and the code that fills its slots have to agree on every name, and they agree by
    /// both asking these functions: a call with no owner slot would be one the bridge could not
    /// route, and a typo here would be a slot wired to nothing.
    #[test]
    fn every_interposed_call_has_an_owner_and_a_target() {
        for call in INTERPOSED_CALLS {
            assert!(owner_slot(call).is_some(), "`{call}` has no owner slot");
            assert!(target_slot(call).starts_with("__mirvm_"));
            assert!(target_slot(call).ends_with("_target"));
        }
        assert_eq!(owner_slot("not_a_call"), None);
    }
}
