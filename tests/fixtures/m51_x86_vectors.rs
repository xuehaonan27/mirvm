use std::arch::x86_64::{
    __m128i, _mm_loadu_si128, _mm_sha256msg1_epu32, _mm_sha256msg2_epu32,
    _mm_sha256rnds2_epu32, _mm_shuffle_epi8, _mm_storeu_si128,
};

fn hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[target_feature(enable = "ssse3")]
unsafe fn pshuf_checksum() -> u64 {
    let input: [u8; 16] = std::array::from_fn(|i| i as u8 + 1);
    let control = [4, 128, 4, 3, 24, 12, 6, 19, 12, 5, 5, 10, 4, 1, 8, 0];
    let a = unsafe { _mm_loadu_si128(input.as_ptr().cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(control.as_ptr().cast::<__m128i>()) };
    let result = _mm_shuffle_epi8(a, b);
    let mut bytes = [0u8; 16];
    unsafe { _mm_storeu_si128(bytes.as_mut_ptr().cast::<__m128i>(), result) };
    hash_bytes(0xcbf2_9ce4_8422_2325, &bytes)
}

#[target_feature(enable = "sha")]
unsafe fn sha_checksum() -> u64 {
    let a = 0xe9b5_dba5_b5c0_fbcf_7137_4491_428a_2f98u128.to_le_bytes();
    let b = 0xab1c_5ed5_923f_82a4_59f1_11f1_3956_c25bu128.to_le_bytes();
    let keys = 0x0000_0000_0000_0000_1283_5b01_d807_aa98u128.to_le_bytes();
    let a = unsafe { _mm_loadu_si128(a.as_ptr().cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.as_ptr().cast::<__m128i>()) };
    let keys = unsafe { _mm_loadu_si128(keys.as_ptr().cast::<__m128i>()) };
    let mut bytes = [0u8; 16];
    let mut hash = 0xcbf2_9ce4_8422_2325;

    for result in [
        _mm_sha256msg1_epu32(a, b),
        _mm_sha256msg2_epu32(a, b),
        _mm_sha256rnds2_epu32(a, b, keys),
    ] {
        unsafe { _mm_storeu_si128(bytes.as_mut_ptr().cast::<__m128i>(), result) };
        hash = hash_bytes(hash, &bytes);
    }
    hash
}

fn main() {
    if std::is_x86_feature_detected!("ssse3") {
        println!("pshuf={:016x}", unsafe { pshuf_checksum() });
    } else {
        println!("pshuf=unavailable");
    }
    if std::is_x86_feature_detected!("sha") {
        println!("sha={:016x}", unsafe { sha_checksum() });
    } else {
        println!("sha=unavailable");
    }
}
