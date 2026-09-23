//! aarch64 implementation summary
//!
//! What a caller outside `src/arch/` may name is declared in `super`; the items here are this
//! architecture's answer.
//!
//! `asm_text` and `reloc` are not here yet, and neither is the `syscall` trampoline. All three name
//! the object format rather than the CPU — the symbol-reference spelling of a rewritten `syscall`,
//! and the relocation numbering a loader reads — so they can only be written once that fact has a
//! home on the axes. Everything in `asmstub` below is instruction encoding and nothing else.

pub mod asmstub;

/// The architecture's ELF machine identity: EM_AARCH64 (`e_machine`). An `e_machine` value is
/// assigned to the CPU rather than to an operating system, so this is the same number on every ELF
/// platform this architecture runs on.
pub const ELF_MACHINE: u16 = 183;

/// The register Cranelift reserves when a module enables the pinned register, as this architecture
/// names it. Cranelift's own register environment documents the choice (`isa/aarch64/abi.rs`).
pub const PINNED_REG: &str = "x21";
