//! `eval_rvalue`: the rvalue arms -- int/float/f128/math/atomic load/SIMD reduce/Cmp128 and
//! the rest. Caller = the Assign arms in `stmt`; the mirror the JIT must match bit for bit is
//! `jit::translate::rvalue`.
//!
//! The identities and the SIMD bodies come from [`crate::vm::semantics`] and are the same ones
//! the JIT helpers call, so only the dispatch is interpreter-specific.

use super::*;
use crate::vm::semantics::arith::{
    bit_un, f128_read, host_ord, int_bin, int_cmp, int_saturating, sext,
};
use crate::vm::semantics::simd;
use crate::vm::semantics::tls::tls_addr;

pub(super) fn eval_rvalue(ctx: *mut Ctx, base: usize, rv: &Rvalue) -> u64 {
    match rv {
        Rvalue::Use(op) => eval_operand(ctx, base, op).0,
        Rvalue::TlsRef(id) => tls_addr(ctx, *id),
        Rvalue::IntBin { op, signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            int_bin(*op, *signed, av, bv, w)
        }
        Rvalue::IntCmp { cc, signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            int_cmp(*cc, *signed, av, bv, w)
        }
        Rvalue::NotBits(a) => {
            let (v, w) = eval_operand(ctx, base, a);
            !v & w.mask()
        }
        Rvalue::NotBool(a) => {
            let (v, _) = eval_operand(ctx, base, a);
            (v ^ 1) & 1
        }
        Rvalue::Neg(a) => {
            let (v, w) = eval_operand(ctx, base, a);
            v.wrapping_neg() & w.mask()
        }
        Rvalue::Cast { from, to, a } => {
            let (v, _) = eval_operand(ctx, base, a);
            let x = if from.1 {
                sext(v, from.0) as u64
            } else {
                v & from.0.mask()
            };
            x & to.mask()
        }
        Rvalue::Ref(expr) => eval_place_addr(ctx, base, expr),
        Rvalue::PtrOffset { ptr, count, stride } => {
            let (p, _) = eval_operand(ctx, base, ptr);
            let (c, cw) = eval_operand(ctx, base, count);
            // count is treated as signed (ptr::sub compiles to a negative-count offset)
            let delta = (sext(c, cw) as u64).wrapping_mul(*stride);
            p.wrapping_add(delta)
        }
        Rvalue::IntCmp3 { signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let ord = if *signed {
                sext(av, w).cmp(&sext(bv, w))
            } else {
                (av & w.mask()).cmp(&(bv & w.mask()))
            };
            (ord as i8 as u8) as u64
        }
        Rvalue::NicheDiscr {
            tag,
            niche_start,
            variants_start,
            variants_len,
            untagged,
        } => {
            let (t, w) = eval_operand(ctx, base, tag);
            let rel = t.wrapping_sub(*niche_start) & w.mask();
            if rel < *variants_len {
                variants_start + rel
            } else {
                *untagged
            }
        }
        Rvalue::FloatBin { op, fw, a, b } => {
            use crate::vm::ir::{FloatOp as F, FloatW};
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            macro_rules! fb {
                ($t:ty, $wide:expr) => {{
                    let (x, y) = (<$t>::from_bits(av as _), <$t>::from_bits(bv as _));
                    (match op {
                        F::Add => x + y,
                        F::Sub => x - y,
                        F::Mul => x * y,
                        F::Div => x / y,
                        F::Rem => x % y,
                    })
                    .to_bits() as u64
                }};
            }
            match fw {
                FloatW::F16 => fb!(f16, false),
                FloatW::F32 => fb!(f32, false),
                FloatW::F64 => fb!(f64, true),
            }
        }
        Rvalue::UMax { a, b } => eval_operand(ctx, base, a)
            .0
            .max(eval_operand(ctx, base, b).0),
        Rvalue::MathUn { op, fw, a } => {
            use crate::vm::ir::{FloatW, MathUnOp as M};
            let (av, _) = eval_operand(ctx, base, a);
            macro_rules! un {
                ($x:expr) => {{
                    let x = $x;
                    match op {
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
                    }
                }};
            }
            match fw {
                // f16 math: std implementation promotes to f32 computation then rounds back -- same lowering
                // as native for *f16 (sqrt via f32 double-rounding is mathematically safe)
                FloatW::F16 => un!(f16::from_bits(av as u16)).to_bits() as u64,
                FloatW::F32 => un!(f32::from_bits(av as u32)).to_bits() as u64,
                FloatW::F64 => un!(f64::from_bits(av)).to_bits(),
            }
        }
        Rvalue::MathBin { op, fw, a, b } => {
            use crate::vm::ir::{FloatW, MathBinOp as M};
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            macro_rules! bin {
                ($x:expr, $y:expr) => {{
                    let x = $x;
                    match op {
                        M::Pow => x.powf($y),
                        M::Powi => x.powi(bv as i32),
                        M::Copysign => x.copysign($y),
                        M::Minnum => x.min($y),
                        M::Maxnum => x.max($y),
                    }
                }};
            }
            match fw {
                FloatW::F16 => {
                    bin!(f16::from_bits(av as u16), f16::from_bits(bv as u16)).to_bits() as u64
                }
                FloatW::F32 => {
                    bin!(f32::from_bits(av as u32), f32::from_bits(bv as u32)).to_bits() as u64
                }
                FloatW::F64 => bin!(f64::from_bits(av), f64::from_bits(bv)).to_bits(),
            }
        }
        Rvalue::MathFma { fw, a, b, c } => {
            use crate::vm::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let (cv, _) = eval_operand(ctx, base, c);
            macro_rules! fma {
                ($t:ty) => {
                    <$t>::from_bits(av as _)
                        .mul_add(<$t>::from_bits(bv as _), <$t>::from_bits(cv as _))
                        .to_bits() as u64
                };
            }
            match fw {
                FloatW::F16 => fma!(f16),
                FloatW::F32 => fma!(f32),
                FloatW::F64 => fma!(f64),
            }
        }
        Rvalue::FloatCmp { cc, fw, a, b } => {
            use crate::vm::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            macro_rules! fc {
                ($t:ty) => {{
                    let (x, y) = (<$t>::from_bits(av as _), <$t>::from_bits(bv as _));
                    match cc {
                        IntCc::Eq => x == y,
                        IntCc::Ne => x != y,
                        IntCc::Lt => x < y,
                        IntCc::Le => x <= y,
                        IntCc::Gt => x > y,
                        IntCc::Ge => x >= y,
                    }
                }};
            }
            (match fw {
                FloatW::F16 => fc!(f16),
                FloatW::F32 => fc!(f32),
                FloatW::F64 => fc!(f64),
            }) as u64
        }
        Rvalue::F128Cmp { cc, a, b } => {
            let (x, y) = (
                f128_read(eval_place_addr(ctx, base, a)),
                f128_read(eval_place_addr(ctx, base, b)),
            );
            (match cc {
                IntCc::Eq => x == y,
                IntCc::Ne => x != y,
                IntCc::Lt => x < y,
                IntCc::Le => x <= y,
                IntCc::Gt => x > y,
                IntCc::Ge => x >= y,
            }) as u64
        }
        Rvalue::FloatNeg { fw, a } => {
            use crate::vm::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            match fw {
                FloatW::F16 => (-f16::from_bits(av as u16)).to_bits() as u64,
                FloatW::F32 => (-f32::from_bits(av as u32)).to_bits() as u64,
                FloatW::F64 => (-f64::from_bits(av)).to_bits(),
            }
        }
        Rvalue::FloatCast { from, to, a } => {
            use crate::vm::ir::FloatW as W;
            let (av, _) = eval_operand(ctx, base, a);
            // all-combinations host `as` (same-width bits pass through)
            match (from, to) {
                (W::F16, W::F16) | (W::F32, W::F32) | (W::F64, W::F64) => av,
                (W::F16, W::F32) => (f16::from_bits(av as u16) as f32).to_bits() as u64,
                (W::F16, W::F64) => (f16::from_bits(av as u16) as f64).to_bits(),
                (W::F32, W::F16) => (f32::from_bits(av as u32) as f16).to_bits() as u64,
                (W::F32, W::F64) => (f32::from_bits(av as u32) as f64).to_bits(),
                (W::F64, W::F16) => (f64::from_bits(av) as f16).to_bits() as u64,
                (W::F64, W::F32) => (f64::from_bits(av) as f32).to_bits() as u64,
            }
        }
        Rvalue::FloatToInt {
            from,
            to,
            signed,
            a,
        } => {
            let (av, _) = eval_operand(ctx, base, a);
            // f16/f32→f64 precisely preserves value => unify through f64; host `as` is Rust saturation semantics (NaN→0, out-of-range→boundary)
            let x = match from {
                crate::vm::ir::FloatW::F16 => f16::from_bits(av as u16) as f64,
                crate::vm::ir::FloatW::F32 => f32::from_bits(av as u32) as f64,
                crate::vm::ir::FloatW::F64 => f64::from_bits(av),
            };
            let v: u64 = if *signed {
                match to {
                    Width::W8 => x as i8 as u64,
                    Width::W16 => x as i16 as u64,
                    Width::W32 => x as i32 as u64,
                    Width::W64 => x as i64 as u64,
                }
            } else {
                match to {
                    Width::W8 => x as u8 as u64,
                    Width::W16 => x as u16 as u64,
                    Width::W32 => x as u32 as u64,
                    Width::W64 => x as u64,
                }
            };
            v & to.mask()
        }
        Rvalue::IntToFloat { from, to, a } => {
            use crate::vm::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            // Each target width uses host direct cast (`as` rounds correctly; avoid double-rounding through an intermediate)
            macro_rules! i2f {
                ($t:ty) => {
                    (if from.1 {
                        sext(av, from.0) as $t
                    } else {
                        (av & from.0.mask()) as $t
                    })
                    .to_bits() as u64
                };
            }
            match to {
                FloatW::F16 => i2f!(f16),
                FloatW::F32 => i2f!(f32),
                FloatW::F64 => i2f!(f64),
            }
        }
        Rvalue::BitUn { op, a } => {
            let (v, w) = eval_operand(ctx, base, a);
            bit_un(*op, v, w)
        }
        Rvalue::AtomicLoad { addr, width, order } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let o = host_ord(*order);
            // A guest atomic must become a real host atomic (the TSan harness's atomic-interop
            // case pins this); ordering as the guest requested (D8j)
            unsafe {
                match width {
                    Width::W8 => AtomicU8::from_ptr(p as *mut u8).load(o) as u64,
                    Width::W16 => AtomicU16::from_ptr(p as *mut u16).load(o) as u64,
                    Width::W32 => AtomicU32::from_ptr(p as *mut u32).load(o) as u64,
                    Width::W64 => AtomicU64::from_ptr(p as *mut u64).load(o),
                }
            }
        }
        Rvalue::PtrDiff { a, b, stride } => {
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            ((av.wrapping_sub(bv) as i64) / *stride as i64) as u64
        }
        Rvalue::SimdBitmask {
            a,
            lanes,
            lane_bytes,
        } => {
            let pa = eval_place_addr(ctx, base, a);
            simd::simd_bitmask_body(pa as *const u8, *lanes, *lane_bytes)
        }
        Rvalue::MemCmp { a, b, n } => {
            let (pa, _) = eval_operand(ctx, base, a);
            let (pb, _) = eval_operand(ctx, base, b);
            let (len, _) = eval_operand(ctx, base, n);
            let sa = unsafe { std::slice::from_raw_parts(pa as *const u8, len as usize) };
            let sb = unsafe { std::slice::from_raw_parts(pb as *const u8, len as usize) };
            (sa.cmp(sb) as i8 as i32) as u32 as u64
        }
        Rvalue::IntSat { op, signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            int_saturating(*op, *signed, av, bv, w)
        }
        Rvalue::SimdReduce {
            all,
            a,
            lanes,
            lane_bytes,
        } => {
            let pa = eval_place_addr(ctx, base, a);
            simd::simd_reduce_body(pa as *const u8, *all, *lanes, *lane_bytes)
        }
        Rvalue::SimdReduceArith {
            op,
            lane,
            a,
            lanes,
            lane_bytes,
        } => {
            let pa = eval_place_addr(ctx, base, a);
            simd::simd_reduce_arith_body(pa as *const u8, *op, *lane, *lanes, *lane_bytes)
        }
        Rvalue::Cmp128 { cc, signed, a, b } => {
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let (x, y) = unsafe {
                (
                    (pa as *const u128).read_unaligned(),
                    (pb as *const u128).read_unaligned(),
                )
            };
            let ord = if *signed {
                (x as i128).cmp(&(y as i128))
            } else {
                x.cmp(&y)
            };
            let t = match cc {
                IntCc::Eq => ord.is_eq(),
                IntCc::Ne => ord.is_ne(),
                IntCc::Lt => ord.is_lt(),
                IntCc::Le => ord.is_le(),
                IntCc::Gt => ord.is_gt(),
                IntCc::Ge => ord.is_ge(),
            };
            t as u64
        }
    }
}
