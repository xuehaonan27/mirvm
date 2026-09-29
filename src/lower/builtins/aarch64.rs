//! This CPU's own guest intrinsic names: none yet.
//!
//! A name lands here beside the [`ir::Builtin`] its guest lowers to, and its body lands in
//! [`crate::arch::aarch64::intrinsics`], exactly as every name in `super::x86_64` pairs with
//! `crate::arch::x86_64::intrinsics`. That file answers each of x86_64's names with one unreachable
//! body so the ladder stays interchangeable; this table has nothing to answer yet, because the
//! intrinsic boundary belongs to the guest and no lowering of this CPU's guest produces such a
//! name. The operations that guest does reach — the portable ones, `simd`, f16/f128, the
//! atomic and allocator families — belong to the shared registration in `super`.

use rustc_data_structures::fx::FxHashMap;
use rustc_span::Symbol;

use crate::vm::ir;

/// Register every guest intrinsic name this CPU's lowering can produce.
pub(super) fn register(_out: &mut FxHashMap<Symbol, ir::Builtin>) {}
