//! The relocation type numbers an x86_64 object can carry.
//!
//! The numbering is the architecture's own (the psABI's `R_X86_64_*`). What each number *means* is
//! [`crate::arch::reloc::Kind`], because that vocabulary is the same on every psABI; this file owns
//! only the numbers and the mapping onto those meanings.

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
