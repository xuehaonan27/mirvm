//! x86_64 hardware intrinsic helpers used by the tcx-free engine.
//!
//! These functions execute the real host instruction behind an `llvm.x86.*` boundary. The guest
//! and host CPU are deliberately the same virtual CPU; callers only reach feature-specific code
//! after the guest's normal CPUID dispatch selected it.

use std::arch::x86_64::{
    __m128i, __m256i, _mm_loadu_si128, _mm_sha256msg1_epu32, _mm_sha256msg2_epu32,
    _mm_sha256rnds2_epu32, _mm_shuffle_epi8, _mm_storeu_si128, _mm256_loadu_si256,
    _mm256_shuffle_epi8, _mm256_storeu_si256,
};

/// SSSE3 `pshufb`, 128-bit form.
#[target_feature(enable = "ssse3")]
pub(super) unsafe fn pshufb128(dst: *mut u8, a: *const u8, control: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let control = unsafe { _mm_loadu_si128(control.cast::<__m128i>()) };
    let result = _mm_shuffle_epi8(a, control);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// AVX2 `vpshufb`, with independent 128-bit lanes.
#[target_feature(enable = "avx2")]
pub(super) unsafe fn pshufb256(dst: *mut u8, a: *const u8, control: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let control = unsafe { _mm256_loadu_si256(control.cast::<__m256i>()) };
    let result = _mm256_shuffle_epi8(a, control);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

#[target_feature(enable = "sha")]
pub(super) unsafe fn sha256msg1(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_sha256msg1_epu32(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "sha")]
pub(super) unsafe fn sha256msg2(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_sha256msg2_epu32(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "sha")]
pub(super) unsafe fn sha256rnds2(dst: *mut u8, a: *const u8, b: *const u8, round_keys: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let round_keys = unsafe { _mm_loadu_si128(round_keys.cast::<__m128i>()) };
    let result = _mm_sha256rnds2_epu32(a, b, round_keys);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[cfg(test)]
mod tests {
    use super::{pshufb128, pshufb256, sha256msg1, sha256msg2, sha256rnds2};

    #[test]
    fn pshufb_matches_its_portable_lane_definition() {
        if !std::is_x86_feature_detected!("ssse3") {
            return;
        }
        let mut a = [0u8; 32];
        for (i, byte) in a.iter_mut().enumerate() {
            *byte = i as u8 + 1;
        }
        let control: [u8; 32] = [
            4, 128, 4, 3, 24, 12, 6, 19, 12, 5, 5, 10, 4, 1, 8, 0, 4, 128, 4, 3, 24, 12, 6, 19, 12,
            5, 5, 10, 4, 1, 8, 0,
        ];
        let reference = |lane_start: usize, i: usize| {
            let c = control[lane_start + i];
            if c & 0x80 != 0 {
                0
            } else {
                a[lane_start + usize::from(c & 0x0f)]
            }
        };

        let mut got128 = [0u8; 16];
        unsafe { pshufb128(got128.as_mut_ptr(), a.as_ptr(), control.as_ptr()) };
        assert_eq!(got128, std::array::from_fn(|i| reference(0, i)));

        if std::is_x86_feature_detected!("avx2") {
            let mut got256 = [0u8; 32];
            unsafe { pshufb256(got256.as_mut_ptr(), a.as_ptr(), control.as_ptr()) };
            let expected: [u8; 32] = std::array::from_fn(|i| reference((i / 16) * 16, i % 16));
            assert_eq!(got256, expected);
        }
    }

    #[test]
    fn sha256_helpers_match_stdarch_known_vectors() {
        if !std::is_x86_feature_detected!("sha") {
            return;
        }
        let a = 0xe9b5_dba5_b5c0_fbcf_7137_4491_428a_2f98u128.to_le_bytes();
        let b = 0xab1c_5ed5_923f_82a4_59f1_11f1_3956_c25bu128.to_le_bytes();
        let keys = 0x0000_0000_0000_0000_1283_5b01_d807_aa98u128.to_le_bytes();
        let mut got = [0u8; 16];

        unsafe { sha256msg1(got.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
        assert_eq!(
            u128::from_le_bytes(got),
            0xeb84_973f_d5cd_a67d_2857_b88f_406b_09ee
        );
        unsafe { sha256msg2(got.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
        assert_eq!(
            u128::from_le_bytes(got),
            0xb587_77ce_887f_d851_15d1_ec8b_73ac_8450
        );
        unsafe { sha256rnds2(got.as_mut_ptr(), a.as_ptr(), b.as_ptr(), keys.as_ptr()) };
        assert_eq!(
            u128::from_le_bytes(got),
            0xd306_3037_effb_15ea_187e_e3db_0d6d_1d19
        );
    }
}
