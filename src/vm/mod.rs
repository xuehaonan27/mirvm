//! `vm` -- model A execution phase (**pure Rust, zero rustc_private**: the tsan harness
//! compiles this module via `#[path]`, so any rustc type leaking in fails the build).
//!
//! The load phase (lowering MIR to this IR) lives in `src/lower/` (rustc_private domain); its
//! product is handed off via [`ir::Module`]. Execution is then four concerns:
//!
//! * [`ir`] -- what a guest body is, and the tables a linked artifact carries.
//! * [`semantics`] -- what an operation computes, defined once for both backends.
//! * [`interp`] and [`jit`] -- the two backends. [`dispatch`] is what enters guest code and
//!   picks between them, and [`verify`] checks an artifact before either one runs it.
//! * [`ctx`] -- the per-thread execution state and the Engine's lifetime, which every one of
//!   the above borrows.
//!
//! The remaining modules are the services execution needs. Guest-visible ones: [`atexit`] (the
//! exit handlers), [`backtrace`] (the symbol carriers and the frame merge), [`ffi`] (foreign
//! calls), [`heap`] (the managed heap), [`signal`] (guest signal installation). Storage the
//! artifact occupies: [`codearena`], [`frame`], [`frozen`], [`instance`], [`mcload`],
//! [`native_instance`]. Engine substrate: [`deferred`], [`thunks`], [`unwind`]. [`stats`]
//! reports on a loaded module for the CLI.

pub(crate) mod atexit;
pub(crate) mod backtrace;
pub(crate) mod codearena;
pub(crate) mod ctx;
pub(crate) mod deferred;
pub(crate) mod dispatch;
pub(crate) mod ffi;
pub(crate) mod frame;
pub(crate) mod frozen;
pub(crate) mod heap;
pub(crate) mod instance;
pub(crate) mod interp;
pub(crate) mod ir;
pub(crate) mod jit;
pub(crate) mod mcload;
pub(crate) mod native_instance;
pub(crate) mod native_lifecycle;
pub(crate) mod semantics;
pub(crate) mod signal;
pub(crate) mod stats;
pub(crate) mod thunks;
pub(crate) mod unwind;
pub(crate) mod verify;

pub use ctx::{Engine, EngineState, WaitClosedError};
pub use interp::{RawReturn, RunError, RunErrorKind, RunOutcome, run_main};

/// Low-level embedding surface for trusted, manually constructed VM IR.
///
/// This is deliberately separate from the safe Engine API: a raw Module may
/// contain native addresses and untyped ABI slots that structural verification
/// cannot prove valid. Construct it only for trusted tooling and fixtures, then
/// pass it to [`Engine::from_module_unchecked`].
pub mod raw {
    pub use super::interp::run_export as run_export_raw;
    pub use super::ir::*;
}

#[cfg(test)]
mod embed_tests;
