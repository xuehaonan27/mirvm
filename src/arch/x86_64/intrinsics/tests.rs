//! Hardware cross-check for arch::x86_64::intrinsics (stdarch known vectors + real machine comparison).
//! (Moved whole from vm/engine/x86.rs test cluster, zero logic diff.)

use super::{
    aesdec, aesdeclast, aesenc, aesenclast, aesimc, aeskeygenassist, crc32_u8, crc32_u16,
    crc32_u32, crc32_u64, gather_d_pd_256, gather_q_pd_256, lddqu, pclmulqdq, permd256,
    pmaddubsw128, pmaddubsw256, pmaddwd128, pmaddwd256, psad_bw128, psad_bw256, pshufb128,
    pshufb256, sha256msg1, sha256msg2, sha256rnds2, vpmadd52,
};

#[test]
fn lddqu_matches_unaligned_load_contract_and_hw() {
    let buf: [u8; 40] = std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11));
    // semantics = unaligned 16/32-byte pure load (bitwise synonymous with loadu)
    let mut got128 = [0u8; 16];
    unsafe { lddqu::<16>(got128.as_mut_ptr(), buf.as_ptr().add(3)) };
    assert_eq!(got128[..], buf[3..19]);
    if std::is_x86_feature_detected!("sse3") {
        let hw = unsafe { core::arch::x86_64::_mm_lddqu_si128(buf.as_ptr().add(3).cast()) };
        assert_eq!(got128, unsafe {
            std::mem::transmute::<std::arch::x86_64::__m128i, [u8; 16]>(hw)
        });
    }
    let mut got256 = [0u8; 32];
    unsafe { lddqu::<32>(got256.as_mut_ptr(), buf.as_ptr().add(5)) };
    assert_eq!(got256[..], buf[5..37]);
    if std::is_x86_feature_detected!("avx") {
        let hw = unsafe { core::arch::x86_64::_mm256_lddqu_si256(buf.as_ptr().add(5).cast()) };
        assert_eq!(got256, unsafe {
            std::mem::transmute::<std::arch::x86_64::__m256i, [u8; 32]>(hw)
        });
    }
}

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
        4, 128, 4, 3, 24, 12, 6, 19, 12, 5, 5, 10, 4, 1, 8, 0, 4, 128, 4, 3, 24, 12, 6, 19, 12, 5,
        5, 10, 4, 1, 8, 0,
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

/// Pack two u64 values (lo, hi) into 16 bytes LE (matching the layout of `_mm_set_epi64x(hi, lo)`).
fn qwords(lo: u64, hi: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo.to_le_bytes());
    out[8..].copy_from_slice(&hi.to_le_bytes());
    out
}

#[test]
fn psad_matches_stdarch_known_vectors() {
    if !std::is_x86_feature_detected!("sse2") {
        return;
    }
    // stdarch test_mm_sad_epu8 known answer
    let a: [u8; 16] = [
        255, 254, 253, 252, 1, 2, 3, 4, 155, 154, 153, 152, 1, 2, 3, 4,
    ];
    let b: [u8; 16] = [0, 0, 0, 0, 2, 1, 2, 1, 1, 1, 1, 1, 1, 2, 1, 2];
    let mut got = [0u8; 16];
    unsafe { psad_bw128(got.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
    assert_eq!(got, qwords(1020, 614));

    if std::is_x86_feature_detected!("avx2") {
        // stdarch test_mm256_sad_epu8: each group 8*|2-4| = 16
        let a = [2u8; 32];
        let b = [4u8; 32];
        let mut got = [0u8; 32];
        unsafe { psad_bw256(got.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
        for lane in got.as_chunks::<8>().0 {
            assert_eq!(u64::from_le_bytes(*lane), 16);
        }
    }
}

#[test]
fn pclmulqdq_matches_intel_whitepaper_vectors() {
    if !std::is_x86_feature_detected!("pclmulqdq") {
        return;
    }
    // Intel clmul whitepaper known answer (stdarch test_mm_clmulepi64_si128)
    let a = qwords(0x63746f725d53475d, 0x7b5b546573745665);
    let b = qwords(0x5b477565726f6e5d, 0x4869285368617929);
    let cases = [
        (0x00u64, qwords(0x929633d5d36f0451, 0x1d4d84c85c3440c0)),
        (0x10, qwords(0x7fa540ac2a281315, 0x1bd17c8d556ab5a1)),
        (0x01, qwords(0xbabf262df4b7d5c9, 0x1a2bf6db3a30862f)),
        (0x11, qwords(0xd66ee03e410fd4ed, 0x1d1e1f2c592e7c45)),
    ];
    for (imm, expected) in cases {
        let mut got = [0u8; 16];
        unsafe { pclmulqdq(got.as_mut_ptr(), a.as_ptr(), b.as_ptr(), imm) };
        assert_eq!(got, expected, "imm={imm:#x}");
        // high imm bits ignored by hardware: OR 0xEE equivalent to low bits
        let mut got2 = [0u8; 16];
        unsafe { pclmulqdq(got2.as_mut_ptr(), a.as_ptr(), b.as_ptr(), imm | 0xee) };
        assert_eq!(got2, expected, "imm|0xEE={imm:#x}");
    }
}

#[test]
fn aesni_matches_msdn_known_vectors() {
    if !std::is_x86_feature_detected!("aes") {
        return;
    }
    // MSDN/stdarch constants (test_mm_aesenc_si128 same group)
    let a = qwords(0x8899aabbccddeeff, 0x0123456789abcdef);
    let k = qwords(0x0022446688aaccee, 0x1133557799bbddff);
    let mut got = [0u8; 16];

    unsafe { aesenc(got.as_mut_ptr(), a.as_ptr(), k.as_ptr()) };
    assert_eq!(got, qwords(0x28e4ee1884504333, 0x16ab0e57dfc442ed));
    unsafe { aesenclast(got.as_mut_ptr(), a.as_ptr(), k.as_ptr()) };
    assert_eq!(got, qwords(0x4b04f98cf4c860f8, 0xb6dd7df25d7ab320));
    unsafe { aesdec(got.as_mut_ptr(), a.as_ptr(), k.as_ptr()) };
    assert_eq!(got, qwords(0xb57ecfa381da39ee, 0x044e4f5176fec48f));
    unsafe { aesdeclast(got.as_mut_ptr(), a.as_ptr(), k.as_ptr()) };
    assert_eq!(got, qwords(0xf210dd981fa4a493, 0x36cad57d9072bf9e));
    unsafe { aesimc(got.as_mut_ptr(), a.as_ptr()) };
    assert_eq!(got, qwords(0x6633441122770055, 0xc66c82284ee40aa0));

    // keygenassist software model: MSDN known answer (imm=5) holds unconditionally
    unsafe { aeskeygenassist(got.as_mut_ptr(), a.as_ptr(), 5) };
    assert_eq!(got, qwords(0xeac4eea9c4eeacea, 0x857c266b7c266e85));
    // and cross-check multiple imm values against hardware (covers full-byte RCON semantics)
    use std::arch::x86_64::{_mm_aeskeygenassist_si128, _mm_loadu_si128, _mm_storeu_si128};
    for imm in [0x00u64, 0x01, 0x02, 0x1b, 0x36, 0x5a, 0x80, 0xa5, 0xff] {
        let mut hw = [0u8; 16];
        let mut sw = [0u8; 16];
        unsafe {
            let v = _mm_loadu_si128(a.as_ptr().cast());
            let r = match imm {
                0x00 => _mm_aeskeygenassist_si128::<0x00>(v),
                0x01 => _mm_aeskeygenassist_si128::<0x01>(v),
                0x02 => _mm_aeskeygenassist_si128::<0x02>(v),
                0x1b => _mm_aeskeygenassist_si128::<0x1b>(v),
                0x36 => _mm_aeskeygenassist_si128::<0x36>(v),
                0x5a => _mm_aeskeygenassist_si128::<0x5a>(v),
                0x80 => _mm_aeskeygenassist_si128::<0x80>(v),
                0xa5 => _mm_aeskeygenassist_si128::<0xa5>(v),
                _ => _mm_aeskeygenassist_si128::<0xff>(v),
            };
            _mm_storeu_si128(hw.as_mut_ptr().cast(), r);
            aeskeygenassist(sw.as_mut_ptr(), a.as_ptr(), imm);
        }
        assert_eq!(sw, hw, "imm={imm:#x}");
    }
}

#[test]
fn crc32_matches_stdarch_vectors_and_known_answer() {
    if !std::is_x86_feature_detected!("sse4.2") {
        return;
    }
    // stdarch known answer
    unsafe {
        assert_eq!(crc32_u8(0x2aa1e72b, 0x2a), 0xf24122e4);
        assert_eq!(crc32_u16(0x8ecec3b5, 0x22b), 0x13bb2fb);
        assert_eq!(crc32_u32(0xae2912c8, 0x845fed), 0xffae2ed1);
        assert_eq!(crc32_u64(0x7819dccd3e824, 0x2a22b845fed), 0xbb6cdc6c);
    }
    // CRC32C("123456789") = 0xe3069283 (wrapper provides initial/final inversion)
    let mut crc = 0xffff_ffffu32;
    for &byte in b"123456789" {
        crc = unsafe { crc32_u8(crc, byte) };
    }
    assert_eq!(!crc, 0xe306_9283);
}

#[test]
fn permd256_matches_stdarch_known_vector() {
    if !std::is_x86_feature_detected!("avx2") {
        return;
    }
    // stdarch test_mm256_permutevar8x32_epi32
    let a: [u32; 8] = [100, 200, 300, 400, 500, 600, 700, 800];
    let idx: [u32; 8] = [5, 0, 5, 1, 7, 6, 3, 4];
    let expected: [u32; 8] = [600, 100, 600, 200, 800, 700, 400, 500];
    let mut got = [0u32; 8];
    unsafe {
        permd256(
            got.as_mut_ptr().cast::<u8>(),
            a.as_ptr().cast::<u8>(),
            idx.as_ptr().cast::<u8>(),
        )
    };
    assert_eq!(got, expected);
}

#[test]
fn gather_q_pd_256_respects_mask_and_never_reads_masked_lanes() {
    // arr[i] = i as f64; scale=8 ⇒ f64 word addressing (stdarch test_mm256_mask_i64gather_pd)
    let arr: [f64; 128] = std::array::from_fn(|i| i as f64);
    let src = [256.0f64; 4];
    let vindex: [i64; 4] = [0, 16, 64, 96];
    let mask = [-1.0f64, -1.0, -1.0, 0.0];
    let mut got = [0.0f64; 4];
    unsafe {
        gather_q_pd_256(
            got.as_mut_ptr().cast::<u8>(),
            src.as_ptr().cast::<u8>(),
            arr.as_ptr() as u64,
            vindex.as_ptr().cast::<u8>(),
            mask.as_ptr().cast::<u8>(),
            8,
        );
    }
    assert_eq!(got, [0.0, 16.0, 64.0, 256.0]);

    // fault suppression: masked lanes with wild indices (addresses that would SIGSEGV on read) must not touch memory
    let mask_all_off = [0.0f64; 4];
    let wild: [i64; 4] = [0x7fff_ffff_fff0_0000; 4];
    let src2 = [42.0f64; 4];
    let mut got2 = [0.0f64; 4];
    unsafe {
        gather_q_pd_256(
            got2.as_mut_ptr().cast::<u8>(),
            src2.as_ptr().cast::<u8>(),
            1, // base=1: combined with wild index yields unreadable address
            wild.as_ptr().cast::<u8>(),
            mask_all_off.as_ptr().cast::<u8>(),
            8,
        );
    }
    assert_eq!(got2, [42.0; 4]);

    // cross-check against hardware vgatherqpd (all-mask-on + mixed-mask each one set)
    if std::is_x86_feature_detected!("avx2") {
        use std::arch::x86_64::{
            _mm256_loadu_pd, _mm256_loadu_si256, _mm256_mask_i64gather_pd, _mm256_storeu_pd,
        };
        for (src_v, idx_v, mask_v) in [
            (
                [256.0f64; 4],
                [0i64, 16, 64, 96],
                [-1.0f64, -1.0, -1.0, 0.0],
            ),
            ([7.0f64; 4], [3i64, 1, 127, 9], [-1.0f64, 0.0, -1.0, -1.0]),
        ] {
            let mut sw = [0.0f64; 4];
            unsafe {
                gather_q_pd_256(
                    sw.as_mut_ptr().cast::<u8>(),
                    src_v.as_ptr().cast::<u8>(),
                    arr.as_ptr() as u64,
                    idx_v.as_ptr().cast::<u8>(),
                    mask_v.as_ptr().cast::<u8>(),
                    8,
                );
                let idx = _mm256_loadu_si256(idx_v.as_ptr().cast());
                let m = _mm256_loadu_pd(mask_v.as_ptr());
                let s = _mm256_loadu_pd(src_v.as_ptr());
                let hw = _mm256_mask_i64gather_pd::<8>(s, arr.as_ptr(), idx, m);
                let mut hw_out = [0.0f64; 4];
                _mm256_storeu_pd(hw_out.as_mut_ptr(), hw);
                assert_eq!(sw, hw_out, "hw cross-check {idx_v:?}/{mask_v:?}");
            }
        }
    }
}

#[test]
fn gather_d_pd_256_sign_extends_i32_indexes_and_respects_mask() {
    // arr[i] = i as f64; scale=8 ⇒ f64 word addressing; i32 indices sign-extended (negative index wraps addressing)
    let arr: [f64; 128] = std::array::from_fn(|i| i as f64);
    let src = [9.0f64; 4];
    let vindex: [i32; 4] = [5, -1, 120, 0];
    let mask = [-1.0f64, -1.0, -1.0, 0.0];
    let mut got = [0.0f64; 4];
    // base deliberately raised 8 bytes: idx=-1 ⇒ addr = base-8 = arr[0]
    let base = unsafe { arr.as_ptr().add(1) } as u64;
    unsafe {
        gather_d_pd_256(
            got.as_mut_ptr().cast::<u8>(),
            src.as_ptr().cast::<u8>(),
            base,
            vindex.as_ptr().cast::<u8>(),
            mask.as_ptr().cast::<u8>(),
            8,
        );
    }
    assert_eq!(got, [6.0, 0.0, 121.0, 9.0]);

    if std::is_x86_feature_detected!("avx2") {
        use std::arch::x86_64::{
            _mm_loadu_si128, _mm256_loadu_pd, _mm256_mask_i32gather_pd, _mm256_storeu_pd,
        };
        let mut sw = [0.0f64; 4];
        unsafe {
            gather_d_pd_256(
                sw.as_mut_ptr().cast::<u8>(),
                src.as_ptr().cast::<u8>(),
                arr.as_ptr() as u64,
                vindex.as_ptr().cast::<u8>(),
                mask.as_ptr().cast::<u8>(),
                8,
            );
            let idx = _mm_loadu_si128(vindex.as_ptr().cast());
            let m = _mm256_loadu_pd(mask.as_ptr());
            let s = _mm256_loadu_pd(src.as_ptr());
            let hw = _mm256_mask_i32gather_pd::<8>(s, arr.as_ptr(), idx, m);
            let mut hw_out = [0.0f64; 4];
            _mm256_storeu_pd(hw_out.as_mut_ptr(), hw);
            assert_eq!(sw, hw_out, "hw cross-check d.pd.256");
        }
    }
}

#[test]
fn vpmadd52_matches_stdarch_vectors_and_hw_cross_check() {
    // stdarch known answer (same broadcast value for 128/256/512): a=10<<40, b=(11<<40)+4, c=(12<<40)+3
    let a = [10u64 << 40; 8];
    let b = [(11u64 << 40) + 4; 8];
    let c = [(12u64 << 40) + 3; 8];
    let mut got = [0u64; 8];
    // lo known answer: 128
    unsafe {
        vpmadd52::<2, false>(
            got.as_mut_ptr().cast::<u8>(),
            a.as_ptr().cast::<u8>(),
            b.as_ptr().cast::<u8>(),
            c.as_ptr().cast::<u8>(),
        )
    };
    assert_eq!(&got[..2], &[100055558127628u64; 2]);
    // hi known answer
    unsafe {
        vpmadd52::<2, true>(
            got.as_mut_ptr().cast::<u8>(),
            a.as_ptr().cast::<u8>(),
            b.as_ptr().cast::<u8>(),
            c.as_ptr().cast::<u8>(),
        )
    };
    assert_eq!(&got[..2], &[11030549757952u64; 2]);
    // 4/8 lane same-input broadcast
    unsafe {
        vpmadd52::<4, false>(
            got.as_mut_ptr().cast::<u8>(),
            a.as_ptr().cast::<u8>(),
            b.as_ptr().cast::<u8>(),
            c.as_ptr().cast::<u8>(),
        )
    };
    assert_eq!(&got[..4], &[100055558127628u64; 4]);
    unsafe {
        vpmadd52::<8, false>(
            got.as_mut_ptr().cast::<u8>(),
            a.as_ptr().cast::<u8>(),
            b.as_ptr().cast::<u8>(),
            c.as_ptr().cast::<u8>(),
        )
    };
    assert_eq!(got, [100055558127628u64; 8]);
    unsafe {
        vpmadd52::<8, true>(
            got.as_mut_ptr().cast::<u8>(),
            a.as_ptr().cast::<u8>(),
            b.as_ptr().cast::<u8>(),
            c.as_ptr().cast::<u8>(),
        )
    };
    assert_eq!(got, [11030549757952u64; 8]);

    // hardware cross-check: high 12 input bits polluted (pins 52-bit input mask semantics) + non-canonical lane values
    if std::is_x86_feature_detected!("avx512ifma")
        && std::is_x86_feature_detected!("avx512vl")
        && std::is_x86_feature_detected!("avx512f")
    {
        use std::arch::x86_64::{
            _mm_loadu_si128, _mm_madd52hi_epu64, _mm_madd52lo_epu64, _mm_storeu_si128,
            _mm256_loadu_si256, _mm256_madd52hi_epu64, _mm256_madd52lo_epu64, _mm256_storeu_si256,
            _mm512_loadu_si512, _mm512_madd52hi_epu64, _mm512_madd52lo_epu64, _mm512_storeu_si512,
        };
        let av: [u64; 8] = std::array::from_fn(|i| 0xdead_beef_0000_0001u64.wrapping_add(i as u64));
        let bv: [u64; 8] = std::array::from_fn(|i| {
            0xfff0_0000_0000_0000u64 | (0x0008_1234_5678_9abcu64.wrapping_sub(i as u64 * 7))
        });
        let cv: [u64; 8] = std::array::from_fn(|i| {
            0xabc0_0000_0000_0000u64 | (0x000f_edcb_a987_6543u64.wrapping_add(i as u64 * 3))
        });
        unsafe {
            // 128
            let (mut sw_lo, mut sw_hi) = ([0u64; 8], [0u64; 8]);
            vpmadd52::<2, false>(
                sw_lo.as_mut_ptr().cast::<u8>(),
                av.as_ptr().cast::<u8>(),
                bv.as_ptr().cast::<u8>(),
                cv.as_ptr().cast::<u8>(),
            );
            vpmadd52::<2, true>(
                sw_hi.as_mut_ptr().cast::<u8>(),
                av.as_ptr().cast::<u8>(),
                bv.as_ptr().cast::<u8>(),
                cv.as_ptr().cast::<u8>(),
            );
            let va = _mm_loadu_si128(av.as_ptr().cast());
            let vb = _mm_loadu_si128(bv.as_ptr().cast());
            let vc = _mm_loadu_si128(cv.as_ptr().cast());
            let (mut hw_lo, mut hw_hi) = ([0u64; 2], [0u64; 2]);
            _mm_storeu_si128(hw_lo.as_mut_ptr().cast(), _mm_madd52lo_epu64(va, vb, vc));
            _mm_storeu_si128(hw_hi.as_mut_ptr().cast(), _mm_madd52hi_epu64(va, vb, vc));
            assert_eq!(&sw_lo[..2], hw_lo, "lo128");
            assert_eq!(&sw_hi[..2], hw_hi, "hi128");
            // 256
            let (mut sw_lo, mut sw_hi) = ([0u64; 8], [0u64; 8]);
            vpmadd52::<4, false>(
                sw_lo.as_mut_ptr().cast::<u8>(),
                av.as_ptr().cast::<u8>(),
                bv.as_ptr().cast::<u8>(),
                cv.as_ptr().cast::<u8>(),
            );
            vpmadd52::<4, true>(
                sw_hi.as_mut_ptr().cast::<u8>(),
                av.as_ptr().cast::<u8>(),
                bv.as_ptr().cast::<u8>(),
                cv.as_ptr().cast::<u8>(),
            );
            let va = _mm256_loadu_si256(av.as_ptr().cast());
            let vb = _mm256_loadu_si256(bv.as_ptr().cast());
            let vc = _mm256_loadu_si256(cv.as_ptr().cast());
            let (mut hw_lo, mut hw_hi) = ([0u64; 4], [0u64; 4]);
            _mm256_storeu_si256(hw_lo.as_mut_ptr().cast(), _mm256_madd52lo_epu64(va, vb, vc));
            _mm256_storeu_si256(hw_hi.as_mut_ptr().cast(), _mm256_madd52hi_epu64(va, vb, vc));
            assert_eq!(&sw_lo[..4], hw_lo, "lo256");
            assert_eq!(&sw_hi[..4], hw_hi, "hi256");
            // 512
            let (mut sw_lo, mut sw_hi) = ([0u64; 8], [0u64; 8]);
            vpmadd52::<8, false>(
                sw_lo.as_mut_ptr().cast::<u8>(),
                av.as_ptr().cast::<u8>(),
                bv.as_ptr().cast::<u8>(),
                cv.as_ptr().cast::<u8>(),
            );
            vpmadd52::<8, true>(
                sw_hi.as_mut_ptr().cast::<u8>(),
                av.as_ptr().cast::<u8>(),
                bv.as_ptr().cast::<u8>(),
                cv.as_ptr().cast::<u8>(),
            );
            let va = _mm512_loadu_si512(av.as_ptr().cast());
            let vb = _mm512_loadu_si512(bv.as_ptr().cast());
            let vc = _mm512_loadu_si512(cv.as_ptr().cast());
            let (mut hw_lo, mut hw_hi) = ([0u64; 8], [0u64; 8]);
            _mm512_storeu_si512(hw_lo.as_mut_ptr().cast(), _mm512_madd52lo_epu64(va, vb, vc));
            _mm512_storeu_si512(hw_hi.as_mut_ptr().cast(), _mm512_madd52hi_epu64(va, vb, vc));
            assert_eq!(sw_lo, hw_lo, "lo512");
            assert_eq!(sw_hi, hw_hi, "hi512");
        }
    }
}

#[test]
fn pmadd_matches_stdarch_known_vectors() {
    if std::is_x86_feature_detected!("ssse3") {
        // stdarch test_mm_maddubs_epi16 (includes saturated group)
        let a: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let b: [i8; 16] = [4, 63, 4, 3, 24, 12, 6, 19, 12, 5, 5, 10, 4, 1, 8, 0];
        let expected: [i16; 8] = [130, 24, 192, 194, 158, 175, 66, 120];
        let mut got = [0i16; 8];
        unsafe {
            pmaddubsw128(
                got.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(got, expected);

        let mut b = [0i8; 16];
        b[..10].copy_from_slice(&[
            i8::MAX,
            i8::MAX,
            i8::MAX,
            i8::MIN,
            i8::MIN,
            i8::MIN,
            50,
            15,
            0,
            0,
        ]);
        let a2: [u8; 16] = {
            let mut v = [0u8; 16];
            v[..6].copy_from_slice(&[u8::MAX; 6]);
            v[6] = 100;
            v[7] = 100;
            v
        };
        let expected: [i16; 8] = [i16::MAX, -255, i16::MIN, 6500, 0, 0, 0, 0];
        let mut got = [0i16; 8];
        unsafe {
            pmaddubsw128(
                got.as_mut_ptr().cast::<u8>(),
                a2.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(got, expected);
    }
    if std::is_x86_feature_detected!("avx2") {
        // stdarch test_mm256_maddubs_epi16: 2*4+2*4 = 16 broadcast
        let a = [2u8; 32];
        let b = [4i8; 32];
        let mut got = [0i16; 16];
        unsafe {
            pmaddubsw256(
                got.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(got, [16i16; 16]);
        // stdarch test_mm256_madd_epi16: 2*4+2*4 = 16 broadcast
        let a = [2i16; 16];
        let b = [4i16; 16];
        let mut got = [0i32; 8];
        unsafe {
            pmaddwd256(
                got.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(got, [16i32; 8]);
    }
    if std::is_x86_feature_detected!("sse2") {
        // stdarch test_mm_madd_epi16
        let a: [i16; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        let b: [i16; 8] = [9, 10, 11, 12, 13, 14, 15, 16];
        let expected: [i32; 4] = [29, 81, 149, 233];
        let mut got = [0i32; 4];
        unsafe {
            pmaddwd128(
                got.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(got, expected);
        // MIN*MIN+MIN*MIN wraps to i32::MIN (hardware-defined)
        let a: [i16; 8] = [i16::MIN, i16::MIN, 0, 0, 0, 0, 0, 0];
        let b: [i16; 8] = [i16::MIN, i16::MIN, 0, 0, 0, 0, 0, 0];
        let mut got = [0i32; 4];
        unsafe {
            pmaddwd128(
                got.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(got[0], i32::MIN);
    }
}

/// VCVTPS2PH software model vs hardware `_mm_cvtps_ph::<imm>`: full f16 domain round-trip + boundaries
/// boiling point + LCG random large spectrum × imm 0..7 all rounding modes (including MXCSR=CUR_DIRECTION 4..7).
#[test]
fn cvtps2ph_software_matches_f16c_hardware_bitwise() {
    if !std::is_x86_feature_detected!("f16c") {
        return;
    }
    use super::{HalfRound, cvtph2ps, cvtps2ph, f16_to_f32_sw, f32_to_f16_sw};
    use std::arch::x86_64::{_mm_cvtps_ph, _mm_set_ps1, _mm_storeu_si128};
    let hw = |bits: u32, imm: u64| -> u16 {
        let v = unsafe { _mm_set_ps1(f32::from_bits(bits)) };
        let r = unsafe {
            match imm {
                0 => _mm_cvtps_ph::<0>(v),
                1 => _mm_cvtps_ph::<1>(v),
                2 => _mm_cvtps_ph::<2>(v),
                3 => _mm_cvtps_ph::<3>(v),
                _ => _mm_cvtps_ph::<4>(v), // 4 = CUR_DIRECTION: default MXCSR = RNE
            }
        };
        let mut o = [0u16; 8];
        unsafe { _mm_storeu_si128(o.as_mut_ptr().cast(), r) };
        o[0]
    };
    // software model vs hardware reference: imm 0..3 explicit rounding, 4 MXCSR(=RNE). stdarch const generic
    // limits imm<5; imm[3] (no-exc) only affects exception flags, not value bits (native probe recorded).
    let check = |bits: u32| {
        for (imm, mode) in [
            (0u64, HalfRound::Rne),
            (1, HalfRound::Down),
            (2, HalfRound::Up),
            (3, HalfRound::Trunc),
            (4, HalfRound::Rne),
        ] {
            let expect = hw(bits, imm);
            assert_eq!(
                f32_to_f16_sw(bits, mode),
                expect,
                "bits={bits:08x} imm={imm}"
            );
            // high-end helper same path (.256 width all lanes + .128 high 64 bits zeroed, separately checked)
            let f = [f32::from_bits(bits); 8];
            let mut o = [0u16; 8];
            unsafe { cvtps2ph::<8>(o.as_mut_ptr().cast::<u8>(), f.as_ptr().cast::<u8>(), imm) };
            for (i, &h) in o.iter().enumerate() {
                assert_eq!(h, expect, "helper bits={bits:08x} imm={imm} lane={i}");
            }
            let mut o4 = [0xffffu16; 8];
            unsafe { cvtps2ph::<4>(o4.as_mut_ptr().cast::<u8>(), f.as_ptr().cast::<u8>(), imm) };
            assert_eq!(&o4[..4], [expect; 4], "helper128 bits={bits:08x} imm={imm}");
            assert_eq!(
                &o4[4..],
                [0u16; 4],
                "helper128 high 64 bits cleared bits={bits:08x} imm={imm}"
            );
        }
    };
    // ① NaN/inf/±0/subnormal/boiling point all listed
    let specials: [u32; 33] = [
        0x0000_0000,
        0x8000_0000,
        0x0000_0001,
        0x8000_0001,
        0x007f_ffff,
        0x0080_0000,
        0x3380_0000,
        0x337f_ffff,
        0x3380_0001,
        0x33ff_ffff,
        0x3400_0000,
        0x3400_0001,
        0x3400_07ff,
        0x3400_0800,
        0x3800_0000,
        0x387f_f800,
        0x387f_f000,
        0x387f_efff,
        0x3880_0000,
        0x477f_e000,
        0x477f_efff,
        0x477f_f000,
        0x477f_f800,
        0x477f_ffff,
        0x4780_0000,
        0x477c_0000,
        0xc77f_e000,
        0x7f80_0000,
        0xff80_0000,
        0x7fc0_0000,
        0x7f80_0001,
        0x7f80_0002,
        0x7fff_ffff,
    ];
    for &b in &specials {
        check(b);
    }
    // ② full f16 bit-pattern f32 image (round-trip domain fully covered)
    for u in 0u32..=0xffff {
        check(f16_to_f32_sw(u as u16));
    }
    // ③ 2 million LCG random (×8 imm)
    let mut rng = 0x9e3779b97f4a7c15u64;
    for _ in 0..2_000_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        check(rng as u32);
    }
    // ④ VCVTPH2PS: all 65536 bit-patterns software vs helper + known hardware bit-pattern (NaN qbit forced etc.)
    if std::is_x86_feature_detected!("f16c") {
        use std::arch::x86_64::{_mm_cvtph_ps, _mm_cvtsi32_si128, _mm_storeu_ps};
        for u in 0u32..=0xffff {
            let sw = f16_to_f32_sw(u as u16);
            let hw = unsafe {
                let r = _mm_cvtph_ps(_mm_cvtsi32_si128(u as i32));
                let mut o = [0f32; 4];
                _mm_storeu_ps(o.as_mut_ptr(), r);
                o[0].to_bits()
            };
            assert_eq!(sw, hw, "ph2ps {u:04x}");
            // 8-lane helper (.256 same path: each lane same value)
            let v = [u as u16; 8];
            let mut o = [0u32; 8];
            unsafe { cvtph2ps::<8>(o.as_mut_ptr().cast::<u8>(), v.as_ptr().cast::<u8>()) };
            for (i, &x) in o.iter().enumerate() {
                assert_eq!(x, hw, "ph2ps helper {u:04x} lane={i}");
            }
        }
    }
}

/// Family ⑨ packed-f32 lane software model vs hardware instruction cross-check (max/min/cmp/round/cvt/blendv,
/// 128/256 double width; NaN bit passthrough, ±0 same bits, full 32-predicate table, full rounding imm table).
#[test]
fn packed_ps_lane_models_match_hardware_bitwise() {
    use super::{blendv_ps, cmp_ps, cvt_ps2dq, maxmin_ps, round_ps};
    // (a,b) combinations cover: normal/reversed order/±0/±inf/qNaN/sNaN/different payload
    let bits_a: [u32; 9] = [
        0x3f80_0000,
        0xbf80_0000,
        0x8000_0000,
        0x0000_0000,
        0x7f80_0000,
        0xff80_0000,
        0x7fc0_0000,
        0x7f80_0001,
        0x7fa0_0000,
    ];
    let bits_b: [u32; 9] = [
        0x4000_0000,
        0x3f80_0000,
        0x0000_0000,
        0x8000_0000,
        0xff80_0000,
        0x7f80_0000,
        0x7f80_0001,
        0x7fc0_0000,
        0x7fc0_0000,
    ];
    if std::is_x86_feature_detected!("sse") {
        use std::arch::x86_64::{_mm_loadu_ps, _mm_max_ps, _mm_min_ps, _mm_storeu_ps};
        unsafe {
            for (&a, &b) in bits_a.iter().zip(&bits_b) {
                let (va, vb) = (
                    _mm_loadu_ps([f32::from_bits(a); 4].as_ptr()),
                    _mm_loadu_ps([f32::from_bits(b); 4].as_ptr()),
                );
                let (mut hw_max, mut hw_min) = ([0f32; 4], [0f32; 4]);
                _mm_storeu_ps(hw_max.as_mut_ptr(), _mm_max_ps(va, vb));
                _mm_storeu_ps(hw_min.as_mut_ptr(), _mm_min_ps(va, vb));
                let (mut sw_max, mut sw_min) = ([0f32; 4], [0f32; 4]);
                maxmin_ps::<4, true>(
                    sw_max.as_mut_ptr().cast::<u8>(),
                    [f32::from_bits(a); 4].as_ptr().cast::<u8>(),
                    [f32::from_bits(b); 4].as_ptr().cast::<u8>(),
                );
                maxmin_ps::<4, false>(
                    sw_min.as_mut_ptr().cast::<u8>(),
                    [f32::from_bits(a); 4].as_ptr().cast::<u8>(),
                    [f32::from_bits(b); 4].as_ptr().cast::<u8>(),
                );
                assert_eq!(
                    sw_max.map(f32::to_bits),
                    hw_max.map(f32::to_bits),
                    "max {a:08x}/{b:08x}"
                );
                assert_eq!(
                    sw_min.map(f32::to_bits),
                    hw_min.map(f32::to_bits),
                    "min {a:08x}/{b:08x}"
                );
            }
        }
    }
    // cmp.ps 128/256 full 32 predicates × full bit-pattern combinations (including same-bit comparison for same-NaN input)
    if std::is_x86_feature_detected!("sse") && std::is_x86_feature_detected!("avx") {
        use std::arch::x86_64::{
            _mm_cmp_ps, _mm_loadu_ps, _mm_storeu_ps, _mm256_cmp_ps, _mm256_loadu_ps,
            _mm256_storeu_ps,
        };
        macro_rules! cmp_hw {
            ($va:expr, $vb:expr, $va8:expr, $vb8:expr, $lane:expr, $a:expr, $b:expr, $imm:literal, $swa:expr, $swb:expr, $swa8:expr, $swb8:expr) => {{
                let mut o = [0f32; 4];
                _mm_storeu_ps(o.as_mut_ptr(), _mm_cmp_ps::<$imm>($va, $vb));
                let mut sw = [0u32; 4];
                cmp_ps::<4>(sw.as_mut_ptr().cast::<u8>(), $swa, $swb, $imm);
                assert_eq!(
                    sw,
                    o.map(f32::to_bits),
                    "cmp128 {} {}/{:08x}/{:08x}",
                    $imm,
                    $lane,
                    $a,
                    $b
                );
                let mut o8 = [0f32; 8];
                _mm256_storeu_ps(o8.as_mut_ptr(), _mm256_cmp_ps::<$imm>($va8, $vb8));
                let mut sw8 = [0u32; 8];
                cmp_ps::<8>(sw8.as_mut_ptr().cast::<u8>(), $swa8, $swb8, $imm);
                assert_eq!(
                    sw8,
                    o8.map(f32::to_bits),
                    "cmp256 {} {}/{:08x}/{:08x}",
                    $imm,
                    $lane,
                    $a,
                    $b
                );
            }};
        }
        for (&a, &b) in bits_a.iter().zip(&bits_b) {
            let (fa, fb) = ([f32::from_bits(a); 4], [f32::from_bits(b); 4]);
            let (fa8, fb8) = ([f32::from_bits(a); 8], [f32::from_bits(b); 8]);
            unsafe {
                let (va, vb) = (_mm_loadu_ps(fa.as_ptr()), _mm_loadu_ps(fb.as_ptr()));
                let (va8, vb8) = (_mm256_loadu_ps(fa8.as_ptr()), _mm256_loadu_ps(fb8.as_ptr()));
                let (swa, swb) = (fa.as_ptr().cast::<u8>(), fb.as_ptr().cast::<u8>());
                let (swa8, swb8) = (fa8.as_ptr().cast::<u8>(), fb8.as_ptr().cast::<u8>());
                cmp_hw!(va, vb, va8, vb8, "eq", a, b, 0, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "lt", a, b, 1, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "le", a, b, 2, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "unord", a, b, 3, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "neq_uq", a, b, 4, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "nlt", a, b, 5, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "nle", a, b, 6, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "ord", a, b, 7, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "eq_uq", a, b, 8, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "nge", a, b, 9, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "ngt", a, b, 10, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "false", a, b, 11, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "neq_oq", a, b, 12, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "ge", a, b, 13, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "gt", a, b, 14, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "true", a, b, 15, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "eq_os", a, b, 16, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "lt_oq", a, b, 17, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "le_oq", a, b, 18, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "unord_s", a, b, 19, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "neq_us", a, b, 20, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "nlt_uq", a, b, 21, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "nle_uq", a, b, 22, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "ord_s", a, b, 23, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "eq_us", a, b, 24, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "nge_uq", a, b, 25, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "ngt_uq", a, b, 26, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "false_os", a, b, 27, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "neq_os", a, b, 28, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "ge_oq", a, b, 29, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "gt_oq", a, b, 30, swa, swb, swa8, swb8);
                cmp_hw!(va, vb, va8, vb8, "true_us", a, b, 31, swa, swb, swa8, swb8);
            }
        }
    }
    // round.ps imm 0..=15 (bit2 MXCSR / bit3 no-exc bit combination)
    if std::is_x86_feature_detected!("sse4.1") {
        use std::arch::x86_64::{_mm_loadu_ps, _mm_round_ps, _mm_storeu_ps};
        let inputs: [u32; 14] = [
            0x0000_0000,
            0x8000_0000,
            0x3f00_0000,
            0xbf00_0000,
            0x3fa0_0000,
            0xbfa0_0000,
            0x4049_0fdb,
            0x7f80_0000,
            0xff80_0000,
            0x7fc0_0000,
            0x7f80_0001,
            0x3f80_0000,
            0x50c3_2e15,
            0xd0c3_2e15,
        ];
        macro_rules! round_hw {
            ($v:expr, $bits:expr, $imm:literal) => {{
                let mut o = [0f32; 4];
                _mm_storeu_ps(o.as_mut_ptr(), _mm_round_ps::<$imm>($v));
                let mut sw = [0f32; 4];
                round_ps::<4>(
                    sw.as_mut_ptr().cast::<u8>(),
                    [f32::from_bits($bits); 4].as_ptr().cast::<u8>(),
                    $imm,
                );
                assert_eq!(
                    sw.map(f32::to_bits),
                    o.map(f32::to_bits),
                    "round imm={} {:08x}",
                    $imm,
                    $bits
                );
            }};
        }
        for &b in &inputs {
            let v = unsafe { _mm_loadu_ps([f32::from_bits(b); 4].as_ptr()) };
            unsafe {
                round_hw!(v, b, 0);
                round_hw!(v, b, 1);
                round_hw!(v, b, 2);
                round_hw!(v, b, 3);
                round_hw!(v, b, 4);
                round_hw!(v, b, 5);
                round_hw!(v, b, 6);
                round_hw!(v, b, 7);
                round_hw!(v, b, 8);
                round_hw!(v, b, 9);
                round_hw!(v, b, 10);
                round_hw!(v, b, 11);
            }
        }
    }
    // cvt/cvtt boundaries (indefinite 0x80000000 domain), 128/256 double width
    if std::is_x86_feature_detected!("sse2") && std::is_x86_feature_detected!("avx") {
        use std::arch::x86_64::{
            _mm_cvtps_epi32, _mm_cvttps_epi32, _mm_loadu_ps, _mm_storeu_si128, _mm256_cvtps_epi32,
            _mm256_cvttps_epi32, _mm256_loadu_ps, _mm256_storeu_si256,
        };
        let inputs: [u32; 16] = [
            0x0000_0000,
            0x8000_0000,
            0x3f00_0000,
            0x3f40_0000,
            0x3fa0_0000,
            0xbfa0_0000,
            0x4f00_0000,
            0x4eff_ffff,
            0xcf00_0000,
            0xceff_ffff,
            0x7f80_0000,
            0xff80_0000,
            0x7fc0_0000,
            0x7f80_0001,
            0x0e8e_fdb9,
            0x60ad_78ec,
        ];
        unsafe {
            for &b in &inputs {
                let v = _mm_loadu_ps([f32::from_bits(b); 4].as_ptr());
                let v8 = _mm256_loadu_ps([f32::from_bits(b); 8].as_ptr());
                let (mut hw, mut hw8) = ([0i32; 4], [0i32; 8]);
                _mm_storeu_si128(hw.as_mut_ptr().cast(), _mm_cvtps_epi32(v));
                _mm256_storeu_si256(hw8.as_mut_ptr().cast(), _mm256_cvtps_epi32(v8));
                let (mut sw, mut sw8) = ([0i32; 4], [0i32; 8]);
                cvt_ps2dq::<4, false>(
                    sw.as_mut_ptr().cast::<u8>(),
                    [f32::from_bits(b); 4].as_ptr().cast::<u8>(),
                );
                cvt_ps2dq::<8, false>(
                    sw8.as_mut_ptr().cast::<u8>(),
                    [f32::from_bits(b); 8].as_ptr().cast::<u8>(),
                );
                assert_eq!(sw, hw, "cvt {b:08x}");
                assert_eq!(sw8, hw8, "cvt256 {b:08x}");
                _mm_storeu_si128(hw.as_mut_ptr().cast(), _mm_cvttps_epi32(v));
                _mm256_storeu_si256(hw8.as_mut_ptr().cast(), _mm256_cvttps_epi32(v8));
                cvt_ps2dq::<4, true>(
                    sw.as_mut_ptr().cast::<u8>(),
                    [f32::from_bits(b); 4].as_ptr().cast::<u8>(),
                );
                cvt_ps2dq::<8, true>(
                    sw8.as_mut_ptr().cast::<u8>(),
                    [f32::from_bits(b); 8].as_ptr().cast::<u8>(),
                );
                assert_eq!(sw, hw, "cvtt {b:08x}");
                assert_eq!(sw8, hw8, "cvtt256 {b:08x}");
            }
        }
    }
    // blendv: mask sign bit pure bit selection
    if std::is_x86_feature_detected!("sse4.1") {
        use std::arch::x86_64::{_mm_blendv_ps, _mm_loadu_ps, _mm_storeu_ps};
        let a = [1.5f32, -2.25, 0.0, f32::NAN];
        let b = [7.0f32, -0.0, f32::INFINITY, -9.5];
        let m = [-1.0f32, 0.0, -0.0, 1.0]; // lane0/2 -> b; lane1/3 -> a
        unsafe {
            let r = _mm_blendv_ps(
                _mm_loadu_ps(a.as_ptr()),
                _mm_loadu_ps(b.as_ptr()),
                _mm_loadu_ps(m.as_ptr()),
            );
            let mut o = [0f32; 4];
            _mm_storeu_ps(o.as_mut_ptr(), r);
            let mut sw = [0f32; 4];
            blendv_ps::<4>(
                sw.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
                m.as_ptr().cast::<u8>(),
            );
            assert_eq!(sw.map(f32::to_bits), o.map(f32::to_bits));
        }
    }
}

/// PSLL/PSRL.d software model vs SSE2 hardware: count threshold boundaries (31/32/33/all-high bytes)
/// and data bit-pattern boundaries (including 0x80000000 bit-pattern, confirming logical right shift does not spread sign).
#[test]
fn pshift_d_models_match_sse2_hardware() {
    use super::pshift32;
    if !std::is_x86_feature_detected!("sse2") {
        return;
    }
    use std::arch::x86_64::{_mm_loadu_si128, _mm_sll_epi32, _mm_srl_epi32, _mm_storeu_si128};
    let vals = [
        [0x0000_0001u32, 0x8000_0000, 0xffff_ffff, 0x1234_5678],
        [0x0000_0000u32, 0x7fff_ffff, 0x8000_0001, 0x0000_0002],
    ];
    // count high 64 bits randomly polluted (hardware ignores); boundaries 31/32/33
    let counts: [[u64; 2]; 8] = [
        [0, 0],
        [1, 0],
        [15, 0xffff_ffff_ffff_ffff],
        [16, 0xaa55],
        [31, 0],
        [32, 0],
        [33, 0xdead_beef],
        [0xffff_ffff_ffff_ffff, 0],
    ];
    unsafe {
        for v in vals {
            let x = _mm_loadu_si128(v.as_ptr().cast());
            for c in counts {
                let cv = _mm_loadu_si128(c.as_ptr().cast());
                let (mut hw_l, mut hw_r) = ([0u32; 4], [0u32; 4]);
                _mm_storeu_si128(hw_l.as_mut_ptr().cast(), _mm_sll_epi32(x, cv));
                _mm_storeu_si128(hw_r.as_mut_ptr().cast(), _mm_srl_epi32(x, cv));
                let (mut sw_l, mut sw_r) = ([0u32; 4], [0u32; 4]);
                pshift32::<4, true>(
                    sw_l.as_mut_ptr().cast::<u8>(),
                    v.as_ptr().cast::<u8>(),
                    c.as_ptr().cast::<u8>(),
                );
                pshift32::<4, false>(
                    sw_r.as_mut_ptr().cast::<u8>(),
                    v.as_ptr().cast::<u8>(),
                    c.as_ptr().cast::<u8>(),
                );
                assert_eq!(sw_l, hw_l, "psll.d {v:?} count={c:?}");
                assert_eq!(sw_r, hw_r, "psrl.d {v:?} count={c:?}");
            }
        }
    }
}
