//! Linux process primitives.
//!
//! Primitives are not adjudicated. Detailed behaviours like single-threaded
//! fork guards, abort documentation, atexit registry, and LIFO callback
//! execution, are all left to the engine. This module only performs honest
//! libc calls.
//! System calls that's unlisted here, should only pass through [`syscall`].
//! Currently os::process does not build a shim for every system call. But may
//! do so later.
//! Address values are u64 encoded (leaf type discipline).

/// Safe wrapper of `getenv(3)`.
/// `name_addr`: guest side NUL-terminated string's  true address.
/// Returns true address, or 0 on failure.
pub fn getenv(name_addr: u64) -> u64 {
    unsafe { libc::getenv(name_addr as *const libc::c_char) as u64 }
}

/// write(2): returns the byte count written or -1 (errno semantics stay with the
/// caller).
pub fn write_fd(fd: i32, buf_addr: u64, len: usize) -> i64 {
    unsafe { libc::write(fd, buf_addr as *const libc::c_void, len) as i64 }
}

/// strlen(3) on a guest-side NUL-terminated string's true address.
pub fn c_strlen(s_addr: u64) -> u64 {
    unsafe { libc::strlen(s_addr as *const libc::c_char) as u64 }
}

/// fork(2): the parent returns the child pid, the child returns 0, failure -1.
/// The single-thread guard lives in the engine; the exec family goes through the
/// foreign passthrough instead of this entry.
pub fn fork() -> i64 {
    unsafe { libc::fork() as i64 }
}

/// raise(3): synchronously delivers a signal to the current host thread. The
/// engine decides the ordering of synchronous guest handler execution upstairs;
/// this layer only preserves libc's return value and errno semantics.
pub fn raise(signum: i32) -> i32 {
    unsafe { libc::raise(signum) }
}

/// Read/write the calling pthread's libc `errno`. Callers must read it
/// immediately after the failing libc operation, before formatting or any
/// other library call can overwrite it.
pub fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

pub fn set_errno(value: i32) {
    unsafe { *libc::__errno_location() = value };
}

/// Terminate this process now, without running a Rust destructor, an `atexit` handler or a stdio
/// flush. Used from the signal mailbox, where the state that would run those is exactly what is
/// already known to be broken; the status is the caller's, because only the caller knows which
/// invariant failed.
pub fn exit_now(status: i32) -> ! {
    unsafe { libc::_exit(status) }
}

/// atexit(3): registers a native trampoline through the libc `atexit` the engine
/// links, not a guest `dlsym`.
pub fn atexit_native(cb: extern "C" fn()) -> i32 {
    unsafe { libc::atexit(cb) }
}

/// libcall symbol address for JIT-compiled code (registered with cranelift
/// `jb.symbol`): compiled code calls the real libc function directly, so address
/// resolution belongs to the OS symbol surface.
pub fn memmove_addr() -> *const u8 {
    libc::memmove as *const u8
}
/// Same as above (memset).
pub fn memset_addr() -> *const u8 {
    libc::memset as *const u8
}
/// Same as above (memcmp).
pub fn memcmp_addr() -> *const u8 {
    libc::memcmp as *const u8
}

/// Dispatch entry for `syscall` instructions that asm stubs rewrote to call
/// `mirvm_syscall_trampoline` through an indirect slot (arch::x86_64::asmstub;
/// integer/flags/xmm/mxcsr state is already preserved under the real syscall
/// discipline).
///
/// Currently a passthrough plus the `MIRVM_SYSCALL_TRACE` knob. This is also the
/// hook point for virtualization semantics: a unified fd space, a fake FS, or
/// accounting would triage here, with no change at the call sites.
///
/// # Safety
/// Called only by the trampoline under the syscall contract: `args` points to 6
/// u64s (a1..a6).
#[unsafe(no_mangle)]
pub extern "C" fn mirvm_syscall_dispatch(nr: i64, args: *const u64) -> i64 {
    let args: &[u64] = unsafe { std::slice::from_raw_parts(args, 6) };
    if crate::options::get().syscall_trace {
        // Direct sink: this is an `extern "C"` entry on the guest syscall path, where taking the
        // capture tee's lock is unsafe. The trace is a dev knob, so its absence from a capture's
        // `diagnostics.log` is intentional.
        crate::diag_direct!(
            Syscall,
            "nr={nr} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x} a6={:#x}",
            args[0],
            args[1],
            args[2],
            args[3],
            args[4],
            args[5]
        );
    }
    let result = syscall(nr, args);
    // A generic `SYS_fork` that returns here is a fork child. The interpreter's
    // HostFork builtin notifies separately; both converge on the same hook so
    // coverage does not depend on how the guest spelled the fork.
    crate::telemetry::capture::fork_child_guard(nr, result);
    result
}
/// syscall(2) varargs passthrough: the only channel for syscall families not
/// listed above. Takes the first 6 args (the x86_64 register limit); the rest are
/// ignored.
pub fn syscall(n: i64, args: &[u64]) -> i64 {
    let a = |i: usize| args.get(i).copied().unwrap_or(0);
    unsafe {
        match args.len() {
            0 => libc::syscall(n),
            1 => libc::syscall(n, a(0)),
            2 => libc::syscall(n, a(0), a(1)),
            3 => libc::syscall(n, a(0), a(1), a(2)),
            4 => libc::syscall(n, a(0), a(1), a(2), a(3)),
            5 => libc::syscall(n, a(0), a(1), a(2), a(3), a(4)),
            _ => libc::syscall(n, a(0), a(1), a(2), a(3), a(4), a(5)),
        }
    }
}
