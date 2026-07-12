use std::arch::x86_64::{__cpuid, _xgetbv};

fn main() {
    let leaf1 = __cpuid(1);
    let has_xsave = leaf1.ecx & (1 << 26) != 0;
    let has_osxsave = leaf1.ecx & (1 << 27) != 0;
    if !has_xsave || !has_osxsave {
        println!("xgetbv unavailable");
        return;
    }

    let xcr0 = unsafe { _xgetbv(0) };
    println!("xcr0={xcr0:#018x}");
    println!(
        "x87={} sse={} avx={}",
        xcr0 & 1 != 0,
        xcr0 & 2 != 0,
        xcr0 & 4 != 0,
    );
}
