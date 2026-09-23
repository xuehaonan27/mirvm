//! Kindle translation both ways: reading the action the kernel actually holds, comparing it with
//! what was requested or recorded, and canonicalizing it back to the caller-visible form.

use super::*;

pub(crate) fn kernel_current(signum: i32) -> SignalResult<Sigaction> {
    Sigaction::query(signum).map_err(|errno| SignalError::libc("sigaction query", signum, errno))
}

/// Recheck a HostRaise retry outside the signal frame. A close may have won
/// after the kernel selected its old fixed stub, in which case retrying the
/// now-current disposition is correct. If a raw writer instead restored that
/// same inactive stub, another raise would only repeat forever.
pub(crate) fn host_raise_retry_is_stale(signum: i32) -> SignalResult<bool> {
    let current = kernel_current(signum)?;
    let registry = REGISTRY.lock().unwrap();
    let Some(descriptor) = registry.stubs.get(&current.handler()) else {
        return Ok(false);
    };
    // A raw writer may restore a closed stub while also changing its mask,
    // flags, or restorer. The inactive handler identity alone makes another
    // raise terminal: the kernel would select the same dead registration
    // again, regardless of whether the surrounding disposition still matches
    // MIRVM's recorded action.
    Ok(descriptor.registration.signum() == signum
        && !descriptor.registration.accepts_kernel_delivery())
}

// libc supplies its private SA_RESTORER detail while installing an action.
// `expected` may therefore be the caller's exact request while `actual` is a
// kernel oldact captured by a raw writer. All caller-controlled flags and the
// full mask still have to match.
pub(crate) fn kernel_request_matches(actual: &Sigaction, expected: &Sigaction) -> bool {
    actual.same_disposition(expected) || actual.is_kernel_normalization_of(expected)
}

pub(crate) fn recorded_kernel_matches(
    actual: &Sigaction,
    requested: &Sigaction,
    accepted: Option<&Sigaction>,
) -> bool {
    accepted.is_some_and(|accepted| actual.same_disposition(accepted))
        || kernel_request_matches(actual, requested)
}

pub(crate) fn node_kernel_matches(actual: &Sigaction, node: &DispositionNode) -> bool {
    recorded_kernel_matches(actual, &node.kernel, node.accepted_kernel.as_ref())
}

pub(crate) fn descriptor_kernel_matches(actual: &Sigaction, descriptor: &StubDescriptor) -> bool {
    recorded_kernel_matches(
        actual,
        &descriptor.kernel,
        descriptor.accepted_kernel.as_ref(),
    )
}

pub(crate) fn node_kernel_action(node: &DispositionNode) -> Sigaction {
    node.accepted_kernel.unwrap_or(node.kernel)
}

pub(crate) fn visible_node_action(node: &DispositionNode, actual: Sigaction) -> Sigaction {
    if node.registration.is_some() {
        actual.canonicalized_from_kernel_stub(&node.kernel, &node.guest)
    } else {
        actual
    }
}

pub(crate) fn visible_current(
    registry: &SignalRegistry,
    signum: i32,
    kernel: Sigaction,
) -> SignalResult<Sigaction> {
    if let Some(top) = registry
        .chains
        .get(&signum)
        .and_then(|chain| chain.nodes.last())
        .filter(|top| node_kernel_matches(&kernel, top))
    {
        return Ok(visible_node_action(top, kernel));
    }
    let Some(stub) = registry.stubs.get(&kernel.handler()) else {
        return Ok(kernel);
    };
    if stub.registration.signum() == signum && descriptor_kernel_matches(&kernel, stub) {
        Ok(kernel.canonicalized_from_kernel_stub(&stub.kernel, &stub.visible))
    } else {
        Err(SignalError::contract(format!(
            "signal {signum} disposition uses a MIRVM fixed-stub address with incompatible flags, mask, or restorer"
        )))
    }
}

pub(crate) fn canonical_guest_action(registry: &SignalRegistry, action: Sigaction) -> Sigaction {
    let Some(descriptor) = registry.stubs.get(&action.handler()) else {
        return action;
    };
    action.canonicalized_from_kernel_stub(&descriptor.kernel, &descriptor.visible)
}
