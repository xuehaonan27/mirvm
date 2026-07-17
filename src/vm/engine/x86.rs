//! x86_64 hardware intrinsic helpers used by the tcx-free engine.
//!
//! These functions execute the real host instruction behind an `llvm.x86.*` boundary. The guest
//! and host CPU are deliberately the same virtual CPU; callers only reach feature-specific code
//! after the guest's normal CPUID dispatch selected it.

use std::arch::x86_64::{
    __m128i, __m256i, _mm_aesdec_si128, _mm_aesdeclast_si128, _mm_aesenc_si128,
    _mm_aesenclast_si128, _mm_aesimc_si128, _mm_clmulepi64_si128, _mm_crc32_u8,
    _mm_crc32_u16, _mm_crc32_u32, _mm_crc32_u64, _mm_loadu_si128, _mm_madd_epi16,
    _mm_maddubs_epi16, _mm_sad_epu8, _mm_sha256msg1_epu32, _mm_sha256msg2_epu32,
    _mm_sha256rnds2_epu32, _mm_shuffle_epi8, _mm_storeu_si128, _mm256_loadu_si256,
    _mm256_madd_epi16, _mm256_maddubs_epi16, _mm256_permutevar8x32_epi32, _mm256_sad_epu8,
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

/// SSE2 `psadbw`：两组 8 字节绝对差和，以 u64 落 qword lane 0/1（其余位清零）。
#[target_feature(enable = "sse2")]
pub(super) unsafe fn psad_bw128(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_sad_epu8(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// AVX2 `vpsadbw`：每 128 位 lane 独立，共 4 个 u64 和。
#[target_feature(enable = "avx2")]
pub(super) unsafe fn psad_bw256(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let b = unsafe { _mm256_loadu_si256(b.cast::<__m256i>()) };
    let result = _mm256_sad_epu8(a, b);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// PCLMULQDQ：imm8 bit0/bit4 各选 a/b 的 qword 做无进位乘法；imm 其余位硬件
/// 忽略（`imm & 0x11` 同构）。imm 是运行时参数，按 4 种合法组合分派 const generic。
#[target_feature(enable = "pclmulqdq")]
pub(super) unsafe fn pclmulqdq(dst: *mut u8, a: *const u8, b: *const u8, imm: u64) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = match imm & 0x11 {
        0x00 => _mm_clmulepi64_si128::<0x00>(a, b),
        0x01 => _mm_clmulepi64_si128::<0x01>(a, b),
        0x10 => _mm_clmulepi64_si128::<0x10>(a, b),
        _ => _mm_clmulepi64_si128::<0x11>(a, b),
    };
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(super) unsafe fn aesenc(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesenc_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(super) unsafe fn aesenclast(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesenclast_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(super) unsafe fn aesdec(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesdec_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(super) unsafe fn aesdeclast(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesdeclast_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(super) unsafe fn aesimc(dst: *mut u8, a: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let result = _mm_aesimc_si128(a);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// Rijndael S-box（`aeskeygenassist` 软件模型的 SubWord 用）。
const AES_SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab,
    0x76, 0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4,
    0x72, 0xc0, 0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71,
    0xd8, 0x31, 0x15, 0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2,
    0xeb, 0x27, 0xb2, 0x75, 0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6,
    0xb3, 0x29, 0xe3, 0x2f, 0x84, 0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb,
    0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf, 0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45,
    0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8, 0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5,
    0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2, 0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44,
    0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73, 0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a,
    0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb, 0xe0, 0x32, 0x3a, 0x0a, 0x49,
    0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79, 0xe7, 0xc8, 0x37, 0x6d,
    0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08, 0xba, 0x78, 0x25,
    0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a, 0x70, 0x3e,
    0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e, 0xe1,
    0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb,
    0x16,
];

/// AESKEYGENASSIST 的软件模型。imm8 是运行时参数（const generic 覆盖不了 256
/// 值）；逐位复刻 SDM：X1/X3 = src 的 dword 1/3，
/// dst = [SubWord(X1), RotWord(SubWord(X1))⊕imm8, SubWord(X3),
/// RotWord(SubWord(X3))⊕imm8]（RotWord：字节序 [b0,b1,b2,b3]→[b1,b2,b3,b0]，
/// 即 u32 循环右移 8）。unit test 与硬件 `aeskeygenassist` 对拍多个 imm 保证
/// 逐位一致。
pub(super) unsafe fn aeskeygenassist(dst: *mut u8, a: *const u8, imm: u64) {
    let sub_word = |w: u32| {
        let mut r = 0u32;
        for k in 0..4 {
            r |= u32::from(AES_SBOX[(w >> (k * 8)) as u8 as usize]) << (k * 8);
        }
        r
    };
    let (x1, x3) = unsafe {
        (
            (a as *const u32).add(1).read_unaligned(),
            (a as *const u32).add(3).read_unaligned(),
        )
    };
    let rcon = u32::from(imm as u8);
    let out = [
        sub_word(x1),
        sub_word(x1).rotate_right(8) ^ rcon,
        sub_word(x3),
        sub_word(x3).rotate_right(8) ^ rcon,
    ];
    unsafe { (dst as *mut [u32; 4]).write_unaligned(out) };
}

/// SSE4.2 CRC32（CRC32C 硬件语义，无首尾取反——包装层负责）。标量通道。
#[target_feature(enable = "sse4.2")]
pub(super) unsafe fn crc32_u8(crc: u32, v: u8) -> u32 {
    _mm_crc32_u8(crc, v)
}

#[target_feature(enable = "sse4.2")]
pub(super) unsafe fn crc32_u16(crc: u32, v: u16) -> u32 {
    _mm_crc32_u16(crc, v)
}

#[target_feature(enable = "sse4.2")]
pub(super) unsafe fn crc32_u32(crc: u32, v: u32) -> u32 {
    _mm_crc32_u32(crc, v)
}

#[target_feature(enable = "sse4.2")]
pub(super) unsafe fn crc32_u64(crc: u64, v: u64) -> u64 {
    _mm_crc32_u64(crc, v)
}

/// AVX2 `vpermd`：dst.dword[i] = a.dword[idx.dword[i] & 7]（跨 lane）。
#[target_feature(enable = "avx2")]
pub(super) unsafe fn permd256(dst: *mut u8, a: *const u8, idx: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let idx = unsafe { _mm256_loadu_si256(idx.cast::<__m256i>()) };
    let result = _mm256_permutevar8x32_epi32(a, idx);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// SSSE3 `pmaddubsw`：a 无符号字节 × b 有符号字节，相邻两积之和饱和到 i16。
#[target_feature(enable = "ssse3")]
pub(super) unsafe fn pmaddubsw128(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_maddubs_epi16(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// AVX2 `vpmaddubsw`：每 128 位 lane 独立。
#[target_feature(enable = "avx2")]
pub(super) unsafe fn pmaddubsw256(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let b = unsafe { _mm256_loadu_si256(b.cast::<__m256i>()) };
    let result = _mm256_maddubs_epi16(a, b);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// SSE2 `pmaddwd`：相邻 i16 对积之和放 i32。
#[target_feature(enable = "sse2")]
pub(super) unsafe fn pmaddwd128(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_madd_epi16(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// LDDQU 族（`llvm.x86.sse3.ldu.dq` / `llvm.x86.avx.ldu.dq.256`；
/// `_mm_lddqu_si128` / `_mm256_lddqu_si256`）：语义 = 普通非对齐 16/32 字节
/// load（与 loadu 逐位同义——lddqu 的未缓存跨行微优化提示在本模型无影响）。
/// corpus 批8 c_tantivy 实锤补建（bitpacking avx2/termdict 列值读取派发点）。
pub(super) unsafe fn lddqu<const W: usize>(dst: *mut u8, src: *const u8) {
    unsafe { std::ptr::copy_nonoverlapping(src, dst, W) };
}

/// AVX2 `vpmaddwd`：每 128 位 lane 独立。
#[target_feature(enable = "avx2")]

pub(super) unsafe fn pmaddwd256(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let b = unsafe { _mm256_loadu_si256(b.cast::<__m256i>()) };
    let result = _mm256_madd_epi16(a, b);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// `vgatherqpd` 的软件模型（`llvm.x86.avx2.gather.q.pd.256`）：mask lane 符号位
/// 置位才读 `base + vindex*scale`（f64），否则拷 src lane。**mask 关闭的 lane
/// 绝不触内存**（fault suppression——野索引 lane 被 mask 时硬件同样不读）。
/// 地址按 64 位回绕算术（硬件同）。scale 合法值为 1/2/4/8（LLVM 发射已约束）。
pub(super) unsafe fn gather_q_pd_256(
    dst: *mut u8,
    src: *const u8,
    base: u64,
    vindex: *const u8,
    mask: *const u8,
    scale: u64,
) {
    for i in 0..4 {
        let mask_lane = unsafe { (mask as *const u64).add(i).read_unaligned() };
        let lane = if mask_lane as i64 >= 0 {
            unsafe { (src as *const u64).add(i).read_unaligned() }
        } else {
            let idx = unsafe { (vindex as *const i64).add(i).read_unaligned() };
            let addr = base.wrapping_add((idx as u64).wrapping_mul(scale));
            unsafe { (addr as *const u64).read_unaligned() }
        };
        unsafe { (dst as *mut u64).add(i).write_unaligned(lane) };
    }
}

/// `vgatherdpd`（256 位 form）的软件模型（`llvm.x86.avx2.gather.d.pd.256`）：
/// 与 q 版同形，但索引是 4×i32，参与地址算术前符号扩展到 64 位。
pub(super) unsafe fn gather_d_pd_256(
    dst: *mut u8,
    src: *const u8,
    base: u64,
    vindex: *const u8,
    mask: *const u8,
    scale: u64,
) {
    for i in 0..4 {
        let mask_lane = unsafe { (mask as *const u64).add(i).read_unaligned() };
        let lane = if mask_lane as i64 >= 0 {
            unsafe { (src as *const u64).add(i).read_unaligned() }
        } else {
            let idx = unsafe { (vindex as *const i32).add(i).read_unaligned() };
            let addr = base.wrapping_add((idx as i64 as u64).wrapping_mul(scale));
            unsafe { (addr as *const u64).read_unaligned() }
        };
        unsafe { (dst as *mut u64).add(i).write_unaligned(lane) };
    }
}

/// AVX512IFMA `vpmadd52l/h.uq` 的软件模型：dst.qword[i] =
/// a[i] + (b[i][51:0] × c[i][51:0]) 的 bit[51:0]（LO）或 bit[103:52]（HI），
/// 加法按 64 位回绕。52×52 → 104 位中间积用 u128 精确承载。unit test 与硬件
/// `_mm*_madd52lo/hi_epu64` 对拍（含高位污染输入，钉死输入掩码语义）。
pub(super) unsafe fn vpmadd52<const LANES: usize, const HI: bool>(
    dst: *mut u8,
    a: *const u8,
    b: *const u8,
    c: *const u8,
) {
    const MASK52: u64 = (1u64 << 52) - 1;
    for i in 0..LANES {
        let acc = unsafe { (a as *const u64).add(i).read_unaligned() };
        let x = u128::from(unsafe { (b as *const u64).add(i).read_unaligned() } & MASK52);
        let y = u128::from(unsafe { (c as *const u64).add(i).read_unaligned() } & MASK52);
        let inter = x * y;
        let add = if HI {
            ((inter >> 52) as u64) & MASK52
        } else {
            (inter as u64) & MASK52
        };
        unsafe {
            (dst as *mut u64).add(i).write_unaligned(acc.wrapping_add(add));
        }
    }
}

// ===== packed-f32 lane 软件模型（族⑨：tiny-skia simd 默认路径）=====
// 全部可精确模型化：`if a > b { a } else { b }` 等 Rust 标量运算与硬件指令同位
// 结果（unordered/±0/NaN 位透传皆同——unit test 逐一对拍 `_mm_*`/`_mm256_*`）。

/// MAXPS/MINPS 逐 lane：`max ? a>b : a<b` 真取 a、否则取 b（unordered → 第二源
/// b；±0 相等 → b；NaN 位透传——对拍确认与 Rust 比较同构）。
pub(super) unsafe fn maxmin_ps<const LANES: usize, const MAX: bool>(
    dst: *mut u8,
    a: *const u8,
    b: *const u8,
) {
    for i in 0..LANES {
        let x = unsafe { (a as *const f32).add(i).read_unaligned() };
        let y = unsafe { (b as *const f32).add(i).read_unaligned() };
        let r = if MAX {
            if x > y { x } else { y }
        } else if x < y {
            x
        } else {
            y
        };
        unsafe { (dst as *mut f32).add(i).write_unaligned(r) };
    }
}

/// CMPPS/VCMPPS 全 32 谓词（S/Q 后缀只差异常旗标，值位相同 → 按值对拍一起）。
/// imm = SDM imm8：真 lane 写 0xFFFF_FFFF，假写 0。
pub(super) unsafe fn cmp_ps<const LANES: usize>(dst: *mut u8, a: *const u8, b: *const u8, imm: u64) {
    for i in 0..LANES {
        let x = unsafe { (a as *const f32).add(i).read_unaligned() };
        let y = unsafe { (b as *const f32).add(i).read_unaligned() };
        let un = x.is_nan() || y.is_nan();
        let r = match imm & 0x1f {
            0 | 16 => x == y,              // EQ_OQ / EQ_OS
            1 | 17 => x < y,               // LT_OS / LT_OQ
            2 | 18 => x <= y,              // LE_OS / LE_OQ
            3 | 19 => un,                  // UNORD_Q / UNORD_S
            4 | 20 => !(!un && x == y),    // NEQ_UQ / NEQ_US
            5 | 21 => !(x < y),            // NLT_US / NLT_UQ
            6 | 22 => !(x <= y),           // NLE_US / NLE_UQ
            7 | 23 => !un,                 // ORD_Q / ORD_S
            8 | 24 => un || x == y,        // EQ_UQ / EQ_US
            9 | 25 => !(x >= y),           // NGE_US / NGE_UQ
            10 | 26 => !(x > y),           // NGT_US / NGT_UQ
            11 | 27 => false,              // FALSE_OQ / FALSE_OS
            12 | 28 => !un && x != y,      // NEQ_OQ / NEQ_OS
            13 | 29 => !un && x >= y,      // GE_OS / GE_OQ
            14 | 30 => !un && x > y,       // GT_OS / GT_OQ
            _ => true,                     // 15|31: TRUE_UQ / TRUE_US
        };
        let m = if r { u32::MAX } else { 0 };
        unsafe { (dst as *mut u32).add(i).write_unaligned(m) };
    }
}

/// ROUNDPS：imm[3:0]：bit2=0 → imm[1:0] 舍入（0=RNE/1=floor/2=ceil/3=trunc）；
/// bit2=1 → MXCSR.RC（引擎恒宿默认 RNE）；bit3 只抑制异常旗标，与值位无关。
/// NaN：硬件语义 = 载荷保留 + qbit 强置——显式臂实现（不显式置信性 libm/
/// roundss 的 NaN 位行为，后者随宿主构建目标特征漂移）。
pub(super) unsafe fn round_ps<const LANES: usize>(dst: *mut u8, a: *const u8, imm: u64) {
    for i in 0..LANES {
        let x = unsafe { (a as *const f32).add(i).read_unaligned() };
        let r = if x.is_nan() {
            f32::from_bits(x.to_bits() | 0x0040_0000)
        } else {
            match imm & 7 {
                1 => x.floor(),
                2 => x.ceil(),
                3 => x.trunc(),
                // 0 或 bit2=1（MXCSR，默认 RNE）
                _ => x.round_ties_even(),
            }
        };
        unsafe { (dst as *mut f32).add(i).write_unaligned(r) };
    }
}

/// CVTPS2DQ/CVTTPS2DQ：取整（RNE 或截断）后 i32；NaN/越界（含 2^31 边界）/±inf
/// → 0x80000000（indefinite，对拍钉死）。取整在 f32 域完成（精确）再饱和检查。
pub(super) unsafe fn cvt_ps2dq<const LANES: usize, const TRUNC: bool>(dst: *mut u8, a: *const u8) {
    for i in 0..LANES {
        let x = unsafe { (a as *const f32).add(i).read_unaligned() };
        let r = if TRUNC { x.trunc() } else { x.round_ties_even() };
        let out = if r.is_nan() || r < -2_147_483_648.0 || r >= 2_147_483_648.0 {
            i32::MIN
        } else {
            r as i32
        };
        unsafe { (dst as *mut i32).add(i).write_unaligned(out) };
    }
}

/// BLENDVPS：mask lane 符号位置位取 b、清零取 a（纯位选择）。
pub(super) unsafe fn blendv_ps<const LANES: usize>(
    dst: *mut u8,
    a: *const u8,
    b: *const u8,
    mask: *const u8,
) {
    for i in 0..LANES {
        let m = unsafe { (mask as *const u32).add(i).read_unaligned() };
        let src = if m as i32 >= 0 { a } else { b };
        let v = unsafe { (src as *const u32).add(i).read_unaligned() };
        unsafe { (dst as *mut u32).add(i).write_unaligned(v) };
    }
}

/// PSLL/PSRL.d 软件模型：count = count 操作数低 64 位（高位字节忽略）；
/// count > 31 → 全零 lane（SDM）。逻辑移位，位进出精确。
pub(super) unsafe fn pshift32<const LANES: usize, const LEFT: bool>(
    dst: *mut u8,
    a: *const u8,
    count: *const u8,
) {
    let c = unsafe { (count as *const u64).read_unaligned() };
    for i in 0..LANES {
        let x = unsafe { (a as *const u32).add(i).read_unaligned() };
        let r = if c > 31 {
            0
        } else {
            let sh = c as u32;
            if LEFT { x << sh } else { x >> sh }
        };
        unsafe { (dst as *mut u32).add(i).write_unaligned(r) };
    }
}

// ===== f16 ↔ f32 软件模型（族⑧ F16C VCVTPS2PH/VCVTPH2PS）=====

/// VCVTPS2PH 舍入模式（imm[1:0]；imm[2]=1 时 = MXCSR.RC，引擎恒宿默认 RNE）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HalfRound {
    Rne,
    Down,
    Up,
    Trunc,
}

/// f32（位型）→ f16（位型），VCVTPS2PH 精确语义：次正规/溢出/四种舍入模式精确；
/// NaN → qbit 强置 + 载荷右移 13 位截断（`7f800001→7e00` 型，对拍钉死）。
pub(super) fn f32_to_f16_sw(bits: u32, mode: HalfRound) -> u16 {
    let sign = ((bits >> 31) as u16) << 15;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x007f_ffff;
    if exp == 0xff {
        if mant != 0 {
            // NaN：qbit 强置 + 载荷高位截断（低 13 位丢弃——硬件不折进 bit0）
            return sign | 0x7e00 | ((mant >> 13) as u16);
        }
        return sign | 0x7c00; // ±inf → ±inf
    }
    if exp == 0 && mant == 0 {
        return sign; // ±0 → ±0（任何舍入模式）
    }
    // 数值 = mant_full × 2^(e-23)；f32 次正规 exp=0 ⇒ mant_full=mant, e=-126
    let (mant_full, e) = if exp == 0 {
        (mant, -126)
    } else {
        (mant | 0x0080_0000, exp - 127)
    };
    // 定向舍入折进幅值域：away = (Up && 非负) || (Down && 负)；Rne/Trunc 无关符号
    let away = matches!(
        (mode, sign != 0),
        (HalfRound::Up, false) | (HalfRound::Down, true)
    );
    if e > 15 {
        // 幅值 ≥ 2^16（RNE 下也必 >65520 沸点）：away/Rne → inf，否则最大正规
        return if matches!(mode, HalfRound::Rne) || away {
            sign | 0x7c00
        } else {
            sign | 0x7bff
        };
    }
    if e >= -14 {
        // 正规道：留 11 位（含隐藏位），丢 13 位
        let keep = mant_full >> 13;
        let dropped = mant_full & 0x1fff;
        let inc = match mode {
            HalfRound::Rne => dropped > 0x1000 || (dropped == 0x1000 && keep & 1 == 1),
            HalfRound::Up | HalfRound::Down => away && dropped != 0,
            HalfRound::Trunc => false,
        };
        let keep = keep + u32::from(inc);
        // 尾数进位上推指数（含 RNE 的 65520→inf 沸点）
        let (e16, mhi) = if keep == 0x800 { (e + 1, 0x400u32) } else { (e, keep) };
        if e16 > 15 {
            return if matches!(mode, HalfRound::Rne) || away {
                sign | 0x7c00
            } else {
                sign | 0x7bff
            };
        }
        return sign | (((e16 + 15) as u16) << 10) | ((mhi & 0x3ff) as u16);
    }
    // 次正规道：结果码即幅值以 2^-24 为 LSB 的整数（码 0x400 = 最小正规，无缝衔接）
    if e < -25 {
        // 低于半 LSB：Rne/Trunc → ±0；away → 1 个 LSB
        return sign | u16::from(away);
    }
    let shift = (-e - 1) as u32; // 14..=24
    let keep = mant_full >> shift;
    let guard = (mant_full >> (shift - 1)) & 1;
    let sticky = mant_full & ((1u32 << (shift - 1)) - 1);
    let inc = match mode {
        HalfRound::Rne => guard == 1 && (sticky != 0 || keep & 1 == 1),
        HalfRound::Up | HalfRound::Down => away && (guard == 1 || sticky != 0),
        HalfRound::Trunc => false,
    };
    sign | (keep + u32::from(inc)) as u16
}

/// f16（位型）→ f32（位型），VCVTPH2PS 精确展开：次正规精确规格化；
/// NaN → qbit 强置 + 载荷左移 13 位（对拍钉死）。
pub(super) fn f16_to_f32_sw(bits: u16) -> u32 {
    let bits = u32::from(bits);
    let sign = (bits & 0x8000) << 16;
    let exp = (bits >> 10) & 0x1f;
    let mant = bits & 0x03ff;
    if exp == 0x1f {
        if mant != 0 {
            // NaN：qbit 强置（f32 bit22）+ 载荷左移 13 位
            return sign | 0x7fc0_0000 | (mant << 13);
        }
        return sign | 0x7f80_0000; // ±inf → ±inf
    }
    if exp == 0 {
        if mant == 0 {
            return sign; // ±0 → ±0
        }
        // 次正规 → 正规规格化（值恒可精确表示）
        let mut m = mant;
        let mut e: i32 = -14;
        while m & 0x400 == 0 {
            m <<= 1;
            e -= 1;
        }
        m &= 0x3ff;
        return sign | (((e + 127) as u32) << 23) | (m << 13);
    }
    sign | (((exp as i32 - 15 + 127) as u32) << 23) | (mant << 13)
}

/// VCVTPS2PH：f32 lanes → f16 打包进输出低 LANES×2 字节，其余清零
/// （.128：LANES=4，输出 16 字节的低 8；.256：LANES=8，输出 16 字节整体）。
pub(super) unsafe fn cvtps2ph<const LANES: usize>(dst: *mut u8, a: *const u8, imm: u64) {
    let mode = match imm & 7 {
        // bit2=1（imm&4）→ MXCSR.RC，引擎恒宿默认 = RNE
        1 => HalfRound::Down,
        2 => HalfRound::Up,
        3 => HalfRound::Trunc,
        4..=7 => HalfRound::Rne,
        _ => HalfRound::Rne,
    };
    for i in 0..LANES {
        let x = unsafe { (a as *const u32).add(i).read_unaligned() };
        let h = f32_to_f16_sw(x, mode);
        unsafe { (dst as *mut u16).add(i).write_unaligned(h) };
    }
    // 输出高位清零（.128 的 v8i16 返回：高 4 lane = 0）
    unsafe { std::ptr::write_bytes(dst.add(LANES * 2), 0, 16 - LANES * 2) };
}

/// VCVTPH2PS：f16 lanes（输入低 LANES×2 字节）→ f32 lanes（输出 LANES×4 字节）
/// （.128：LANES=4 → 16 字节；.256：LANES=8 → 32 字节）。
pub(super) unsafe fn cvtph2ps<const LANES: usize>(dst: *mut u8, a: *const u8) {
    for i in 0..LANES {
        let h = unsafe { (a as *const u16).add(i).read_unaligned() };
        let x = f16_to_f32_sw(h);
        unsafe { (dst as *mut u32).add(i).write_unaligned(x) };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        aesdec, aesdeclast, aesenc, aesenclast, aesimc, aeskeygenassist, crc32_u8, crc32_u16,
        crc32_u32, crc32_u64, gather_d_pd_256, gather_q_pd_256, lddqu, pclmulqdq, permd256,
        pmaddubsw128, pmaddubsw256, pmaddwd128, pmaddwd256, psad_bw128, psad_bw256, pshufb128,
        pshufb256, sha256msg1, sha256msg2, sha256rnds2, vpmadd52,
    };

    #[test]
    fn lddqu_matches_unaligned_load_contract_and_hw() {
        let buf: [u8; 40] = std::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11));
        // 语义 = 非对齐 16/32 字节纯 load（与 loadu 逐位同义）
        let mut got128 = [0u8; 16];
        unsafe { lddqu::<16>(got128.as_mut_ptr(), buf.as_ptr().add(3)) };
        assert_eq!(got128[..], buf[3..19]);
        if std::is_x86_feature_detected!("sse3") {
            let hw = unsafe { core::arch::x86_64::_mm_lddqu_si128(buf.as_ptr().add(3).cast()) };
            assert_eq!(got128, unsafe { std::mem::transmute::<_, [u8; 16]>(hw) });
        }
        let mut got256 = [0u8; 32];
        unsafe { lddqu::<32>(got256.as_mut_ptr(), buf.as_ptr().add(5)) };
        assert_eq!(got256[..], buf[5..37]);
        if std::is_x86_feature_detected!("avx") {
            let hw = unsafe { core::arch::x86_64::_mm256_lddqu_si256(buf.as_ptr().add(5).cast()) };
            assert_eq!(got256, unsafe { std::mem::transmute::<_, [u8; 32]>(hw) });
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

    /// 把 (lo, hi) 两个 u64 拼成 16 字节 LE（对应 `_mm_set_epi64x(hi, lo)` 的布局）。
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
        // stdarch test_mm_sad_epu8 已知答案
        let a: [u8; 16] = [
            255, 254, 253, 252, 1, 2, 3, 4, 155, 154, 153, 152, 1, 2, 3, 4,
        ];
        let b: [u8; 16] = [0, 0, 0, 0, 2, 1, 2, 1, 1, 1, 1, 1, 1, 2, 1, 2];
        let mut got = [0u8; 16];
        unsafe { psad_bw128(got.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
        assert_eq!(got, qwords(1020, 614));

        if std::is_x86_feature_detected!("avx2") {
            // stdarch test_mm256_sad_epu8：每组 8×|2-4| = 16
            let a = [2u8; 32];
            let b = [4u8; 32];
            let mut got = [0u8; 32];
            unsafe { psad_bw256(got.as_mut_ptr(), a.as_ptr(), b.as_ptr()) };
            for lane in got.chunks_exact(8) {
                assert_eq!(u64::from_le_bytes(lane.try_into().unwrap()), 16);
            }
        }
    }

    #[test]
    fn pclmulqdq_matches_intel_whitepaper_vectors() {
        if !std::is_x86_feature_detected!("pclmulqdq") {
            return;
        }
        // Intel clmul 白皮书已知答案（stdarch test_mm_clmulepi64_si128）
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
            // 高位 imm 位硬件忽略：OR 0xEE 后与低位等价
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
        // MSDN/stdarch 常量（test_mm_aesenc_si128 等同组）
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

        // keygenassist 软件模型：MSDN 已知答案（imm=5）无条件成立
        unsafe { aeskeygenassist(got.as_mut_ptr(), a.as_ptr(), 5) };
        assert_eq!(got, qwords(0xeac4eea9c4eeacea, 0x857c266b7c266e85));
        // 并与硬件对拍多个 imm（覆盖 RCON 全字节语义）
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
        // stdarch 已知答案
        unsafe {
            assert_eq!(crc32_u8(0x2aa1e72b, 0x2a), 0xf24122e4);
            assert_eq!(crc32_u16(0x8ecec3b5, 0x22b), 0x13bb2fb);
            assert_eq!(crc32_u32(0xae2912c8, 0x845fed), 0xffae2ed1);
            assert_eq!(crc32_u64(0x7819dccd3e824, 0x2a22b845fed), 0xbb6cdc6c);
        }
        // CRC32C("123456789") = 0xe3069283（包装层提供首尾取反）
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
        // arr[i] = i as f64；scale=8 ⇒ f64 字寻址（stdarch test_mm256_mask_i64gather_pd）
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

        // fault suppression：被 mask 的 lane 挂野索引（读即 SIGSEGV 的地址）也不得触内存
        let mask_all_off = [0.0f64; 4];
        let wild: [i64; 4] = [0x7fff_ffff_fff0_0000; 4];
        let src2 = [42.0f64; 4];
        let mut got2 = [0.0f64; 4];
        unsafe {
            gather_q_pd_256(
                got2.as_mut_ptr().cast::<u8>(),
                src2.as_ptr().cast::<u8>(),
                1, // base=1：配合野索引必为不可读地址
                wild.as_ptr().cast::<u8>(),
                mask_all_off.as_ptr().cast::<u8>(),
                8,
            );
        }
        assert_eq!(got2, [42.0; 4]);

        // 与硬件 vgatherqpd 对拍（全 mask 开 + 混合 mask 各一组）
        if std::is_x86_feature_detected!("avx2") {
            use std::arch::x86_64::{
                _mm256_loadu_pd, _mm256_loadu_si256, _mm256_mask_i64gather_pd, _mm256_storeu_pd,
            };
            for (src_v, idx_v, mask_v) in [
                ([256.0f64; 4], [0i64, 16, 64, 96], [-1.0f64, -1.0, -1.0, 0.0]),
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
        // arr[i] = i as f64；scale=8 ⇒ f64 字寻址；i32 索引符号扩展（负索引回卷寻址）
        let arr: [f64; 128] = std::array::from_fn(|i| i as f64);
        let src = [9.0f64; 4];
        let vindex: [i32; 4] = [5, -1, 120, 0];
        let mask = [-1.0f64, -1.0, -1.0, 0.0];
        let mut got = [0.0f64; 4];
        // base 故意抬 8 字节：idx=-1 ⇒ addr = base-8 = arr[0]
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
                _mm256_loadu_pd, _mm256_mask_i32gather_pd, _mm256_storeu_pd, _mm_loadu_si128,
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
        // stdarch 已知答案（128/256/512 同值广播）：a=10<<40, b=(11<<40)+4, c=(12<<40)+3
        let a = [10u64 << 40; 8];
        let b = [(11u64 << 40) + 4; 8];
        let c = [(12u64 << 40) + 3; 8];
        let mut got = [0u64; 8];
        // lo 已知答案：128
        unsafe {
            vpmadd52::<2, false>(
                got.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
                c.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(&got[..2], &[100055558127628u64; 2]);
        // hi 已知答案
        unsafe {
            vpmadd52::<2, true>(
                got.as_mut_ptr().cast::<u8>(),
                a.as_ptr().cast::<u8>(),
                b.as_ptr().cast::<u8>(),
                c.as_ptr().cast::<u8>(),
            )
        };
        assert_eq!(&got[..2], &[11030549757952u64; 2]);
        // 4/8 lane 同输入广播
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

        // 硬件对拍：输入高 12 位污染（钉死 52 位输入掩码语义）+ 非常规 lane 值
        if std::is_x86_feature_detected!("avx512ifma")
            && std::is_x86_feature_detected!("avx512vl")
            && std::is_x86_feature_detected!("avx512f")
        {
            use std::arch::x86_64::{
                _mm256_loadu_si256, _mm256_madd52hi_epu64, _mm256_madd52lo_epu64,
                _mm256_storeu_si256, _mm512_loadu_si512, _mm512_madd52hi_epu64,
                _mm512_madd52lo_epu64, _mm512_storeu_si512, _mm_loadu_si128,
                _mm_madd52hi_epu64, _mm_madd52lo_epu64, _mm_storeu_si128,
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
                vpmadd52::<2, false>(sw_lo.as_mut_ptr().cast::<u8>(), av.as_ptr().cast::<u8>(), bv.as_ptr().cast::<u8>(), cv.as_ptr().cast::<u8>());
                vpmadd52::<2, true>(sw_hi.as_mut_ptr().cast::<u8>(), av.as_ptr().cast::<u8>(), bv.as_ptr().cast::<u8>(), cv.as_ptr().cast::<u8>());
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
                vpmadd52::<4, false>(sw_lo.as_mut_ptr().cast::<u8>(), av.as_ptr().cast::<u8>(), bv.as_ptr().cast::<u8>(), cv.as_ptr().cast::<u8>());
                vpmadd52::<4, true>(sw_hi.as_mut_ptr().cast::<u8>(), av.as_ptr().cast::<u8>(), bv.as_ptr().cast::<u8>(), cv.as_ptr().cast::<u8>());
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
                vpmadd52::<8, false>(sw_lo.as_mut_ptr().cast::<u8>(), av.as_ptr().cast::<u8>(), bv.as_ptr().cast::<u8>(), cv.as_ptr().cast::<u8>());
                vpmadd52::<8, true>(sw_hi.as_mut_ptr().cast::<u8>(), av.as_ptr().cast::<u8>(), bv.as_ptr().cast::<u8>(), cv.as_ptr().cast::<u8>());
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
            // stdarch test_mm_maddubs_epi16（含饱和组）
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
                i8::MAX, i8::MAX, i8::MAX, i8::MIN, i8::MIN, i8::MIN, 50, 15, 0, 0,
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
            // stdarch test_mm256_maddubs_epi16：2*4+2*4 = 16 广播
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
            // stdarch test_mm256_madd_epi16：2*4+2*4 = 16 广播
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
            // MIN*MIN+MIN*MIN 回绕为 i32::MIN（硬件定义）
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

    /// VCVTPS2PH 软件模型 vs 硬件 `_mm_cvtps_ph::<imm>`：全 f16 域往返 + 边界
    /// 沸点 + LCG 随机大谱系 × imm 0..7 全舍入模式（含 MXCSR=CUR_DIRECTION 4..7）。
    #[test]
    fn cvtps2ph_software_matches_f16c_hardware_bitwise() {
        if !std::is_x86_feature_detected!("f16c") {
            return;
        }
        use super::{cvtph2ps, cvtps2ph, f16_to_f32_sw, f32_to_f16_sw, HalfRound};
        use std::arch::x86_64::{_mm_cvtps_ph, _mm_set_ps1, _mm_storeu_si128};
        let hw = |bits: u32, imm: u64| -> u16 {
            let v = unsafe { _mm_set_ps1(f32::from_bits(bits)) };
            let r = unsafe {
                match imm {
                    0 => _mm_cvtps_ph::<0>(v),
                    1 => _mm_cvtps_ph::<1>(v),
                    2 => _mm_cvtps_ph::<2>(v),
                    3 => _mm_cvtps_ph::<3>(v),
                    _ => _mm_cvtps_ph::<4>(v), // 4 = CUR_DIRECTION：默认 MXCSR = RNE
                }
            };
            let mut o = [0u16; 8];
            unsafe { _mm_storeu_si128(o.as_mut_ptr().cast(), r) };
            o[0]
        };
        // 软件模型与硬件对照：imm 0..3 显式舍入、4 MXCSR(=RNE)。stdarch const 泛型
        // 限 imm<5；imm[3]（no-exc）只影响异常旗标、不影响值位（native 探针实录）。
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
                // 高端 helper 同路（.256 宽度全 lane + .128 高 64 位清零另验）
                let f = [f32::from_bits(bits); 8];
                let mut o = [0u16; 8];
                unsafe {
                    cvtps2ph::<8>(
                        o.as_mut_ptr().cast::<u8>(),
                        f.as_ptr().cast::<u8>(),
                        imm,
                    )
                };
                for (i, &h) in o.iter().enumerate() {
                    assert_eq!(h, expect, "helper bits={bits:08x} imm={imm} lane={i}");
                }
                let mut o4 = [0xffffu16; 8];
                unsafe {
                    cvtps2ph::<4>(
                        o4.as_mut_ptr().cast::<u8>(),
                        f.as_ptr().cast::<u8>(),
                        imm,
                    )
                };
                assert_eq!(&o4[..4], [expect; 4], "helper128 bits={bits:08x} imm={imm}");
                assert_eq!(&o4[4..], [0u16; 4], "helper128 高 64 位清零 bits={bits:08x} imm={imm}");
            }
        };
        // ① NaN/inf/±0/次正规/沸点 全列出
        let specials: [u32; 33] = [
            0x0000_0000, 0x8000_0000, 0x0000_0001, 0x8000_0001, 0x007f_ffff, 0x0080_0000,
            0x3380_0000, 0x337f_ffff, 0x3380_0001, 0x33ff_ffff, 0x3400_0000, 0x3400_0001,
            0x3400_07ff, 0x3400_0800, 0x3800_0000, 0x387f_f800, 0x387f_f000, 0x387f_efff,
            0x3880_0000, 0x477f_e000, 0x477f_efff, 0x477f_f000, 0x477f_f800, 0x477f_ffff,
            0x4780_0000, 0x477c_0000, 0xc77f_e000, 0x7f80_0000, 0xff80_0000,
            0x7fc0_0000, 0x7f80_0001, 0x7f80_0002, 0x7fff_ffff,
        ];
        for &b in &specials {
            check(b);
        }
        // ② 全 f16 位型的 f32 映像（往返域全盖）
        for u in 0u32..=0xffff {
            check(f16_to_f32_sw(u as u16));
        }
        // ③ LCG 随机 200 万（×8 imm）
        let mut rng = 0x9e3779b97f4a7c15u64;
        for _ in 0..2_000_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            check(rng as u32);
        }
        // ④ VCVTPH2PS：全 65536 位型软件 vs helper + 已知硬件位型（NaN qbit 强置等）
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
                // 8-lane helper（.256 同路：每 lane 同值）
                let v = [u as u16; 8];
                let mut o = [0u32; 8];
                unsafe { cvtph2ps::<8>(o.as_mut_ptr().cast::<u8>(), v.as_ptr().cast::<u8>()) };
                for (i, &x) in o.iter().enumerate() {
                    assert_eq!(x, hw, "ph2ps helper {u:04x} lane={i}");
                }
            }
        }
    }

    /// 族⑨ packed-f32 lane 软件模型 vs 硬件指令对拍（max/min/cmp/round/cvt/blendv，
    /// 128/256 双宽；NaN 位透传、±0 同位、32 谓词全表、舍入 imm 全表）。
    #[test]
    fn packed_ps_lane_models_match_hardware_bitwise() {
        use super::{blendv_ps, cmp_ps, cvt_ps2dq, maxmin_ps, round_ps};
        // (a,b) 组合覆盖：正常序/逆序/±0/±inf/qNaN/sNaN/载荷相异
        let bits_a: [u32; 9] = [
            0x3f80_0000, 0xbf80_0000, 0x8000_0000, 0x0000_0000, 0x7f80_0000, 0xff80_0000,
            0x7fc0_0000, 0x7f80_0001, 0x7fa0_0000,
        ];
        let bits_b: [u32; 9] = [
            0x4000_0000, 0x3f80_0000, 0x0000_0000, 0x8000_0000, 0xff80_0000, 0x7f80_0000,
            0x7f80_0001, 0x7fc0_0000, 0x7fc0_0000,
        ];
        if std::is_x86_feature_detected!("sse") {
            use std::arch::x86_64::{
                _mm_loadu_ps, _mm_max_ps, _mm_min_ps, _mm_storeu_ps,
            };
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
                    assert_eq!(sw_max.map(f32::to_bits), hw_max.map(f32::to_bits), "max {a:08x}/{b:08x}");
                    assert_eq!(sw_min.map(f32::to_bits), hw_min.map(f32::to_bits), "min {a:08x}/{b:08x}");
                }
            }
        }
        // cmp.ps 128/256 全 32 谓词 × 全位型组合（含相同 NaN 输入的同位比较）
        if std::is_x86_feature_detected!("sse") && std::is_x86_feature_detected!("avx") {
            use std::arch::x86_64::{
                _mm256_cmp_ps, _mm256_loadu_ps, _mm256_storeu_ps, _mm_cmp_ps, _mm_loadu_ps,
                _mm_storeu_ps,
            };
            macro_rules! cmp_hw {
                ($va:expr, $vb:expr, $va8:expr, $vb8:expr, $lane:expr, $a:expr, $b:expr, $imm:literal, $swa:expr, $swb:expr, $swa8:expr, $swb8:expr) => {{
                    let mut o = [0f32; 4];
                    _mm_storeu_ps(o.as_mut_ptr(), _mm_cmp_ps::<$imm>($va, $vb));
                    let mut sw = [0u32; 4];
                    cmp_ps::<4>(sw.as_mut_ptr().cast::<u8>(), $swa, $swb, $imm);
                    assert_eq!(sw, o.map(f32::to_bits), "cmp128 {} {}/{:08x}/{:08x}", $imm, $lane, $a, $b);
                    let mut o8 = [0f32; 8];
                    _mm256_storeu_ps(o8.as_mut_ptr(), _mm256_cmp_ps::<$imm>($va8, $vb8));
                    let mut sw8 = [0u32; 8];
                    cmp_ps::<8>(sw8.as_mut_ptr().cast::<u8>(), $swa8, $swb8, $imm);
                    assert_eq!(sw8, o8.map(f32::to_bits), "cmp256 {} {}/{:08x}/{:08x}", $imm, $lane, $a, $b);
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
        // round.ps imm 0..=15（bit2 MXCSR / bit3 no-exc 位组合）
        if std::is_x86_feature_detected!("sse4.1") {
            use std::arch::x86_64::{_mm_loadu_ps, _mm_round_ps, _mm_storeu_ps};
            let inputs: [u32; 14] = [
                0x0000_0000, 0x8000_0000, 0x3f00_0000, 0xbf00_0000, 0x3fa0_0000, 0xbfa0_0000,
                0x4049_0fdb, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0x7f80_0001, 0x3f80_0000,
                0x50c3_2e15, 0xd0c3_2e15,
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
        // cvt/cvtt 边界（indefinite 0x80000000 域），128/256 双宽
        if std::is_x86_feature_detected!("sse2") && std::is_x86_feature_detected!("avx") {
            use std::arch::x86_64::{
                _mm256_cvtps_epi32, _mm256_cvttps_epi32, _mm256_loadu_ps, _mm256_storeu_si256,
                _mm_cvtps_epi32, _mm_cvttps_epi32, _mm_loadu_ps, _mm_storeu_si128,
            };
            let inputs: [u32; 16] = [
                0x0000_0000, 0x8000_0000, 0x3f00_0000, 0x3f40_0000, 0x3fa0_0000, 0xbfa0_0000,
                0x4f00_0000, 0x4eff_ffff, 0xcf00_0000, 0xceff_ffff, 0x7f80_0000, 0xff80_0000,
                0x7fc0_0000, 0x7f80_0001, 0x0e8e_fdb9, 0x60ad_78ec,
            ];
            unsafe {
                for &b in &inputs {
                    let v = _mm_loadu_ps([f32::from_bits(b); 4].as_ptr());
                    let v8 = _mm256_loadu_ps([f32::from_bits(b); 8].as_ptr());
                    let (mut hw, mut hw8) = ([0i32; 4], [0i32; 8]);
                    _mm_storeu_si128(hw.as_mut_ptr().cast(), _mm_cvtps_epi32(v));
                    _mm256_storeu_si256(hw8.as_mut_ptr().cast(), _mm256_cvtps_epi32(v8));
                    let (mut sw, mut sw8) = ([0i32; 4], [0i32; 8]);
                    cvt_ps2dq::<4, false>(sw.as_mut_ptr().cast::<u8>(), [f32::from_bits(b); 4].as_ptr().cast::<u8>());
                    cvt_ps2dq::<8, false>(sw8.as_mut_ptr().cast::<u8>(), [f32::from_bits(b); 8].as_ptr().cast::<u8>());
                    assert_eq!(sw, hw, "cvt {b:08x}");
                    assert_eq!(sw8, hw8, "cvt256 {b:08x}");
                    _mm_storeu_si128(hw.as_mut_ptr().cast(), _mm_cvttps_epi32(v));
                    _mm256_storeu_si256(hw8.as_mut_ptr().cast(), _mm256_cvttps_epi32(v8));
                    cvt_ps2dq::<4, true>(sw.as_mut_ptr().cast::<u8>(), [f32::from_bits(b); 4].as_ptr().cast::<u8>());
                    cvt_ps2dq::<8, true>(sw8.as_mut_ptr().cast::<u8>(), [f32::from_bits(b); 8].as_ptr().cast::<u8>());
                    assert_eq!(sw, hw, "cvtt {b:08x}");
                    assert_eq!(sw8, hw8, "cvtt256 {b:08x}");
                }
            }
        }
        // blendv：mask 符号位纯位选择
        if std::is_x86_feature_detected!("sse4.1") {
            use std::arch::x86_64::{
                _mm_blendv_ps, _mm_loadu_ps, _mm_storeu_ps,
            };
            let a = [1.5f32, -2.25, 0.0, f32::NAN];
            let b = [7.0f32, -0.0, f32::INFINITY, -9.5];
            let m = [-1.0f32, 0.0, -0.0, 1.0]; // lane0/2 → b；lane1/3 → a
            unsafe {
                let r = _mm_blendv_ps(_mm_loadu_ps(a.as_ptr()), _mm_loadu_ps(b.as_ptr()), _mm_loadu_ps(m.as_ptr()));
                let mut o = [0f32; 4];
                _mm_storeu_ps(o.as_mut_ptr(), r);
                let mut sw = [0f32; 4];
                blendv_ps::<4>(sw.as_mut_ptr().cast::<u8>(), a.as_ptr().cast::<u8>(), b.as_ptr().cast::<u8>(), m.as_ptr().cast::<u8>());
                assert_eq!(sw.map(f32::to_bits), o.map(f32::to_bits));
            }
        }
    }

    /// PSLL/PSRL.d 软件模型 vs SSE2 硬件：count 阈值边界（31/32/33/全高字节）
    /// 与数据位型边界（含 0x80000000 位型，确认逻辑右移不铺符号）。
    #[test]
    fn pshift_d_models_match_sse2_hardware() {
        use super::pshift32;
        if !std::is_x86_feature_detected!("sse2") {
            return;
        }
        use std::arch::x86_64::{
            _mm_cvtsi32_si128, _mm_loadu_si128, _mm_sll_epi32, _mm_srl_epi32, _mm_storeu_si128,
        };
        let vals = [
            [0x0000_0001u32, 0x8000_0000, 0xffff_ffff, 0x1234_5678],
            [0x0000_0000u32, 0x7fff_ffff, 0x8000_0001, 0x0000_0002],
        ];
        // count 高 64 位随意污染（硬件忽略）；边界 31/32/33
        let counts: [[u64; 2]; 8] = [
            [0, 0], [1, 0], [15, 0xffff_ffff_ffff_ffff], [16, 0xaa55], [31, 0],
            [32, 0], [33, 0xdead_beef], [0xffff_ffff_ffff_ffff, 0],
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
}
