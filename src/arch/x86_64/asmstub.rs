//! arch::x86_64::asmstub — x86_64 机器码字节发射与单发指令原语。
//!
//! 归并：codearena 的条目 stub 字节工厂（`movabs rax, target; jmp rax`）
//! 与 interp 的两条 asm!（int3 断点、xgetbv）。纯发射/执行——stub 的
//! 地址域语义（addrlayout）、断点的终止语义（native 同）都留调用方。
//!
//! T5（decision-history §7.18）：`mirvm_syscall_trampoline`——asm-stub 内
//! `syscall` 指令改写落点。保 syscall 全契约（真 syscall 只破坏 rcx/r11、
//! 不动 flags 与向量态，wrapper 必须同纪律）后调 `mirvm_syscall_dispatch`。

/// stub 字节：`movabs rax, target; jmp rax`（48 B8 <imm64> FF E0），12B 实长。
/// （STUB_STRIDE=16 的由 12B 上对齐得来，常量在 vm/engine/codearena。）
pub fn emit_stub_bytes(target: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0] = 0x48;
    b[1] = 0xb8;
    b[2..10].copy_from_slice(&target.to_le_bytes());
    b[10] = 0xff;
    b[11] = 0xe0;
    b
}

/// 真 int3：未被跟踪时 = SIGTRAP 终止（native 同语义）。
pub fn int3() {
    unsafe { std::arch::asm!("int3", options(nomem, nostack, preserves_flags)) };
}

/// xgetbv：XCR(xcr) → (edx:eax) 拼 u64。
pub fn xgetbv(xcr: u32) -> u64 {
    let (eax, edx): (u32, u32);
    unsafe {
        std::arch::asm!(
            "xgetbv",
            in("ecx") xcr,
            out("eax") eax,
            out("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(edx) << 32) | u64::from(eax)
}

// ===== T5：syscall 拦截 trampoline（decision-history §7.18）=====
//
// 契约（与真 syscall 逐位一致）：
// - 入：rax=nr、rdi/rsi/rdx/r10/r8/r9=a1..a6；出：rax=返回值。
// - 真 syscall 只破坏 rcx/r11，不动 flags、不动 xmm/mxcsr——
//   `call mirvm_syscall_dispatch`（普通 SysV fn）的破坏面大得多，
//   故 trampoline 全量保全（dispatch 本体不碰向量态，TRACE 打印会碰，
//   无条件保全是诚实纪律）。
// - 栈：6 个参数槽兼作 dispatch 的 args 数组（*(rsp)=a1..*(rsp+40)=a6）。
//
// 栈对齐核算：入口 rsp≡8（call 压过返回地址）；pushfq→0；sub 272→0；
// 6 push→0（48≡0 mod 16）；`call` 时 rsp≡0 ✓。

unsafe extern "C" {
    fn mirvm_syscall_trampoline();
}

/// T5 trampoline 真地址（asm-stub/global-asm 间接槽重填用）。
pub fn syscall_trampoline_addr() -> u64 {
    mirvm_syscall_trampoline as *const () as u64
}

std::arch::global_asm!(
    ".globl mirvm_syscall_trampoline",
    ".p2align 4",
    "mirvm_syscall_trampoline:",
    "pushfq",
    "sub rsp, 272",
    "movdqu [rsp], xmm0",
    "movdqu [rsp+16], xmm1",
    "movdqu [rsp+32], xmm2",
    "movdqu [rsp+48], xmm3",
    "movdqu [rsp+64], xmm4",
    "movdqu [rsp+80], xmm5",
    "movdqu [rsp+96], xmm6",
    "movdqu [rsp+112], xmm7",
    "movdqu [rsp+128], xmm8",
    "movdqu [rsp+144], xmm9",
    "movdqu [rsp+160], xmm10",
    "movdqu [rsp+176], xmm11",
    "movdqu [rsp+192], xmm12",
    "movdqu [rsp+208], xmm13",
    "movdqu [rsp+224], xmm14",
    "movdqu [rsp+240], xmm15",
    "stmxcsr [rsp+256]",
    "push r9",
    "push r8",
    "push r10",
    "push rdx",
    "push rsi",
    "push rdi",
    "mov rdi, rax",
    "mov rsi, rsp",
    "call mirvm_syscall_dispatch",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop r10",
    "pop r8",
    "pop r9",
    "ldmxcsr [rsp+256]",
    "movdqu xmm0, [rsp+0]",
    "movdqu xmm1, [rsp+16]",
    "movdqu xmm2, [rsp+32]",
    "movdqu xmm3, [rsp+48]",
    "movdqu xmm4, [rsp+64]",
    "movdqu xmm5, [rsp+80]",
    "movdqu xmm6, [rsp+96]",
    "movdqu xmm7, [rsp+112]",
    "movdqu xmm8, [rsp+128]",
    "movdqu xmm9, [rsp+144]",
    "movdqu xmm10, [rsp+160]",
    "movdqu xmm11, [rsp+176]",
    "movdqu xmm12, [rsp+192]",
    "movdqu xmm13, [rsp+208]",
    "movdqu xmm14, [rsp+224]",
    "movdqu xmm15, [rsp+240]",
    "add rsp, 272",
    "popfq",
    "ret",
);
