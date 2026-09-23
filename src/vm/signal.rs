//! Process-wide signal dispositions with per-Engine deferred delivery.

mod inbox;
#[cfg(test)]
mod tests;

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

#[derive(Clone)]
struct DispositionNode {
    install_control: Arc<EngineControl>,
    install_owner: u64,
    callback_owner: Option<u64>,
    guest: Sigaction,
    /// Exact request passed at the single installation linearization point.
    kernel: Sigaction,
    /// Exact post-install kernel form, known only when a read-only query still
    /// observed this candidate before a raw writer replaced it.
    accepted_kernel: Option<Sigaction>,
    registration: Option<&'static SignalRegistration>,
}

#[derive(Clone)]
struct SignalChain {
    base: Sigaction,
    nodes: Vec<DispositionNode>,
}

#[derive(Clone)]
struct StubDescriptor {
    install_control: Arc<EngineControl>,
    install_owner: u64,
    callback_owner: u64,
    registration: &'static SignalRegistration,
    visible: Sigaction,
    /// Exact request passed at the single installation linearization point.
    kernel: Sigaction,
    /// Exact post-install kernel form when it was observed without writing the
    /// target signal a second time.
    accepted_kernel: Option<Sigaction>,
    /// Complete logical prefix that this stub replaced when it committed. A
    /// plain Sigaction is insufficient because native nodes also have an
    /// installer owner that must be removed from detached history at close.
    fallback: SignalChain,
}

#[derive(Default)]
struct SignalRegistry {
    chains: HashMap<i32, SignalChain>,
    stubs: HashMap<usize, StubDescriptor>,
}

static REGISTRY: LazyLock<Mutex<SignalRegistry>> =
    LazyLock::new(|| Mutex::new(SignalRegistry::default()));

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

fn validate_guest_action(signum: i32, action: &Sigaction) -> Result<(), String> {
    if signum <= 0 || signum as usize >= SIGNAL_SLOTS {
        return Err(format!(
            "guest handler for signal {signum} is outside the supported traditional signal range"
        ));
    }
    if matches!(
        signum,
        crate::os::signal::SIGSEGV
            | crate::os::signal::SIGBUS
            | crate::os::signal::SIGFPE
            | crate::os::signal::SIGILL
            | crate::os::signal::SIGTRAP
    ) {
        return Err(format!(
            "guest handler for synchronous fault signal {signum} is unsupported because host and guest faults cannot be distinguished"
        ));
    }
    if crate::os::signal::is_realtime(signum) {
        return Err(format!(
            "guest handler for realtime signal {signum} requires queued siginfo delivery"
        ));
    }
    if action.has_unsupported_guest_flags() {
        return Err(format!(
            "guest sigaction for signal {signum} uses unsupported SA_SIGINFO, SA_ONSTACK, SA_NODEFER, or SA_RESETHAND semantics"
        ));
    }
    Ok(())
}

fn kernel_current(signum: i32) -> SignalResult<Sigaction> {
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
fn kernel_request_matches(actual: &Sigaction, expected: &Sigaction) -> bool {
    actual.same_disposition(expected) || actual.is_kernel_normalization_of(expected)
}

fn recorded_kernel_matches(
    actual: &Sigaction,
    requested: &Sigaction,
    accepted: Option<&Sigaction>,
) -> bool {
    accepted.is_some_and(|accepted| actual.same_disposition(accepted))
        || kernel_request_matches(actual, requested)
}

fn node_kernel_matches(actual: &Sigaction, node: &DispositionNode) -> bool {
    recorded_kernel_matches(actual, &node.kernel, node.accepted_kernel.as_ref())
}

fn descriptor_kernel_matches(actual: &Sigaction, descriptor: &StubDescriptor) -> bool {
    recorded_kernel_matches(
        actual,
        &descriptor.kernel,
        descriptor.accepted_kernel.as_ref(),
    )
}

fn node_kernel_action(node: &DispositionNode) -> Sigaction {
    node.accepted_kernel.unwrap_or(node.kernel)
}

fn visible_node_action(node: &DispositionNode, actual: Sigaction) -> Sigaction {
    if node.registration.is_some() {
        actual.canonicalized_from_kernel_stub(&node.kernel, &node.guest)
    } else {
        actual
    }
}

fn visible_current(
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

fn canonical_guest_action(registry: &SignalRegistry, action: Sigaction) -> Sigaction {
    let Some(descriptor) = registry.stubs.get(&action.handler()) else {
        return action;
    };
    action.canonicalized_from_kernel_stub(&descriptor.kernel, &descriptor.visible)
}

enum ResolvedCallback {
    ExternalNative,
    Deferred {
        control: Arc<EngineControl>,
        callback: DeferredSignalCallback,
    },
}

fn resolve_callback(
    registry: &SignalRegistry,
    action: Sigaction,
    resolution: super::thunks::SignalHandlerResolution,
) -> SignalResult<ResolvedCallback> {
    use super::thunks::SignalHandlerResolution;

    let address = action.handler();
    if address == crate::os::signal::SIG_DFL || address == crate::os::signal::SIG_IGN {
        return match resolution {
            SignalHandlerResolution::Unknown => Ok(ResolvedCallback::ExternalNative),
            _ => Err(SignalError::contract(
                "SIG_DFL/SIG_IGN was also resolved as MIRVM code",
            )),
        };
    }

    match resolution {
        SignalHandlerResolution::Valid { control, func } => Ok(ResolvedCallback::Deferred {
            control,
            callback: DeferredSignalCallback::Guest(func),
        }),
        SignalHandlerResolution::KnownInvalid => Err(SignalError::contract(format!(
            "signal handler {address:#x} is a MIRVM callback with an incompatible ABI or a closed owner"
        ))),
        SignalHandlerResolution::Unknown => {
            if let Some(descriptor) = registry.stubs.get(&address) {
                let registration = descriptor.registration;
                if !registration.control.accepts_signal_install() {
                    return Err(SignalError::contract(format!(
                        "signal handler {address:#x} belongs to a closed MIRVM Engine"
                    )));
                }
                return Ok(ResolvedCallback::Deferred {
                    control: Arc::clone(registration.control()),
                    callback: registration.callback(),
                });
            }
            if let Some(control) = super::native_instance::owner_of_executable_address(address) {
                if !control.accepts_signal_install() {
                    return Err(SignalError::contract(format!(
                        "signal handler {address:#x} belongs to a closed MIRVM native image"
                    )));
                }
                return Ok(ResolvedCallback::Deferred {
                    control,
                    callback: DeferredSignalCallback::ImageNative(address),
                });
            }
            // MIRVM-owned addresses have already been classified above. What
            // remains is an ordinary host function pointer, including the
            // common first-use case where guest code obtained it from dlsym.
            Ok(ResolvedCallback::ExternalNative)
        }
    }
}

fn reject_committed_candidate(
    signum: i32,
    requested: Sigaction,
    accepted: Option<Sigaction>,
    previous: Sigaction,
    registration: Option<&'static SignalRegistration>,
) -> SignalResult<()> {
    // The successful replace already exposed `requested`: a raw writer may
    // have retained it even when compensation restores `previous` now. Keep
    // its descriptor for process lifetime and close its delivery gate so a
    // later raw restoration fails loudly instead of dropping a signal.
    let _ = rollback_committed_candidate(signum, requested, accepted, previous)?;
    if let Some(registration) = registration {
        registration.deactivate();
        registration.wait_for_kernel_deliveries();
    }
    Ok(())
}

fn descriptor_belongs_to(descriptor: &StubDescriptor, owner: u64) -> bool {
    descriptor.install_owner == owner || descriptor.callback_owner == owner
}

fn node_belongs_to(node: &DispositionNode, owner: u64) -> bool {
    node.install_owner == owner || node.callback_owner == Some(owner)
}

/// Keep every live fixed stub's saved predecessor aligned with the current
/// logical chain. This is what makes non-LIFO removal work: removing A from
/// base <- A <- B rewrites B's fallback to base.
fn sync_chain_descriptors(registry: &mut SignalRegistry, signum: i32) {
    let Some(chain) = registry.chains.get(&signum).cloned() else {
        return;
    };
    let mut fallback = SignalChain {
        base: chain.base,
        nodes: Vec::new(),
    };
    for node in chain.nodes {
        if let Some(registration) = node.registration {
            let descriptor = registry
                .stubs
                .get_mut(&node.kernel.handler())
                .unwrap_or_else(|| {
                    panic!(
                        "signal {signum} live fixed stub is missing its process-lifetime descriptor"
                    )
                });
            assert!(ptr::eq(descriptor.registration, registration));
            descriptor.kernel = node.kernel;
            descriptor.accepted_kernel = node.accepted_kernel;
            descriptor.fallback = fallback.clone();
        }
        fallback.nodes.push(node);
    }
}

/// Closing an Engine also invalidates detached process-lifetime stubs that are
/// no longer in a live chain. Fold every surviving descriptor around those
/// stubs so a later raw restoration cannot revive a dead predecessor.
fn deactivate_owner_descriptors(
    registry: &mut SignalRegistry,
    owner: u64,
    registrations: &mut Vec<&'static SignalRegistration>,
) {
    for descriptor in registry.stubs.values() {
        if descriptor_belongs_to(descriptor, owner) {
            descriptor.registration.deactivate();
            registrations.push(descriptor.registration);
        }
    }
    for descriptor in registry.stubs.values_mut() {
        descriptor
            .fallback
            .nodes
            .retain(|node| !node_belongs_to(node, owner));
    }
}

fn validate_rebuilt_node(
    registry: &SignalRegistry,
    signum: i32,
    node: &DispositionNode,
) -> SignalResult<()> {
    if node.install_owner != node.install_control.id()
        || !node.install_control.accepts_signal_install()
    {
        return Err(SignalError::contract(format!(
            "signal {signum} disposition contains an installation from a closed MIRVM Engine"
        )));
    }
    let Some(registration) = node.registration else {
        if node.callback_owner.is_some() {
            return Err(SignalError::contract(format!(
                "signal {signum} native disposition has a callback owner"
            )));
        }
        return Ok(());
    };
    let Some(descriptor) = registry.stubs.get(&node.kernel.handler()) else {
        return Err(SignalError::contract(format!(
            "signal {signum} fallback contains an unknown MIRVM fixed stub"
        )));
    };
    if !node.kernel.same_disposition(&descriptor.kernel)
        || node.install_owner != descriptor.install_owner
        || node.install_control.id() != descriptor.install_control.id()
        || descriptor.install_owner != descriptor.install_control.id()
        || node
            .accepted_kernel
            .is_some_and(|accepted| !recorded_kernel_matches(&accepted, &node.kernel, None))
        || descriptor
            .accepted_kernel
            .is_some_and(|accepted| !recorded_kernel_matches(&accepted, &descriptor.kernel, None))
        || !ptr::eq(registration, descriptor.registration)
        || node.callback_owner != Some(descriptor.callback_owner)
        || descriptor.callback_owner != registration.control().id()
        || !registration.control().accepts_signal_install()
    {
        return Err(SignalError::contract(format!(
            "signal {signum} fallback contains an invalid or closed MIRVM fixed stub"
        )));
    }
    Ok(())
}

/// Rebuild the managed prefix ending at `top_kernel` from the complete logical
/// snapshot saved when that process-lifetime fixed stub committed.
fn plan_stub_chain(
    registry: &SignalRegistry,
    signum: i32,
    top_kernel: Sigaction,
) -> SignalResult<Option<(Sigaction, SignalChain)>> {
    let Some(top_descriptor) = registry.stubs.get(&top_kernel.handler()).cloned() else {
        return Ok(None);
    };
    if !descriptor_kernel_matches(&top_kernel, &top_descriptor) {
        return Err(SignalError::contract(format!(
            "signal {signum} disposition uses a MIRVM fixed-stub address with incompatible flags, mask, or restorer"
        )));
    }
    if top_descriptor.registration.signum() != signum {
        return Err(SignalError::contract(format!(
            "signal {signum} disposition contains a MIRVM fixed stub for signal {}",
            top_descriptor.registration.signum()
        )));
    }
    let mut chain = top_descriptor.fallback.clone();
    for node in &chain.nodes {
        validate_rebuilt_node(registry, signum, node)?;
    }
    let mut top = DispositionNode {
        install_control: Arc::clone(&top_descriptor.install_control),
        install_owner: top_descriptor.install_owner,
        callback_owner: Some(top_descriptor.callback_owner),
        guest: top_descriptor.visible,
        kernel: top_descriptor.kernel,
        accepted_kernel: top_descriptor.accepted_kernel,
        registration: Some(top_descriptor.registration),
    };
    validate_rebuilt_node(registry, signum, &top)?;
    // `top_kernel` has already matched this descriptor's strict request or a
    // previously accepted snapshot. It is the exact action observed now, so
    // retain it only after validating the descriptor's own stored identity.
    top.accepted_kernel = Some(top_kernel);
    chain.nodes.push(top);
    let visible =
        top_kernel.canonicalized_from_kernel_stub(&top_descriptor.kernel, &top_descriptor.visible);
    Ok(Some((visible, chain)))
}

fn managed_chain_for_current(
    registry: &SignalRegistry,
    signum: i32,
    current: Sigaction,
) -> SignalResult<Option<SignalChain>> {
    if let Some((chain, position)) = registry.chains.get(&signum).and_then(|chain| {
        chain
            .nodes
            .iter()
            .rposition(|node| node_kernel_matches(&current, node))
            .map(|position| (chain, position))
    }) {
        let mut prefix = chain.clone();
        prefix.nodes.truncate(position + 1);
        prefix.nodes[position].accepted_kernel = Some(current);
        return Ok(Some(prefix));
    }
    let Some(descriptor) = registry.stubs.get(&current.handler()) else {
        return Ok(None);
    };
    if descriptor.registration.signum() != signum
        || !descriptor_kernel_matches(&current, descriptor)
    {
        // A raw writer reused an internal address with a different action.
        // It is not the MIRVM disposition described by this registry entry,
        // so close must leave it untouched.
        return Ok(None);
    }
    plan_stub_chain(registry, signum, current).map(|planned| planned.map(|(_, chain)| chain))
}

/// Replace only the disposition observed by the caller. `sigaction` has no
/// compare-and-swap operation, so an intervening raw writer is detected from
/// the old action returned at the replacement linearization point. In that
/// case, restore the most recent displaced writer and ask the caller to retry.
fn replace_observed(
    signum: i32,
    observed: Sigaction,
    replacement: Sigaction,
) -> SignalResult<bool> {
    let actual_old = replacement
        .replace_exact(signum)
        .map_err(|errno| SignalError::libc("sigaction restore", signum, errno))?;
    if actual_old.same_disposition(&observed) {
        return Ok(true);
    }

    compensate_concurrent_writer(signum, replacement, actual_old)?;
    Ok(false)
}

/// Roll back a candidate that was installed as a libc request. The first
/// comparison accepts libc/kernel normalization of that request; after this
/// point every value came from an exact kernel snapshot and must compare
/// exactly during compensation.
fn rollback_committed_candidate(
    signum: i32,
    requested: Sigaction,
    accepted: Option<Sigaction>,
    previous: Sigaction,
) -> SignalResult<bool> {
    let actual_old = previous
        .replace_exact(signum)
        .map_err(|errno| SignalError::libc("sigaction rollback", signum, errno))?;
    if recorded_kernel_matches(&actual_old, &requested, accepted.as_ref()) {
        return Ok(true);
    }
    compensate_concurrent_writer(signum, previous, actual_old)?;
    Ok(false)
}

fn compensate_concurrent_writer(
    signum: i32,
    mut installed: Sigaction,
    mut displaced: Sigaction,
) -> SignalResult<()> {
    loop {
        let actual = displaced.replace_exact(signum).map_err(|errno| {
            SignalError::libc("sigaction concurrent-writer restore", signum, errno)
        })?;
        if actual.same_disposition(&installed) {
            return Ok(());
        }
        // Another raw writer arrived before our compensating replacement.
        // Preserve that newer action instead, repeating until one replacement
        // observes exactly the action installed by the preceding step.
        installed = displaced;
        displaced = actual;
    }
}

fn chain_has_owner(chain: &SignalChain, owner: u64) -> bool {
    chain.nodes.iter().any(|node| node_belongs_to(node, owner))
}

fn relevant_signal_numbers(registry: &SignalRegistry, owner: u64) -> Vec<i32> {
    let mut signums = registry
        .chains
        .iter()
        .filter(|(_, chain)| chain_has_owner(chain, owner))
        .map(|(&signum, _)| signum)
        .collect::<HashSet<_>>();
    signums.extend(
        registry
            .stubs
            .values()
            .filter(|descriptor| {
                descriptor_belongs_to(descriptor, owner)
                    || chain_has_owner(&descriptor.fallback, owner)
            })
            .map(|descriptor| descriptor.registration.signum()),
    );
    let mut signums = signums.into_iter().collect::<Vec<_>>();
    signums.sort_unstable();
    signums
}

fn remove_owner_for_signal(
    registry: &mut SignalRegistry,
    signum: i32,
    owner: u64,
) -> SignalResult<bool> {
    loop {
        let observed = kernel_current(signum)?;
        let managed_current = managed_chain_for_current(registry, signum, observed)?;
        let current_is_managed = managed_current.is_some();
        let Some(mut chain) = managed_current.or_else(|| registry.chains.get(&signum).cloned())
        else {
            return Ok(false);
        };
        let removed_current_top = current_is_managed
            && chain
                .nodes
                .last()
                .is_some_and(|node| node_belongs_to(node, owner));
        let old_len = chain.nodes.len();
        chain.nodes.retain(|node| !node_belongs_to(node, owner));
        let removed = chain.nodes.len() != old_len;

        if removed_current_top {
            let replacement = chain.nodes.last().map_or(chain.base, node_kernel_action);
            if !replace_observed(signum, observed, replacement)? {
                continue;
            }
        }

        if chain.nodes.is_empty() {
            registry.chains.remove(&signum);
        } else {
            registry.chains.insert(signum, chain);
            sync_chain_descriptors(registry, signum);
        }
        return Ok(removed);
    }
}

/// Reconcile MIRVM's logical chain with the disposition atomically returned by
/// sigaction(new, old). Native code may have restored a lower or detached
/// process-lifetime fixed stub since MIRVM last observed this signal.
fn reconcile_prior_chain(
    registry: &mut SignalRegistry,
    signum: i32,
    actual_old: Sigaction,
) -> SignalResult<Sigaction> {
    let matching_position = registry.chains.get(&signum).and_then(|chain| {
        chain
            .nodes
            .iter()
            .rposition(|node| node_kernel_matches(&actual_old, node))
    });
    if let Some(position) = matching_position {
        let chain = registry.chains.get_mut(&signum).unwrap();
        chain.nodes.truncate(position + 1);
        let visible = visible_node_action(&chain.nodes[position], actual_old);
        chain.nodes[position].accepted_kernel = Some(actual_old);
        sync_chain_descriptors(registry, signum);
        return Ok(visible);
    }

    // Validate the detached stub and its complete fallback prefix before
    // altering the stale logical chain. If validation fails, the caller can
    // still roll the just-installed candidate back to `actual_old`.
    let rebuilt = plan_stub_chain(registry, signum, actual_old)?;
    registry.chains.remove(&signum);
    if let Some((visible, chain)) = rebuilt {
        registry.chains.insert(signum, chain);
        sync_chain_descriptors(registry, signum);
        Ok(visible)
    } else {
        Ok(actual_old)
    }
}

/// Install/query a guest-visible sigaction while keeping MIRVM's fixed stub
/// out of oldact. `sigaction(new, &old)` is the installation linearization
/// point; the `old` it returns, not an earlier query, decides whether the
/// existing ownership chain is still current.
fn install_sigaction_value(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    resolution: Option<super::thunks::SignalHandlerResolution>,
) -> SignalResult<Sigaction> {
    let mut registry = REGISTRY.lock().unwrap();
    let current = kernel_current(signum)?;
    let old = visible_current(&registry, signum, current)?;
    let Some(requested_action) = action.map(Sigaction::normalized_for_kernel) else {
        return Ok(old);
    };

    if !control.accepts_signal_install() {
        return Err(SignalError::contract(
            "signal installer Engine is finalizing or closed",
        ));
    }
    let resolution = resolution.unwrap_or(super::thunks::SignalHandlerResolution::Unknown);
    let resolved = resolve_callback(&registry, requested_action, resolution)?;
    let guest_action = canonical_guest_action(&registry, requested_action);
    let special = matches!(
        guest_action.handler(),
        crate::os::signal::SIG_DFL | crate::os::signal::SIG_IGN
    );
    if !special && matches!(resolved, ResolvedCallback::Deferred { .. }) {
        validate_guest_action(signum, &guest_action).map_err(SignalError::contract)?;
    }

    let mut target_hold = None;
    let candidate = match resolved {
        ResolvedCallback::ExternalNative => None,
        ResolvedCallback::Deferred {
            control: callback_control,
            callback,
        } => {
            // The registry lock serialises this target-phase check with close's
            // final seal. Once committed, target close necessarily sees the
            // node before it can enter Finalizing.
            if callback_control.id() == control.id() {
                if !callback_control.accepts_signal_install() {
                    return Err(SignalError::contract(format!(
                        "signal handler {:#x} belongs to a closing MIRVM Engine",
                        guest_action.handler()
                    )));
                }
            } else {
                target_hold = Some(
                    super::ctx::DeferredHold::acquire(&callback_control, false).map_err(|_| {
                        SignalError::contract(format!(
                            "signal handler {:#x} belongs to a closing MIRVM Engine",
                            guest_action.handler()
                        ))
                    })?,
                );
            }
            let registration =
                SignalRegistration::new(callback_control, callback, signum, guest_action);
            let stub = match materialize_signal_stub(registration) {
                Ok(stub) => stub,
                Err(error) => {
                    registration.deactivate();
                    return Err(SignalError::contract(error));
                }
            };
            registration.control.signal_inbox.register(registration);
            Some((registration, guest_action.for_kernel_stub(stub), stub))
        }
    };
    let requested_kernel = candidate.map_or(guest_action, |(_, kernel, _)| {
        kernel.with_runtime_restorer()
    });
    let actual_old = match requested_kernel.replace(signum) {
        Ok(old) => old,
        Err(errno) => {
            if let Some((registration, _, _)) = candidate {
                registration.deactivate();
                registration.wait_for_kernel_deliveries();
            }
            return Err(SignalError::libc("sigaction install", signum, errno));
        }
    };

    #[cfg(test)]
    run_after_install_replace_hook(signum, requested_kernel, actual_old);

    // The successful replace above is the only write of this candidate. A
    // following query can record the exact kernel-normalized form only if it
    // still observes the same handler and every normalized field is valid. If
    // a raw writer already won, leave it untouched and retain only the request
    // so a captured oldact can still be recognized later.
    let accepted_kernel = Sigaction::query(signum).ok().filter(|observed| {
        observed.handler() == requested_kernel.handler()
            && kernel_request_matches(observed, &requested_kernel)
    });

    // A successful sigaction replace is the installation linearization
    // point. Commit the process-lifetime descriptor before doing any logical
    // reconciliation: a raw writer can already have captured this exact stub
    // address and may restore it later.
    if let Some((registration, _, stub)) = candidate {
        registry.stubs.insert(
            stub,
            StubDescriptor {
                install_control: Arc::clone(control),
                install_owner: control.id(),
                callback_owner: registration.control.id(),
                registration,
                visible: guest_action,
                kernel: requested_kernel,
                accepted_kernel,
                fallback: SignalChain {
                    base: actual_old,
                    nodes: Vec::new(),
                },
            },
        );
    }

    let visible_old = match reconcile_prior_chain(&mut registry, signum, actual_old) {
        Ok(old) => old,
        Err(error) => {
            reject_committed_candidate(
                signum,
                requested_kernel,
                accepted_kernel,
                actual_old,
                candidate.map(|(registration, _, _)| registration),
            )?;
            return Err(error);
        }
    };

    let callback_owner = candidate.map(|(registration, _, _)| registration.control.id());
    let fallback = registry
        .chains
        .get(&signum)
        .cloned()
        .unwrap_or(SignalChain {
            base: actual_old,
            nodes: Vec::new(),
        });
    let chain = registry
        .chains
        .entry(signum)
        .or_insert_with(|| SignalChain {
            base: actual_old,
            nodes: Vec::new(),
        });
    chain.nodes.push(DispositionNode {
        install_control: Arc::clone(control),
        install_owner: control.id(),
        callback_owner,
        guest: guest_action,
        kernel: requested_kernel,
        accepted_kernel,
        registration: candidate.map(|(registration, _, _)| registration),
    });
    if let Some((_, _, stub)) = candidate {
        registry.stubs.get_mut(&stub).unwrap().fallback = fallback;
    }
    sync_chain_descriptors(&mut registry, signum);
    drop(target_hold);
    Ok(visible_old)
}

#[cfg(test)]
pub(crate) fn install_sigaction(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    guest: Option<(FuncId, u64)>,
    oldact: u64,
) -> SignalResult<i32> {
    let resolution = guest.map(|(func, _)| super::thunks::SignalHandlerResolution::Valid {
        control: Arc::clone(control),
        func,
    });
    install_sigaction_resolved(control, signum, action, resolution, oldact)
}

pub(crate) fn install_sigaction_resolved(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    resolution: Option<super::thunks::SignalHandlerResolution>,
    oldact: u64,
) -> SignalResult<i32> {
    let old = install_sigaction_value(control, signum, action, resolution)?;
    old.write_to(oldact);
    Ok(0)
}

#[cfg(test)]
pub(crate) fn install_signal(
    control: &Arc<EngineControl>,
    signum: i32,
    handler: usize,
    guest: Option<(FuncId, u64)>,
) -> SignalResult<usize> {
    let resolution = guest.map_or(
        super::thunks::SignalHandlerResolution::Unknown,
        |(func, _)| super::thunks::SignalHandlerResolution::Valid {
            control: Arc::clone(control),
            func,
        },
    );
    install_signal_resolved(control, signum, handler, resolution)
}

pub(crate) fn install_signal_resolved(
    control: &Arc<EngineControl>,
    signum: i32,
    handler: usize,
    resolution: super::thunks::SignalHandlerResolution,
) -> SignalResult<usize> {
    install_sigaction_value(
        control,
        signum,
        Some(Sigaction::for_signal(handler)),
        Some(resolution),
    )
    .map(|old| old.handler())
}

#[cfg(test)]
pub(crate) fn current_delivery(signum: i32) -> Option<SignalDeliveryGuard> {
    let registry = REGISTRY.lock().unwrap();
    let current = kernel_current(signum).ok()?;
    let registration = registry
        .chains
        .get(&signum)
        .into_iter()
        .flat_map(|chain| chain.nodes.iter().rev())
        .find(|node| node_kernel_matches(&current, node))
        .and_then(|node| node.registration)
        .or_else(|| {
            registry
                .stubs
                .get(&current.handler())
                .filter(|descriptor| {
                    descriptor.registration.signum() == signum
                        && descriptor_kernel_matches(&current, descriptor)
                })
                .map(|descriptor| descriptor.registration)
        });
    registration.and_then(SignalRegistration::safe_point_delivery)
}

/// Remove all dispositions owned by an Engine, restore the surviving top (or
/// the original native action), and wait until every old kernel frame has left
/// the fixed adapter. Pending events are intentionally retained for a final
/// ordinary-state drain by the close path.
pub(crate) fn deactivate_engine(control: &EngineControl) -> SignalResult<bool> {
    let mut removed_any = false;
    {
        let mut registry = REGISTRY.lock().unwrap();
        let mut registrations = Vec::new();
        let signums = relevant_signal_numbers(&registry, control.id());
        for &signum in &signums {
            removed_any |= remove_owner_for_signal(&mut registry, signum, control.id())?;
        }

        removed_any |= registry
            .stubs
            .values()
            .any(|descriptor| descriptor_belongs_to(descriptor, control.id()));
        deactivate_owner_descriptors(&mut registry, control.id(), &mut registrations);

        // A raw writer can restore a detached stub during the first scan.
        // Audit the same finite signal set after closing its delivery gate; an
        // exact current stub is still reconstructable from its descriptor and
        // is restored before the Engine is allowed to reach Finalizing.
        for signum in signums {
            removed_any |= remove_owner_for_signal(&mut registry, signum, control.id())?;
        }

        registrations.sort_unstable_by_key(|registration| ptr::from_ref(*registration) as usize);
        registrations.dedup_by_key(|registration| ptr::from_ref(*registration) as usize);
        // Keep the registry locked until every adapter frame that passed the
        // gate has published. A callback owner cannot seal Finalizing in the
        // small interval between node removal and pending publication.
        for registration in registrations {
            registration.wait_for_kernel_deliveries();
        }
    }
    Ok(removed_any)
}

pub(crate) fn has_engine_registrations(control: &EngineControl) -> bool {
    REGISTRY.lock().unwrap().chains.values().any(|chain| {
        chain.nodes.iter().any(|node| {
            node.install_owner == control.id() || node.callback_owner == Some(control.id())
        })
    })
}

/// Seal signal registration and the Engine lifecycle in one registry critical
/// section. Every install rechecks both owners under this same lock, so no late
/// callback can appear between the final empty check and Finalizing.
pub(crate) fn try_seal_engine(control: &EngineControl) -> bool {
    let registry = REGISTRY.lock().unwrap();
    if registry.chains.values().any(|chain| {
        chain.nodes.iter().any(|node| {
            node.install_owner == control.id() || node.callback_owner == Some(control.id())
        })
    }) || control.signal_inbox.has_pending()
    {
        return false;
    }
    control.begin_finalizing_with_permit()
}

pub(crate) fn has_engine_pending(control: &EngineControl) -> bool {
    control.signal_inbox.has_pending()
}

/// Runtime bridge used by self-produced native archive images. The hidden
/// owner argument identifies the Engine whose P1 entry address may appear as
/// `handler`; handlers inside a MIRVM-produced image are deferred too.
pub(crate) unsafe extern "C-unwind" fn native_signal(
    signum: i32,
    handler: usize,
    owner: u64,
) -> usize {
    let Some(control) = super::ctx::control_for_engine(owner) else {
        set_errno(ESRCH);
        return crate::os::signal::SIG_ERR;
    };
    let Ok(lease) = super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(ESRCH);
        return crate::os::signal::SIG_ERR;
    };
    let _activation = super::ctx::activate(lease.shared());
    let resolution = super::thunks::resolve_signal_handler(lease.shared(), handler as u64);
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
        Err(SignalError::Contract(message)) => super::interp::engine_abort(&message),
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
    let Some(control) = super::ctx::control_for_engine(owner) else {
        set_errno(ESRCH);
        return -1;
    };
    let Ok(lease) = super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(ESRCH);
        return -1;
    };
    let _activation = super::ctx::activate(lease.shared());
    let action = unsafe { action.as_ref() }.copied();
    let resolution = action.as_ref().map(|action| {
        super::thunks::resolve_signal_handler(lease.shared(), action.handler() as u64)
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
        Err(SignalError::Contract(message)) => super::interp::engine_abort(&message),
    }
}

pub(crate) unsafe extern "C-unwind" fn native_raise(signum: i32, owner: u64) -> i32 {
    let Some(control) = super::ctx::control_for_engine(owner) else {
        set_errno(ESRCH);
        return -1;
    };
    let Ok(lease) = super::ctx::ExecutionLease::for_thunk(&control) else {
        set_errno(ESRCH);
        return -1;
    };
    let activation = super::ctx::activate(lease.shared());
    super::ctx::raise_signal(activation.ctx(), signum)
}

fn set_errno(value: i32) {
    crate::os::process::set_errno(value);
}

fn materialize_signal_stub(registration: &'static SignalRegistration) -> Result<usize, String> {
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
