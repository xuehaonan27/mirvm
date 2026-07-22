//! SIMD 15 件 + Sat128 + SIMD rvalue 三件的执行本体（T1-d 自 stmt.rs/
//! rvalue.rs 各对应臂整搬）：interp 薄壳（eval_place_addr/eval_operand 求值
//! 后调用）与 JIT 助手（mirvm_simd_stmt/mirvm_simd_rv）共享同一实现——
//! 零漂移纪律：循环体逐行保持 interp 臂原语义，engine_abort 文案逐字保留。
//! 地址参数 = 已求值的真地址裸指针（体内部即转回 u64，与 interp 臂同形）。

use super::*;
use crate::vm::engine::ir::{LaneKind, SimdBinOp, SimdReduceOp, SimdUnOp};

/// SimdBin 本体（interp stmt.rs SimdBin 臂整搬）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
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
                            engine_abort("simd 整除以零（guest UB）");
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
                    S::MinNum | S::MaxNum => {
                        engine_abort(&format!("simd {op:?} 不适用于整数 lane（lower 校验缺口）"))
                    }
                    S::Shl | S::Shr => {
                        if y >= u64::from(lw.bytes() * 8) {
                            engine_abort("simd 移位量超过 lane 位宽（guest UB）");
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
            // 浮点 lane（D8b）：IEEE 语义直算——比较不是位比较
            //（+0.0==−0.0、NaN 不自反），算术不是整数加。
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
                                    "simd {op:?} 不适用于浮点 lane（lower 校验缺口）"
                                ))
                            }
                        }
                    }};
                }
                match lw {
                    Width::W32 => fl!(f32, x, y),
                    Width::W64 => fl!(f64, x, y),
                    _ => engine_abort("浮点 lane 宽度非 4/8（lower 校验缺口）"),
                }
            }
        };
        mem_write(pd + i * lb, lw, r);
    }
}

/// SimdUn 本体（interp stmt.rs SimdUn 臂整搬）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
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
                                    "simd {op:?} 不适用于浮点 lane（lower 校验缺口）"
                                ))
                            }
                        })
                        .to_bits() as u64
                    }};
                }
                match lw {
                    Width::W32 => fu!(f32),
                    Width::W64 => fu!(f64),
                    _ => engine_abort("浮点 lane 宽度非 4/8（lower 校验缺口）"),
                }
            }
            (LaneKind::Int { .. }, other) => engine_abort(&format!(
                "simd {other:?} 不适用于整数 lane（lower 校验缺口）"
            )),
        };
        mem_write(pd + i * lb, lw, r);
    }
}

/// SimdFma 本体（interp stmt.rs SimdFma 臂整搬）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
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
            _ => engine_abort("simd_fma lane 宽度非 4/8（lower 校验缺口）"),
        };
        mem_write(pd + i * lb, lw, r);
    }
}

/// SimdFunnel 本体（interp stmt.rs SimdFunnel 臂整搬；ps = shift 向量地址）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    let bits = u64::from(lw.bytes() * 8);
    for i in 0..lanes as u64 {
        let x = mem_read(pa + i * lb, lw) as u128;
        let y = mem_read(pb + i * lb, lw) as u128;
        let s = mem_read(ps + i * lb, lw);
        if s >= bits {
            engine_abort("simd_funnel 移位量超过 lane 位宽（guest UB）");
        }
        // 拼接 [a:b]（2W 位），窗口取高/低 W 位
        let cat = (x << bits) | y;
        let r = if left { cat << s >> bits } else { cat >> s };
        mem_write(pd + i * lb, lw, r as u64 & lw.mask());
    }
}

/// SimdCast 本体（interp stmt.rs SimdCast 臂整搬；sb/db = 源/目的 lane 字节数）。
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
    let sw = Width::from_bytes(sb).expect("src lane 宽度");
    let dw = Width::from_bytes(db).expect("dst lane 宽度");
    for i in 0..lanes as u64 {
        let v = mem_read(ps + i * sb, sw);
        let r: u64 = match (src_lane, dst_lane) {
            (L::Int { signed }, L::Int { .. }) => {
                // 窄化截断 / 加宽按源符号扩展
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
                    _ => engine_abort("simd_cast 浮点 lane 宽度非 2/4/8"),
                }
            }
            (L::Float, L::Int { signed }) => {
                // f32/f16→f64 精确保值 ⇒ 统一经 f64；宿主 `as` 即饱和语义
                //（simd_as；simd_cast 界外是 guest UB，饱和值在允许集合内）
                let x = match sw {
                    Width::W16 => {
                        f64::from(f32::from_bits(crate::arch::x86_64::f16_to_f32_sw(v as u16)))
                    }
                    Width::W32 => f32::from_bits(v as u32) as f64,
                    Width::W64 => f64::from_bits(v),
                    _ => engine_abort("simd_cast 浮点 lane 宽度非 2/4/8"),
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
                // f16 lane（D8c 向量形态）：确定性软件模型——native 在
                // target_feature(f16c) 函数内经 VCVTPH2PS/VCVTPS2PH 执行硬件
                // 语义（sNaN qbit 强置等）；宿主 libcall 的 NaN 位行为随构建
                // 目标漂移，不可依赖（half 探针 h0x7c01 实锤）
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
                _ => v, // 同宽：位透传
            },
        };
        mem_write(pd + i * db, dw, r);
    }
}

/// SimdSelect 本体（interp stmt.rs SimdSelect 臂整搬；mb = mask_bytes）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    for i in 0..lanes as u64 {
        // mask lane 全 1/全 0（类型不变量）：按符号位（末字节最高位）判
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        let src = if top >> 7 != 0 { pa } else { pb };
        mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
    }
}

/// SimdSelectBitmask 本体（interp stmt.rs SimdSelectBitmask 臂整搬）。
pub(crate) fn simd_select_bitmask_body(
    pd: *mut u8,
    mask: u64,
    pa: *const u8,
    pb: *const u8,
    lanes: u16,
    lb: u8,
) {
    let m = mask;
    let (pd, pa, pb) = (pd as u64, pa as u64, pb as u64);
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    for i in 0..lanes as u64 {
        let src = if m >> i & 1 != 0 { pa } else { pb };
        mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
    }
}

/// SimdGather 本体（interp stmt.rs SimdGather 臂整搬；ppass = passthru 地址，
/// pp = ptrs 地址，pm = mask 地址）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    for i in 0..lanes as u64 {
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        // 假 lane 绝不佯读（指针可能无效——这正是 mask 的语义）
        let v = if top >> 7 != 0 {
            mem_read(mem_read(pp + i * 8, Width::W64), lw)
        } else {
            mem_read(pv + i * lb, lw)
        };
        mem_write(pd + i * lb, lw, v);
    }
}

/// SimdScatter 本体（interp stmt.rs SimdScatter 臂整搬；pv = values 地址）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
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

/// SimdMaskedLoad 本体（interp stmt.rs SimdMaskedLoad 臂整搬；base = 已求值的
/// 元素基址标量，ppass = passthru 地址）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
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

/// SimdMaskedStore 本体（interp stmt.rs SimdMaskedStore 臂整搬）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    for i in 0..lanes as u64 {
        let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
        if top >> 7 != 0 {
            mem_write(pbase + i * lb, lw, mem_read(pv + i * lb, lw));
        }
    }
}

/// SimdExtractDyn 本体（interp stmt.rs SimdExtractDyn 臂整搬；返回抽出的
/// lane 标量，落点写回由调用方做——interp 侧 place_write / JIT 侧
/// write_scalar_place）。
pub(crate) fn simd_extract_dyn_body(ps: *const u8, idx: u64, lanes: u16, lb: u8) -> u64 {
    let ps = ps as u64;
    let i = idx;
    if i >= u64::from(lanes) {
        engine_abort(&format!(
            "simd_extract_dyn 索引 {i} 越界（lanes={lanes}，guest UB）"
        ));
    }
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    mem_read(ps + i * lb, lw)
}

/// SimdInsertDyn 本体（interp stmt.rs SimdInsertDyn 臂整搬；v = 已求值的
/// 插入标量）。
pub(crate) fn simd_insert_dyn_body(
    pd: *mut u8,
    ps: *const u8,
    idx: u64,
    v: u64,
    lanes: u16,
    lb: u8,
) {
    let (pd, ps) = (pd as u64, ps as u64);
    let i = idx;
    if i >= u64::from(lanes) {
        engine_abort(&format!(
            "simd_insert_dyn 索引 {i} 越界（lanes={lanes}，guest UB）"
        ));
    }
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    let total = u64::from(lanes) * lb;
    // dst 可能与 src 同址（x = insert_dyn(x,…)）：整体搬运用 memmove
    unsafe {
        std::ptr::copy(ps as *const u8, pd as *mut u8, total as usize);
    }
    mem_write(pd + i * lb, lw, v);
}

/// SimdArithOffset 本体（interp stmt.rs SimdArithOffset 臂整搬；恒 lanes×8）。
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

/// SimdSplat 本体（interp stmt.rs SimdSplat 臂整搬）。
pub(crate) fn simd_splat_body(pd: *mut u8, v: u64, lanes: u16, lb: u8) {
    let pd = pd as u64;
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane 宽度");
    for i in 0..lanes as u64 {
        mem_write(pd + i * lb, lw, v);
    }
}

/// Sat128 本体（interp stmt.rs Sat128 臂整搬）。
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

/// SimdBitmask 本体（interp rvalue.rs SimdBitmask 臂整搬）。
pub(crate) fn simd_bitmask_body(pa: *const u8, lanes: u16, lb: u8) -> u64 {
    let pa = pa as u64;
    let lb = lb as u64;
    let mut mask = 0u64;
    for i in 0..lanes as u64 {
        // 小端 lane 的符号位在末字节最高位
        let top = unsafe { *((pa + i * lb + lb - 1) as *const u8) };
        mask |= ((top >> 7) as u64) << i;
    }
    mask
}

/// SimdReduce 本体（interp rvalue.rs SimdReduce 臂整搬）。
pub(crate) fn simd_reduce_body(pa: *const u8, all: bool, lanes: u16, lb: u8) -> u64 {
    let pa = pa as u64;
    let lb = lb as u64;
    let lw = Width::from_bytes(lb).expect("lane 宽度");
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

/// SimdReduceArith 本体（interp rvalue.rs SimdReduceArith 臂整搬）。
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
    let lw = Width::from_bytes(lb).expect("lane 宽度");
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
                            // minnum/maxnum 语义（与 LLVM reduce.fmin/fmax 一致）
                            R::Min => fa.min(fx),
                            R::Max => fa.max(fx),
                            R::And | R::Or | R::Xor => {
                                engine_abort(&format!("simd reduce {op:?} 不适用于浮点 lane"))
                            }
                        })
                        .to_bits() as u64
                    }};
                }
                match lw {
                    Width::W32 => fr!(f32),
                    Width::W64 => fr!(f64),
                    _ => engine_abort("浮点 lane 宽度非 4/8（lower 校验缺口）"),
                }
            }
        };
    }
    acc
}
