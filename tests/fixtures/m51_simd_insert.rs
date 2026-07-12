use std::arch::x86_64::{__m128i, _mm_extract_epi64, _mm_insert_epi64};

#[target_feature(enable = "sse4.1")]
unsafe fn run_cases() {
    let input: __m128i =
        unsafe { std::mem::transmute([0x0123_4567_89ab_cdef_u64, 0xfedc_ba98_7654_3210_u64]) };
    let lane0 = _mm_insert_epi64::<0>(input, 0x1111_2222_3333_4444_i64);
    let lane1 = _mm_insert_epi64::<1>(input, 0x5555_6666_7777_8888_u64 as i64);
    let lane0: [u64; 2] = unsafe { std::mem::transmute(lane0) };
    let lane1: [u64; 2] = unsafe { std::mem::transmute(lane1) };
    let extract0 = _mm_extract_epi64::<0>(input) as u64;
    let extract1 = _mm_extract_epi64::<1>(input) as u64;

    println!("insert0={:016x},{:016x}", lane0[0], lane0[1]);
    println!("insert1={:016x},{:016x}", lane1[0], lane1[1]);
    println!("extract0={extract0:016x}");
    println!("extract1={extract1:016x}");
}

fn main() {
    if !std::arch::is_x86_feature_detected!("sse4.1") {
        println!("sse4.1 unavailable");
        return;
    }
    unsafe { run_cases() };
}
