//! T5 asm-stub syscall 拦截探针（decision-history §7.18，E19③④）：guest
//! inline-asm 裸 `syscall` 指令——asm-stub 文本生成点被改写为经间接槽调
//! mirvm_syscall_trampoline（tramp 保全整数/flags/xmm/mxcsr）→ dispatch 直通。
//! v1 行为契约 = 与 native 逐字节一致（直通即零行为变化）；拦截实证 =
//! `MIRVM_SYSCALL_TRACE=1` 时 mirvm stderr 有 mirvm-syscall 行。
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
            out("rcx") _,   // syscall 契约的可坏寄存器（trampoline 不保全，同真 syscall）
            out("r11") _,
        );
    }
    ret
}

fn main() {
    let msg = b"raw-syscall write ok\n";
    let r = raw_write(1, msg);
    println!("write ret={r}");
    // 第二个裸 syscall（getpid 族形态，非确定值只判符号）
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
