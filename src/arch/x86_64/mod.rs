//! x86_64 implementation summary
//!
//! What a caller outside `src/arch/` may name is declared in `super`; the items here are this
//! architecture's answer, plus the instruction implementations `super` names directly.

pub mod asm_text;
pub mod asmstub;
pub mod intrinsics;

/// The architecture's ELF machine identity: EM_X86_64 (`e_machine`). An `e_machine` value is
/// assigned to the CPU rather than to an operating system, so this is the same number on every ELF
/// platform this architecture runs on.
pub const ELF_MACHINE: u16 = 62;

/// The register Cranelift reserves when a module enables the pinned register, as this architecture
/// names it. Cranelift's own register environment documents the choice (`isa/x64/inst/regs.rs`,
/// there as matching Spidermonkey's `HeapReg`).
pub const PINNED_REG: &str = "r15";

/// This architecture's answer to the 16-bit float lane conversions [`super`](crate::arch) names.
///
/// A conversion is one instruction only where the F16C feature is present, and the guest's own
/// build decides that rather than this one, so the answer here is the software model that
/// [`intrinsics`] already carries: it is bit-identical to `VCVTPS2PH`/`VCVTPH2PS` including the
/// forced quiet bit, which a host libcall's NaN handling does not guarantee.
pub mod f16 {
    use super::intrinsics::{HalfRound, f16_to_f32_sw, f32_to_f16_sw};

    /// f16 bit pattern → f32 bit pattern.
    pub fn to_f32(bits: u16) -> u32 {
        f16_to_f32_sw(bits)
    }

    /// f32 bit pattern → f16 bit pattern, rounding to nearest even.
    pub fn to_f16(bits: u32) -> u16 {
        f32_to_f16_sw(bits, HalfRound::Rne)
    }
}
