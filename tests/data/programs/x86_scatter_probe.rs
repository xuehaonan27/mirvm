// Permanent differential probe for the whole AVX-512 scatter family: 8 forms at 512 bits and 16 at
// 128/256 bits. A scatter's result is memory, so each line scatters into a fresh sentinel buffer and
// prints it: native on the same machine is the authority, and a lane that was written, missed, or
// written to the wrong address all look different.
//
// The probe pins the three rules a returned-vector check could not see: an excluded lane stores
// nothing at all (the sentinel survives), the offsets address memory in the order the values sit in
// their register, and a form whose index vector is shorter than the value register ignores the lanes
// past the index count — `scatterdiv4.si`/`scatterdiv4.sf` run with every mask bit set precisely to
// show that.
use std::arch::x86_64::*;

const SENTINEL: u8 = 0xaa;
const K4: __mmask8 = 0b1011;
const K4_ALL: __mmask8 = 0b1111;
const K8: __mmask8 = 0b1011_0101;
const K16: __mmask16 = 0b1010_0101_1010_0101;

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[target_feature(enable = "avx512f")]
unsafe fn scatter_512(
    idx32: &[i32],
    idx64: &[i64],
    vals_i32: &[i32],
    vals_i64: &[i64],
    vals_f32: &[f32],
    vals_f64: &[f64],
) {
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i32scatter_pd::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx32.as_ptr().cast()),
            _mm512_loadu_pd(vals_f64.as_ptr()),
        )
    };
    println!("scatter.dpd.512 = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i32scatter_ps::<4>(
            buf.as_mut_ptr().cast(),
            K16,
            _mm512_loadu_si512(idx32.as_ptr().cast()),
            _mm512_loadu_ps(vals_f32.as_ptr()),
        )
    };
    println!("scatter.dps.512 = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i64scatter_pd::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm512_loadu_si512(idx64.as_ptr().cast()),
            _mm512_loadu_pd(vals_f64.as_ptr()),
        )
    };
    println!("scatter.qpd.512 = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i64scatter_ps::<4>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm512_loadu_si512(idx64.as_ptr().cast()),
            _mm256_loadu_ps(vals_f32.as_ptr()),
        )
    };
    println!("scatter.qps.512 = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i32scatter_epi64::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx32.as_ptr().cast()),
            _mm512_loadu_si512(vals_i64.as_ptr().cast()),
        )
    };
    println!("scatter.dpq.512 = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i32scatter_epi32::<4>(
            buf.as_mut_ptr().cast(),
            K16,
            _mm512_loadu_si512(idx32.as_ptr().cast()),
            _mm512_loadu_si512(vals_i32.as_ptr().cast()),
        )
    };
    println!("scatter.dpi.512 = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i64scatter_epi64::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm512_loadu_si512(idx64.as_ptr().cast()),
            _mm512_loadu_si512(vals_i64.as_ptr().cast()),
        )
    };
    println!("scatter.qpq.512 = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm512_mask_i64scatter_epi32::<4>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm512_loadu_si512(idx64.as_ptr().cast()),
            _mm256_loadu_si256(vals_i32.as_ptr().cast()),
        )
    };
    println!("scatter.qpi.512 = {}", hex(&buf[..128]));
}
#[target_feature(enable = "avx512f,avx512vl")]
unsafe fn scatter_128(
    idx32: &[i32],
    idx64: &[i64],
    vals_i32: &[i32],
    vals_i64: &[i64],
    vals_f32: &[f32],
    vals_f64: &[f64],
) {
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i32scatter_epi32::<4>(
            buf.as_mut_ptr().cast(),
            K4,
            _mm_loadu_si128(idx32.as_ptr().cast()),
            _mm_loadu_si128(vals_i32.as_ptr().cast()),
        )
    };
    println!("scattersiv4.si = {}", hex(&buf[..64]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i32scatter_epi64::<8>(
            buf.as_mut_ptr().cast(),
            K4,
            _mm_loadu_si128(idx32.as_ptr().cast()),
            _mm_loadu_si128(vals_i64.as_ptr().cast()),
        )
    };
    println!("scattersiv2.di = {}", hex(&buf[..64]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i32scatter_pd::<8>(
            buf.as_mut_ptr().cast(),
            K4,
            _mm_loadu_si128(idx32.as_ptr().cast()),
            _mm_loadu_pd(vals_f64.as_ptr()),
        )
    };
    println!("scattersiv2.df = {}", hex(&buf[..64]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i32scatter_ps::<4>(
            buf.as_mut_ptr().cast(),
            K4,
            _mm_loadu_si128(idx32.as_ptr().cast()),
            _mm_loadu_ps(vals_f32.as_ptr()),
        )
    };
    println!("scattersiv4.sf = {}", hex(&buf[..64]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i64scatter_epi32::<4>(
            buf.as_mut_ptr().cast(),
            K4_ALL,
            _mm_loadu_si128(idx64.as_ptr().cast()),
            _mm_loadu_si128(vals_i32.as_ptr().cast()),
        )
    };
    println!("scatterdiv4.si = {}", hex(&buf[..64]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i64scatter_epi64::<8>(
            buf.as_mut_ptr().cast(),
            K4,
            _mm_loadu_si128(idx64.as_ptr().cast()),
            _mm_loadu_si128(vals_i64.as_ptr().cast()),
        )
    };
    println!("scatterdiv2.di = {}", hex(&buf[..64]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i64scatter_pd::<8>(
            buf.as_mut_ptr().cast(),
            K4,
            _mm_loadu_si128(idx64.as_ptr().cast()),
            _mm_loadu_pd(vals_f64.as_ptr()),
        )
    };
    println!("scatterdiv2.df = {}", hex(&buf[..64]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm_mask_i64scatter_ps::<4>(
            buf.as_mut_ptr().cast(),
            K4_ALL,
            _mm_loadu_si128(idx64.as_ptr().cast()),
            _mm_loadu_ps(vals_f32.as_ptr()),
        )
    };
    println!("scatterdiv4.sf = {}", hex(&buf[..64]));
}
#[target_feature(enable = "avx512f,avx512vl")]
unsafe fn scatter_256(
    idx32: &[i32],
    idx64: &[i64],
    vals_i32: &[i32],
    vals_i64: &[i64],
    vals_f32: &[f32],
    vals_f64: &[f64],
) {
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i32scatter_epi32::<4>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx32.as_ptr().cast()),
            _mm256_loadu_si256(vals_i32.as_ptr().cast()),
        )
    };
    println!("scattersiv8.si = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i32scatter_epi64::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm_loadu_si128(idx32.as_ptr().cast()),
            _mm256_loadu_si256(vals_i64.as_ptr().cast()),
        )
    };
    println!("scattersiv4.di = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i32scatter_pd::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm_loadu_si128(idx32.as_ptr().cast()),
            _mm256_loadu_pd(vals_f64.as_ptr()),
        )
    };
    println!("scattersiv4.df = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i32scatter_ps::<4>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx32.as_ptr().cast()),
            _mm256_loadu_ps(vals_f32.as_ptr()),
        )
    };
    println!("scattersiv8.sf = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i64scatter_epi32::<4>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx64.as_ptr().cast()),
            _mm_loadu_si128(vals_i32.as_ptr().cast()),
        )
    };
    println!("scatterdiv8.si = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i64scatter_epi64::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx64.as_ptr().cast()),
            _mm256_loadu_si256(vals_i64.as_ptr().cast()),
        )
    };
    println!("scatterdiv4.di = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i64scatter_pd::<8>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx64.as_ptr().cast()),
            _mm256_loadu_pd(vals_f64.as_ptr()),
        )
    };
    println!("scatterdiv4.df = {}", hex(&buf[..128]));
    let mut buf = [SENTINEL; 256];
    unsafe {
        _mm256_mask_i64scatter_ps::<4>(
            buf.as_mut_ptr().cast(),
            K8,
            _mm256_loadu_si256(idx64.as_ptr().cast()),
            _mm_loadu_ps(vals_f32.as_ptr()),
        )
    };
    println!("scatterdiv8.sf = {}", hex(&buf[..128]));
}

fn main() {
    // Offsets are small and distinct, so every store lands inside the printed prefix and a store to
    // the wrong lane is visible by position as well as by value.
    let idx32: [i32; 16] = std::array::from_fn(|i| i as i32);
    let idx64: [i64; 8] = std::array::from_fn(|i| i as i64);
    let vals_i32: [i32; 16] = std::array::from_fn(|i| 0x1111_1111u32.wrapping_add(i as u32) as i32);
    let vals_i64: [i64; 8] =
        std::array::from_fn(|i| 0x2222_2222_2222_2222u64.wrapping_add(i as u64) as i64);
    let vals_f32: [f32; 16] = std::array::from_fn(|i| 1.5 + i as f32);
    let vals_f64: [f64; 8] = std::array::from_fn(|i| 2.5 + i as f64);

    if !std::is_x86_feature_detected!("avx512f") {
        println!("avx512f: unavailable");
        return;
    }
    unsafe {
        scatter_512(&idx32, &idx64, &vals_i32, &vals_i64, &vals_f32, &vals_f64);
        if std::is_x86_feature_detected!("avx512vl") {
            scatter_128(&idx32, &idx64, &vals_i32, &vals_i64, &vals_f32, &vals_f64);
            scatter_256(&idx32, &idx64, &vals_i32, &vals_i64, &vals_f32, &vals_f64);
        } else {
            println!("avx512vl: unavailable");
        }
    }
}
