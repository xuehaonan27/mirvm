//! OS Module
//! MirVM is the sole channel to the true OS. The business code of the engine
//! (`src/vm/`) and the loading phase (src/lower/) only calls primitives here,
//! and the `libc::` / `std::os::unix` touchpoints in engine should be banned.
//!
//! # Boundary Contract
//!
//! - **Leaf**: This layer does not depend on engine/lower/rustc_private;
//!   function signatures only include [`usize`]/[`u64`]/raw pointers/own small
//!   enumerations. No guest concept allowed. The platform's own ABI structures
//!   (`libc::sigaction`, `libc::ucontext_t`) are the one exception, and only
//!   across this boundary: they are what the kernel speaks, and the engine sees
//!   wrapped types instead.
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
//!
//! What is specific to one kernel *and* one CPU at once is not here: it is in
//! [`crate::os_arch`], whose pair ladder this module reaches through an ordinary
//! subsystem name, so a caller does not have to know the pair.

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

/// Why a host primitive did not do what it was asked.
///
/// This layer only posts what the kernel or libc said, so the classes are the calls themselves: the
/// loader refusing a library (with libc's own message as the detail) and `mprotect` refusing a
/// protection change (whose operands travel as fields, because a caller that has to act on this
/// needs the address and the return code, not the sentence). The layer's boundary contract holds:
/// no guest concept appears here.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    #[error("{detail}")]
    Dlopen { detail: String },

    #[error("mprotect({addr:#x}, {size:#x}) failed rc={rc}")]
    Mprotect { addr: usize, size: usize, rc: i32 },
}

crate::diag_codes! {
    Error: Os => {
        Dlopen => "os.dlopen",
        Mprotect => "os.mprotect",
    }
}

#[cfg(not(target_os = "linux"))]
compile_error!(
    "The OS module currently only implements Linux (with the same prerequisites as the x86_64 hard gate of global_asm/asm-stub)."
);
