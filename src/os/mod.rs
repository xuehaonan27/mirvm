//! OS Module
//! MirVM's sole channel to the platform outside itself: the C library (pthread, `dlopen`, the math
//! symbols, `errno`) and the kernel (mappings, `/proc`, process and signal primitives). The engine
//! (`src/vm/`) and the loading phase (`src/lower/`) call primitives here and name no platform item
//! themselves; `repo-quality`'s platform-boundary check is what holds that.
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
//! # The Surface a Platform Provides
//!
//! The seven subsystems are `dll` (the dynamic loader), `fs` (descriptors and paths), `mem`
//! (anonymous mappings), `process` (the process and its exit), `signal` (dispositions and masks),
//! `thread` (pthreads and mirvm's own thread accounting), and `unwind` (the Itanium unwinder).
//! Every call site spells `os::<subsystem>::…`, so a port fills in a directory rather than teaching
//! call sites a new name.
//!
//! A subsystem whose knowledge is entirely the platform's is dispatched here. A subsystem that also
//! carries vocabulary every platform shares — a mask, a protection, a load mode, a counter of
//! mirvm's own threads — owns a file at this level instead, which holds the shared part and selects
//! the platform's half through one `#[cfg]` ladder of its own. A subsystem no platform answers
//! differently at all owns a file here with no ladder and no platform half. That is what keeps a
//! second platform from restating what is not its own.
//!
//! # Platform Selection
//! `linux/` and `macos/` are the implementations; a platform that is neither fails to compile at
//! compile time. Adding one means a parallel implementation directory and an arm in each ladder
//! that names a platform.
//!
//! What is specific to one kernel *and* one CPU at once is not here: it is in
//! [`crate::os_arch`], whose pair ladder this module reaches through an ordinary
//! subsystem name, so a caller does not have to know the pair.

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;

/// Subsystems carrying vocabulary or bodies every platform shares; each selects its own
/// implementation through its own ladder.
pub(crate) mod dll;
pub(crate) mod fs;
pub(crate) mod mem;
pub(crate) mod signal;
pub(crate) mod thread;

/// The Itanium unwinder, which is the same library interface on every platform: no ladder.
pub(crate) mod unwind;

/// Subsystems whose knowledge is entirely the platform's, dispatched here.
#[cfg(target_os = "linux")]
pub(crate) use linux::{linker, process};
#[cfg(target_os = "macos")]
pub(crate) use macos::{linker, process};

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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Not implemented for this target.");
