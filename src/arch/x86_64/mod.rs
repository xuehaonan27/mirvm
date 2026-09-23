//! x86_64 implementation summary
//!
//! What a caller outside `src/arch/` may name is declared in `super`; the items here are this
//! architecture's answer, plus the instruction implementations `super` names directly.

pub mod asm_text;
pub mod asmstub;
pub mod intrinsics;
pub mod reloc;

pub(crate) use intrinsics::*;

/// The architecture's ELF machine identity: EM_X86_64 (`e_machine`). An `e_machine` value is
/// assigned to the CPU rather than to an operating system, so this is the same number on every ELF
/// platform this architecture runs on.
pub const ELF_MACHINE: u16 = 62;
