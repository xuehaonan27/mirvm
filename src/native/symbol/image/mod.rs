//! The synthetic symbol image: bytes that make an instruction-pointer token resolvable.
//!
//! A process symbolizer discovers the objects a process has loaded and reads their symbol tables
//! itself, so the way to make a synthetic instruction-pointer token resolvable is to hand it a
//! real, loadable object that defines a symbol at that address. [`build`] writes one for the format
//! the platform's loader accepts, and the two writers below agree on the one thing a caller depends
//! on: what the returned offset means.

pub(crate) mod elf;
pub(crate) mod macho;

/// The object a process symbolizer is named by, in the format `format` names.
///
/// Which format is worth writing is the platform's answer, because it is the platform's loader that
/// has to accept the result; the bytes are this layer's either way.
pub fn build(
    format: crate::native::object::ObjectFormat,
    names: &[Box<str>],
) -> Result<(Vec<u8>, usize), crate::error::Error> {
    match format {
        crate::native::object::ObjectFormat::Elf => elf::build_elf(names),
        crate::native::object::ObjectFormat::MachO => macho::build(names),
    }
}
