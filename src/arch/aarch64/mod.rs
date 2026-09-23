//! aarch64 implementation summary
//!
//! What a caller outside `src/arch/` may name is declared in `super`; the items here are this
//! architecture's answer.
//!
//! `reloc` is not here yet: the relocation numbering a loader reads is the object format's, so it
//! can only be written once that fact has a home on the axes. The `syscall` rewrite text is no
//! longer part of this architecture either — it names the format as well as the instruction, and
//! lives in `crate::os_arch::syscall_asm`.

pub mod asm_text;
pub mod asmstub;

/// The architecture's ELF machine identity: EM_AARCH64 (`e_machine`). An `e_machine` value is
/// assigned to the CPU rather than to an operating system, so this is the same number on every ELF
/// platform this architecture runs on.
pub const ELF_MACHINE: u16 = 183;

/// The register Cranelift reserves when a module enables the pinned register, as this architecture
/// names it. Cranelift's own register environment documents the choice (`isa/aarch64/abi.rs`).
pub const PINNED_REG: &str = "x21";
