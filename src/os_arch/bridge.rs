//! The assembly text of the runtime-interposition bridge.
//!
//! An image built from a guest's native objects borrows the host's C library, and a few of those
//! calls the engine has to own: `crate::vm::interpose` lists them and says which slot carries the
//! engine and which carries the replacement. Turning that into a link is three facts from three
//! different owners, and this is the one place they meet:
//!
//! - the *name* each entry has to be defined under is the platform's, because it is the platform's
//!   linker that routes the call ([`crate::os::linker::bridge_entry_name`]);
//! - the *instructions* are the pair's ([`entry_asm`]), because they are this CPU's sequence with
//!   the operands this object format spells;
//! - how a symbol is declared, made private and given a region is the object format's
//!   ([`crate::native::asmtext`]).
//!
//! None of the three is derivable from the others, which is why the text is composed here rather
//! than written out per pair: a pair that wrote its own would restate the call list, and a call
//! added to `vm::interpose` would then be a call the bridge silently stopped covering.

use crate::native::asmtext::{Region, Visibility, Vocabulary};
use crate::os::linker;
use crate::vm::interpose::{INTERPOSED_CALLS, owner_slot, target_slot};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use super::linux_x86_64::bridge::{entry_asm, slot_jump_asm};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use super::macos_aarch64::bridge::{entry_asm, slot_jump_asm};

/// The bridge's assembly text: one entry per interposed call, then the slots the entries read.
///
/// The slots are private to the image and filled by `crate::vm::native_instance::wire` once the
/// owning Engine exists, which is why the entries carry no address of their own.
pub(crate) fn bridge_asm() -> Result<String, String> {
    let format = Vocabulary::of(crate::os::dll::OBJECT_FORMAT);
    let mut out = String::from(crate::arch::asm_text::DIRECTIVE_INTEL);
    // The entries carry the visibility the platform's redirection needs; the slots carry the one
    // the engine's route to them needs. Both answers are the platform's, because both follow from
    // how its linker and its loader treat a name.
    let entry_visibility = if linker::BRIDGE_ENTRY_IS_EXPORTED {
        Visibility::Exported
    } else {
        Visibility::Private
    };
    let slot_visibility = if linker::BRIDGE_SLOT_IS_EXPORTED {
        Visibility::Exported
    } else {
        Visibility::Private
    };
    for &call in INTERPOSED_CALLS {
        let owner = owner_slot(call)
            .ok_or_else(|| format!("interposed call `{call}` has no owner slot"))?;
        let register = crate::arch::asmstub::bridge_owner_register(call)
            .ok_or_else(|| format!("interposed call `{call}` has no owner register"))?;
        let entry = linker::bridge_entry_name(call);
        // Aligned, because an entry is branched to directly and the platform's own linker would
        // have aligned a function of its own making.
        out.push_str(".balign 16\n");
        format.define_fn(&mut out, &entry, entry_visibility);
        out.push_str(&entry_asm(
            register,
            &format.symbol(owner),
            &format.symbol(&target_slot(call)),
        ));
        out.push('\n');
        format.end_fn(&mut out, &entry);
    }
    // One region per engine-wide owner rather than one for the whole bridge, so that an image
    // carrying more than one family keeps them apart the way the owners are.
    for owner in ["__mirvm_pthread_owner", "__mirvm_signal_owner"] {
        format.open(&mut out, Region::Slots { name: &owner[9..] });
        out.push_str(".balign 8\n");
        format.define_slot(&mut out, owner, slot_visibility);
        for &call in INTERPOSED_CALLS
            .iter()
            .filter(|call| owner_slot(call) == Some(owner))
        {
            format.define_slot(&mut out, &target_slot(call), slot_visibility);
        }
        format.close(&mut out);
    }
    format.no_executable_stack(&mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// Every entry has to carry the name its platform routes the call to, because that name is the
    /// only thing connecting the two halves. Whether the text assembles and whether a call really
    /// lands on it are checks for the layer that builds the artifact, which is `crate::native`.
    #[test]
    fn every_entry_carries_its_platforms_name() {
        let asm = super::bridge_asm().expect("a bridge");
        for &call in crate::vm::interpose::INTERPOSED_CALLS {
            let entry = crate::os::linker::bridge_entry_name(call);
            assert!(
                asm.contains(&entry),
                "the bridge does not define `{entry}` for `{call}`"
            );
            assert!(asm.contains(&crate::vm::interpose::target_slot(call)));
        }
    }
}
