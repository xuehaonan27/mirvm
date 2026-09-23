//! The relocation types an x86_64 object can carry.
//!
//! A relocation record patches one field of the mapped image, and its type says what the field
//! means: an absolute value, a place-relative one, or a slot the loader fills from a symbol. The
//! numbering is the architecture's own (the psABI's `R_X86_64_*`), which is why it is named here
//! rather than in a loader; applying a relocation to a mapped image is the loader's business, and
//! so is the failure wording for a type this engine does not apply.

/// What a relocation type asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// Nothing to patch.
    None,
    /// The place holds the load bias plus the addend.
    Relative,
    /// The place holds the symbol's address plus the addend.
    Abs64,
    /// The place holds `symbol + addend - place` in a signed 32-bit field.
    Pc32,
    /// The place holds the symbol's address (a GOT entry).
    Symbol,
    /// A thread-local storage relocation. mirvm maps no dynamic TLS block, so an image that needs
    /// one is refused rather than patched wrongly.
    Tls,
    /// A copy relocation: the object expects the loader to copy a symbol's bytes into it.
    Copy,
    /// A type this architecture defines and this engine does not apply.
    Unsupported,
}

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

/// The width of the field a type patches, in bytes.
pub(crate) fn field_width(kind: Kind) -> usize {
    match kind {
        Kind::Pc32 => 4,
        _ => 8,
    }
}
