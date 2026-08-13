//! M4 引擎（真身）：类型化字节码 + 字节区帧 + 类型化 interp_frame。
//! 纯 Rust、零 rustc_private——tsan harness 同源编译 = 执行相纯度的机械门禁。
//! 加载相（MIR→本 IR 的降低）在 src/lower/（rustc_private 域），产物经 `ir::Module` 交接。

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
#[allow(dead_code)]
pub(crate) mod tsan_mt;
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
