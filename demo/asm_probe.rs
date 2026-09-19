// Permanent differential probe for the asm-stub factory: three faces (div/syscall/cpuid)
// + class allocation + explicit registers + inout/lateout clobber -- compared bit-for-bit with native on the same machine.
// cpuid differential validity comes from "virtual CPU = real host CPU" (native on the same machine sees the same features).
use std::arch::asm;

/// Face 3 (arithmetic primitive): 128/64 wide division (same shape as numbigint div_wide).
/// Explicit inout("dx")/("ax") + in(reg) class operands.
fn div_wide(hi: u64, lo: u64, d: u64) -> (u64, u64) {
    let (q, r);
    unsafe {
        asm!("div {0:r}", in(reg) d, inout("dx") hi => r, inout("ax") lo => q,
             options(pure, nomem, nostack));
    }
    (q, r)
}

/// Face 2 (raw syscall): write(2) goes straight to the kernel (same shape as rustix syscall3).
/// The output is itself the differential evidence; it bypasses std buffering, so it must run before any println!.
fn raw_write(buf: &[u8]) -> u64 {
    let mut nr: u64 = 1; // SYS_write
    unsafe {
        asm!("syscall", inlateout("ax") nr, in("di") 1usize, in("si") buf.as_ptr(),
             in("dx") buf.len(), lateout("cx") _, lateout("r11") _,
             options(nostack, preserves_flags));
    }
    nr
}

/// Face 1 (cpuid feature detection): leaf 0 vendor string (the rbx-preserving idiom of std_detect __cpuid).
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

/// Register-class operand + immediate + default flags clobber.
fn add5(x: u64) -> u64 {
    let mut v = x;
    unsafe {
        asm!("add {0}, 5", inout(reg) v, options(nostack));
    }
    v
}

fn main() {
    let n = raw_write(b"syscall-write: ok\n");
    // (2^64 + 2^63) / 3 = 2^63 exactly
    let (q, r) = div_wide(1, 0x8000_0000_0000_0000, 3);
    println!("div: q={q:#x} r={r} (wrote {n} bytes)");
    println!("cpuid vendor: {}", cpuid_vendor());
    println!("add5(37) = {}", add5(37));
}
