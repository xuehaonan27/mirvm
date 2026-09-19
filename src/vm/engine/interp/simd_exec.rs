//! Execution bodies for the SIMD statements, Sat128, and the SIMD rvalues. The
//! interpreter's thin shells (which evaluate with `eval_place_addr`/`eval_operand` before
//! calling in) and the JIT helpers (`mirvm_simd_stmt`/`mirvm_simd_rv`) share these bodies, so
//! the two backends cannot drift: the loops keep the interpreter arms' exact semantics and
//! the `engine_abort` wording is shared byte for byte.
//!
//! Address parameters are already-evaluated raw true addresses; each body converts them back
//! to u64, matching the interpreter arms.

use super::*;
use crate::vm::engine::ir::{LaneKind, SimdBinOp, SimdReduceOp, SimdUnOp};

/// Body of the SimdBin statement.
pub(crate) fn simd_bin_body(
    pd: *mut u8,
    pa: *const u8,
    pb: *const u8,
    op: SimdBinOp,
    lane: LaneKind,
    lanes: u16,
    lb: u8,
) {
    use crate::vm::engine::ir::{LaneKind, SimdBinOp as S};
    let (pd, pa, pb) = (pd as u64, pa as u64, pb as u64);
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let x = mem_read(pa + i * lb, lw);
        let y = mem_read(pb + i * lb, lw);
        let r: u64 = match lane {
            LaneKind::Int { signed } => {
                let cmp = |cc| (int_cmp(cc, signed, x, y, lw) != 0) as u64 * lw.mask();
                match op {
                    S::Eq => cmp(IntCc::Eq),
                    S::Ne => cmp(IntCc::Ne),
                    S::Lt => cmp(IntCc::Lt),
                    S::Le => cmp(IntCc::Le),
                    S::Gt => cmp(IntCc::Gt),
                    S::Ge => cmp(IntCc::Ge),
                    S::And => x & y,
                    S::Or => x | y,
                    S::Xor => x ^ y,
                    S::Add => x.wrapping_add(y) & lw.mask(),
                    S::Sub => x.wrapping_sub(y) & lw.mask(),
                    S::Mul => x.wrapping_mul(y) & lw.mask(),
                    S::Div | S::Rem => {
                        if y == 0 {
                            engine_abort("simd integer division by zero (guest UB)");
                        }
                        let o = if matches!(op, S::Div) {
                            IntBinOp::Div
                        } else {
                            IntBinOp::Rem
                        };
                        int_bin(o, signed, x, y, lw)
                    }
                    S::SatAdd => int_saturating(OvfOp::Add, signed, x, y, lw),
                    S::SatSub => int_saturating(OvfOp::Sub, signed, x, y, lw),
                    S::MinNum | S::MaxNum => engine_abort(&format!(
                        "simd {op:?} does not apply to an integer lane (lowering validation gap)"
                    )),
                    S::Shl | S::Shr => {
                        if y >= u64::from(lw.bytes() * 8) {
                            engine_abort("simd shift amount exceeds the lane bit width (guest UB)");
                        }
                        let o = if matches!(op, S::Shl) {
                            IntBinOp::Shl
                        } else {
                            IntBinOp::Shr
                        };
                        int_bin(o, signed, x, y, lw)
                    }
                }
            }
            // Float lanes compute IEEE semantics directly: a comparison is not a bit
            // comparison (+0.0 == -0.0, NaN is not self-equal) and arithmetic is not integer
            // addition.
            LaneKind::Float => {
                macro_rules! fl {
                    ($t:ty, $xb:expr, $yb:expr) => {{
                        let (fx, fy) = (<$t>::from_bits($xb as _), <$t>::from_bits($yb as _));
                        let cmp = |t: bool| t as u64 * lw.mask();
                        match op {
                            S::Eq => cmp(fx == fy),
                            S::Ne => cmp(fx != fy),
                            S::Lt => cmp(fx < fy),
                            S::Le => cmp(fx <= fy),
                            S::Gt => cmp(fx > fy),
                            S::Ge => cmp(fx >= fy),
                            S::Add => (fx + fy).to_bits() as u64,
                            S::Sub => (fx - fy).to_bits() as u64,
                            S::Mul => (fx * fy).to_bits() as u64,
                            S::Div => (fx / fy).to_bits() as u64,
                            S::Rem => (fx % fy).to_bits() as u64,
                            S::MinNum => fx.min(fy).to_bits() as u64,
                            S::MaxNum => fx.max(fy).to_bits() as u64,
                            S::And | S::Or | S::Xor | S::SatAdd | S::SatSub | S::Shl | S::Shr => {
                                engine_abort(&format!(
                                    "simd {op:?} does not apply to a float lane (lowering validation gap)"
                                ))
                            }
                        }
                    }};
                }
                match lw {
                    Width::W32 => fl!(f32, x, y),
                    Width::W64 => fl!(f64, x, y),
                    _ => engine_abort("float lane width is not 4/8 (lowering validation gap)"),
                }
            }
        };
        mem_write(pd + i * lb, lw, r);
    }
}

/// Body of the SimdUn statement.
pub(crate) fn simd_un_body(
    pd: *mut u8,
    pa: *const u8,
    op: SimdUnOp,
    lane: LaneKind,
    lanes: u16,
    lb: u8,
) {
    use crate::vm::engine::ir::{BitUnOp, LaneKind, SimdUnOp as U};
    let (pd, pa) = (pd as u64, pa as u64);
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let x = mem_read(pa + i * lb, lw);
        let r: u64 = match (lane, op) {
            (LaneKind::Int { .. }, U::Neg) => x.wrapping_neg() & lw.mask(),
            (LaneKind::Int { .. }, U::Ctlz) => bit_un(BitUnOp::Ctlz, x, lw),
            (LaneKind::Int { .. }, U::Cttz) => bit_un(BitUnOp::Cttz, x, lw),
            (LaneKind::Int { .. }, U::Ctpop) => bit_un(BitUnOp::Popcount, x, lw),
            (LaneKind::Int { .. }, U::Bswap) => bit_un(BitUnOp::Bswap, x, lw),
            (LaneKind::Int { .. }, U::Bitreverse) => bit_un(BitUnOp::Bitreverse, x, lw),
            (LaneKind::Float, _) => {
                macro_rules! fu {
                    ($t:ty) => {{
                        let f = <$t>::from_bits(x as _);
                        (match op {
                            U::Neg => -f,
                            U::Fabs => f.abs(),
                            U::Fsqrt => f.sqrt(),
                            U::Ceil => f.ceil(),
                            U::Floor => f.floor(),
                            U::Round => f.round(),
                            U::RoundTiesEven => f.round_ties_even(),
                            U::Trunc => f.trunc(),
                            U::Fsin => f.sin(),
                            U::Fcos => f.cos(),
                            U::Fexp => f.exp(),
                            U::Fexp2 => f.exp2(),
                            U::Flog => f.ln(),
                            U::Flog2 => f.log2(),
                            U::Flog10 => f.log10(),
                            U::Ctlz | U::Cttz | U::Ctpop | U::Bswap | U::Bitreverse => {
                                engine_abort(&format!(
                                    "simd {op:?} does not apply to a float lane (lowering validation gap)"
                                ))
                            }
                        })
                        .to_bits() as u64
                    }};
                }
                match lw {
                    Width::W32 => fu!(f32),
                    Width::W64 => fu!(f64),
                    _ => engine_abort("float lane width is not 4/8 (lowering validation gap)"),
                }
            }
            (LaneKind::Int { .. }, other) => engine_abort(&format!(
                "simd {other:?} does not apply to an integer lane (lowering validation gap)"
            )),
        };
        mem_write(pd + i * lb, lw, r);
    }
}

/// Body of the SimdFma statement.
pub(crate) fn simd_fma_body(
    pd: *mut u8,
    pa: *const u8,
    pb: *const u8,
    pc: *const u8,
    lanes: u16,
    lb: u8,
) {
    let (pd, pa, pb, pc) = (pd as u64, pa as u64, pb as u64, pc as u64);
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let (x, y, z) = (
            mem_read(pa + i * lb, lw),
            mem_read(pb + i * lb, lw),
            mem_read(pc + i * lb, lw),
        );
        let r = match lw {
            Width::W32 => f32::from_bits(x as u32)
                .mul_add(f32::from_bits(y as u32), f32::from_bits(z as u32))
                .to_bits() as u64,
            Width::W64 => f64::from_bits(x)
                .mul_add(f64::from_bits(y), f64::from_bits(z))
                .to_bits(),
            _ => engine_abort("simd_fma lane width is not 4/8 (lowering validation gap)"),
        };
        mem_write(pd + i * lb, lw, r);
    }
}

/// Body of the SimdFunnel statement; `ps` is the shift vector address.
pub(crate) fn simd_funnel_body(
    pd: *mut u8,
    pa: *const u8,
    pb: *const u8,
    ps: *const u8,
    left: bool,
    lanes: u16,
    lb: u8,
) {
    let (pd, pa, pb, ps) = (pd as u64, pa as u64, pb as u64, ps as u64);
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    let bits = u64::from(lw.bytes() * 8);
    for i in 0..lanes as u64 {
        let x = mem_read(pa + i * lb, lw) as u128;
        let y = mem_read(pb + i * lb, lw) as u128;
        let s = mem_read(ps + i * lb, lw);
        if s >= bits {
            engine_abort("simd_funnel shift amount exceeds the lane bit width (guest UB)");
        }
        // Concatenate to [a:b] (2W bits) and take the high or low W-bit window.
        let cat = (x << bits) | y;
        let r = if left { cat << s >> bits } else { cat >> s };
        mem_write(pd + i * lb, lw, r as u64 & lw.mask());
    }
}

/// Body of the SimdCast statement; `sb`/`db` are the source/destination lane byte counts.
pub(crate) fn simd_cast_body(
    pd: *mut u8,
    ps: *const u8,
    lanes: u16,
    src_lane: LaneKind,
    sb: u8,
    dst_lane: LaneKind,
    db: u8,
) {
    use crate::vm::engine::ir::LaneKind as L;
    let (pd, ps) = (pd as u64, ps as u64);
    let (sb, db) = (sb as u64, db as u64);
    let sw = Width::from_bytes(sb).expect("src lane width");
    let dw = Width::from_bytes(db).expect("dst lane width");
    for i in 0..lanes as u64 {
        let v = mem_read(ps + i * sb, sw);
        let r: u64 = match (src_lane, dst_lane) {
            (L::Int { signed }, L::Int { .. }) => {
                // Narrowing truncates; widening sign-extends from the source.
                let x = if signed { sext(v, sw) as u64 } else { v };
                x & dw.mask()
            }
            (L::Int { signed }, L::Float) => {
                let (f16b, f32b, f64b) = if signed {
                    let x = sext(v, sw);
                    (
                        (x as f16).to_bits() as u64,
                        (x as f32).to_bits() as u64,
                        (x as f64).to_bits(),
                    )
                } else {
                    (
                        (v as f16).to_bits() as u64,
                        (v as f32).to_bits() as u64,
                        (v as f64).to_bits(),
                    )
                };
                match dw {
                    Width::W16 => f16b,
                    Width::W32 => f32b,
                    Width::W64 => f64b,
                    _ => engine_abort("simd_cast float lane width is not 2/4/8"),
                }
            }
            (L::Float, L::Int { signed }) => {
                // f16/f32 -> f64 preserves the value exactly, so unify through f64; the host
                // `as` cast already has the saturation semantics simd_as needs (an
                // out-of-range simd_cast is guest UB, and a saturated value stays inside the
                // allowed set).
                let x = match sw {
                    Width::W16 => {
                        f64::from(f32::from_bits(crate::arch::x86_64::f16_to_f32_sw(v as u16)))
                    }
                    Width::W32 => f32::from_bits(v as u32) as f64,
                    Width::W64 => f64::from_bits(v),
                    _ => engine_abort("simd_cast float lane width is not 2/4/8"),
                };
                let out = if signed {
                    match dw {
                        Width::W8 => x as i8 as u64,
                        Width::W16 => x as i16 as u64,
                        Width::W32 => x as i32 as u64,
                        Width::W64 => x as i64 as u64,
                    }
                } else {
                    match dw {
                        Width::W8 => x as u8 as u64,
                        Width::W16 => x as u16 as u64,
                        Width::W32 => x as u32 as u64,
                        Width::W64 => x as u64,
                    }
                };
                out & dw.mask()
            }
            (L::Float, L::Float) => match (sw, dw) {
                (Width::W32, Width::W64) => (f32::from_bits(v as u32) as f64).to_bits(),
                (Width::W64, Width::W32) => (f64::from_bits(v) as f32).to_bits() as u64,
                // f16 lanes use a deterministic software model: native executes hardware
                // semantics (VCVTPH2PS/VCVTPS2PH, including forcing the sNaN quiet bit)
                // inside a target_feature(f16c) function, while a host libcall's NaN bit
                // behavior drifts with the build target and cannot be relied on.
                (Width::W16, Width::W32) => u64::from(crate::arch::x86_64::f16_to_f32_sw(v as u16)),
                (Width::W16, Width::W64) => {
                    f64::from(f32::from_bits(crate::arch::x86_64::f16_to_f32_sw(v as u16)))
                        .to_bits()
                }
                (Width::W32, Width::W16) => u64::from(crate::arch::x86_64::f32_to_f16_sw(
                    v as u32,
                    crate::arch::x86_64::HalfRound::Rne,
                )),
                (Width::W64, Width::W16) => (f64::from_bits(v) as f16).to_bits() as u64,
                _ => v, // same width: bitwise passthrough
            },
        };
        mem_write(pd + i * db, dw, r);
    }
}

/// Body of the SimdSelect statement; `mb` is `mask_bytes`.
pub(crate) fn simd_select_body(
    pd: *mut u8,
    pm: *const u8,
    mb: u8,
    pa: *const u8,
    pb: *const u8,
    lanes: u16,
    lb: u8,
) {
    let (pd, pm, pa, pb) = (pd as u64, pm as u64, pa as u64, pb as u64);
    let (mb, lb) = (mb as u64, lb as u64);
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        // A mask lane is all-ones or all-zeros (type invariant), so test its sign bit (the
        // top bit of the last byte).
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        let src = if top >> 7 != 0 { pa } else { pb };
        mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
    }
}

/// Body of the SimdSelectBitmask statement.
pub(crate) fn simd_select_bitmask_body(
    pd: *mut u8,
    mask: u64,
    pa: *const u8,
    pb: *const u8,
    lanes: u16,
    lb: u8,
) {
    let (pd, pa, pb) = (pd as u64, pa as u64, pb as u64);
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let src = if mask >> i & 1 != 0 { pa } else { pb };
        mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
    }
}

/// Body of the SimdGather statement; `ppass` is the passthru address, `pp` the ptrs address
/// and `pm` the mask address.
pub(crate) fn simd_gather_body(
    pd: *mut u8,
    ppass: *const u8,
    pp: *const u8,
    pm: *const u8,
    mb: u8,
    lanes: u16,
    lb: u8,
) {
    let (pv, pp, pm, pd) = (ppass as u64, pp as u64, pm as u64, pd as u64);
    let (mb, lb) = (mb as u64, lb as u64);
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        // A masked-off lane never even pretends to read, since the pointer may be invalid;
        // that is exactly what the mask means.
        let v = if top >> 7 != 0 {
            mem_read(mem_read(pp + i * 8, Width::W64), lw)
        } else {
            mem_read(pv + i * lb, lw)
        };
        mem_write(pd + i * lb, lw, v);
    }
}

/// Body of the SimdScatter statement; `pv` is the values address.
pub(crate) fn simd_scatter_body(
    pv: *const u8,
    pp: *const u8,
    pm: *const u8,
    mb: u8,
    lanes: u16,
    lb: u8,
) {
    let (pv, pp, pm) = (pv as u64, pp as u64, pm as u64);
    let (mb, lb) = (mb as u64, lb as u64);
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        if top >> 7 != 0 {
            mem_write(
                mem_read(pp + i * 8, Width::W64),
                lw,
                mem_read(pv + i * lb, lw),
            );
        }
    }
}

/// Body of the SimdMaskedLoad statement; `base` is the already-evaluated element base scalar
/// and `ppass` the passthru address.
pub(crate) fn simd_masked_load_body(
    pd: *mut u8,
    pm: *const u8,
    mb: u8,
    base: u64,
    ppass: *const u8,
    lanes: u16,
    lb: u8,
) {
    let (pm, pbase, pv, pd) = (pm as u64, base, ppass as u64, pd as u64);
    let (mb, lb) = (mb as u64, lb as u64);
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        let v = if top >> 7 != 0 {
            mem_read(pbase + i * lb, lw)
        } else {
            mem_read(pv + i * lb, lw)
        };
        mem_write(pd + i * lb, lw, v);
    }
}

/// Body of the SimdMaskedStore statement.
pub(crate) fn simd_masked_store_body(
    pm: *const u8,
    mb: u8,
    base: u64,
    pv: *const u8,
    lanes: u16,
    lb: u8,
) {
    let (pm, pbase, pv) = (pm as u64, base, pv as u64);
    let (mb, lb) = (mb as u64, lb as u64);
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        if top >> 7 != 0 {
            mem_write(pbase + i * lb, lw, mem_read(pv + i * lb, lw));
        }
    }
}

/// Body of the SimdExtractDyn statement; returns the extracted lane scalar. The caller writes
/// it to the destination (the interpreter through `place_write`, the JIT through
/// `write_scalar_place`).
pub(crate) fn simd_extract_dyn_body(ps: *const u8, idx: u64, lanes: u16, lb: u8) -> u64 {
    let ps = ps as u64;
    if idx >= u64::from(lanes) {
        engine_abort(&format!(
            "simd_extract_dyn index {idx} out of bounds (lanes={lanes}, guest UB)"
        ));
    }
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    mem_read(ps + idx * lb, lw)
}

/// Body of the SimdInsertDyn statement; `v` is the already-evaluated inserted scalar.
///
/// Evaluation-order note: the thin shells (this same body on both the interpreter and JIT
/// sides) necessarily evaluate `val` before entering the body's bounds check, so only when
/// `idx` is out of bounds *and* evaluating `val` itself aborts does the diagnostic order
/// differ -- a double-UB corner in which both sides still exit 70.
pub(crate) fn simd_insert_dyn_body(
    pd: *mut u8,
    ps: *const u8,
    idx: u64,
    v: u64,
    lanes: u16,
    lb: u8,
) {
    let (pd, ps) = (pd as u64, ps as u64);
    if idx >= u64::from(lanes) {
        engine_abort(&format!(
            "simd_insert_dyn index {idx} out of bounds (lanes={lanes}, guest UB)"
        ));
    }
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    let total = u64::from(lanes) * lb;
    // dst may alias src (x = insert_dyn(x, ...)), so the whole-vector copy must be a memmove.
    unsafe {
        std::ptr::copy(ps as *const u8, pd as *mut u8, total as usize);
    }
    mem_write(pd + idx * lb, lw, v);
}

/// Body of the SimdArithOffset statement (always lanes x 8 bytes).
pub(crate) fn simd_arith_offset_body(
    pd: *mut u8,
    pp: *const u8,
    po: *const u8,
    stride: u64,
    lanes: u16,
) {
    let (pd, pp, po) = (pd as u64, pp as u64, po as u64);
    for i in 0..lanes as u64 {
        let p = mem_read(pp + i * 8, Width::W64);
        let off = mem_read(po + i * 8, Width::W64);
        mem_write(
            pd + i * 8,
            Width::W64,
            p.wrapping_add(off.wrapping_mul(stride)),
        );
    }
}

/// Body of the SimdSplat statement.
pub(crate) fn simd_splat_body(pd: *mut u8, v: u64, lanes: u16, lb: u8) {
    let pd = pd as u64;
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    for i in 0..lanes as u64 {
        mem_write(pd + i * lb, lw, v);
    }
}

/// Body of the Sat128 statement.
pub(crate) fn sat128_body(pa: *const u8, pb: *const u8, pd: *mut u8, op: OvfOp, signed: bool) {
    let x = unsafe { (pa as *const u128).read_unaligned() };
    let y = unsafe { (pb as *const u128).read_unaligned() };
    let r = if signed {
        let (xs, ys) = (x as i128, y as i128);
        (match op {
            OvfOp::Add => xs.saturating_add(ys),
            OvfOp::Sub => xs.saturating_sub(ys),
            OvfOp::Mul => xs.saturating_mul(ys),
        }) as u128
    } else {
        match op {
            OvfOp::Add => x.saturating_add(y),
            OvfOp::Sub => x.saturating_sub(y),
            OvfOp::Mul => x.saturating_mul(y),
        }
    };
    unsafe { (pd as *mut u128).write_unaligned(r) };
}

/// Body of the SimdBitmask rvalue.
pub(crate) fn simd_bitmask_body(pa: *const u8, lanes: u16, lb: u8) -> u64 {
    let pa = pa as u64;
    let lb = lb as u64;
    let mut mask = 0u64;
    for i in 0..lanes as u64 {
        // On a little-endian lane the sign bit is the top bit of the last byte.
        let top = unsafe { *((pa + i * lb + lb - 1) as *const u8) };
        mask |= ((top >> 7) as u64) << i;
    }
    mask
}

/// Body of the SimdReduce rvalue.
pub(crate) fn simd_reduce_body(pa: *const u8, all: bool, lanes: u16, lb: u8) -> u64 {
    let pa = pa as u64;
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    let mut acc = all;
    for i in 0..lanes as u64 {
        let truthy = mem_read(pa + i * lb, lw) != 0;
        if all {
            acc &= truthy;
        } else {
            acc |= truthy;
        }
    }
    acc as u64
}

/// Body of the SimdReduceArith rvalue.
pub(crate) fn simd_reduce_arith_body(
    pa: *const u8,
    op: SimdReduceOp,
    lane: LaneKind,
    lanes: u16,
    lb: u8,
) -> u64 {
    use crate::vm::engine::ir::{LaneKind, SimdReduceOp as R};
    let pa = pa as u64;
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane width");
    let mut acc = mem_read(pa, lw);
    for i in 1..lanes as u64 {
        let x = mem_read(pa + i * lb, lw);
        acc = match lane {
            LaneKind::Int { signed } => match op {
                R::Add => acc.wrapping_add(x) & lw.mask(),
                R::Mul => acc.wrapping_mul(x) & lw.mask(),
                R::And => acc & x,
                R::Or => acc | x,
                R::Xor => acc ^ x,
                R::Min | R::Max => {
                    let take_x = if signed {
                        let (a, b) = (sext(acc, lw), sext(x, lw));
                        if matches!(op, R::Min) { b < a } else { b > a }
                    } else if matches!(op, R::Min) {
                        x < acc
                    } else {
                        x > acc
                    };
                    if take_x { x } else { acc }
                }
            },
            LaneKind::Float => {
                macro_rules! fr {
                    ($t:ty) => {{
                        let (fa, fx) = (<$t>::from_bits(acc as _), <$t>::from_bits(x as _));
                        (match op {
                            R::Add => fa + fx,
                            R::Mul => fa * fx,
                            // minnum/maxnum semantics, matching LLVM reduce.fmin/fmax
                            R::Min => fa.min(fx),
                            R::Max => fa.max(fx),
                            R::And | R::Or | R::Xor => engine_abort(&format!(
                                "simd reduce {op:?} does not apply to a float lane"
                            )),
                        })
                        .to_bits() as u64
                    }};
                }
                match lw {
                    Width::W32 => fr!(f32),
                    Width::W64 => fr!(f64),
                    _ => engine_abort("float lane width is not 4/8 (lowering validation gap)"),
                }
            }
        };
    }
    acc
}
