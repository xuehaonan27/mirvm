//! The relocation types a Mach-O image on this pair can carry.
//!
//! Mach-O does not use the ELF psABI's numbering: it has its own `ARM64_RELOC_*` set, addressed by
//! offset rather than by symbol index, and the loader that would apply them does not exist yet —
//! `vm/mcload` reads ELF64 headers today, and the Mach-O half of that work is not written.
//!
//! So this file classifies nothing as applicable. Every type reports
//! [`crate::arch::reloc::Kind::Unsupported`], which the loader turns into a refusal naming the
//! type; that is the honest answer while the Mach-O loader is missing, and it is strictly better
//! than mapping numbers onto meanings they do not have. The ELF pair beside this one shows what a
//! complete classification looks like.

use crate::arch::reloc::Kind;

/// Classifies a relocation type.
///
/// Always [`Kind::Unsupported`] until the Mach-O loader exists: see the module header.
pub(crate) fn classify(_ty: u32) -> Kind {
    Kind::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_classified_as_applicable_before_the_loader_exists() {
        for ty in [0_u32, 1, 2, 8, 16, 0xffff_ffff] {
            assert_eq!(classify(ty), Kind::Unsupported);
        }
    }
}
