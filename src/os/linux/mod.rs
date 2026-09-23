//! Linux implementations summary.
//! See the respective module header for the contract of each submodule.
//!
//! Everything here holds on every CPU Linux runs on. What one CPU encodes differently is in the
//! matching `crate::os_arch` pair, reached through a submodule's own re-export so callers keep
//! one name: `os::signal` and `os::thread` each forward their x86_64 half.
//!
//! Only the part of a subsystem this kernel answers differently from the other platform is here;
//! the shared part, and the whole of a subsystem no platform answers differently, live one level
//! up in the file named after the subsystem.

pub mod dll;
pub mod fs;
pub mod mem;
pub mod process;
pub mod signal;
pub mod thread;
