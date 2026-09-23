//! Engine lifecycle: whether an Engine still has registrations or pending deliveries, what its
//! close must deactivate, and whether its finalization can be sealed.

use super::*;

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
