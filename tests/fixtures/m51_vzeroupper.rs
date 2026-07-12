use std::arch::x86_64::_mm256_zeroupper;

#[target_feature(enable = "avx")]
unsafe fn issue_vzeroupper() {
    _mm256_zeroupper();
}

fn main() {
    if !std::arch::is_x86_feature_detected!("avx") {
        println!("avx unavailable");
        return;
    }

    println!("before cpu hints");
    std::hint::spin_loop();
    unsafe { issue_vzeroupper() };
    println!("after cpu hints");
}
