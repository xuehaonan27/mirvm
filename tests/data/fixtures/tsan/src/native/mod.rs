//! The pure-Rust half of the native layer, source-shared.
//!
//! `src/native` also holds the archive converter, which needs a rustc session and therefore stays
//! out of this harness. What is left is pure Rust and is what `vm` calls: the byte layouts, the
//! symbol-table reader, the symbol image it builds for the backtrace, where an image's constructors
//! are, and the assembler vocabulary. The paths below are relative to this file, so the mirror keeps
//! the shape of the tree they come from.

#[path = "../../../../../../src/native/object/mod.rs"]
pub(crate) mod object;
#[path = "../../../../../../src/native/symbol/mod.rs"]
pub(crate) mod symbol;
pub(crate) mod artifact;
