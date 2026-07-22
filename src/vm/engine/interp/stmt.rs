//! exec_stmt（自 interp.rs I8 整搬）：40+ Stmt 臂——原子族/memcpy-set/
//! SIMD（本体 T1-d 整搬至 simd_exec.rs，interp/JIT 共享）/128 位·f128/
//! Fence/RepeatBytes。调用方 = runblocks 主循环。

use super::*;
use super::{
    rvalue::eval_rvalue,
    volatile::{mem_read_volatile, mem_write_volatile},
};

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
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            simd_exec::simd_bin_body(
                pd as *mut u8,
                pa as *const u8,
                pb as *const u8,
                *op,
                *lane,
                *lanes,
                *lane_bytes,
            );
        }
        Stmt::SimdUn {
            op,
            lane,
            dst,
            a,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            simd_exec::simd_un_body(
                pd as *mut u8,
                pa as *const u8,
                *op,
                *lane,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_fma_body(
                pd as *mut u8,
                pa as *const u8,
                pb as *const u8,
                pc as *const u8,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_funnel_body(
                pd as *mut u8,
                pa as *const u8,
                pb as *const u8,
                ps as *const u8,
                *left,
                *lanes,
                *lane_bytes,
            );
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
            let pd = eval_place_addr(ctx, base, dst);
            let ps = eval_place_addr(ctx, base, src);
            simd_exec::simd_cast_body(
                pd as *mut u8,
                ps as *const u8,
                *lanes,
                *src_lane,
                *src_bytes,
                *dst_lane,
                *dst_bytes,
            );
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
            simd_exec::simd_select_body(
                pd as *mut u8,
                pm as *const u8,
                *mask_bytes,
                pa as *const u8,
                pb as *const u8,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_select_bitmask_body(
                pd as *mut u8,
                m,
                pa as *const u8,
                pb as *const u8,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_gather_body(
                pd as *mut u8,
                pv as *const u8,
                pp as *const u8,
                pm as *const u8,
                *mask_bytes,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_scatter_body(
                pv as *const u8,
                pp as *const u8,
                pm as *const u8,
                *mask_bytes,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_masked_load_body(
                pd as *mut u8,
                pm as *const u8,
                *mask_bytes,
                pbase,
                pv as *const u8,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_masked_store_body(
                pm as *const u8,
                *mask_bytes,
                pbase,
                pv as *const u8,
                *lanes,
                *lane_bytes,
            );
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
            let r = simd_exec::simd_extract_dyn_body(ps as *const u8, i, *lanes, *lane_bytes);
            place_write(ctx, base, dst, r);
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
            let (v, _) = eval_operand(ctx, base, val);
            simd_exec::simd_insert_dyn_body(
                pd as *mut u8,
                ps as *const u8,
                i,
                v,
                *lanes,
                *lane_bytes,
            );
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
            simd_exec::simd_arith_offset_body(
                pd as *mut u8,
                pp as *const u8,
                po as *const u8,
                *stride,
                *lanes,
            );
        }
        Stmt::SimdSplat {
            dst,
            val,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let (v, _) = eval_operand(ctx, base, val);
            simd_exec::simd_splat_body(pd as *mut u8, v, *lanes, *lane_bytes);
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
            let pb = eval_place_addr(ctx, base, b);
            let pd = eval_place_addr(ctx, base, dst);
            simd_exec::sat128_body(
                pa as *const u8,
                pb as *const u8,
                pd as *mut u8,
                *op,
                *signed,
            );
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
