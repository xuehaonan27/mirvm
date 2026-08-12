//! OS Module
//! MirVM is the sole channel to the true OS. The business code of the engine
//! (`src/vm/`) and the loading phase (src/lower/) only calls primitives here,
//! and the `libc::` / `std::os::unix` touchpoints in engine should be banned.
//!
//! # Boundary Contract
//!
//! - **Leaf**: This layer does not depend on engine/lower/rustc_private;
//!   function signatures only include [`usize`]/[`u64`]/raw pointers/own small
//!   enumerations. No guest concept allowed.
//! - **Primitives**: Guest semantic adjudication, examples:
//!   - single-threaded fork guards
//!   - signal handler whitelists
//!   - sigaction structure modification and copying
//!   - loud rejection statements)
//!     all remain on the engine business side.
//!     This module only performs honest OS calls and error posting.
//! - **Passthrough Priority**: If passthrough is possible, avoid wrapping
//!   (C10: never extensively intercept native operations). Unlisted syscall
//!   families are passed through single-point parameter variation via
//!   `os::process::syscall6`, without creating a shell for each syscall.
//!
//! # Platform Selection
//! Currently, the only implementation is `linux/` (same premise as the x86_64
//! hard gate of `lower/global_asm.rs` and the asm-stub factory). Non-Linux
//! targets will fail to compile at compile time—honestly, portability is not
//! implemented. Adding a new platform means parallel implementation directory,
//! and cfg dispatch here.

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(not(target_os = "linux"))]
compile_error!(
    "The OS module currently only implements Linux (with the same prerequisites as the x86_64 hard gate of global_asm/asm-stub)."
);
