//! The `syscall` rewrite this pair's kernel and CPU agree on.
//!
//! A rewritten `syscall` has to reach the trampoline in [`crate::arch::asmstub`] through a slot the
//! assembler lays down and mirvm refills at load time. Neither half is the CPU's alone: the slot is
//! a `.data` quad the object format carries, and the load is a relocation against it that only the
//! object format spells. That is why this text belongs to the pair rather than to the architecture.

/// The indirect slot definition, appended once to a `.s` when a `syscall` rewrite hits.
pub const SLOT_DEF: &str =
    ".data\n.globl mirvm_syscall_slot\n.p2align 3\nmirvm_syscall_slot: .quad 0\n.text\n";

/// Replacement body for a `syscall` instruction: a RIP-relative indirect call through the GOT entry
/// and then the slot. The asm-stub region is wrapped in `.intel_syntax noprefix`, so the Intel form
/// is required; GAS rejects AT&T's `*(%rip)`. `r11` is exactly the scratch register the syscall
/// contract allows to be clobbered, so using it as the springboard breaks nothing.
pub const CALL: &str = "    mov r11, QWORD PTR [rip+mirvm_syscall_slot@GOTPCREL]\n    call [r11]\n";
