//! arch::x86_64::asmstub — x86_64 机器码字节发射与单发指令原语。
//!
//! 归并：codearena 的条目 stub 字节工厂（`movabs rax, target; jmp rax`）
//! 与 interp 的两条 asm!（int3 断点、xgetbv）。纯发射/执行——stub 的
//! 地址域语义（addrlayout）、断点的终止语义（native 同）都留调用方。

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
