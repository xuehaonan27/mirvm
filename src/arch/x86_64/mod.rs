//! x86_64 implementation summary

pub mod asmstub;
pub mod intrinsics;

pub(crate) use intrinsics::*;

/// The architecture's ELF machine identity: EM_X86_64 (`e_machine`). An `e_machine` value is
/// assigned to the CPU rather than to an operating system, so this is the same number on every ELF
/// platform this architecture runs on.
pub const ELF_MACHINE: u16 = 62;
