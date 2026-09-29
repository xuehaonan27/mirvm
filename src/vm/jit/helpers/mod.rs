//! The bodies compiled guest code calls: `mirvm_*` helpers reached through import symbols, the
//! direct-eval lanes (128-bit, f128, f16), the standard C math symbols, and the helper statistics
//! `compiler.rs` registers.
//!
//! A body with a portable shape calls the matching [`crate::vm::semantics`] body rather than
//! repeating it, so a guest value cannot differ between the two backends.
//!
//! The files are split by what a caller asks for: [`entries`] holds every `mirvm_*` entry point
//! the import whitelist registers, [`libm`] the C math addresses, [`floats`] the wide-float
//! direct-eval lanes, and [`stats`] the counters and the ledgers built on them.

mod entries;
mod floats;
mod libm;
mod stats;

use super::*;

/// Re-exported at the parent so `compiler.rs` keeps resolving every helper symbol through
/// `helpers::*`, wherever the body lives.
pub(crate) use entries::*;
pub(crate) use floats::*;
pub(crate) use libm::*;
pub(crate) use stats::*;
