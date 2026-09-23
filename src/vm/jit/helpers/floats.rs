//! Host direct-eval helpers for 128-bit integers and the f16/f128 families. Rust
//! lowers their arithmetic to the same compiler-builtins and glibc libm symbols the
//! interpreter uses, so both paths are bit-identical.

use super::*;

// ===== f16/f128/128-bit helpers: the interpreter's host direct-eval channel. Rust
// f16/f128/i128/u128 arithmetic lowers to the same compiler-builtins `__*tf*` and glibc
// `*f128` libm symbols that interp/native use, so both paths are bit-identical. =====

pub(crate) fn lo_hi(lo: u64, hi: u64) -> u128 {
    (lo as u128) | ((hi as u128) << 64)
}
pub(crate) fn hi_lo(v: u128) -> (u64, u64) {
    (v as u64, (v >> 64) as u64)
}
pub(crate) fn f128_of(lo: u64, hi: u64) -> f128 {
    f128::from_bits(lo_hi(lo, hi))
}
pub(crate) fn pair_of(v: f128) -> (u64, u64) {
    hi_lo(v.to_bits())
}

/// i128/u128 `overflowing_add/sub/mul` for `Bin128`: returns the overflow flag and
/// writes the result pair to `out`.
pub(crate) extern "C-unwind" fn mirvm_bin128_ovf(
    op: u64,
    signed: bool,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) -> u64 {
    stat(S_BIN128);
    let (r, ovf) = if signed {
        let (x, y) = (lo_hi(alo, ahi) as i128, lo_hi(blo, bhi) as i128);
        match op {
            0 => x.overflowing_add(y),
            1 => x.overflowing_sub(y),
            _ => x.overflowing_mul(y),
        }
    } else {
        let (x, y) = (lo_hi(alo, ahi), lo_hi(blo, bhi));
        let (v, ovf) = match op {
            0 => x.overflowing_add(y),
            1 => x.overflowing_sub(y),
            _ => x.overflowing_mul(y),
        };
        (v as i128, ovf)
    };
    let (lo, hi) = hi_lo(r as u128);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
    ovf as u64
}

/// `Bin128` Div/Rem. Cranelift's ISLE has no I128 division (it reports
/// `udiv.i128 "should be implemented in ISLE"`), so this cannot compile to CLIF. The host
/// wrapping arithmetic matches the interpreter; a zero divisor takes the 128-bit div_zero
/// diagnostic (kind 2/3).
pub(crate) extern "C-unwind" fn mirvm_bin128_divrem(
    is_rem: u64,
    signed: bool,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) {
    let (a, b) = (lo_hi(alo, ahi), lo_hi(blo, bhi));
    if b == 0 {
        mirvm_jit_div_zero(2 + is_rem);
    }
    let r: u128 = if signed {
        let (x, y) = (a as i128, b as i128);
        (if is_rem != 0 {
            x.wrapping_rem(y)
        } else {
            x.wrapping_div(y)
        }) as u128
    } else if is_rem != 0 {
        a.wrapping_rem(b)
    } else {
        a.wrapping_div(b)
    };
    let (lo, hi) = hi_lo(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 arithmetic (op: 0=add, 1=sub, 2=mul, 3=rem (`fmodf128`), 4=div).
pub(crate) extern "C-unwind" fn mirvm_f128_bin(
    op: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) {
    let (a, b) = (f128_of(alo, ahi), f128_of(blo, bhi));
    let r = match op {
        0 => a + b,
        1 => a - b,
        2 => a * b,
        4 => a / b,
        _ => a % b,
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 comparison (cc: 0=Eq, 1=Ne, 2=Lt, 3=Le, 4=Gt, 5=Ge). IEEE semantics: NaN makes
/// every predicate false except Ne.
pub(crate) extern "C-unwind" fn mirvm_f128_cmp(
    cc: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
) -> u64 {
    let (a, b) = (f128_of(alo, ahi), f128_of(blo, bhi));
    match cc {
        0 => (a == b) as u64,
        1 => (a != b) as u64,
        2 => (a < b) as u64,
        3 => (a <= b) as u64,
        4 => (a > b) as u64,
        _ => (a >= b) as u64,
    }
}

/// f128 unary (op: 0=neg; the rest are the unary math family, glibc `*f128` libm
/// symbols).
pub(crate) extern "C-unwind" fn mirvm_f128_un(op: u64, alo: u64, ahi: u64, out: *mut u64) {
    let a = f128_of(alo, ahi);
    let r = match op {
        0 => -a,
        1 => a.sqrt(),
        2 => a.sin(),
        3 => a.cos(),
        4 => a.exp(),
        5 => a.exp2(),
        6 => a.ln(),
        7 => a.log2(),
        8 => a.log10(),
        9 => a.abs(),
        10 => a.floor(),
        11 => a.ceil(),
        12 => a.trunc(),
        13 => a.round(),
        _ => a.round_ties_even(),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 binary math (op: 0=pow, 1=powi, 2=copysign, 3=minnum, 4=maxnum, 5=fma with the
/// addend in `clo`/`chi`).
pub(crate) extern "C-unwind" fn mirvm_f128_math(
    op: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    clo: u64,
    chi: u64,
    out: *mut u64,
) {
    let (a, b, c) = (f128_of(alo, ahi), f128_of(blo, bhi), f128_of(clo, chi));
    let r = match op {
        0 => a.powf(b),
        // powi's rhs is an i32 scalar (the F128Rhs::Scalar contract; the interpreter's
        // stmt.rs reads it as a raw scalar too). `blo` holds the raw integer bits, so it
        // must never go through `f128_of`.
        1 => a.powi(blo as i32),
        2 => a.copysign(b),
        3 => a.min(b),
        4 => a.max(b),
        _ => a.mul_add(b, c),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// Scalar -> f128 (kind: 0=f16, 1=f32, 2=f64, then 3=i8, 4=u8, 5=i16, 6=u16, 7=i32,
/// 8=u32, 9=i64, 10=u64; anything else is taken as u64).
pub(crate) extern "C-unwind" fn mirvm_f128_from_scalar(kind: u64, v: u64, out: *mut u64) {
    let r = match kind {
        0 => f128::from(f16::from_bits(v as u16)),
        1 => f128::from(f32::from_bits(v as u32)),
        2 => f128::from(f64::from_bits(v)),
        3 => f128::from(v as i8),
        4 => f128::from(v as u8),
        5 => f128::from(v as i16),
        6 => f128::from(v as u16),
        7 => f128::from(v as i32),
        8 => f128::from(v as u32),
        9 => f128::from(v as i64),
        _ => f128::from(v),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 -> scalar (the same kind codes; float conversions preserve the bit pattern,
/// integer conversions use `as` saturation semantics).
pub(crate) extern "C-unwind" fn mirvm_f128_to_scalar(kind: u64, alo: u64, ahi: u64) -> u64 {
    let a = f128_of(alo, ahi);
    match kind {
        0 => (a as f16).to_bits() as u64,
        1 => (a as f32).to_bits() as u64,
        2 => (a as f64).to_bits(),
        3 => (a as i8) as u8 as u64,
        4 => (a as u8) as u64,
        5 => (a as i16) as u16 as u64,
        6 => (a as u16) as u64,
        7 => (a as i32) as u32 as u64,
        8 => (a as u32) as u64,
        9 => (a as i64) as u64,
        _ => a as u64,
    }
}

/// i128/u128 -> f128 (signed: 0=unsigned, 1=signed). The reverse direction is
/// `mirvm_f128_to_wide`, which saturates.
pub(crate) extern "C-unwind" fn mirvm_f128_from_wide(
    signed: bool,
    lo: u64,
    hi: u64,
    out: *mut u64,
) {
    let r = if signed {
        (lo_hi(lo, hi) as i128) as f128
    } else {
        lo_hi(lo, hi) as f128
    };
    let (l, h) = pair_of(r);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}
pub(crate) extern "C-unwind" fn mirvm_f128_to_wide(
    signed: bool,
    alo: u64,
    ahi: u64,
    out: *mut u64,
) {
    let a = f128_of(alo, ahi);
    let v: u128 = if signed {
        (a as i128) as u128
    } else {
        a as u128
    };
    let (l, h) = hi_lo(v);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}

/// float -> i128/u128 with saturation, the reverse of `Wide128ToFloat`
/// (kind: 0=f16, 1=f32, 2=f64).
pub(crate) extern "C-unwind" fn mirvm_float_to_wide(
    kind: u64,
    v: u64,
    signed: bool,
    out: *mut u64,
) {
    let r: u128 = match (kind, signed) {
        (0, true) => (f16::from_bits(v as u16) as i128) as u128,
        (0, false) => f16::from_bits(v as u16) as u128,
        (1, true) => (f32::from_bits(v as u32) as i128) as u128,
        (1, false) => f32::from_bits(v as u32) as u128,
        (2, true) => (f64::from_bits(v) as i128) as u128,
        _ => f64::from_bits(v) as u128,
    };
    let (l, h) = hi_lo(r);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}

/// i128/u128 -> f16, the f16 target of `Wide128ToFloat`.
///
/// NOTE: the cast below is `__floattihf`/`__floatuntihf` from compiler-rt, which the
/// aarch64-apple-darwin sysroot does not carry — a plain Rust program whose `u128 as f16`
/// runs at runtime fails to link there for the same reason. So this and the interpreter's
/// copy of the same cast are the last two symbols keeping the macOS build from linking, and
/// the software model that replaces them needs a bit-for-bit differential test against this
/// cast, which is available on the platform that has it.
pub(crate) extern "C-unwind" fn mirvm_wide_to_f16(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f16
    } else {
        lo_hi(lo, hi) as f16
    };
    v.to_bits() as u64
}

/// i128/u128 -> f32/f64. Computed with a host `as` cast (round to nearest, the same
/// semantics as compiler-builtins `__float*ti*`) rather than by calling those symbols:
/// they return in XMM0 while the helper call ABI reads RAX, so the call would read
/// garbage. Returning the bit pattern as u64 keeps the return lane unambiguous.
pub(crate) extern "C-unwind" fn mirvm_wide_to_f32(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f32
    } else {
        lo_hi(lo, hi) as f32
    };
    v.to_bits() as u64
}

pub(crate) extern "C-unwind" fn mirvm_wide_to_f64(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f64
    } else {
        lo_hi(lo, hi) as f64
    };
    v.to_bits()
}

// ===== f16 helpers on the interpreter's host direct-eval channel =====

/// f16 arithmetic (op as in `mirvm_f128_bin`: 0=add, 1=sub, 2=mul, 3=rem, 4=div;
/// arguments and result are f16 bit patterns in u64). Note that div is the fallback arm,
/// so an op-4 Div is never answered with `%`.
pub(crate) extern "C-unwind" fn mirvm_f16_bin(op: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    let r = match op {
        0 => x + y,
        1 => x - y,
        2 => x * y,
        3 => x % y,
        _ => x / y,
    };
    r.to_bits() as u64
}
pub(crate) extern "C-unwind" fn mirvm_f16_cmp(cc: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    match cc {
        0 => (x == y) as u64,
        1 => (x != y) as u64,
        2 => (x < y) as u64,
        3 => (x <= y) as u64,
        4 => (x > y) as u64,
        _ => (x >= y) as u64,
    }
}
pub(crate) extern "C-unwind" fn mirvm_f16_neg(a: u64) -> u64 {
    (-f16::from_bits(a as u16)).to_bits() as u64
}

/// f16 unary math (op order matches the interpreter's `MathUn` macro; the same host f16
/// methods, so there is no drift). These helpers exist so that MathUn/MathBin/MathFma
/// never reach `as_float(F16)`: that panics on the compiler thread, which would silently
/// leave a compressible function interpreted.
pub(crate) extern "C-unwind" fn mirvm_f16_math_un(op: u64, a: u64) -> u64 {
    let x = f16::from_bits(a as u16);
    let r = match op {
        0 => x.sqrt(),
        1 => x.sin(),
        2 => x.cos(),
        3 => x.exp(),
        4 => x.exp2(),
        5 => x.ln(),
        6 => x.log2(),
        7 => x.log10(),
        8 => x.abs(),
        9 => x.floor(),
        10 => x.ceil(),
        11 => x.trunc(),
        12 => x.round(),
        _ => x.round_ties_even(),
    };
    r.to_bits() as u64
}

/// f16 binary math (op: 0=pow, 1=powi, 2=copysign, 3=minnum, 4=maxnum; the interpreter's
/// MathBin f16 arm has the same shape). For powi, `b` is the raw i32 bits and must not go
/// through `from_bits`.
pub(crate) extern "C-unwind" fn mirvm_f16_math_bin(op: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    let r = match op {
        0 => x.powf(y),
        1 => x.powi(b as i32),
        2 => x.copysign(y),
        3 => x.min(y),
        _ => x.max(y),
    };
    r.to_bits() as u64
}

/// f16 fused multiply-add (host `mul_add`, a single rounding; the interpreter's MathFma
/// f16 arm has the same shape).
pub(crate) extern "C-unwind" fn mirvm_f16_fma(a: u64, b: u64, c: u64) -> u64 {
    f16::from_bits(a as u16)
        .mul_add(f16::from_bits(b as u16), f16::from_bits(c as u16))
        .to_bits() as u64
}
/// f16 conversions (kind: 1=f16->f32, 2=f16->f64, 3=f32->f16, 4=f64->f16).
pub(crate) extern "C-unwind" fn mirvm_f16_cast(kind: u64, v: u64) -> u64 {
    match kind {
        1 => (f16::from_bits(v as u16) as f32).to_bits() as u64,
        2 => (f16::from_bits(v as u16) as f64).to_bits(),
        3 => (f32::from_bits(v as u32) as f16).to_bits() as u64,
        _ => (f64::from_bits(v) as f16).to_bits() as u64,
    }
}
/// f16 <-> int (`to`: 0=i8, 1=u8, 2=i16, 3=u16, 4=i32, 5=u32, 6=i64, 7=u64; `from` uses
/// the same codes).
pub(crate) extern "C-unwind" fn mirvm_f16_to_int(kind: u64, a: u64) -> u64 {
    let x = f16::from_bits(a as u16);
    match kind {
        0 => (x as i8) as u8 as u64,
        1 => (x as u8) as u64,
        2 => (x as i16) as u16 as u64,
        3 => (x as u16) as u64,
        4 => (x as i32) as u32 as u64,
        5 => (x as u32) as u64,
        6 => (x as i64) as u64,
        _ => x as u64,
    }
}
pub(crate) extern "C-unwind" fn mirvm_f16_from_int(kind: u64, v: u64) -> u64 {
    let r = match kind {
        0 => (v as i8) as f16,
        1 => (v as u8) as f16,
        2 => (v as i16) as f16,
        3 => (v as u16) as f16,
        4 => (v as i32) as f16,
        5 => (v as u32) as f16,
        6 => (v as i64) as f16,
        _ => v as f16,
    };
    r.to_bits() as u64
}

// powi goes through compiler-builtins: Rust's `powi` lowers to the same symbols.
unsafe extern "C" {
    fn __powidf2(x: f64, n: i32) -> f64;
    fn __powisf2(x: f32, n: i32) -> f32;
}
