//! Frozen pre-M4 spike artifacts, sealed once each corresponding risk was retired.
//!
//! - `spike1`: minimal model A skeleton (tree-walking interp_frame + slaved operand region +
//!   real addresses)
//! - `spike2`: i2c/c2i mixed-stack adapters (origin of the raw-ptr vmctx)
//! - `spike3`: mixed-stack unwind (candidate A confirmed; prototype of the FrameGuard protocol)
//! - `spike4`: concurrent TSan (engine Sync determination; `runtime.tsan` is the gate carrier)
//! - `spike5`: real Cranelift integration (P vs R data, eh_frame self-registration)
//! - `bytecode`/`frame`/`interp`/`memory`: spike-only frozen infrastructure; the real engine
//!   lives in ../engine/.
//!
//! Regression self-check (`mirvm spike1..5`) and code reference only; no further extension.

pub mod bytecode;
pub mod frame;
pub mod interp;
pub mod memory;
pub mod spike1;
pub mod spike2;
pub mod spike3;
pub mod spike4;
#[cfg(feature = "cranelift")]
pub mod spike5;
