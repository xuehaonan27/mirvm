//! The pure-Rust half of the native layer, source-shared.
//!
//! `src/native` also holds the archive converter, which needs a rustc session and therefore stays
//! out of this harness; the symbol-table reader below it is pure Rust and is what `vm` calls.

#[path = "../../../../../src/native/symtab.rs"]
pub(crate) mod symtab;
