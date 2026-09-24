//! The runtime-interposition bridge's entry body, as this pair's CPU and object format encode it.
//!
//! See the module one level up for why the bridge is composed there and what each layer
//! contributes. What is here is the one part that is neither the CPU's alone nor the format's
//! alone: the instruction sequence reaches the slots through the page and offset relocations this
//! format spells, so the pair is the only place that can write both halves of it.

/// The body of one bridge entry: load the engine out of `owner_slot` into `owner_register`, then
/// jump indirect through `target_slot`.
///
/// Both slots are hidden globals, so each is one RIP-relative load and nothing is named that a
/// relocation would have to reach. The call's own arguments are never touched, and the jump leaves
/// the return address the caller pushed in place, so the replacement returns to the caller.
pub fn entry_asm(owner_register: &str, owner_slot: &str, target_slot: &str) -> String {
    format!(
        "    mov {owner_register}, QWORD PTR [rip + {owner_slot}]\n    \
         jmp QWORD PTR [rip + {target_slot}]"
    )
}

/// The body of a trampoline that reaches a native entry through the hidden 8-byte slot `slot`: one
/// RIP-relative load and a jump through it.
///
/// This is what the rlib rescue defines one of per symbol a native archive left undefined, and the
/// owning Engine writes the address the trampoline should reach into the slot. The slot is hidden,
/// so its address is a link-time constant and nothing here needs a relocation the object format
/// would have to spell.
pub fn slot_jump_asm(slot: &str) -> String {
    format!("    jmp QWORD PTR [rip + {slot}]")
}
