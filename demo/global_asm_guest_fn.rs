use std::arch::global_asm;

// C7：global_asm 的 `sym` 指向解释态 guest fn（open-issues C7 转正验收）。
// 机器码（.so 内的 probe_native_chain）经 `call {guest}` 调 guest fn——该符号在
// mirvm 侧经 P1 条目 stub 预算成 ABS 定义，call 直接落到条目 stub、蹦床回解释器；
// native 侧则是普通直接调用。两侧可观察行为（返回值 + 指针回写）须逐位一致。
#[unsafe(no_mangle)]
pub extern "C" fn probe_guest(x: u64, slot: *mut u64) -> u64 {
    unsafe {
        *slot = *slot + 1;
    }
    x * 3 + 1
}

global_asm!(
    r#"
.globl probe_native_chain
.type probe_native_chain,@function
probe_native_chain:
    push rbx
    mov rbx, rdi
    mov rdi, rbx
    call {guest}
    add rbx, rax
    mov rax, rbx
    pop rbx
    ret
"#,
    guest = sym probe_guest
);

unsafe extern "C" {
    fn probe_native_chain(x: u64, slot: *mut u64) -> u64;
}

fn main() {
    let mut slot: u64 = 40;
    let y = unsafe { probe_native_chain(7, &mut slot) };
    // 预期：guest(7, &slot) = slot 41、ret 22；chain = 7 + 22 = 29
    println!("chain={y} slot={slot}");
}
