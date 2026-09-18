//! jit 模块（M5.3+）：per-fn 分层的状态基座 + cranelift 编译管线。
//!
//! - `state`（feature-free，TSan harness 同源编译）：JitState = PLT 槽表 +
//!   调用计数 + 编译请求通道；发布协议 = 编译线程 Release 写 / call_guest
//!   Acquire 读（m5.3-design D4）。
//! - 编译管线（`#[cfg(feature = "cranelift")]`；语义契约 = 与解释器逐位一致）：
//!   `compiler`（逐 Engine start/stop/worker + Compiler/JITModule + eh_frames 注册）、
//!   `admit`（准入族：拒绝 = 永留解释）、`helpers`（mirvm_* 运行期助手 +
//!   libm 符号表）、`translate`（Translator：槽 SSA + place 求值 + 三个大
//!   match）、`frame`（FrameMap/analyze_frame 取址分析保守全集）、
//!   `lsda_probe`（M5.4 前置 LSDA 管线验证，cfg(test)）。
//!
//! 与 `interp` 的共享状态耦合：interp::call_guest 是发布协议读侧锚点，
//! 本模块 worker 是写侧——两侧函数的语义注释不可分离（结构重构战役片5）。

mod state;
pub use state::*;

#[cfg(feature = "cranelift")]
mod admit;
#[cfg(feature = "cranelift")]
mod compiler;
#[cfg(feature = "cranelift")]
mod frame;
#[cfg(feature = "cranelift")]
mod helpers;
#[cfg(all(
    test,
    feature = "cranelift",
    target_arch = "x86_64",
    target_os = "linux"
))]
mod lsda_probe;
#[cfg(feature = "cranelift")]
mod translate;

#[cfg(feature = "cranelift")]
pub(crate) use compiler::{start, stop};
/// Test-only view of the helper frequency table, so a test can prove a path ran
/// rather than only that its effects match another path's.
#[cfg(all(test, feature = "cranelift"))]
pub(crate) use helpers::stat_value;

/// Register one complete `.eh_frame` section with the process unwinder.
///
/// The CIE records at the start of the section are shared by its FDE records, so
/// the registration unit must be the complete, zero-terminated section. The
/// unwinder retains the bytes for the process lifetime.
#[cfg(feature = "cranelift")]
pub(crate) fn register_eh_frame_section(mut bytes: Vec<u8>) {
    unsafe extern "C" {
        fn __register_frame(begin: *const u8);
    }

    bytes.extend_from_slice(&[0, 0, 0, 0]);
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    unsafe { __register_frame(bytes.as_ptr()) };
}

/// Enter one packed trace body through the trace domain's boundary entry.
///
/// A trace body reads this thread's recorder from the pinned register, so it may
/// only run behind the trampoline that installs that register -- which also
/// restores it afterwards, on the unwinding path as well as the normal one. The
/// producer is always handed in from the activation, so a native callback that
/// re-enters the guest can never be served a register value some other ABI left
/// behind.
///
/// # Safety
///
/// `enter` must be the boundary entry the trace compiler published, `body` a
/// packed body compiled into the same module, and `producer` the calling
/// thread's live recorder.
///
/// Without the code generator no trace entry can be published, so that build
/// keeps the same signature and never reaches the body.
#[cfg(not(feature = "cranelift"))]
pub(crate) unsafe fn call_trace_body(
    _enter: u64,
    _producer: *mut crate::telemetry::capture::Producer,
    _body: u64,
    _args: &[u64],
    _ret: &mut [u64; 2],
) -> (u64, u64) {
    unreachable!("this build has no code generator, so it publishes no trace body")
}

/// Enter one packed trace body through the trace domain's boundary entry.
///
/// A trace body reads this thread's recorder from the pinned register, so it may
/// only run behind the trampoline that installs that register -- which also
/// restores it afterwards, on the unwinding path as well as the normal one. The
/// producer is always handed in from the activation, so a native callback that
/// re-enters the guest can never be served a register value some other ABI left
/// behind.
///
/// # Safety
///
/// `enter` must be the boundary entry the trace compiler published, `body` a
/// packed body compiled into the same module, and `producer` the calling
/// thread's live recorder.
#[cfg(feature = "cranelift")]
pub(crate) unsafe fn call_trace_body(
    enter: u64,
    producer: *mut crate::telemetry::capture::Producer,
    body: u64,
    args: &[u64],
    ret: &mut [u64; 2],
) -> (u64, u64) {
    type TraceEnter = unsafe extern "C-unwind" fn(u64, u64, *const u64, *mut u64);
    let f: TraceEnter = unsafe { std::mem::transmute(enter as usize) };
    unsafe { f(producer as u64, body, args.as_ptr(), ret.as_mut_ptr()) };
    (ret[0], ret[1])
}

// 编译管线的共享 imports（feature 门内；子模块经 `use super::*` 继承）。
#[cfg(feature = "cranelift")]
use crate::vm::engine::ctx::Shared;
#[cfg(feature = "cranelift")]
use crate::vm::engine::ir::{
    self, IntBinOp, IntCc, Operand, OvfOp, ParamAbi, RetAbi, RetDest, ScalarPlace, Slot, Stmt,
    SwitchDiscr, Terminator, UnwindAction, Width,
};
#[cfg(feature = "cranelift")]
use cranelift_codegen::ir::condcodes::IntCC;
#[cfg(feature = "cranelift")]
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlagsData, Signature, StackSlot, StackSlotData, StackSlotKind,
    TrapCode, Value, types,
};
#[cfg(feature = "cranelift")]
use cranelift_codegen::isa::unwind::UnwindInfo;
#[cfg(feature = "cranelift")]
use cranelift_codegen::settings::{self, Configurable};
#[cfg(feature = "cranelift")]
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
#[cfg(feature = "cranelift")]
use cranelift_jit::{JITBuilder, JITModule};
#[cfg(feature = "cranelift")]
use cranelift_module::{FuncId as ClifFuncId, Linkage, Module as ClifModule};
#[cfg(feature = "cranelift")]
use std::sync::atomic::Ordering;
#[cfg(feature = "cranelift")]
use std::sync::mpsc::{Receiver, Sender};
