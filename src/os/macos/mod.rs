//! macOS implementations summary.
//!
//! See the respective module header for the contract of each submodule. Everything this platform
//! answers the same way as the other one lives one level up, in the file named after the
//! subsystem; what is here is only the difference.

pub mod dll;
pub mod fs;
pub mod linker;
pub mod mem;
pub mod process;
pub mod thread;

unsafe extern "C" {
    /// The kernel port for this task.
    ///
    /// Declared here rather than taken from `libc`, all of whose Mach bindings carry a deprecation
    /// notice pointing at a crate mirvm does not depend on. This layer already declares the
    /// platform symbols it needs; the value is the one the C library itself reads.
    static mach_task_self_: libc::mach_port_t;
}

/// The task port this process's Mach calls need.
pub(crate) fn task_self() -> libc::mach_port_t {
    unsafe { mach_task_self_ }
}
