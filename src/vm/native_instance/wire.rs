//! Wiring a mapped image into the Engine.
//!
//! A self-produced object calls back into mirvm through private bridge slots, so an image is
//! usable only after those slots hold real addresses: [`patch_entry_slots`] resolves the P1
//! recipes and [`patch_pthread_slots`] fills the runtime interposition slots. They are two
//! halves of that one job and stay in this file together. Once the slots are valid the image is
//! committed -- it will never be unmapped, and its executable ranges stay attributable for the
//! process -- and its constructors run; [`run_finalizers`] is the reverse at Engine teardown.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::super::instance::Instance;
use super::super::ir::{Module, native_entry_slot_name};
use super::super::native_lifecycle::InitializerArgs;
use super::NativeImage;
use super::{OWNED_NATIVE_CODE, OwnedNativeCode};

/// Fill every native bridge slot after all per-Engine P1 closures exist.
pub(crate) fn patch_entry_slots(module: &Module, instance: &Instance) -> Result<(), String> {
    let mut slots = BTreeMap::<String, u64>::new();
    for site in module.entry_stub_sites.iter().chain(
        instance
            .image_entry_stubs
            .iter()
            .flat_map(|(_, sites, _)| sites.iter()),
    ) {
        let target = instance.try_resolve_link_addr(site.link_addr)?;
        slots.insert(native_entry_slot_name(site.link_addr), target);
    }
    if slots.is_empty() {
        return Ok(());
    }

    for image in &instance.mc_images {
        for (name, &value) in image
            .symbols
            .iter()
            .filter(|(name, _)| name.starts_with("__mirvm_p1_target_"))
        {
            let target = slots
                .get(name.as_ref())
                .ok_or_else(|| format!("native entry slot `{name}` has no P1 recipe"))?;
            let slot = (image.load_bias() as u64)
                .checked_add(value)
                .ok_or_else(|| format!("native entry slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(*target) };
        }
    }

    if instance.native_images.len() != module.required_native_libs.len() {
        return Err("native image/path count mismatch".into());
    }
    for image in &instance.native_images {
        for (name, &value) in image
            .hidden_symbol_values()
            .iter()
            .filter(|(name, _)| name.starts_with("__mirvm_p1_target_"))
        {
            let target = slots
                .get(name.as_ref())
                .ok_or_else(|| format!("native entry slot `{name}` has no P1 recipe"))?;
            let slot = image
                .bias
                .checked_add(value)
                .ok_or_else(|| format!("native entry slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(*target) };
        }
    }
    Ok(())
}

/// Fill the private runtime interposition slots injected into every
/// self-produced machine-code image. Images are already relocated but no
/// constructor has run yet.
pub(crate) fn patch_pthread_slots(instance: &Instance, engine_id: u64) -> Result<(), String> {
    use super::super::interpose::{INTERPOSED_CALLS, owner_slot};
    let targets = [
        (
            "__mirvm_pthread_create_target",
            super::super::deferred::native_pthread_create as *const () as usize as u64,
        ),
        (
            "__mirvm_pthread_key_create_target",
            super::super::deferred::native_pthread_key_create as *const () as usize as u64,
        ),
        (
            "__mirvm_pthread_setspecific_target",
            super::super::deferred::native_pthread_setspecific as *const () as usize as u64,
        ),
        (
            "__mirvm_pthread_key_delete_target",
            super::super::deferred::native_pthread_key_delete as *const () as usize as u64,
        ),
        (
            "__mirvm_signal_target",
            super::super::signal::native_signal as *const () as usize as u64,
        ),
        (
            "__mirvm_sigaction_target",
            super::super::signal::native_sigaction as *const () as usize as u64,
        ),
        (
            "__mirvm_raise_target",
            super::super::signal::native_raise as *const () as usize as u64,
        ),
        (
            "__mirvm___cxa_atexit_target",
            super::super::atexit::native_cxa_atexit as *const () as usize as u64,
        ),
    ];
    // The owner slots the bridge defines, named by the one function that also names them in the
    // bridge text: a name written twice is a slot wired to nothing when the two spellings drift.
    let owners: Vec<&str> = INTERPOSED_CALLS
        .iter()
        .filter_map(|call| owner_slot(call))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    debug_assert!(targets.iter().all(|(name, _)| {
        INTERPOSED_CALLS
            .iter()
            .any(|call| super::super::interpose::target_slot(call) == *name)
    }));
    let total = targets.len() + owners.len();

    for image in &instance.native_images {
        // Two ways, because the platform decides whether the slots are exported: a loader only
        // offers what an image exports, and where the bridge is an object of this image the entries
        // reach their slots PC-relative, which forbids exporting them. So the loader is asked first
        // -- it is the only way to reach a slot that belongs to the library the image links against
        // -- and the image's own symbol table is the answer where it is not.
        let address = |name: &str| -> usize {
            let cname = std::ffi::CString::new(name).expect("a slot name contains no NUL");
            let exported = crate::os::dll::sym(image.handle(), &cname);
            if exported != 0 {
                return exported;
            }
            image
                .hidden_symbol_values()
                .get(name)
                .and_then(|value| image.bias().checked_add(*value))
                .map_or(0, |address| address as usize)
        };
        let present = targets
            .iter()
            .map(|(name, _)| *name)
            .chain(owners.iter().copied())
            .filter(|name| address(name) != 0)
            .count();
        if present == 0 {
            continue;
        }
        if present != total {
            return Err("self-produced native image has an incomplete runtime bridge".into());
        }
        for &(name, target) in &targets {
            unsafe { (address(name) as *mut u64).write(target) };
        }
        for name in &owners {
            unsafe { (address(name) as *mut u64).write(engine_id) };
        }
    }
    // A self-mapped image was never handed to a loader, so there is none to ask and the offsets the
    // image carries are the only way to reach it.
    for image in &instance.mc_images {
        let bias = image.load_bias() as u64;
        let symbols = &image.symbols;
        let present = targets
            .iter()
            .filter(|(name, _)| symbols.contains_key(*name))
            .count()
            + owners
                .iter()
                .filter(|name| symbols.contains_key(**name))
                .count();
        if present == 0 {
            continue;
        }
        if present != total {
            return Err("self-produced native image has an incomplete runtime bridge".into());
        }
        for &(name, target) in &targets {
            let slot = bias
                .checked_add(symbols[name])
                .ok_or_else(|| format!("runtime bridge slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(target) };
        }
        for name in &owners {
            let slot = bias
                .checked_add(symbols[*name])
                .ok_or_else(|| format!("runtime owner slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(engine_id) };
        }
    }
    Ok(())
}

/// Take ownership of every executable range for the process and mark the images so their `Drop`
/// releases no mapping. `control` is the Engine that stays answerable for these addresses.
pub(crate) fn commit_images(instance: &Instance, control: &Arc<super::super::ctx::EngineControl>) {
    for image in &instance.native_images {
        image
            .committed
            .store(true, std::sync::atomic::Ordering::Release);
    }
    for image in &instance.mc_images {
        image.commit();
    }
    let mut owned = OWNED_NATIVE_CODE.write().unwrap();
    for &(start, end) in instance
        .native_images
        .iter()
        .flat_map(NativeImage::executable_ranges)
        .chain(
            instance
                .mc_images
                .iter()
                .flat_map(super::super::mcload::McImage::executable_ranges),
        )
    {
        owned.push(OwnedNativeCode {
            start,
            end,
            control: Arc::clone(control),
        });
    }
}

pub(crate) fn run_initializers(instance: &Instance) -> Result<(), String> {
    let mut args = InitializerArgs::capture()?;
    for image in &instance.native_images {
        image.lifecycle.run_initializers(&mut args);
    }
    for image in &instance.mc_images {
        image.run_initializers(&mut args);
    }
    Ok(())
}

pub(crate) fn run_finalizers(instance: &Instance) {
    for image in instance.mc_images.iter().rev() {
        image.run_finalizers();
    }
    for image in instance.native_images.iter().rev() {
        image.lifecycle.run_finalizers();
    }
}
