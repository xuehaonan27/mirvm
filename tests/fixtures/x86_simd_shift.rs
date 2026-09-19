use std::arch::x86_64::{__m256i, _mm256_slli_epi32, _mm256_srai_epi32, _mm256_srli_epi32};

fn print_lanes(label: &str, lanes: [u32; 8]) {
    println!(
        "{label}={:08x},{:08x},{:08x},{:08x},{:08x},{:08x},{:08x},{:08x}",
        lanes[0], lanes[1], lanes[2], lanes[3], lanes[4], lanes[5], lanes[6], lanes[7],
    );
}

#[target_feature(enable = "avx2")]
unsafe fn run_cases() {
    let input: __m256i = unsafe {
        std::mem::transmute([
            0x8000_0001_u32,
            0xffff_ffff,
            0x7fff_ffff,
            0x0123_4567,
            0xf000_000f,
            0x4000_0000,
            0x0000_0001,
            0xdead_beef,
        ])
    };

    let logical: [u32; 8] = unsafe { std::mem::transmute(_mm256_srli_epi32::<14>(input)) };
    let arithmetic: [u32; 8] = unsafe { std::mem::transmute(_mm256_srai_epi32::<7>(input)) };
    let logical_limit: [u32; 8] =
        unsafe { std::mem::transmute(_mm256_srli_epi32::<31>(input)) };
    let logical_oob: [u32; 8] =
        unsafe { std::mem::transmute(_mm256_srli_epi32::<32>(input)) };
    let arithmetic_oob: [u32; 8] =
        unsafe { std::mem::transmute(_mm256_srai_epi32::<255>(input)) };
    let left: [u32; 8] = unsafe { std::mem::transmute(_mm256_slli_epi32::<5>(input)) };
    let left_limit: [u32; 8] = unsafe { std::mem::transmute(_mm256_slli_epi32::<31>(input)) };
    let left_oob: [u32; 8] = unsafe { std::mem::transmute(_mm256_slli_epi32::<32>(input)) };

    print_lanes("logical14", logical);
    print_lanes("arithmetic7", arithmetic);
    print_lanes("logical31", logical_limit);
    print_lanes("logical32", logical_oob);
    print_lanes("arithmetic255", arithmetic_oob);
    print_lanes("left5", left);
    print_lanes("left31", left_limit);
    print_lanes("left32", left_oob);
}

fn main() {
    if !std::arch::is_x86_feature_detected!("avx2") {
        println!("avx2 unavailable");
        return;
    }
    unsafe { run_cases() };
}
