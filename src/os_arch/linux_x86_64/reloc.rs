//! The relocation type numbers an ELF64 image on this pair can carry.
//!
//! The numbering is the *object format's*, not the CPU's: these `R_X86_64_*` values are the ELF
//! psABI's, and the same CPU carries a different set (`X86_64_RELOC_*`) in a Mach-O object. What
//! each number *means* is [`crate::arch::reloc::Kind`], which every format classifies onto and
//! which therefore stays in the architecture axis.
//!
//! Applying a relocation to a mapped image, and the failure wording for a meaning this engine does
//! not apply, stay with the loader in `crate::vm::mcload`.

use crate::arch::reloc::Kind;

/// The relocation type numbers, named so no caller spells a raw one.
pub(crate) const R_X86_64_NONE: u32 = 0;
pub(crate) const R_X86_64_64: u32 = 1;
pub(crate) const R_X86_64_PC32: u32 = 2;
pub(crate) const R_X86_64_COPY: u32 = 5;
pub(crate) const R_X86_64_GLOB_DAT: u32 = 6;
pub(crate) const R_X86_64_JUMP_SLOT: u32 = 7;
pub(crate) const R_X86_64_RELATIVE: u32 = 8;
pub(crate) const R_X86_64_DTPMOD64: u32 = 16;
pub(crate) const R_X86_64_DTPOFF64: u32 = 17;
pub(crate) const R_X86_64_TPOFF64: u32 = 18;

/// Classifies a relocation type. A type outside this architecture's set is [`Kind::Unsupported`]
/// rather than an error, so the caller keeps its own error wording.
pub(crate) fn classify(ty: u32) -> Kind {
    match ty {
        R_X86_64_NONE => Kind::None,
        R_X86_64_64 => Kind::Abs64,
        R_X86_64_PC32 => Kind::Pc32,
        R_X86_64_COPY => Kind::Copy,
        R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT => Kind::Symbol,
        R_X86_64_RELATIVE => Kind::Relative,
        R_X86_64_DTPMOD64 | R_X86_64_DTPOFF64 | R_X86_64_TPOFF64 => Kind::Tls,
        _ => Kind::Unsupported,
    }
}
