//! The disposition registry: the chain of nodes one signal has accumulated, the stub descriptor a
//! node installs, and the reconciliation a chain needs when an owner installs, replaces or closes.

use super::*;

#[derive(Clone)]
pub(crate) struct DispositionNode {
    pub(crate) install_control: Arc<EngineControl>,
    pub(crate) install_owner: u64,
    pub(crate) callback_owner: Option<u64>,
    pub(crate) guest: Sigaction,
    /// Exact request passed at the single installation linearization point.
    pub(crate) kernel: Sigaction,
    /// Exact post-install kernel form, known only when a read-only query still
    /// observed this candidate before a raw writer replaced it.
    pub(crate) accepted_kernel: Option<Sigaction>,
    pub(crate) registration: Option<&'static SignalRegistration>,
}

#[derive(Clone)]
pub(crate) struct SignalChain {
    pub(crate) base: Sigaction,
    pub(crate) nodes: Vec<DispositionNode>,
}

#[derive(Clone)]
pub(crate) struct StubDescriptor {
    pub(crate) install_control: Arc<EngineControl>,
    pub(crate) install_owner: u64,
    pub(crate) callback_owner: u64,
    pub(crate) registration: &'static SignalRegistration,
    pub(crate) visible: Sigaction,
    /// Exact request passed at the single installation linearization point.
    pub(crate) kernel: Sigaction,
    /// Exact post-install kernel form when it was observed without writing the
    /// target signal a second time.
    pub(crate) accepted_kernel: Option<Sigaction>,
    /// Complete logical prefix that this stub replaced when it committed. A
    /// plain Sigaction is insufficient because native nodes also have an
    /// installer owner that must be removed from detached history at close.
    pub(crate) fallback: SignalChain,
}

#[derive(Default)]
pub(crate) struct SignalRegistry {
    pub(crate) chains: HashMap<i32, SignalChain>,
    pub(crate) stubs: HashMap<usize, StubDescriptor>,
}

pub(crate) static REGISTRY: LazyLock<Mutex<SignalRegistry>> =
    LazyLock::new(|| Mutex::new(SignalRegistry::default()));

pub(crate) enum ResolvedCallback {
    ExternalNative,
    Deferred {
        control: Arc<EngineControl>,
        callback: DeferredSignalCallback,
    },
}

pub(crate) fn resolve_callback(
    registry: &SignalRegistry,
    action: Sigaction,
    resolution: super::super::thunks::SignalHandlerResolution,
) -> SignalResult<ResolvedCallback> {
    use super::super::thunks::SignalHandlerResolution;

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
            if let Some(control) =
                super::super::native_instance::owner_of_executable_address(address)
            {
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

pub(crate) fn reject_committed_candidate(
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

pub(crate) fn descriptor_belongs_to(descriptor: &StubDescriptor, owner: u64) -> bool {
    descriptor.install_owner == owner || descriptor.callback_owner == owner
}

pub(crate) fn node_belongs_to(node: &DispositionNode, owner: u64) -> bool {
    node.install_owner == owner || node.callback_owner == Some(owner)
}

/// Keep every live fixed stub's saved predecessor aligned with the current
/// logical chain. This is what makes non-LIFO removal work: removing A from
/// base <- A <- B rewrites B's fallback to base.
pub(crate) fn sync_chain_descriptors(registry: &mut SignalRegistry, signum: i32) {
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
pub(crate) fn deactivate_owner_descriptors(
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

pub(crate) fn validate_rebuilt_node(
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
pub(crate) fn plan_stub_chain(
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

pub(crate) fn managed_chain_for_current(
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

pub(crate) fn chain_has_owner(chain: &SignalChain, owner: u64) -> bool {
    chain.nodes.iter().any(|node| node_belongs_to(node, owner))
}

pub(crate) fn relevant_signal_numbers(registry: &SignalRegistry, owner: u64) -> Vec<i32> {
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

pub(crate) fn remove_owner_for_signal(
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
pub(crate) fn reconcile_prior_chain(
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
