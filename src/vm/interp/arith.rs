//! The shared arithmetic mirrors: sign extension, integer binary and comparison, the checked and
//! saturating families, bit operations, the atomic ordering a guest memory order maps to, and the
//! f128 read. The JIT's translator mirrors these bit for bit.

use super::*;

/// Sign-extends `bits` to i64 at its declared width.
#[inline]
pub(crate) fn sext(bits: u64, w: Width) -> i64 {
    match w {
        Width::W8 => bits as u8 as i8 as i64,
        Width::W16 => bits as u16 as i16 as i64,
        Width::W32 => bits as u32 as i32 as i64,
        Width::W64 => bits as i64,
    }
}

pub(crate) fn int_bin(op: IntBinOp, signed: bool, a: u64, b: u64, w: Width) -> u64 {
    let m = w.mask();
    let r = if signed {
        let (x, y) = (sext(a, w), sext(b, w));
        match op {
            IntBinOp::Add => x.wrapping_add(y) as u64,
            IntBinOp::Sub => x.wrapping_sub(y) as u64,
            IntBinOp::Mul => x.wrapping_mul(y) as u64,
            IntBinOp::Div => {
                if y == 0 {
                    engine_abort("guest integer division by zero");
                }
                x.wrapping_div(y) as u64
            }
            IntBinOp::Rem => {
                if y == 0 {
                    engine_abort("guest integer remainder by zero");
                }
                x.wrapping_rem(y) as u64
            }
            IntBinOp::BitAnd => a & b,
            IntBinOp::BitOr => a | b,
            IntBinOp::BitXor => a ^ b,
            IntBinOp::Shl => (x as u64).wrapping_shl(b as u32),
            IntBinOp::Shr => (x >> (b as u32 & 63)) as u64, // arithmetic shift right
        }
    } else {
        match op {
            IntBinOp::Add => a.wrapping_add(b),
            IntBinOp::Sub => a.wrapping_sub(b),
            IntBinOp::Mul => a.wrapping_mul(b),
            IntBinOp::Div => {
                if b == 0 {
                    engine_abort("guest integer division by zero");
                }
                a / b
            }
            IntBinOp::Rem => {
                if b == 0 {
                    engine_abort("guest integer remainder by zero");
                }
                a % b
            }
            IntBinOp::BitAnd => a & b,
            IntBinOp::BitOr => a | b,
            IntBinOp::BitXor => a ^ b,
            IntBinOp::Shl => a.wrapping_shl(b as u32),
            IntBinOp::Shr => (a & m).wrapping_shr(b as u32), // logical shift right
        }
    };
    r & m
}

/// f128 place bit read/write, safe for 16-byte unaligned access.
pub(crate) fn f128_read(p: u64) -> f128 {
    f128::from_bits(unsafe { (p as *const u128).read_unaligned() })
}
pub(crate) fn f128_write(p: u64, v: f128) {
    unsafe { (p as *mut u128).write_unaligned(v.to_bits()) }
}

/// Frozen `MemOrd` to host `Ordering`: the guard order the guest asked for is the one
/// executed.
pub(crate) fn host_ord(o: super::super::ir::MemOrd) -> std::sync::atomic::Ordering {
    use std::sync::atomic::Ordering as O;
    match o {
        super::super::ir::MemOrd::Relaxed => O::Relaxed,
        super::super::ir::MemOrd::Acquire => O::Acquire,
        super::super::ir::MemOrd::Release => O::Release,
        super::super::ir::MemOrd::AcqRel => O::AcqRel,
        super::super::ir::MemOrd::SeqCst => O::SeqCst,
    }
}

/// Bitwise unary ops, shared by the BitUn rvalue and SIMD lanes.
pub(crate) fn bit_un(op: super::super::ir::BitUnOp, v: u64, w: Width) -> u64 {
    use super::super::ir::BitUnOp as B;
    match (op, w) {
        (B::Popcount, _) => (v & w.mask()).count_ones() as u64,
        (B::Ctlz, Width::W8) => (v as u8).leading_zeros() as u64,
        (B::Ctlz, Width::W16) => (v as u16).leading_zeros() as u64,
        (B::Ctlz, Width::W32) => (v as u32).leading_zeros() as u64,
        (B::Ctlz, Width::W64) => v.leading_zeros() as u64,
        (B::Cttz, Width::W8) => (v as u8).trailing_zeros() as u64,
        (B::Cttz, Width::W16) => (v as u16).trailing_zeros() as u64,
        (B::Cttz, Width::W32) => (v as u32).trailing_zeros() as u64,
        (B::Cttz, Width::W64) => v.trailing_zeros() as u64,
        (B::Bswap, Width::W8) => v & 0xff,
        (B::Bswap, Width::W16) => (v as u16).swap_bytes() as u64,
        (B::Bswap, Width::W32) => (v as u32).swap_bytes() as u64,
        (B::Bswap, Width::W64) => v.swap_bytes(),
        (B::Bitreverse, Width::W8) => (v as u8).reverse_bits() as u64,
        (B::Bitreverse, Width::W16) => (v as u16).reverse_bits() as u64,
        (B::Bitreverse, Width::W32) => (v as u32).reverse_bits() as u64,
        (B::Bitreverse, Width::W64) => v.reverse_bits(),
    }
}

/// Saturating add/sub/mul, shared by the IntSat rvalue and SIMD SatAdd/SatSub.
pub(crate) fn int_saturating(op: OvfOp, signed: bool, av: u64, bv: u64, w: Width) -> u64 {
    let (v, ovf) = int_ovf(op, signed, av, bv, w);
    if !ovf {
        v
    } else if signed {
        // Direction: a positive add overflow saturates to MAX; the rest follow from the
        // operand signs.
        let (x, y) = (sext(av, w), sext(bv, w));
        let toward_max = match op {
            OvfOp::Add => y > 0,
            OvfOp::Sub => y < 0,
            OvfOp::Mul => (x > 0) == (y > 0),
        };
        let m = w.mask();
        if toward_max {
            m >> 1
        } else {
            ((m >> 1) + 1) & m
        }
    } else {
        match op {
            OvfOp::Sub => 0,
            _ => w.mask(),
        }
    }
}

pub(crate) fn int_cmp(cc: IntCc, signed: bool, a: u64, b: u64, w: Width) -> u64 {
    let ord = if signed {
        sext(a, w).cmp(&sext(b, w))
    } else {
        (a & w.mask()).cmp(&(b & w.mask()))
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

/// `*WithOverflow`: computes at 128 bits and decides overflow from the width and
/// signedness.
pub(crate) fn int_ovf(op: OvfOp, signed: bool, a: u64, b: u64, w: Width) -> (u64, bool) {
    if signed {
        let (x, y) = (sext(a, w) as i128, sext(b, w) as i128);
        let r = match op {
            OvfOp::Add => x + y,
            OvfOp::Sub => x - y,
            OvfOp::Mul => x * y,
        };
        let (lo, hi) = match w {
            Width::W8 => (i8::MIN as i128, i8::MAX as i128),
            Width::W16 => (i16::MIN as i128, i16::MAX as i128),
            Width::W32 => (i32::MIN as i128, i32::MAX as i128),
            Width::W64 => (i64::MIN as i128, i64::MAX as i128),
        };
        ((r as u64) & w.mask(), r < lo || r > hi)
    } else {
        let (x, y) = ((a & w.mask()) as u128, (b & w.mask()) as u128);
        let r = match op {
            OvfOp::Add => x + y,
            OvfOp::Sub => x.wrapping_sub(y),
            OvfOp::Mul => x * y,
        };
        let ovf = match op {
            OvfOp::Sub => x < y,
            _ => r > w.mask() as u128,
        };
        ((r as u64) & w.mask(), ovf)
    }
}
