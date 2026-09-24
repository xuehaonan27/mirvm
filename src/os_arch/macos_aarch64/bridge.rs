//! The runtime-interposition bridge's entry body, as this pair's CPU and object format encode it.
//!
//! See the module one level up for why the bridge is composed there and what each layer
//! contributes. What is here is the one part that is neither the CPU's alone nor the format's
//! alone: reaching a global's address through the page and offset relocations is a sequence of this
//! CPU's instructions whose operands this format spells `@PAGE`/`@PAGEOFF`, where the ELF spelling
//! of the same pair of relocations is a bare symbol and `:lo12:`.

/// The body of one bridge entry: load the engine out of `owner_slot` into `owner_register`, then
/// branch indirect through `target_slot`.
///
/// Both slots are hidden globals, so the pair of relocations reaches each without a GOT, and `x16`
/// and `x17` are the procedure call standard's intra-procedure scratch registers: they hold no
/// argument and no result, which is why the entry may use them without saving anything.
///
/// The call's own arguments are never touched, and the branch leaves the return address in `x30` in
/// place, so the replacement returns to the caller.
pub fn entry_asm(owner_register: &str, owner_slot: &str, target_slot: &str) -> String {
    format!(
        "    adrp x16, {owner_slot}@PAGE\n    ldr {owner_register}, [x16, {owner_slot}@PAGEOFF]\n    \
         adrp x17, {target_slot}@PAGE\n    ldr x17, [x17, {target_slot}@PAGEOFF]\n    br x17"
    )
}

/// The body of a trampoline that reaches a native entry through the hidden 8-byte slot `slot`: the
/// pair of relocations that load its address, then a branch through it.
///
/// This is what the rlib rescue defines one of per symbol a native archive left undefined, and the
/// owning Engine writes the address the trampoline should reach into the slot. The slot is hidden,
/// so the relocations reach it without a GOT, and `x16` is one of the two scratch registers the
/// procedure call standard reserves -- the same one `entry_asm` uses, for the same reason.
pub fn slot_jump_asm(slot: &str) -> String {
    format!("    adrp x16, {slot}@PAGE\n    ldr x16, [x16, {slot}@PAGEOFF]\n    br x16")
}
