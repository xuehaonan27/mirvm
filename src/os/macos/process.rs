//! macOS process primitives.
//!
//! Primitives are not adjudicated. Detailed behaviours like single-threaded fork guards, abort
//! documentation, atexit registry, and LIFO callback execution, are all left to the engine. This
//! module only performs honest libc calls.
//! System calls that's unlisted here, should only pass through [`syscall`].
//! Address values are u64 encoded (leaf type discipline).
//!
//! Two things differ from Linux and are visible in the signatures: this platform has no `gettid`,
//! so a thread is numbered by the Mach thread the C library maps a pthread to, and there is no
//! separate `exit_group`, so `exit` ends the process for every thread.

/// This process's id.
pub fn getpid() -> i32 {
    unsafe { libc::getpid() }
}

/// The calling thread's id as the kernel numbers it, which is not the C library's `pthread_t`.
///
/// Zero when the kernel refuses the query, which is a value no live thread has.
pub fn gettid() -> i32 {
    let mut tid: u64 = 0;
    if unsafe { libc::pthread_threadid_np(0, &mut tid) } == 0 {
        tid as i32
    } else {
        0
    }
}

/// The syscall numbers the capture path recognises by number rather than by effect.
///
/// A number is the kernel's, so this is where they are named; a caller that must branch before the
/// syscall can look at the effect instead. These are the BSD numbers of this kernel's
/// `sys/syscall.h`, spelled here because the C library exposes no `SYS_*` names for this platform.
pub const SYS_FORK: i64 = 2;
pub const SYS_EXIT: i64 = 1;
/// This platform ends the process with `exit(2)`; there is no separate group call.
pub const SYS_EXIT_GROUP: i64 = SYS_EXIT;
pub const SYS_RT_SIGRETURN: i64 = 184;

/// The query numbers [`syscall`] answers, from this kernel's `sys/syscall.h`; the C library
/// exposes no `SYS_*` names for this platform.
pub const SYS_GETPID: i64 = 20;
pub const SYS_GETPPID: i64 = 39;

/// The C library's error numbers this crate has to compare against.
///
/// The rest reach a caller as the raw value [`errno`] returns; a number is named here only when a
/// caller branches on it, and naming it here is what keeps `libc` out of the layers above.
pub const ESRCH: i32 = libc::ESRCH;

// The same argument, for the numbers only a test compares against.
#[cfg(test)]
pub const EDOM: i32 = libc::EDOM;
#[cfg(test)]
pub const EINVAL: i32 = libc::EINVAL;
#[cfg(test)]
pub const ENOSYS: i32 = libc::ENOSYS;

/// Safe wrapper of `getenv(3)`.
/// `name_addr`: guest side NUL-terminated string's  true address.
/// Returns true address, or 0 on failure.
pub fn getenv(name_addr: u64) -> u64 {
    unsafe { libc::getenv(name_addr as *const libc::c_char) as u64 }
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

/// The calling pthread's `errno` slot.
///
/// The pointer is the primitive, not only the value: a caller that must run syscalls of its own
/// without the guest observing a changed `errno` hands the slot to the code that saves and
/// restores around them, and such a caller cannot re-fetch it later from another thread.
pub fn errno_location() -> *mut i32 {
    unsafe { libc::__error() }
}

/// Read the calling pthread's libc `errno`. Callers must read it immediately after the failing
/// libc operation, before formatting or any other library call can overwrite it.
pub fn errno() -> i32 {
    unsafe { *errno_location() }
}

pub fn set_errno(value: i32) {
    unsafe { *errno_location() = value };
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

/// A child's termination, decoded from the kernel's wait status.
///
/// The status word is the kernel's encoding, so this is where it is decoded; a caller that reaped
/// a child asks a question instead of masking bits. The C library names this kernel's encoding in
/// macros rather than functions, so the two shifts are written out here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
pub struct ExitStatus(i32);

#[cfg(test)]
impl ExitStatus {
    /// Whether the child terminated normally, which is what makes [`ExitStatus::code`] meaningful.
    pub fn exited(self) -> bool {
        self.0 & 0x7f == 0
    }

    /// The child's exit code, meaningful only when [`ExitStatus::exited`].
    pub fn code(self) -> i32 {
        (self.0 >> 8) & 0xff
    }
}

/// `waitpid(2)` for one child: block until `pid` terminates.
///
/// `Err` is the library's `errno`, because the caller that asked for this child is the only one
/// that can say what a missing one means.
#[cfg(test)]
pub fn wait(pid: i32) -> Result<ExitStatus, i32> {
    let mut status: i32 = 0;
    let result = unsafe { libc::waitpid(pid, &mut status, 0) };
    if result < 0 {
        Err(errno())
    } else {
        Ok(ExitStatus(status))
    }
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
/// `mirvm_syscall_trampoline` through an indirect slot (arch::asmstub;
/// integer/flags/vector state is already preserved under the real syscall
/// discipline).
///
/// Currently the `MIRVM_SYSCALL_TRACE` knob plus whatever `syscall` below does — a passthrough on
/// Linux, a refusal on macOS. This is also the hook point for virtualization semantics: a unified fd space, a fake FS, or
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

/// The syscall numbers this platform answers from the C library, and why the rest are refused.
///
/// This kernel has no generic entry point, so a number is either answered by name or not at all:
/// `syscall(2)` is documented as deprecated here, and on arm64 it is not merely deprecated — the
/// process is terminated with SIGSYS. Measured: `syscall(SYS_getpid)` dies with signal 12 rather
/// than returning. Forwarding therefore costs the whole process, where refusing costs the guest one
/// failed call, which is the errno a guest already handles for a kernel that lacks a call.
///
/// What is answered is what the C library has an exact counterpart for, called on the guest's
/// behalf. A number this table does not name is refused rather than approximated, and the guest
/// sees the refusal as a call this kernel does not have — the same thing it sees on the platform
/// whose kernel is asked directly.
///
/// The engine's own accounting does not depend on which spelling arrived: the fork hook runs on
/// this path for whatever number it recognises, so a guest that reached a fork through its own
/// instruction and one that reached the builtin are treated the same.
///
/// The parameter is the syscall number and the six argument slots, kept so callers and the
/// trampoline above remain architecture-independent.
pub fn syscall(n: i64, args: &[u64]) -> i64 {
    match n {
        // SAFETY: each of these is the C library's own call for the number, and it is what the
        // guest asked this kernel for: two identity queries, a fork, and the exit that does not
        // return.
        SYS_GETPID => i64::from(unsafe { libc::getpid() }),
        SYS_GETPPID => i64::from(unsafe { libc::getppid() }),
        SYS_FORK => i64::from(unsafe { libc::fork() }),
        // [`SYS_EXIT_GROUP`] is the same number, so this arm is the one both names take.
        SYS_EXIT => unsafe { libc::_exit(args[0] as libc::c_int) },
        _ => {
            crate::diag_direct!(
                Syscall,
                "nr={n} refused: this platform cannot forward a raw syscall"
            );
            // The C library's own convention for a call the kernel does not implement, which is
            // what a guest reading the result expects: -1, with the reason in `errno`.
            crate::os::process::set_errno(libc::ENOSYS);
            -1
        }
    }
}
