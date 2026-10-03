// Permanent differential probe for the wider x86 crypto forms: each 256/512-bit VAES round and each
// wider carryless multiply must be the 128-bit operation applied per 128-bit lane. The lane-by-lane
// 128-bit results are printed beside the wide ones so the equality is visible in the output, and
// native on the same machine is the authority for every value. Nothing here is a known-answer test:
// the two legs run the same source on the same CPU and must agree bit-for-bit.
//
// A form whose CPU feature is absent prints one marker line instead, and because both legs see the
// same CPU that line is identical on both sides rather than a reason to skip.
use std::arch::x86_64::*;

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// SAFETY: each function's own `#[target_feature]` is what the caller checks for with
// `is_x86_feature_detected!`; that check is the precondition of the call.
#[target_feature(enable = "aes")]
unsafe fn aes_128_forms(a: &[u8; 64], k: &[u8; 64]) -> [[u8; 64]; 4] {
    let mut out = [[0u8; 64]; 4];
    for lane in 0..4 {
        let (va, vk) = unsafe {
            (
                _mm_loadu_si128(a.as_ptr().add(lane * 16).cast()),
                _mm_loadu_si128(k.as_ptr().add(lane * 16).cast()),
            )
        };
        let values = [
            _mm_aesenc_si128(va, vk),
            _mm_aesenclast_si128(va, vk),
            _mm_aesdec_si128(va, vk),
            _mm_aesdeclast_si128(va, vk),
        ];
        for form in 0..4 {
            unsafe { _mm_storeu_si128(out[form].as_mut_ptr().add(lane * 16).cast(), values[form]) };
        }
    }
    out
}

#[target_feature(enable = "vaes")]
unsafe fn aes_256_forms(a: &[u8; 64], k: &[u8; 64]) -> [[u8; 32]; 4] {
    let (va, vk) = unsafe {
        (
            _mm256_loadu_si256(a.as_ptr().cast()),
            _mm256_loadu_si256(k.as_ptr().cast()),
        )
    };
    let values = [
        _mm256_aesenc_epi128(va, vk),
        _mm256_aesenclast_epi128(va, vk),
        _mm256_aesdec_epi128(va, vk),
        _mm256_aesdeclast_epi128(va, vk),
    ];
    let mut out = [[0u8; 32]; 4];
    for form in 0..4 {
        unsafe { _mm256_storeu_si256(out[form].as_mut_ptr().cast(), values[form]) };
    }
    out
}

#[target_feature(enable = "vaes,avx512f")]
unsafe fn aes_512_forms(a: &[u8; 64], k: &[u8; 64]) -> [[u8; 64]; 4] {
    let (va, vk) = unsafe {
        (
            _mm512_loadu_si512(a.as_ptr().cast()),
            _mm512_loadu_si512(k.as_ptr().cast()),
        )
    };
    let values = [
        _mm512_aesenc_epi128(va, vk),
        _mm512_aesenclast_epi128(va, vk),
        _mm512_aesdec_epi128(va, vk),
        _mm512_aesdeclast_epi128(va, vk),
    ];
    let mut out = [[0u8; 64]; 4];
    for form in 0..4 {
        unsafe { _mm512_storeu_si512(out[form].as_mut_ptr().cast(), values[form]) };
    }
    out
}

#[target_feature(enable = "pclmulqdq")]
unsafe fn clmul_128(a: &[u8; 64], k: &[u8; 64], imm: u32) -> [u8; 64] {
    let mut out = [0u8; 64];
    for lane in 0..4 {
        let (va, vb) = unsafe {
            (
                _mm_loadu_si128(a.as_ptr().add(lane * 16).cast()),
                _mm_loadu_si128(k.as_ptr().add(lane * 16).cast()),
            )
        };
        let value = match imm & 0x11 {
            0x00 => _mm_clmulepi64_si128::<0x00>(va, vb),
            0x01 => _mm_clmulepi64_si128::<0x01>(va, vb),
            0x10 => _mm_clmulepi64_si128::<0x10>(va, vb),
            _ => _mm_clmulepi64_si128::<0x11>(va, vb),
        };
        unsafe { _mm_storeu_si128(out.as_mut_ptr().add(lane * 16).cast(), value) };
    }
    out
}

#[target_feature(enable = "vpclmulqdq,avx512f,avx512vl")]
unsafe fn clmul_256(a: &[u8; 64], k: &[u8; 64], imm: u32) -> [u8; 32] {
    let (va, vb) = unsafe {
        (
            _mm256_loadu_si256(a.as_ptr().cast()),
            _mm256_loadu_si256(k.as_ptr().cast()),
        )
    };
    let value = match imm & 0x11 {
        0x00 => _mm256_clmulepi64_epi128::<0x00>(va, vb),
        0x01 => _mm256_clmulepi64_epi128::<0x01>(va, vb),
        0x10 => _mm256_clmulepi64_epi128::<0x10>(va, vb),
        _ => _mm256_clmulepi64_epi128::<0x11>(va, vb),
    };
    let mut out = [0u8; 32];
    unsafe { _mm256_storeu_si256(out.as_mut_ptr().cast(), value) };
    out
}

#[target_feature(enable = "vpclmulqdq,avx512f")]
unsafe fn clmul_512(a: &[u8; 64], k: &[u8; 64], imm: u32) -> [u8; 64] {
    let (va, vb) = unsafe {
        (
            _mm512_loadu_si512(a.as_ptr().cast()),
            _mm512_loadu_si512(k.as_ptr().cast()),
        )
    };
    let value = match imm & 0x11 {
        0x00 => _mm512_clmulepi64_epi128::<0x00>(va, vb),
        0x01 => _mm512_clmulepi64_epi128::<0x01>(va, vb),
        0x10 => _mm512_clmulepi64_epi128::<0x10>(va, vb),
        _ => _mm512_clmulepi64_epi128::<0x11>(va, vb),
    };
    let mut out = [0u8; 64];
    unsafe { _mm512_storeu_si512(out.as_mut_ptr().cast(), value) };
    out
}

fn main() {
    let a: [u8; 64] = std::array::from_fn(|i| (i as u8).wrapping_mul(29).wrapping_add(5));
    let k: [u8; 64] = std::array::from_fn(|i| (i as u8).wrapping_mul(17).wrapping_add(201));
    println!("a = {}", hex(&a));
    println!("k = {}", hex(&k));

    if !is_x86_feature_detected!("aes") {
        println!("aes: unavailable");
    } else {
        let lanes = unsafe { aes_128_forms(&a, &k) };
        for form in 0..4 {
            println!("aes.form{form}.128 = {}", hex(&lanes[form]));
        }
        if is_x86_feature_detected!("vaes") {
            let wide = unsafe { aes_256_forms(&a, &k) };
            for form in 0..4 {
                println!("aes.form{form}.256 = {}", hex(&wide[form]));
            }
        } else {
            println!("aes.256: unavailable");
        }
        if is_x86_feature_detected!("vaes") && is_x86_feature_detected!("avx512f") {
            let wide = unsafe { aes_512_forms(&a, &k) };
            for form in 0..4 {
                println!("aes.form{form}.512 = {}", hex(&wide[form]));
            }
        } else {
            println!("aes.512: unavailable");
        }
    }

    if !is_x86_feature_detected!("pclmulqdq") {
        println!("pclmulqdq: unavailable");
        return;
    }
    for imm in [0x00u32, 0x01, 0x10, 0x11] {
        println!(
            "clmul.128.{imm:02x} = {}",
            hex(&unsafe { clmul_128(&a, &k, imm) })
        );
    }
    if is_x86_feature_detected!("vpclmulqdq")
        && is_x86_feature_detected!("avx512f")
        && is_x86_feature_detected!("avx512vl")
    {
        for imm in [0x00u32, 0x01, 0x10, 0x11] {
            println!(
                "clmul.256.{imm:02x} = {}",
                hex(&unsafe { clmul_256(&a, &k, imm) })
            );
        }
    } else {
        println!("clmul.256: unavailable");
    }
    if is_x86_feature_detected!("vpclmulqdq") && is_x86_feature_detected!("avx512f") {
        for imm in [0x00u32, 0x01, 0x10, 0x11] {
            println!(
                "clmul.512.{imm:02x} = {}",
                hex(&unsafe { clmul_512(&a, &k, imm) })
            );
        }
    } else {
        println!("clmul.512: unavailable");
    }
}
