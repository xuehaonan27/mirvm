//! `vm` -- model A execution phase (**pure Rust, zero rustc_private**: the tsan harness
//! compiles this module via `#[path]`, so any rustc type leaking in fails the build).
//!
//! Typed bytecode + byte-region frames + typed interp_frame. The load phase (lowering MIR to this
//! IR) lives in `src/lower/` (rustc_private domain); its product is handed off via `ir::Module`.

pub(crate) mod addrlayout;
pub(crate) mod backtrace;
pub(crate) mod codearena;
pub(crate) mod ctx;
pub(crate) mod deferred;
pub(crate) mod ffi;
pub(crate) mod frame;
pub(crate) mod frozen;
pub(crate) mod heap;
pub(crate) mod interp;
pub(crate) mod ir;
pub(crate) mod jit;
pub(crate) mod mcload;
pub(crate) mod native_instance;
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
