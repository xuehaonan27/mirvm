//! arch::x86_64::intrinsics — x86_64 硬件 intrinsic 执行体（arch 层纯件）。
//!
//! 这些函数在 `llvm.x86.*` 边界后执行真宿主指令；guest 与宿主是同一个
//! 虚拟 CPU，调用方必经 guest 正常 CPUID 派发到达。只摸裸指针/整数——
//! 无引擎类型、无 OS 依赖（arch/ 层 leaf 纪律，os/ 同）。
//! （自 vm/engine/x86.rs 整搬，可见性 pub(super)→pub(crate)，零逻辑 diff。）

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
pub(crate) unsafe fn pshufb128(dst: *mut u8, a: *const u8, control: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let control = unsafe { _mm_loadu_si128(control.cast::<__m128i>()) };
    let result = _mm_shuffle_epi8(a, control);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// AVX2 `vpshufb`, with independent 128-bit lanes.
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn pshufb256(dst: *mut u8, a: *const u8, control: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let control = unsafe { _mm256_loadu_si256(control.cast::<__m256i>()) };
    let result = _mm256_shuffle_epi8(a, control);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

#[target_feature(enable = "sha")]
pub(crate) unsafe fn sha256msg1(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_sha256msg1_epu32(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "sha")]
pub(crate) unsafe fn sha256msg2(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_sha256msg2_epu32(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "sha")]
pub(crate) unsafe fn sha256rnds2(dst: *mut u8, a: *const u8, b: *const u8, round_keys: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let round_keys = unsafe { _mm_loadu_si128(round_keys.cast::<__m128i>()) };
    let result = _mm_sha256rnds2_epu32(a, b, round_keys);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// SSE2 `psadbw`：两组 8 字节绝对差和，以 u64 落 qword lane 0/1（其余位清零）。
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn psad_bw128(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_sad_epu8(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// AVX2 `vpsadbw`：每 128 位 lane 独立，共 4 个 u64 和。
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn psad_bw256(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let b = unsafe { _mm256_loadu_si256(b.cast::<__m256i>()) };
    let result = _mm256_sad_epu8(a, b);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// PCLMULQDQ：imm8 bit0/bit4 各选 a/b 的 qword 做无进位乘法；imm 其余位硬件
/// 忽略（`imm & 0x11` 同构）。imm 是运行时参数，按 4 种合法组合分派 const generic。
#[target_feature(enable = "pclmulqdq")]
pub(crate) unsafe fn pclmulqdq(dst: *mut u8, a: *const u8, b: *const u8, imm: u64) {
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
pub(crate) unsafe fn aesenc(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesenc_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(crate) unsafe fn aesenclast(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesenclast_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(crate) unsafe fn aesdec(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesdec_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(crate) unsafe fn aesdeclast(dst: *mut u8, a: *const u8, round_key: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let round_key = unsafe { _mm_loadu_si128(round_key.cast::<__m128i>()) };
    let result = _mm_aesdeclast_si128(a, round_key);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

#[target_feature(enable = "aes")]
pub(crate) unsafe fn aesimc(dst: *mut u8, a: *const u8) {
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
pub(crate) unsafe fn aeskeygenassist(dst: *mut u8, a: *const u8, imm: u64) {
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
pub(crate) unsafe fn crc32_u8(crc: u32, v: u8) -> u32 {
    _mm_crc32_u8(crc, v)
}

#[target_feature(enable = "sse4.2")]
pub(crate) unsafe fn crc32_u16(crc: u32, v: u16) -> u32 {
    _mm_crc32_u16(crc, v)
}

#[target_feature(enable = "sse4.2")]
pub(crate) unsafe fn crc32_u32(crc: u32, v: u32) -> u32 {
    _mm_crc32_u32(crc, v)
}

#[target_feature(enable = "sse4.2")]
pub(crate) unsafe fn crc32_u64(crc: u64, v: u64) -> u64 {
    _mm_crc32_u64(crc, v)
}

/// AVX2 `vpermd`：dst.dword[i] = a.dword[idx.dword[i] & 7]（跨 lane）。
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn permd256(dst: *mut u8, a: *const u8, idx: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let idx = unsafe { _mm256_loadu_si256(idx.cast::<__m256i>()) };
    let result = _mm256_permutevar8x32_epi32(a, idx);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// SSSE3 `pmaddubsw`：a 无符号字节 × b 有符号字节，相邻两积之和饱和到 i16。
#[target_feature(enable = "ssse3")]
pub(crate) unsafe fn pmaddubsw128(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_maddubs_epi16(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// AVX2 `vpmaddubsw`：每 128 位 lane 独立。
#[target_feature(enable = "avx2")]
pub(crate) unsafe fn pmaddubsw256(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let b = unsafe { _mm256_loadu_si256(b.cast::<__m256i>()) };
    let result = _mm256_maddubs_epi16(a, b);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// SSE2 `pmaddwd`：相邻 i16 对积之和放 i32。
#[target_feature(enable = "sse2")]
pub(crate) unsafe fn pmaddwd128(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm_loadu_si128(a.cast::<__m128i>()) };
    let b = unsafe { _mm_loadu_si128(b.cast::<__m128i>()) };
    let result = _mm_madd_epi16(a, b);
    unsafe { _mm_storeu_si128(dst.cast::<__m128i>(), result) };
}

/// LDDQU 族（`llvm.x86.sse3.ldu.dq` / `llvm.x86.avx.ldu.dq.256`；
/// `_mm_lddqu_si128` / `_mm256_lddqu_si256`）：语义 = 普通非对齐 16/32 字节
/// load（与 loadu 逐位同义——lddqu 的未缓存跨行微优化提示在本模型无影响）。
/// corpus 批8 c_tantivy 实锤补建（bitpacking avx2/termdict 列值读取派发点）。
pub(crate) unsafe fn lddqu<const W: usize>(dst: *mut u8, src: *const u8) {
    unsafe { std::ptr::copy_nonoverlapping(src, dst, W) };
}

/// AVX2 `vpmaddwd`：每 128 位 lane 独立。
#[target_feature(enable = "avx2")]

pub(crate) unsafe fn pmaddwd256(dst: *mut u8, a: *const u8, b: *const u8) {
    let a = unsafe { _mm256_loadu_si256(a.cast::<__m256i>()) };
    let b = unsafe { _mm256_loadu_si256(b.cast::<__m256i>()) };
    let result = _mm256_madd_epi16(a, b);
    unsafe { _mm256_storeu_si256(dst.cast::<__m256i>(), result) };
}

/// `vgatherqpd` 的软件模型（`llvm.x86.avx2.gather.q.pd.256`）：mask lane 符号位
/// 置位才读 `base + vindex*scale`（f64），否则拷 src lane。**mask 关闭的 lane
/// 绝不触内存**（fault suppression——野索引 lane 被 mask 时硬件同样不读）。
/// 地址按 64 位回绕算术（硬件同）。scale 合法值为 1/2/4/8（LLVM 发射已约束）。
pub(crate) unsafe fn gather_q_pd_256(
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
pub(crate) unsafe fn gather_d_pd_256(
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
pub(crate) unsafe fn vpmadd52<const LANES: usize, const HI: bool>(
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
pub(crate) unsafe fn maxmin_ps<const LANES: usize, const MAX: bool>(
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
pub(crate) unsafe fn cmp_ps<const LANES: usize>(dst: *mut u8, a: *const u8, b: *const u8, imm: u64) {
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
pub(crate) unsafe fn round_ps<const LANES: usize>(dst: *mut u8, a: *const u8, imm: u64) {
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
pub(crate) unsafe fn cvt_ps2dq<const LANES: usize, const TRUNC: bool>(dst: *mut u8, a: *const u8) {
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
pub(crate) unsafe fn blendv_ps<const LANES: usize>(
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
pub(crate) unsafe fn pshift32<const LANES: usize, const LEFT: bool>(
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
pub(crate) enum HalfRound {
    Rne,
    Down,
    Up,
    Trunc,
}

/// f32（位型）→ f16（位型），VCVTPS2PH 精确语义：次正规/溢出/四种舍入模式精确；
/// NaN → qbit 强置 + 载荷右移 13 位截断（`7f800001→7e00` 型，对拍钉死）。
pub(crate) fn f32_to_f16_sw(bits: u32, mode: HalfRound) -> u16 {
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
pub(crate) fn f16_to_f32_sw(bits: u16) -> u32 {
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
pub(crate) unsafe fn cvtps2ph<const LANES: usize>(dst: *mut u8, a: *const u8, imm: u64) {
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
pub(crate) unsafe fn cvtph2ps<const LANES: usize>(dst: *mut u8, a: *const u8) {
    for i in 0..LANES {
        let h = unsafe { (a as *const u16).add(i).read_unaligned() };
        let x = f16_to_f32_sw(h);
        unsafe { (dst as *mut u32).add(i).write_unaligned(x) };
    }
}


#[cfg(test)]
mod tests;
