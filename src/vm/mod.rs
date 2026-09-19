//! `vm` -- model A execution phase (**pure Rust, zero rustc_private**: the tsan harness
//! compiles this module via `#[path]`, so any rustc type leaking in fails the build).
//!
//! - `engine/`: the engine (typed bytecode IR / place evaluation / FrameGuard unwind /
//!   mimalloc heap / frozen region / dlsym+libffi direct calls).
//! - `spikes/`: frozen validation artifacts (model A skeleton, i2c/c2i, mixed-stack
//!   unwind, concurrent TSan, real Cranelift). Regression self-check via
//!   `mirvm spike1..5` only; do not modify or extend.

pub mod engine;
pub mod spikes;
