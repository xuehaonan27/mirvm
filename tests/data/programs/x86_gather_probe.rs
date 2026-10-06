// Permanent differential probe for the whole x86 gather family: 16 VEX forms whose mask is a vector
// of sign bits and 24 EVEX forms whose mask is a k register. Each line prints one form's raw result
// bytes with a fixed source vector and a fixed mask, so native on the same machine is the authority
// for every value and mirvm has to match it byte for byte.
//
// The probe also pins the parts a value check alone would miss: masked-off lanes must carry their own
// `src` lane, not another lane's, and no lane may be read from memory when its mask bit is clear —
// the offsets here address a buffer whose tail is deliberately unmatched by the mask.
//
// A feature the CPU lacks prints one marker line instead, and both legs see the same CPU, so the
// marker is identical on both sides rather than a reason to skip.
use std::arch::x86_64::*;

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// One form's result, printed as bytes so the width and the masked-off lanes are both visible.
unsafe fn show<T: Copy>(label: &str, value: T, bytes: usize) {
    let mut out = [0u8; 64];
    unsafe { std::ptr::write_unaligned(out.as_mut_ptr().cast::<T>(), value) };
    println!("{label} = {}", hex(&out[..bytes]));
}

/// Inclusion masks: every interesting pattern, so a form that ignores the mask or shifts it shows up.
const K4: __mmask8 = 0b1011;
const K8: __mmask8 = 0b1011_0101;
const K16: __mmask16 = 0b1010_0101_1010_0101;

/// The source lanes the intrinsics carry over for a masked-off destination lane. They are distinct
/// per position, so a wrong lane or a wrong mask leaves a visible trace.
fn sources() -> ([i32; 16], [i64; 8], [f32; 16], [f64; 8]) {
    let ints = std::array::from_fn(|i| 0x1000_0000u32.wrapping_add(i as u32) as i32);
    let longs = std::array::from_fn(|i| 0x2000_0000_0000_0000u64.wrapping_add(i as u64) as i64);
    let floats = std::array::from_fn(|i| 1.0 + i as f32 * 0.5);
    let doubles = std::array::from_fn(|i| 2.0 + i as f64 * 0.25);
    (ints, longs, floats, doubles)
}

/// The sign-bit masks the VEX forms read. The pattern differs from the EVEX k masks on purpose, so a
/// result one family produced cannot be mistaken for the other's.
fn sign_masks() -> ([i32; 16], [i64; 8], [f32; 16], [f64; 8]) {
    let mut ints = [0i32; 16];
    let mut longs = [0i64; 8];
    let mut floats = [0f32; 16];
    let mut doubles = [0f64; 8];
    for lane in 0..16 {
        if (u32::from(K16) >> lane) & 1 != 0 {
            ints[lane] = i32::MIN;
            floats[lane] = f32::from_bits(0x8000_0000);
        }
    }
    for lane in 0..8 {
        if (u32::from(K8) >> lane) & 1 != 0 {
            longs[lane] = i64::MIN;
            doubles[lane] = f64::from_bits(0x8000_0000_0000_0000);
        }
    }
    (ints, longs, floats, doubles)
}

#[target_feature(enable = "avx2")]
unsafe fn vex_forms(ints: &[i32], longs: &[i64], floats: &[f32], doubles: &[f64]) {
    let (src_i32, src_i64, src_f32, src_f64) = sources();
    let (mask_i32, mask_i64, mask_f32, mask_f64) = sign_masks();
    let idx_i32: [i32; 16] = std::array::from_fn(|i| i as i32);
    let idx_i64: [i64; 8] = std::array::from_fn(|i| i as i64);
    unsafe {
        let value: __m128i = _mm_mask_i32gather_epi32::<4>(
            _mm_loadu_si128(src_i32.as_ptr().cast()),
            ints.as_ptr(),
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            _mm_loadu_si128(mask_i32.as_ptr().cast()),
        );
        show("d.d.128", value, 16);
    }
    unsafe {
        let value: __m256i = _mm256_mask_i32gather_epi32::<4>(
            _mm256_loadu_si256(src_i32.as_ptr().cast()),
            ints.as_ptr(),
            _mm256_loadu_si256(idx_i32.as_ptr().cast()),
            _mm256_loadu_si256(mask_i32.as_ptr().cast()),
        );
        show("d.d.256", value, 32);
    }
    unsafe {
        let value: __m128i = _mm_mask_i32gather_epi64::<8>(
            _mm_loadu_si128(src_i64.as_ptr().cast()),
            longs.as_ptr(),
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            _mm_loadu_si128(mask_i64.as_ptr().cast()),
        );
        show("d.q.128", value, 16);
    }
    unsafe {
        let value: __m256i = _mm256_mask_i32gather_epi64::<8>(
            _mm256_loadu_si256(src_i64.as_ptr().cast()),
            longs.as_ptr(),
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            _mm256_loadu_si256(mask_i64.as_ptr().cast()),
        );
        show("d.q.256", value, 32);
    }
    unsafe {
        let value: __m128i = _mm_mask_i64gather_epi32::<4>(
            _mm_loadu_si128(src_i32.as_ptr().cast()),
            ints.as_ptr(),
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            _mm_loadu_si128(mask_i32.as_ptr().cast()),
        );
        show("q.d.128", value, 16);
    }
    unsafe {
        let value: __m128i = _mm256_mask_i64gather_epi32::<4>(
            _mm_loadu_si128(src_i32.as_ptr().cast()),
            ints.as_ptr(),
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            _mm_loadu_si128(mask_i32.as_ptr().cast()),
        );
        show("q.d.256", value, 16);
    }
    unsafe {
        let value: __m128i = _mm_mask_i64gather_epi64::<8>(
            _mm_loadu_si128(src_i64.as_ptr().cast()),
            longs.as_ptr(),
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            _mm_loadu_si128(mask_i64.as_ptr().cast()),
        );
        show("q.q.128", value, 16);
    }
    unsafe {
        let value: __m256i = _mm256_mask_i64gather_epi64::<8>(
            _mm256_loadu_si256(src_i64.as_ptr().cast()),
            longs.as_ptr(),
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            _mm256_loadu_si256(mask_i64.as_ptr().cast()),
        );
        show("q.q.256", value, 32);
    }
    unsafe {
        let value: __m128 = _mm_mask_i32gather_ps::<4>(
            _mm_loadu_ps(src_f32.as_ptr()),
            floats.as_ptr(),
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            _mm_loadu_ps(mask_f32.as_ptr()),
        );
        show("d.ps.128", value, 16);
    }
    unsafe {
        let value: __m256 = _mm256_mask_i32gather_ps::<4>(
            _mm256_loadu_ps(src_f32.as_ptr()),
            floats.as_ptr(),
            _mm256_loadu_si256(idx_i32.as_ptr().cast()),
            _mm256_loadu_ps(mask_f32.as_ptr()),
        );
        show("d.ps.256", value, 32);
    }
    unsafe {
        let value: __m128 = _mm_mask_i64gather_ps::<4>(
            _mm_loadu_ps(src_f32.as_ptr()),
            floats.as_ptr(),
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            _mm_loadu_ps(mask_f32.as_ptr()),
        );
        show("q.ps.128", value, 16);
    }
    unsafe {
        let value: __m128 = _mm256_mask_i64gather_ps::<4>(
            _mm_loadu_ps(src_f32.as_ptr()),
            floats.as_ptr(),
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            _mm_loadu_ps(mask_f32.as_ptr()),
        );
        show("q.ps.256", value, 16);
    }
    unsafe {
        let value: __m128d = _mm_mask_i32gather_pd::<8>(
            _mm_loadu_pd(src_f64.as_ptr()),
            doubles.as_ptr(),
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            _mm_loadu_pd(mask_f64.as_ptr()),
        );
        show("d.pd.128", value, 16);
    }
    unsafe {
        let value: __m256d = _mm256_mask_i32gather_pd::<8>(
            _mm256_loadu_pd(src_f64.as_ptr()),
            doubles.as_ptr(),
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            _mm256_loadu_pd(mask_f64.as_ptr()),
        );
        show("d.pd.256", value, 32);
    }
    unsafe {
        let value: __m128d = _mm_mask_i64gather_pd::<8>(
            _mm_loadu_pd(src_f64.as_ptr()),
            doubles.as_ptr(),
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            _mm_loadu_pd(mask_f64.as_ptr()),
        );
        show("q.pd.128", value, 16);
    }
    unsafe {
        let value: __m256d = _mm256_mask_i64gather_pd::<8>(
            _mm256_loadu_pd(src_f64.as_ptr()),
            doubles.as_ptr(),
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            _mm256_loadu_pd(mask_f64.as_ptr()),
        );
        show("q.pd.256", value, 32);
    }
}

#[target_feature(enable = "avx512f")]
unsafe fn evex_forms(ints: &[i32], longs: &[i64], floats: &[f32], doubles: &[f64]) {
    let (src_i32, src_i64, src_f32, src_f64) = sources();
    let (mask_i32, mask_i64, mask_f32, mask_f64) = sign_masks();
    let idx_i32: [i32; 16] = std::array::from_fn(|i| i as i32);
    let idx_i64: [i64; 8] = std::array::from_fn(|i| i as i64);
    unsafe {
        let value: __m512d = _mm512_mask_i32gather_pd::<8>(
            _mm512_loadu_pd(src_f64.as_ptr()),
            K8,
            _mm256_loadu_si256(idx_i32.as_ptr().cast()),
            doubles.as_ptr(),
        );
        show("gather.dpd.512", value, 64);
    }
    unsafe {
        let value: __m512 = _mm512_mask_i32gather_ps::<4>(
            _mm512_loadu_ps(src_f32.as_ptr()),
            K16,
            _mm512_loadu_si512(idx_i32.as_ptr().cast()),
            floats.as_ptr(),
        );
        show("gather.dps.512", value, 64);
    }
    unsafe {
        let value: __m512i = _mm512_mask_i32gather_epi64::<8>(
            _mm512_loadu_si512(src_i64.as_ptr().cast()),
            K8,
            _mm256_loadu_si256(idx_i32.as_ptr().cast()),
            longs.as_ptr(),
        );
        show("gather.dpq.512", value, 64);
    }
    unsafe {
        let value: __m512i = _mm512_mask_i32gather_epi32::<4>(
            _mm512_loadu_si512(src_i32.as_ptr().cast()),
            K16,
            _mm512_loadu_si512(idx_i32.as_ptr().cast()),
            ints.as_ptr(),
        );
        show("gather.dpi.512", value, 64);
    }
    unsafe {
        let value: __m512d = _mm512_mask_i64gather_pd::<8>(
            _mm512_loadu_pd(src_f64.as_ptr()),
            K8,
            _mm512_loadu_si512(idx_i64.as_ptr().cast()),
            doubles.as_ptr(),
        );
        show("gather.qpd.512", value, 64);
    }
    unsafe {
        let value: __m256 = _mm512_mask_i64gather_ps::<4>(
            _mm256_loadu_ps(src_f32.as_ptr()),
            K8,
            _mm512_loadu_si512(idx_i64.as_ptr().cast()),
            floats.as_ptr(),
        );
        show("gather.qps.512", value, 32);
    }
    unsafe {
        let value: __m512i = _mm512_mask_i64gather_epi64::<8>(
            _mm512_loadu_si512(src_i64.as_ptr().cast()),
            K8,
            _mm512_loadu_si512(idx_i64.as_ptr().cast()),
            longs.as_ptr(),
        );
        show("gather.qpq.512", value, 64);
    }
    unsafe {
        let value: __m256i = _mm512_mask_i64gather_epi32::<4>(
            _mm256_loadu_si256(src_i32.as_ptr().cast()),
            K8,
            _mm512_loadu_si512(idx_i64.as_ptr().cast()),
            ints.as_ptr(),
        );
        show("gather.qpi.512", value, 32);
    }
}

#[target_feature(enable = "avx512f,avx512vl")]
unsafe fn evex_vl_forms(ints: &[i32], longs: &[i64], floats: &[f32], doubles: &[f64]) {
    let (src_i32, src_i64, src_f32, src_f64) = sources();
    let (mask_i32, mask_i64, mask_f32, mask_f64) = sign_masks();
    let idx_i32: [i32; 16] = std::array::from_fn(|i| i as i32);
    let idx_i64: [i64; 8] = std::array::from_fn(|i| i as i64);
    unsafe {
        let value: __m128i = _mm_mmask_i32gather_epi32::<4>(
            _mm_loadu_si128(src_i32.as_ptr().cast()),
            K4,
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            ints.as_ptr(),
        );
        show("gather3siv4.si", value, 16);
    }
    unsafe {
        let value: __m128i = _mm_mmask_i32gather_epi64::<8>(
            _mm_loadu_si128(src_i64.as_ptr().cast()),
            K4,
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            longs.as_ptr(),
        );
        show("gather3siv2.di", value, 16);
    }
    unsafe {
        let value: __m128d = _mm_mmask_i32gather_pd::<8>(
            _mm_loadu_pd(src_f64.as_ptr()),
            K4,
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            doubles.as_ptr(),
        );
        show("gather3siv2.df", value, 16);
    }
    unsafe {
        let value: __m128 = _mm_mmask_i32gather_ps::<4>(
            _mm_loadu_ps(src_f32.as_ptr()),
            K4,
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            floats.as_ptr(),
        );
        show("gather3siv4.sf", value, 16);
    }
    unsafe {
        let value: __m128i = _mm_mmask_i64gather_epi32::<4>(
            _mm_loadu_si128(src_i32.as_ptr().cast()),
            K4,
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            ints.as_ptr(),
        );
        show("gather3div4.si", value, 16);
    }
    unsafe {
        let value: __m128i = _mm_mmask_i64gather_epi64::<8>(
            _mm_loadu_si128(src_i64.as_ptr().cast()),
            K4,
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            longs.as_ptr(),
        );
        show("gather3div2.di", value, 16);
    }
    unsafe {
        let value: __m128d = _mm_mmask_i64gather_pd::<8>(
            _mm_loadu_pd(src_f64.as_ptr()),
            K4,
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            doubles.as_ptr(),
        );
        show("gather3div2.df", value, 16);
    }
    unsafe {
        let value: __m128 = _mm_mmask_i64gather_ps::<4>(
            _mm_loadu_ps(src_f32.as_ptr()),
            K4,
            _mm_loadu_si128(idx_i64.as_ptr().cast()),
            floats.as_ptr(),
        );
        show("gather3div4.sf", value, 16);
    }
    unsafe {
        let value: __m256i = _mm256_mmask_i32gather_epi32::<4>(
            _mm256_loadu_si256(src_i32.as_ptr().cast()),
            K8,
            _mm256_loadu_si256(idx_i32.as_ptr().cast()),
            ints.as_ptr(),
        );
        show("gather3siv8.si", value, 32);
    }
    unsafe {
        let value: __m256i = _mm256_mmask_i32gather_epi64::<8>(
            _mm256_loadu_si256(src_i64.as_ptr().cast()),
            K8,
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            longs.as_ptr(),
        );
        show("gather3siv4.di", value, 32);
    }
    unsafe {
        let value: __m256d = _mm256_mmask_i32gather_pd::<8>(
            _mm256_loadu_pd(src_f64.as_ptr()),
            K8,
            _mm_loadu_si128(idx_i32.as_ptr().cast()),
            doubles.as_ptr(),
        );
        show("gather3siv4.df", value, 32);
    }
    unsafe {
        let value: __m256 = _mm256_mmask_i32gather_ps::<4>(
            _mm256_loadu_ps(src_f32.as_ptr()),
            K8,
            _mm256_loadu_si256(idx_i32.as_ptr().cast()),
            floats.as_ptr(),
        );
        show("gather3siv8.sf", value, 32);
    }
    unsafe {
        let value: __m128i = _mm256_mmask_i64gather_epi32::<4>(
            _mm_loadu_si128(src_i32.as_ptr().cast()),
            K8,
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            ints.as_ptr(),
        );
        show("gather3div8.si", value, 16);
    }
    unsafe {
        let value: __m256i = _mm256_mmask_i64gather_epi64::<8>(
            _mm256_loadu_si256(src_i64.as_ptr().cast()),
            K8,
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            longs.as_ptr(),
        );
        show("gather3div4.di", value, 32);
    }
    unsafe {
        let value: __m256d = _mm256_mmask_i64gather_pd::<8>(
            _mm256_loadu_pd(src_f64.as_ptr()),
            K8,
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            doubles.as_ptr(),
        );
        show("gather3div4.df", value, 32);
    }
    unsafe {
        let value: __m128 = _mm256_mmask_i64gather_ps::<4>(
            _mm_loadu_ps(src_f32.as_ptr()),
            K8,
            _mm256_loadu_si256(idx_i64.as_ptr().cast()),
            floats.as_ptr(),
        );
        show("gather3div8.sf", value, 16);
    }
}

fn main() {
    // The gathered-from buffers: `ints`/`floats` at 32-bit element scale and `longs`/`doubles` at
    // 64-bit, each large enough for every offset the forms above use.
    let ints: [i32; 32] = std::array::from_fn(|i| (i as i32).wrapping_mul(7).wrapping_sub(3));
    let longs: [i64; 32] = std::array::from_fn(|i| (i as i64).wrapping_mul(11).wrapping_add(5));
    let floats: [f32; 32] = std::array::from_fn(|i| (i as f32) * 1.5 - 4.0);
    let doubles: [f64; 32] = std::array::from_fn(|i| (i as f64) * 0.25 + 1.0);

    if std::is_x86_feature_detected!("avx2") {
        unsafe { vex_forms(&ints, &longs, &floats, &doubles) };
    } else {
        println!("avx2: unavailable");
    }
    if std::is_x86_feature_detected!("avx512f") {
        unsafe { evex_forms(&ints, &longs, &floats, &doubles) };
        if std::is_x86_feature_detected!("avx512vl") {
            unsafe { evex_vl_forms(&ints, &longs, &floats, &doubles) };
        } else {
            println!("avx512vl: unavailable");
        }
    } else {
        println!("avx512f: unavailable");
    }
}
