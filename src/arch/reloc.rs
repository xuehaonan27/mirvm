//! What a relocation asks for, which is not the architecture's to say.
//!
//! A relocation record patches one field of an image, and the *meaning* of that patch is one
//! vocabulary on every psABI: an absolute value, a place-relative one, a slot the loader fills from
//! a symbol, a thread-local one, or a copy. No architecture and no object format owns this
//! vocabulary — each psABI and each format classifies its own numbering onto it — so a caller reads
//! the meaning here without naming either, which is what lets the loader in `crate::vm::mcload` stay
//! neutral. The *classification* is the pair's, in `crate::os_arch::reloc`, because the numbering it
//! reads belongs to the object format.
//!
//! Applying a patch to a mapped image, and the failure wording for a meaning this engine does not
//! apply, stay with that loader.

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

/// The width of the field a meaning patches, in bytes.
///
/// A property of the meaning rather than of the encoding: a place-relative patch is 32-bit on every
/// psABI this engine targets, and every other meaning patches a full pointer-sized field. The
/// loader bounds-checks with it before writing.
pub(crate) fn field_width(kind: Kind) -> usize {
    match kind {
        Kind::Pc32 => 4,
        _ => 8,
    }
}
