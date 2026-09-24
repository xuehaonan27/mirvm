//! The pure-Rust half of the native layer, source-shared.
//!
//! `src/native` also holds the archive converter, which needs a rustc session and therefore stays
//! out of this harness. What is left is pure Rust and is what `vm` calls: the symbol-table reader,
//! the symbol image it builds for the backtrace, where an image's constructors are, and the byte
//! layouts it and the loader work on.

#[path = "../../../../../src/native/symtab.rs"]
pub(crate) mod symtab;
#[path = "../../../../../src/native/elf.rs"]
pub(crate) mod elf;
#[path = "../../../../../src/native/ar.rs"]
pub(crate) mod ar;
#[path = "../../../../../src/native/symimage.rs"]
pub(crate) mod symimage;
#[path = "../../../../../src/native/macho.rs"]
pub(crate) mod macho;
#[path = "../../../../../src/native/lifecycle.rs"]
pub(crate) mod lifecycle;
