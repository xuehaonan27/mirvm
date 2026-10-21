//! AArch64 instructions behind the guest's LLVM intrinsic boundary. Guest and host share
//! a CPU; optional instructions are reached through the guest's feature detection.
//! The x86 signatures below are unreachable counterparts for the architecture dispatch.

use std::arch::aarch64 as neon;

// Load every input before storing the result: the guest destination may alias an input.
// Guest vector slots need not have the host vector type's alignment.
macro_rules! vector_result {
    ($dst:ident, $op:ident, $($input:ident),+) => {
        unsafe {
            let result = neon::$op($(std::ptr::read_unaligned($input.cast())),+);
            std::ptr::write_unaligned($dst.cast(), result);
        }
    };
}

/// Reads an 8- or 16-byte vector and writes pairwise sums in lanes twice as wide.
#[target_feature(enable = "neon")]
pub(crate) unsafe fn neon_add_long(
    dst: *mut u8,
    a: *const u8,
    signed: bool,
    lane_bytes: u8,
    vector_bytes: u8,
) {
    match (signed, lane_bytes, vector_bytes) {
        (false, 1, 8) => vector_result!(dst, vpaddl_u8, a),
        (false, 1, 16) => vector_result!(dst, vpaddlq_u8, a),
        (false, 2, 8) => vector_result!(dst, vpaddl_u16, a),
        (false, 2, 16) => vector_result!(dst, vpaddlq_u16, a),
        (false, 4, 8) => vector_result!(dst, vpaddl_u32, a),
        (false, 4, 16) => vector_result!(dst, vpaddlq_u32, a),
        (true, 1, 8) => vector_result!(dst, vpaddl_s8, a),
        (true, 1, 16) => vector_result!(dst, vpaddlq_s8, a),
        (true, 2, 8) => vector_result!(dst, vpaddl_s16, a),
        (true, 2, 16) => vector_result!(dst, vpaddlq_s16, a),
        (true, 4, 8) => vector_result!(dst, vpaddl_s32, a),
        (true, 4, 16) => vector_result!(dst, vpaddlq_s32, a),
        _ => unreachable!("invalid NEON pairwise widening shape"),
    }
}

/// Concatenates the wrapping pairwise sums of two vectors of the same shape.
#[target_feature(enable = "neon")]
pub(crate) unsafe fn neon_add_pair(
    dst: *mut u8,
    a: *const u8,
    b: *const u8,
    lane_bytes: u8,
    vector_bytes: u8,
) {
    match (lane_bytes, vector_bytes) {
        (1, 8) => vector_result!(dst, vpadd_u8, a, b),
        (1, 16) => vector_result!(dst, vpaddq_u8, a, b),
        (2, 8) => vector_result!(dst, vpadd_u16, a, b),
        (2, 16) => vector_result!(dst, vpaddq_u16, a, b),
        (4, 8) => vector_result!(dst, vpadd_u32, a, b),
        (4, 16) => vector_result!(dst, vpaddq_u32, a, b),
        (8, 16) => vector_result!(dst, vpaddq_u64, a, b),
        _ => unreachable!("invalid NEON pairwise addition shape"),
    }
}

/// Updates the uncomplemented CRC state, consuming the scalar's low bytes first.
#[target_feature(enable = "crc")]
pub(crate) unsafe fn aarch64_crc32(crc: u32, value: u64, castagnoli: bool, bytes: u8) -> u32 {
    match (castagnoli, bytes) {
        (false, 1) => neon::__crc32b(crc, value as u8),
        (false, 2) => neon::__crc32h(crc, value as u16),
        (false, 4) => neon::__crc32w(crc, value as u32),
        (false, 8) => neon::__crc32d(crc, value),
        (true, 1) => neon::__crc32cb(crc, value as u8),
        (true, 2) => neon::__crc32ch(crc, value as u16),
        (true, 4) => neon::__crc32cw(crc, value as u32),
        (true, 8) => neon::__crc32cd(crc, value),
        _ => unreachable!("invalid AArch64 CRC width"),
    }
}

/// One unreachable body per signature, so the refusal reads the same wherever it is written.
macro_rules! no_such_instruction {
    ($( fn $name:ident [ $($generics:tt)* ] ( $($args:tt)* ) $( -> $ret:ty)? ; )*) => {$(
        #[allow(unused_variables)]
        pub(crate) unsafe fn $name $($generics)* ( $($args)* ) $(-> $ret)? {
            unreachable!(concat!("this CPU has no `", stringify!($name), "` instruction"))
        }
    )*};
}

no_such_instruction! {
    fn xgetbv [] (xcr: u32) -> u64;
    fn pshufb128 [] (dst: *mut u8, a: *const u8, control: *const u8) ;
    fn pshufb256 [] (dst: *mut u8, a: *const u8, control: *const u8) ;
    fn sha256msg1 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn sha256msg2 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn sha256rnds2 [] (dst: *mut u8, a: *const u8, b: *const u8, round_keys: *const u8) ;
    fn psad_bw128 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn psad_bw256 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn pclmulqdq [] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64) ;
    fn pclmulqdq256 [] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64) ;
    fn pclmulqdq512 [] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64) ;
    fn aesenc [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesenclast [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdec [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdeclast [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesimc [] (dst: *mut u8, a: *const u8) ;
    fn aeskeygenassist [] (dst: *mut u8, a: *const u8, imm: u64) ;
    fn aesenc256 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesenclast256 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdec256 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdeclast256 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesenc512 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesenclast512 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdec512 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn aesdeclast512 [] (dst: *mut u8, a: *const u8, round_key: *const u8) ;
    fn crc32_u8 [] (crc: u32, v: u8) -> u32;
    fn crc32_u16 [] (crc: u32, v: u16) -> u32;
    fn crc32_u32 [] (crc: u32, v: u32) -> u32;
    fn crc32_u64 [] (crc: u64, v: u64) -> u64;
    fn permd256 [] (dst: *mut u8, a: *const u8, idx: *const u8) ;
    fn pmaddubsw128 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn pmaddubsw256 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn pmaddwd128 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn lddqu [<const W: usize>] (dst: *mut u8, src: *const u8) ;
    fn pmaddwd256 [] (dst: *mut u8, a: *const u8, b: *const u8) ;
    fn gather_sign_mask [<const REG: usize, const IDX: usize, const IDX64: bool, const ELT64: bool>] (dst: *mut u8, src: *const u8, base: u64, offsets: *const u8, mask: *const u8, scale: u64,) ;
    fn gather_k_mask [<const REG: usize, const IDX: usize, const IDX64: bool, const ELT64: bool>] (dst: *mut u8, src: *const u8, base: u64, offsets: *const u8, mask: u64, scale: u64,) ;
    fn scatter_k_mask [<const REG: usize, const IDX: usize, const IDX64: bool, const ELT64: bool>] (base: u64, mask: u64, offsets: *const u8, values: *const u8, scale: u64) ;
    fn vpmadd52 [<const LANES: usize, const HI: bool>] (dst: *mut u8, a: *const u8, b: *const u8, c: *const u8,) ;
    fn maxmin_ps [<const LANES: usize, const MAX: bool>] (dst: *mut u8, a: *const u8, b: *const u8,) ;
    fn maxmin_pd [<const LANES: usize, const MAX: bool>] (dst: *mut u8, a: *const u8, b: *const u8,) ;
    fn cmp_ps [<const LANES: usize>] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64,) ;
    fn cmp_pd [<const LANES: usize>] (dst: *mut u8, a: *const u8, b: *const u8, imm: u64,) ;
    fn round_ps [<const LANES: usize>] (dst: *mut u8, a: *const u8, imm: u64) ;
    fn cvt_ps2dq [<const LANES: usize, const TRUNC: bool>] (dst: *mut u8, a: *const u8) ;
    fn blendv_ps [<const LANES: usize>] (dst: *mut u8, a: *const u8, b: *const u8, mask: *const u8,) ;
    fn pshift32 [<const LANES: usize, const LEFT: bool>] (dst: *mut u8, a: *const u8, count: *const u8,) ;
    fn cvtps2ph [<const LANES: usize>] (dst: *mut u8, a: *const u8, imm: u64) ;
    fn cvtph2ps [<const LANES: usize>] (dst: *mut u8, a: *const u8) ;
}
