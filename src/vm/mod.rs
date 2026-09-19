//! `vm` -- model A execution phase (**pure Rust, zero rustc_private**: the tsan harness
//! compiles this module via `#[path]`, so any rustc type leaking in fails the build).
//!
//! - `engine/`: the engine (typed bytecode IR / place evaluation / FrameGuard unwind /
//!   mimalloc heap / frozen region / dlsym+libffi direct calls).
//!
//! The pre-M4 spike tree that used to live here was archived: its conclusions are in
//! `docs/history/spike*.md`, and the one case set still worth running under TSan (engine
//! Sync, blocking-syscall liveness, host-atomic interop) now lives in the harness itself
//! (`tsan/src/spike4/`), so nothing in the product depends on it.

pub mod engine;
