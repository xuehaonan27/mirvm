//! Per-function tier-up state and the cranelift compilation pipeline.
//!
//! - `state` (feature-free, so the TSan harness can compile it as well):
//!   `JitState` holds the PLT slot table, the call counters and the
//!   compile-request channel. The publication protocol is Release stores by the
//!   compile thread and Acquire loads in `dispatch::call_guest`.
//! - The pipeline (`#[cfg(feature = "cranelift")]`; its semantic contract is
//!   bit-for-bit agreement with the interpreter): `compiler` (per-Engine
//!   start/stop/worker plus Compiler/JITModule and eh_frame registration),
//!   `admit` (admission; a rejection means the function stays interpreted
//!   forever), `helpers` (mirvm_* runtime helpers and the libm symbol table),
//!   `translate` (slot SSA, place evaluation, three large matches), `frame`
//!   (conservative whole-set address-taken analysis) and `lsda_probe` (a
//!   cfg(test) check of the LSDA pipeline).
//!
//! The `state` publication protocol spans `dispatch::call_guest` (the read side)
//! and the compile workers here (the write side), so both sides must be read
//! together when either changes.

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
#[cfg(all(test, feature = "cranelift"))]
mod lsda_probe;
#[cfg(feature = "cranelift")]
mod translate;

#[cfg(feature = "cranelift")]
pub(crate) use compiler::{start, stop};
/// Test-only view of the helper frequency table, so a test can prove a path ran
/// rather than only that its effects match another path's.
#[cfg(all(test, feature = "cranelift"))]
pub(crate) use helpers::stat_value;

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

// Shared imports for the compilation pipeline, behind the feature gate; child
// modules inherit them through `use super::*`.
#[cfg(feature = "cranelift")]
use crate::vm::ctx::Shared;
#[cfg(feature = "cranelift")]
use crate::vm::ir::{
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
