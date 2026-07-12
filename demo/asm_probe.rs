// M5.0 asm-stub 工厂的永久差分探针：三面孔（div/syscall/cpuid，corpus §2.2）
// + 类分配 + 显式寄存器 + inout/lateout clobber —— 与 native 同机对拍。
// cpuid 差分有效性来自"虚拟 CPU = 真宿主 CPU"（同机 native 看到相同特性）。
use std::arch::asm;

/// 面孔 3（算术原语）：128/64 宽除法（numbigint div_wide 同形）。
/// 显式 inout("dx")/("ax") + in(reg) 类操作数。
fn div_wide(hi: u64, lo: u64, d: u64) -> (u64, u64) {
    let (q, r);
    unsafe {
        asm!("div {0:r}", in(reg) d, inout("dx") hi => r, inout("ax") lo => q,
             options(pure, nomem, nostack));
    }
    (q, r)
}

/// 面孔 2（裸 syscall）：write(2) 直发内核（rustix syscall3 同形）。
/// 输出本身就是差分证据；绕过 std 缓冲，故在任何 println! 之前调用。
fn raw_write(buf: &[u8]) -> u64 {
    let mut nr: u64 = 1; // SYS_write
    unsafe {
        asm!("syscall", inlateout("ax") nr, in("di") 1usize, in("si") buf.as_ptr(),
             in("dx") buf.len(), lateout("cx") _, lateout("r11") _,
             options(nostack, preserves_flags));
    }
    nr
}

/// 面孔 1（cpuid 特性检测）：leaf 0 厂商串（std_detect __cpuid 的 rbx 保存惯用法）。
fn cpuid_vendor() -> String {
    let mut leaf: u32 = 0;
    let (b, c, d): (u32, u32, u32);
    unsafe {
        asm!("mov {0:r}, rbx", "cpuid", "xchg {0:r}, rbx",
             out(reg) b, inout("eax") leaf, out("ecx") c, out("edx") d,
             options(nostack, preserves_flags));
    }
    let _ = leaf;
    let mut s = Vec::new();
    for w in [b, d, c] {
        s.extend_from_slice(&w.to_le_bytes());
    }
    String::from_utf8(s).unwrap()
}

/// 类分配 + 立即数 + 默认 flags clobber（corpus §2.2 文档例句同形）。
fn add5(x: u64) -> u64 {
    let mut v = x;
    unsafe {
        asm!("add {0}, 5", inout(reg) v, options(nostack));
    }
    v
}

fn main() {
    let n = raw_write(b"syscall-write: ok\n");
    // (2^64 + 2^63) / 3 = 2^63 整除
    let (q, r) = div_wide(1, 0x8000_0000_0000_0000, 3);
    println!("div: q={q:#x} r={r} (wrote {n} bytes)");
    println!("cpuid vendor: {}", cpuid_vendor());
    println!("add5(37) = {}", add5(37));
}
