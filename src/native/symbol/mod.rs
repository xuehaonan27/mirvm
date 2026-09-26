//! What an object's symbols, lifecycle and synthetic naming mean to mirvm.
//!
//! The byte layouts are [`crate::native::object`]'s; this is the layer that resolves a name through
//! them: the symbol tables a static archive's converted image exposes ([`symtab`]), the object that
//! makes a synthetic instruction pointer resolvable ([`image`]), and where an image's constructors
//! and destructors are ([`lifecycle`]). One file per format where a format says something a reader
//! has to interpret, because the difference between two formats here is what each one calls the same
//! thing rather than how the bytes are laid out.
//!
//! Nothing here drives a toolchain: everything it answers comes out of bytes it was handed.

pub(crate) mod image;
pub(crate) mod lifecycle;
pub(crate) mod symtab;
