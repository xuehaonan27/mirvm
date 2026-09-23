//! x86_64 implementation summary
//!
//! What a caller outside `src/arch/` may name is declared in `super`; the items here are this
//! architecture's answer, plus the instruction implementations `super` names directly.

pub mod asm_text;
pub mod asmstub;
pub mod intrinsics;

pub(crate) use intrinsics::*;

/// The architecture's ELF machine identity: EM_X86_64 (`e_machine`). An `e_machine` value is
/// assigned to the CPU rather than to an operating system, so this is the same number on every ELF
/// platform this architecture runs on.
pub const ELF_MACHINE: u16 = 62;

/// The register Cranelift reserves when a module enables the pinned register, as this architecture
/// names it. Cranelift's own register environment documents the choice (`isa/x64/inst/regs.rs`,
/// there as matching Spidermonkey's `HeapReg`).
pub const PINNED_REG: &str = "r15";
