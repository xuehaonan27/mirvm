//! The `syscall` rewrite this pair's kernel and CPU agree on.
//!
//! See the x86_64 pair's file for why this text is the pair's rather than the architecture's. The
//! slot definition is the same text as there — `.data` and `.quad` are what both object formats
//! call them — and only the call body differs, because a PC-relative load to a symbol is spelled
//! with this format's page/offset relocations.

/// The indirect slot definition, appended once to a `.s` when a `syscall` rewrite hits.
pub const SLOT_DEF: &str =
    ".data\n.globl mirvm_syscall_slot\n.p2align 3\nmirvm_syscall_slot: .quad 0\n.text\n";

/// Replacement body for a `svc` instruction: load the slot's address, load the trampoline out of
/// it, and branch. The two-level indirection keeps the property the x86 form has — nothing external
/// is named, so the `.so` stays self-contained.
///
/// The springboard is `x17`, not `x16`: this platform's syscall ABI carries the call number in
/// `x16`, so a rewrite that loaded through it would destroy the number before the trampoline could
/// read it. `x17` is the other register that ABI already reserves for this kind of use, and no
/// argument travels in it.
pub const CALL: &str = "    adrp x17, mirvm_syscall_slot@PAGE\n    ldr x17, [x17, mirvm_syscall_slot@PAGEOFF]\n    blr x17\n";
