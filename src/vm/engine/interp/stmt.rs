//! exec_stmt（自 interp.rs I8 整搬）：40+ Stmt 臂——原子族/memcpy-set/
//! SIMD（544 行）/128 位·f128（306 行）/Fence/RepeatBytes。调用方 =
//! runblocks 主循环；SIMD/128 子带的臂级再拆记档（战役 §7.16 不做）。

use super::*;
use super::{rvalue::eval_rvalue, volatile::{mem_read_volatile, mem_write_volatile}};

pub(super) fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign { dst, rv } => {
            let v = eval_rvalue(ctx, base, rv);
            place_write(ctx, base, dst, v);
        }
        Stmt::AssignOverflow {
            op,
            signed,
            a,
            b,
            dst_val,
            dst_flag,
        } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let (v, f) = int_ovf(*op, *signed, av, bv, w);
            place_write(ctx, base, dst_val, v);
            place_write(ctx, base, dst_flag, f as u64);
        }
        Stmt::Copy { dst, src, size } => {
            let d = eval_place_addr(ctx, base, dst);
            let s = eval_place_addr(ctx, base, src);
            // memmove 语义（guest 侧重叠是 UB，但引擎自身不因此崩——防御性）
            unsafe { std::ptr::copy(s as *const u8, d as *mut u8, *size as usize) };
        }
        Stmt::RepeatScalar {
            dst,
            val,
            count,
            elem_size,
        } => {
            let d = eval_place_addr(ctx, base, dst);
            let (v, w) = eval_operand(ctx, base, val);
            debug_assert_eq!(w.bytes(), *elem_size);
            for i in 0..*count {
                mem_write(d + i * *elem_size as u64, w, v);
            }
        }
        Stmt::AtomicStore { addr, val, order } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (v, w) = eval_operand(ctx, base, val);
            let o = host_ord(*order);
            unsafe {
                match w {
                    Width::W8 => AtomicU8::from_ptr(p as *mut u8).store(v as u8, o),
                    Width::W16 => AtomicU16::from_ptr(p as *mut u16).store(v as u16, o),
                    Width::W32 => AtomicU32::from_ptr(p as *mut u32).store(v as u32, o),
                    Width::W64 => AtomicU64::from_ptr(p as *mut u64).store(v, o),
                }
            }
        }
        Stmt::VolatileLoad { addr, dst, size } => {
            let (p, _) = eval_operand(ctx, base, addr);
            let d = eval_place_addr(ctx, base, dst);
            mem_read_volatile(p, d, *size);
        }
        Stmt::VolatileStore { addr, src, size } => {
            let (p, _) = eval_operand(ctx, base, addr);
            let s = eval_place_addr(ctx, base, src);
            mem_write_volatile(p, s, *size);
        }
        Stmt::AtomicCxchg {
            addr,
            expected,
            new,
            dst_val,
            dst_ok,
            weak,
            succ,
            fail,
        } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (e, w) = eval_operand(ctx, base, expected);
            let (n, _) = eval_operand(ctx, base, new);
            macro_rules! cx {
                ($t:ty, $at:ty) => {{
                    let a = unsafe { <$at>::from_ptr(p as *mut $t) };
                    let (so, fo) = (host_ord(*succ), host_ord(*fail));
                    let r = if *weak {
                        a.compare_exchange_weak(e as $t, n as $t, so, fo)
                    } else {
                        a.compare_exchange(e as $t, n as $t, so, fo)
                    };
                    match r {
                        Ok(old) => (old as u64, 1u64),
                        Err(old) => (old as u64, 0u64),
                    }
                }};
            }
            let (old, ok) = match w {
                Width::W8 => cx!(u8, AtomicU8),
                Width::W16 => cx!(u16, AtomicU16),
                Width::W32 => cx!(u32, AtomicU32),
                Width::W64 => cx!(u64, AtomicU64),
            };
            place_write(ctx, base, dst_val, old);
            place_write(ctx, base, dst_ok, ok);
        }
        Stmt::AtomicRmw {
            op,
            addr,
            val,
            dst,
            order,
        } => {
            use crate::vm::engine::ir::RmwOp as R;
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (v, w) = eval_operand(ctx, base, val);
            let o = host_ord(*order);
            macro_rules! rmw {
                ($t:ty, $at:ty, $it:ty, $iat:ty) => {{
                    let a = unsafe { <$at>::from_ptr(p as *mut $t) };
                    (match op {
                        R::Xchg => a.swap(v as $t, o),
                        R::Add => a.fetch_add(v as $t, o),
                        R::Sub => a.fetch_sub(v as $t, o),
                        R::And => a.fetch_and(v as $t, o),
                        R::Or => a.fetch_or(v as $t, o),
                        R::Xor => a.fetch_xor(v as $t, o),
                        R::Nand => a.fetch_nand(v as $t, o),
                        // fetch_max/min：有符号变体经同址 AtomicI*（位型回写零扩展）
                        R::UMax => a.fetch_max(v as $t, o),
                        R::UMin => a.fetch_min(v as $t, o),
                        R::Max => unsafe { <$iat>::from_ptr(p as *mut $it) }
                            .fetch_max(v as $it, o) as $t,
                        R::Min => unsafe { <$iat>::from_ptr(p as *mut $it) }
                            .fetch_min(v as $it, o) as $t,
                    }) as u64
                }};
            }
            let old = match w {
                Width::W8 => rmw!(u8, AtomicU8, i8, AtomicI8),
                Width::W16 => rmw!(u16, AtomicU16, i16, AtomicI16),
                Width::W32 => rmw!(u32, AtomicU32, i32, AtomicI32),
                Width::W64 => rmw!(u64, AtomicU64, i64, AtomicI64),
            };
            place_write(ctx, base, dst, old);
        }
        Stmt::MemCopy {
            dst,
            src,
            count,
            elem_size,
            overlap,
        } => {
            let (d, _) = eval_operand(ctx, base, dst);
            let (s, _) = eval_operand(ctx, base, src);
            let (c, _) = eval_operand(ctx, base, count);
            let bytes = (c as usize).wrapping_mul(*elem_size as usize);
            unsafe {
                if *overlap {
                    std::ptr::copy(s as *const u8, d as *mut u8, bytes);
                } else {
                    std::ptr::copy_nonoverlapping(s as *const u8, d as *mut u8, bytes);
                }
            }
        }
        Stmt::MemSet {
            dst,
            val,
            count,
            elem_size,
        } => {
            let (d, _) = eval_operand(ctx, base, dst);
            let (v, _) = eval_operand(ctx, base, val);
            let (c, _) = eval_operand(ctx, base, count);
            let bytes = (c as usize).wrapping_mul(*elem_size as usize);
            unsafe { std::ptr::write_bytes(d as *mut u8, v as u8, bytes) };
        }
        Stmt::SimdBin {
            op,
            lane,
            dst,
            a,
            b,
            lanes,
            lane_bytes,
        } => {
            use crate::vm::engine::ir::{LaneKind, SimdBinOp as S};
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw);
                let y = mem_read(pb + i * lb, lw);
                let r: u64 = match *lane {
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
                            S::MinNum | S::MaxNum => engine_abort(&format!(
                                "simd {op:?} 不适用于整数 lane（lower 校验缺口）"
                            )),
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
                                let (fx, fy) = (
                                    <$t>::from_bits($xb as _),
                                    <$t>::from_bits($yb as _),
                                );
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
                                    S::And | S::Or | S::Xor | S::SatAdd | S::SatSub
                                    | S::Shl | S::Shr => engine_abort(&format!(
                                        "simd {op:?} 不适用于浮点 lane（lower 校验缺口）"
                                    )),
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
        Stmt::SimdUn {
            op,
            lane,
            dst,
            a,
            lanes,
            lane_bytes,
        } => {
            use crate::vm::engine::ir::{BitUnOp, LaneKind, SimdUnOp as U};
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw);
                let r: u64 = match (*lane, op) {
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
                                    U::Ctlz | U::Cttz | U::Ctpop | U::Bswap
                                    | U::Bitreverse => engine_abort(&format!(
                                        "simd {op:?} 不适用于浮点 lane（lower 校验缺口）"
                                    )),
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
        Stmt::SimdFma {
            dst,
            a,
            b,
            c,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let pc = eval_place_addr(ctx, base, c);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
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
        Stmt::SimdFunnel {
            left,
            dst,
            a,
            b,
            shift,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let ps = eval_place_addr(ctx, base, shift);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            let bits = u64::from(lw.bytes() * 8);
            for i in 0..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw) as u128;
                let y = mem_read(pb + i * lb, lw) as u128;
                let s = mem_read(ps + i * lb, lw);
                if s >= bits {
                    engine_abort("simd_funnel 移位量超过 lane 位宽（guest UB）");
                }
                // 拼接 [a:b]（2W 位），窗口取高/低 W 位
                let cat = (x << bits) | y;
                let r = if *left { cat << s >> bits } else { cat >> s };
                mem_write(pd + i * lb, lw, r as u64 & lw.mask());
            }
        }
        Stmt::SimdCast {
            dst,
            src,
            lanes,
            src_lane,
            src_bytes,
            dst_lane,
            dst_bytes,
        } => {
            use crate::vm::engine::ir::LaneKind as L;
            let pd = eval_place_addr(ctx, base, dst);
            let ps = eval_place_addr(ctx, base, src);
            let (sb, db) = (*src_bytes as u64, *dst_bytes as u64);
            let sw = Width::from_bytes(sb).expect("src lane 宽度");
            let dw = Width::from_bytes(db).expect("dst lane 宽度");
            for i in 0..*lanes as u64 {
                let v = mem_read(ps + i * sb, sw);
                let r: u64 = match (*src_lane, *dst_lane) {
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
                        (Width::W16, Width::W32) => {
                            u64::from(crate::arch::x86_64::f16_to_f32_sw(v as u16))
                        }
                        (Width::W16, Width::W64) => {
                            f64::from(f32::from_bits(crate::arch::x86_64::f16_to_f32_sw(v as u16)))
                                .to_bits()
                        }
                        (Width::W32, Width::W16) => {
                            u64::from(crate::arch::x86_64::f32_to_f16_sw(
                                v as u32,
                                crate::arch::x86_64::HalfRound::Rne,
                            ))
                        }
                        (Width::W64, Width::W16) => {
                            (f64::from_bits(v) as f16).to_bits() as u64
                        }
                        _ => v, // 同宽：位透传
                    },
                };
                mem_write(pd + i * db, dw, r);
            }
        }
        Stmt::SimdSelect {
            mask,
            mask_bytes,
            a,
            b,
            dst,
            lanes,
            lane_bytes,
        } => {
            let pm = eval_place_addr(ctx, base, mask);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let pd = eval_place_addr(ctx, base, dst);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                // mask lane 全 1/全 0（类型不变量）：按符号位（末字节最高位）判
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                let src = if top >> 7 != 0 { pa } else { pb };
                mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
            }
        }
        Stmt::SimdSelectBitmask {
            mask,
            a,
            b,
            dst,
            lanes,
            lane_bytes,
        } => {
            let (m, _) = eval_operand(ctx, base, mask);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let pd = eval_place_addr(ctx, base, dst);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let src = if m >> i & 1 != 0 { pa } else { pb };
                mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
            }
        }
        Stmt::SimdGather {
            passthru,
            ptrs,
            mask,
            mask_bytes,
            dst,
            lanes,
            lane_bytes,
        } => {
            let pv = eval_place_addr(ctx, base, passthru);
            let pp = eval_place_addr(ctx, base, ptrs);
            let pm = eval_place_addr(ctx, base, mask);
            let pd = eval_place_addr(ctx, base, dst);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
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
        Stmt::SimdScatter {
            values,
            ptrs,
            mask,
            mask_bytes,
            lanes,
            lane_bytes,
        } => {
            let pv = eval_place_addr(ctx, base, values);
            let pp = eval_place_addr(ctx, base, ptrs);
            let pm = eval_place_addr(ctx, base, mask);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
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
        Stmt::SimdMaskedLoad {
            mask,
            mask_bytes,
            base: base_op,
            passthru,
            dst,
            lanes,
            lane_bytes,
        } => {
            let pm = eval_place_addr(ctx, base, mask);
            let (pbase, _) = eval_operand(ctx, base, base_op);
            let pv = eval_place_addr(ctx, base, passthru);
            let pd = eval_place_addr(ctx, base, dst);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                let v = if top >> 7 != 0 {
                    mem_read(pbase + i * lb, lw)
                } else {
                    mem_read(pv + i * lb, lw)
                };
                mem_write(pd + i * lb, lw, v);
            }
        }
        Stmt::SimdMaskedStore {
            mask,
            mask_bytes,
            base: base_op,
            values,
            lanes,
            lane_bytes,
        } => {
            let pm = eval_place_addr(ctx, base, mask);
            let (pbase, _) = eval_operand(ctx, base, base_op);
            let pv = eval_place_addr(ctx, base, values);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                if top >> 7 != 0 {
                    mem_write(pbase + i * lb, lw, mem_read(pv + i * lb, lw));
                }
            }
        }
        Stmt::SimdExtractDyn {
            src,
            idx,
            dst,
            lanes,
            lane_bytes,
        } => {
            let ps = eval_place_addr(ctx, base, src);
            let (i, _) = eval_operand(ctx, base, idx);
            if i >= u64::from(*lanes) {
                engine_abort(&format!(
                    "simd_extract_dyn 索引 {i} 越界（lanes={lanes}，guest UB）"
                ));
            }
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            place_write(ctx, base, dst, mem_read(ps + i * lb, lw));
        }
        Stmt::SimdInsertDyn {
            src,
            idx,
            val,
            dst,
            lanes,
            lane_bytes,
        } => {
            let ps = eval_place_addr(ctx, base, src);
            let pd = eval_place_addr(ctx, base, dst);
            let (i, _) = eval_operand(ctx, base, idx);
            if i >= u64::from(*lanes) {
                engine_abort(&format!(
                    "simd_insert_dyn 索引 {i} 越界（lanes={lanes}，guest UB）"
                ));
            }
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            let (v, _) = eval_operand(ctx, base, val);
            let total = u64::from(*lanes) * lb;
            // dst 可能与 src 同址（x = insert_dyn(x,…)）：整体搬运用 memmove
            unsafe {
                std::ptr::copy(ps as *const u8, pd as *mut u8, total as usize);
            }
            mem_write(pd + i * lb, lw, v);
        }
        Stmt::SimdArithOffset {
            ptrs,
            offsets,
            stride,
            dst,
            lanes,
        } => {
            let pp = eval_place_addr(ctx, base, ptrs);
            let po = eval_place_addr(ctx, base, offsets);
            let pd = eval_place_addr(ctx, base, dst);
            for i in 0..*lanes as u64 {
                let p = mem_read(pp + i * 8, Width::W64);
                let off = mem_read(po + i * 8, Width::W64);
                mem_write(
                    pd + i * 8,
                    Width::W64,
                    p.wrapping_add(off.wrapping_mul(*stride)),
                );
            }
        }
        Stmt::SimdSplat {
            dst,
            val,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let (v, _) = eval_operand(ctx, base, val);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                mem_write(pd + i * lb, lw, v);
            }
        }
        Stmt::Bin128 {
            op,
            signed,
            a,
            b,
            dst,
            with_overflow,
        } => {
            use crate::vm::engine::ir::Bin128Rhs;
            let pa = eval_place_addr(ctx, base, a);
            let x = unsafe { (pa as *const u128).read_unaligned() };
            let y = match b {
                Bin128Rhs::Wide(pb) => {
                    let pb = eval_place_addr(ctx, base, pb);
                    unsafe { (pb as *const u128).read_unaligned() }
                }
                Bin128Rhs::Scalar(o) => eval_operand(ctx, base, o).0 as u128,
            };
            let pd = eval_place_addr(ctx, base, dst);
            let (r, ovf): (u128, bool) = if *signed {
                let (xs, ys) = (x as i128, y as i128);
                let (v, o) = match op {
                    IntBinOp::Add => xs.overflowing_add(ys),
                    IntBinOp::Sub => xs.overflowing_sub(ys),
                    IntBinOp::Mul => xs.overflowing_mul(ys),
                    IntBinOp::Div => {
                        if ys == 0 {
                            engine_abort("guest 128 位整除以零");
                        }
                        (xs.wrapping_div(ys), false)
                    }
                    IntBinOp::Rem => {
                        if ys == 0 {
                            engine_abort("guest 128 位取余以零");
                        }
                        (xs.wrapping_rem(ys), false)
                    }
                    IntBinOp::BitAnd => (xs & ys, false),
                    IntBinOp::BitOr => (xs | ys, false),
                    IntBinOp::BitXor => (xs ^ ys, false),
                    IntBinOp::Shl => (xs.wrapping_shl(y as u32), false),
                    IntBinOp::Shr => (xs.wrapping_shr(y as u32), false),
                };
                (v as u128, o)
            } else {
                let (v, o) = match op {
                    IntBinOp::Add => x.overflowing_add(y),
                    IntBinOp::Sub => x.overflowing_sub(y),
                    IntBinOp::Mul => x.overflowing_mul(y),
                    IntBinOp::Div => {
                        if y == 0 {
                            engine_abort("guest 128 位整除以零");
                        }
                        (x / y, false)
                    }
                    IntBinOp::Rem => {
                        if y == 0 {
                            engine_abort("guest 128 位取余以零");
                        }
                        (x % y, false)
                    }
                    IntBinOp::BitAnd => (x & y, false),
                    IntBinOp::BitOr => (x | y, false),
                    IntBinOp::BitXor => (x ^ y, false),
                    IntBinOp::Shl => (x.wrapping_shl(y as u32), false),
                    IntBinOp::Shr => (x.wrapping_shr(y as u32), false),
                };
                (v, o)
            };
            unsafe { (pd as *mut u128).write_unaligned(r) };
            if *with_overflow {
                unsafe { *((pd + 16) as *mut u8) = ovf as u8 };
            }
        }
        Stmt::Sat128 {
            op,
            signed,
            a,
            b,
            dst,
        } => {
            let pa = eval_place_addr(ctx, base, a);
            let x = unsafe { (pa as *const u128).read_unaligned() };
            let pb = eval_place_addr(ctx, base, b);
            let y = unsafe { (pb as *const u128).read_unaligned() };
            let r = if *signed {
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
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(r) };
        }
        Stmt::NicheDiscr128 {
            tag,
            niche_start,
            variants_start,
            variants_len,
            untagged,
            dst,
        } => {
            let p = eval_place_addr(ctx, base, tag);
            let t = unsafe { (p as *const u128).read_unaligned() };
            let rel = t.wrapping_sub(*niche_start);
            let v = if rel < *variants_len as u128 {
                variants_start.wrapping_add(rel as u64)
            } else {
                *untagged
            };
            place_write(ctx, base, dst, v);
        }
        Stmt::Wide128ToFloat {
            src,
            signed,
            to,
            dst,
        } => {
            use crate::vm::engine::ir::FloatW;
            let p = eval_place_addr(ctx, base, src);
            let x = unsafe { (p as *const u128).read_unaligned() };
            macro_rules! w2f {
                ($t:ty) => {
                    (if *signed { x as i128 as $t } else { x as $t }).to_bits() as u64
                };
            }
            let bits = match to {
                FloatW::F16 => w2f!(f16),
                FloatW::F32 => w2f!(f32),
                FloatW::F64 => w2f!(f64),
            };
            place_write(ctx, base, dst, bits);
        }
        Stmt::Bit128 { op, src, dst } => {
            use crate::vm::engine::ir::BitUnOp as B;
            let x = unsafe { (eval_place_addr(ctx, base, src) as *const u128).read_unaligned() };
            let r = match op {
                B::Bswap => x.swap_bytes(),
                B::Bitreverse => x.reverse_bits(),
                _ => engine_abort("Bit128 只承载 bswap/bitreverse"),
            };
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(r) };
        }
        Stmt::Bit128Count { op, src, dst } => {
            use crate::vm::engine::ir::BitUnOp as B;
            let x = unsafe { (eval_place_addr(ctx, base, src) as *const u128).read_unaligned() };
            let r = match op {
                B::Popcount => x.count_ones(),
                B::Ctlz => x.leading_zeros(),
                B::Cttz => x.trailing_zeros(),
                _ => engine_abort("Bit128Count 只承载 ctpop/ctlz/cttz"),
            };
            place_write(ctx, base, dst, r as u64);
        }
        Stmt::FloatToWide128 {
            src,
            from,
            signed,
            dst,
        } => {
            use crate::vm::engine::ir::FloatW;
            let (v, _) = eval_operand(ctx, base, src);
            // f16/f32→f64 精确保值 ⇒ 统一经 f64；宿主 `as` 即饱和语义（NaN→0、越界→边界）
            let x = match from {
                FloatW::F16 => f16::from_bits(v as u16) as f64,
                FloatW::F32 => f32::from_bits(v as u32) as f64,
                FloatW::F64 => f64::from_bits(v),
            };
            let bits: u128 = if *signed {
                x as i128 as u128
            } else {
                x as u128
            };
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(bits) };
        }
        // ===== f128 宽通道（D8c）=====
        Stmt::F128Bin { op, a, b, dst } => {
            use crate::vm::engine::ir::FloatOp as F;
            let (x, y) = (
                f128_read(eval_place_addr(ctx, base, a)),
                f128_read(eval_place_addr(ctx, base, b)),
            );
            let r = match op {
                F::Add => x + y,
                F::Sub => x - y,
                F::Mul => x * y,
                F::Div => x / y,
                F::Rem => x % y,
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128MathBin { op, a, b, dst } => {
            use crate::vm::engine::ir::{F128Rhs, MathBinOp as M};
            let x = f128_read(eval_place_addr(ctx, base, a));
            let r = match (op, b) {
                (M::Powi, F128Rhs::Scalar(o)) => x.powi(eval_operand(ctx, base, o).0 as i32),
                (M::Powi, F128Rhs::Wide(_)) => engine_abort("f128 powi rhs 形态"),
                (op, F128Rhs::Wide(pb)) => {
                    let y = f128_read(eval_place_addr(ctx, base, pb));
                    match op {
                        M::Pow => x.powf(y),
                        M::Copysign => x.copysign(y),
                        M::Minnum => x.min(y),
                        M::Maxnum => x.max(y),
                        M::Powi => unreachable!(),
                    }
                }
                (_, F128Rhs::Scalar(_)) => engine_abort("f128 math rhs 形态"),
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128Un { op, a, dst } => {
            use crate::vm::engine::ir::{F128UnOp as U, MathUnOp as M};
            let x = f128_read(eval_place_addr(ctx, base, a));
            let r = match op {
                U::Neg => -x,
                U::Math(m) => match m {
                    M::Sqrt => x.sqrt(),
                    M::Sin => x.sin(),
                    M::Cos => x.cos(),
                    M::Exp => x.exp(),
                    M::Exp2 => x.exp2(),
                    M::Ln => x.ln(),
                    M::Log2 => x.log2(),
                    M::Log10 => x.log10(),
                    M::Fabs => x.abs(),
                    M::Floor => x.floor(),
                    M::Ceil => x.ceil(),
                    M::Trunc => x.trunc(),
                    M::Round => x.round(),
                    M::RoundTiesEven => x.round_ties_even(),
                },
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128Fma { a, b, c, dst } => {
            let x = f128_read(eval_place_addr(ctx, base, a));
            let y = f128_read(eval_place_addr(ctx, base, b));
            let z = f128_read(eval_place_addr(ctx, base, c));
            f128_write(eval_place_addr(ctx, base, dst), x.mul_add(y, z));
        }
        Stmt::F128FromScalar { src, kind, dst } => {
            use crate::vm::engine::ir::{F128Scalar as K, FloatW};
            let (v, w) = eval_operand(ctx, base, src);
            let r: f128 = match kind {
                K::F(FloatW::F16) => f16::from_bits(v as u16) as f128,
                K::F(FloatW::F32) => f32::from_bits(v as u32) as f128,
                K::F(FloatW::F64) => f64::from_bits(v) as f128,
                K::Int { signed: true } => sext(v, w) as f128,
                K::Int { signed: false } => (v & w.mask()) as f128,
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128ToScalar { src, kind, w, dst } => {
            use crate::vm::engine::ir::{F128Scalar as K, FloatW};
            let x = f128_read(eval_place_addr(ctx, base, src));
            let bits: u64 = match kind {
                K::F(FloatW::F16) => (x as f16).to_bits() as u64,
                K::F(FloatW::F32) => (x as f32).to_bits() as u64,
                K::F(FloatW::F64) => (x as f64).to_bits(),
                // `as` 饱和语义（NaN→0、越界→边界）
                K::Int { signed: true } => match w {
                    Width::W8 => x as i8 as u64,
                    Width::W16 => x as i16 as u64,
                    Width::W32 => x as i32 as u64,
                    Width::W64 => x as i64 as u64,
                },
                K::Int { signed: false } => match w {
                    Width::W8 => x as u8 as u64,
                    Width::W16 => x as u16 as u64,
                    Width::W32 => x as u32 as u64,
                    Width::W64 => x as u64,
                },
            };
            place_write(ctx, base, dst, bits & w.mask());
        }
        Stmt::F128FromWideInt { src, signed, dst } => {
            let p = eval_place_addr(ctx, base, src);
            let x = unsafe { (p as *const u128).read_unaligned() };
            let r: f128 = if *signed {
                x as i128 as f128
            } else {
                x as f128
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128ToWideInt { src, signed, dst } => {
            let x = f128_read(eval_place_addr(ctx, base, src));
            let bits: u128 = if *signed {
                x as i128 as u128
            } else {
                x as u128
            };
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(bits) };
        }
        Stmt::Trap(reason) => engine_abort(&format!("TRAP: {reason}")),
        Stmt::Nop => {}
        // `[expr; N]` 聚合元素：dst[0] 为模板铺满其余
        Stmt::RepeatBytes {
            first,
            count,
            elem_size,
        } => {
            let src = eval_place_addr(ctx, base, first);
            for i in 1..*count {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src as *const u8,
                        (src + i * elem_size) as *mut u8,
                        *elem_size as usize,
                    );
                }
            }
        }
        // 栅栏补真（M4.4 D4）：guest 任意序 → 宿主 SeqCst（最强序在 RAM non-det 包络内）
        Stmt::Fence {
            single_thread,
            order,
        } => {
            use std::sync::atomic::{compiler_fence, fence};
            let o = host_ord(*order);
            if *single_thread {
                compiler_fence(o);
            } else {
                fence(o);
            }
        }
    }
}
