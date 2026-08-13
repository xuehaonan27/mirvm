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

use crate::mirvm_log;

/// Safe wrapper of `getenv(3)`.
/// `name_addr`: guest side NUL-terminated string's  true address.
/// Returns true address, or 0 on failure.
pub fn getenv(name_addr: u64) -> u64 {
    unsafe { libc::getenv(name_addr as *const libc::c_char) as u64 }
}

/// write(2)：返回已写字节数或 -1（errno 语义留给调用方）。
pub fn write_fd(fd: i32, buf_addr: u64, len: usize) -> i64 {
    unsafe { libc::write(fd, buf_addr as *const libc::c_void, len) as i64 }
}

/// strlen(3)（guest 侧 NUL 结尾字符串真地址）。
pub fn c_strlen(s_addr: u64) -> u64 {
    unsafe { libc::strlen(s_addr as *const libc::c_char) as u64 }
}

/// fork(2)：父进程返回子 pid，子进程返回 0，失败 -1。
/// 守卫（单线程放行）在引擎侧；exec 族走 foreign 直通，不经此。
pub fn fork() -> i64 {
    unsafe { libc::fork() as i64 }
}

/// raise(3)：向当前宿主线程同步投递信号。Engine 对 guest handler 的同步
/// 执行顺序在上层裁决；本层只保留 libc 返回值/errno 语义。
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

/// atexit(3)：挂 native trampoline（引擎链接的 libc atexit，非 guest dlsym）。
pub fn atexit_native(cb: extern "C" fn()) -> i32 {
    unsafe { libc::atexit(cb) }
}

/// JIT 编译码 libcall 符号地址（cranelift `jb.symbol` 注册用——编译码直接
/// call 真 libc 函数，地址解析属 OS 符号面）。
pub fn memmove_addr() -> *const u8 {
    libc::memmove as *const u8
}
/// 同上（memset）。
pub fn memset_addr() -> *const u8 {
    libc::memset as *const u8
}
/// 同上（memcmp）。
pub fn memcmp_addr() -> *const u8 {
    libc::memcmp as *const u8
}

/// T5 asm-stub syscall 拦截 dispatch（decision-history §7.18）：asm-stub 内
/// `syscall` 指令被改写为经间接槽调 `mirvm_syscall_trampoline`（arch::
/// x86_64::asmstub，整数/flags/xmm/mxcsr 已按真 syscall 纪律保全），落此。
/// v1 = 直通 + `MIRVM_SYSCALL_TRACE` 旋钮；**D10 虚拟化语义的钩子挂载点**——
/// 统一 fd 空间/假 FS/计费进来时在本函数分诊，调用方零改动。
///
/// # Safety
/// 仅由 trampoline 以 syscall 契约调起：args 指向 6 个 u64（a1..a6）。
#[unsafe(no_mangle)]
pub extern "C" fn mirvm_syscall_dispatch(nr: i64, args: *const u64) -> i64 {
    let args: &[u64] = unsafe { std::slice::from_raw_parts(args, 6) };
    if std::env::var_os("MIRVM_SYSCALL_TRACE").is_some() {
        mirvm_log!(
            stderr,
            "mirvm-syscall: nr={nr} a1={:#x} a2={:#x} a3={:#x} a4={:#x} a5={:#x} a6={:#x}",
            args[0],
            args[1],
            args[2],
            args[3],
            args[4],
            args[5]
        );
    }
    syscall(nr, args)
}

/// syscall(2) 变参直通：全未列举 syscall 族的唯一通道。args 取前 6 参
/// （x86_64 寄存器上限），超出忽略——与归并前 HostSyscall 臂的界一致。
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
