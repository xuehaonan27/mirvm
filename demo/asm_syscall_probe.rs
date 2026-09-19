//! Probe for asm-stub `syscall` interception: a guest inline-asm raw `syscall` is
//! rewritten at the asm-stub text generation point to call `mirvm_syscall_trampoline`
//! through an indirect slot (the trampoline preserves integer/flags/xmm/mxcsr state).
//! Behavior must stay byte-for-byte identical to native; interception is evidenced by
//! `mirvm-syscall` lines on mirvm stderr when `MIRVM_SYSCALL_TRACE=1`.
use std::arch::asm;

fn raw_write(fd: i64, buf: &[u8]) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") 1i64, // SYS_write
            in("rdi") fd,
            in("rsi") buf.as_ptr(),
            in("rdx") buf.len(),
            lateout("rax") ret,
            out("rcx") _,   // clobbered by the syscall contract (the trampoline does not preserve it, as with a real syscall)
            out("r11") _,
        );
    }
    ret
}

fn main() {
    let msg = b"raw-syscall write ok\n";
    let r = raw_write(1, msg);
    println!("write ret={r}");
    // Second raw syscall (getpid shape): the value is nondeterministic, so only its sign is checked
    let pid: i64;
    unsafe {
        asm!(
            "syscall",
            in("rax") 39i64, // SYS_getpid
            lateout("rax") pid,
            out("rcx") _,
            out("r11") _,
        );
    }
    println!("pid nonzero={}", pid > 0);
}
