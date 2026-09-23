//! Installation: validating a guest request, resolving its handler, replacing the kernel action,
//! and the rollback a concurrent writer or a rejected candidate forces.

use super::*;

pub(crate) fn validate_guest_action(signum: i32, action: &Sigaction) -> Result<(), String> {
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

/// Replace only the disposition observed by the caller. `sigaction` has no
/// compare-and-swap operation, so an intervening raw writer is detected from
/// the old action returned at the replacement linearization point. In that
/// case, restore the most recent displaced writer and ask the caller to retry.
pub(crate) fn replace_observed(
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
pub(crate) fn rollback_committed_candidate(
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

pub(crate) fn compensate_concurrent_writer(
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

/// Install/query a guest-visible sigaction while keeping MIRVM's fixed stub
/// out of oldact. `sigaction(new, &old)` is the installation linearization
/// point; the `old` it returns, not an earlier query, decides whether the
/// existing ownership chain is still current.
pub(crate) fn install_sigaction_value(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    resolution: Option<super::super::thunks::SignalHandlerResolution>,
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
    let resolution = resolution.unwrap_or(super::super::thunks::SignalHandlerResolution::Unknown);
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
                    super::super::deferred::DeferredHold::acquire(&callback_control, false)
                        .map_err(|_| {
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
    let resolution = guest.map(
        |(func, _)| super::super::thunks::SignalHandlerResolution::Valid {
            control: Arc::clone(control),
            func,
        },
    );
    install_sigaction_resolved(control, signum, action, resolution, oldact)
}

pub(crate) fn install_sigaction_resolved(
    control: &Arc<EngineControl>,
    signum: i32,
    action: Option<Sigaction>,
    resolution: Option<super::super::thunks::SignalHandlerResolution>,
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
        super::super::thunks::SignalHandlerResolution::Unknown,
        |(func, _)| super::super::thunks::SignalHandlerResolution::Valid {
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
    resolution: super::super::thunks::SignalHandlerResolution,
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
