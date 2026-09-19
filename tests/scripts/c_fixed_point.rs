#!/usr/bin/env mirvm
---
[dependencies]
fixed = "1"
---
// fixed 1.31 (binary fixed-point arithmetic: 8/16/32/64/128-bit storage x variable
// fractional bits) differential. The overflow spectrum is the 128-bit stress point:
// I64F64/U64F64 mul/div, parse scaling and hypot go through the crate's int256
// double-word intermediates (src/int256.rs U256/I256), plus wide_mul (I32F32 ->
// FixedI128) 64->128-bit widening multiplication. The narrower I4F12 and I0F16
// types expose half-LSB rounding and result overflow respectively.
//
// Every line printed here is compared byte-for-byte against the native oracle; the
// printed bits pin the exact fixed-point encoding, so a divergence in any width shows
// up as a different line rather than as a wrong-looking number. The 128-bit chains
// (sqrt, hypot, recip, from_num/to_num) are the ones most likely to expose a defect in
// the wide-integer paths, and are kept as regression sentinels.
//
// Crate semantics (not mirvm-specific) shape the coverage: fixed's sat/wrap/ovf
// division panics on a zero divisor, sqrt panics on negative input, and from_num on
// NaN or +-Inf panics; only the checked_* forms return None. Those error paths are
// exercised through checked_* in section 4.
//
// Transcendental functions: fixed's lib.rs states `No trigonometric functions ... no`
// `pow ... no log or exp` and points at the cordic crate instead. The nearest APIs the
// crate actually offers are sqrt (including the 64/128-bit isqrt paths), hypot (int256
// sum of squares, then root) and recip; this fixture uses that trio and prints their bits.
//
// Coverage:
// ① Parse spectrum (decimal/binary/octal/hex/exponent forms and various error texts,
//    including the int256 scaling path for >27 significant digits and parse rounding).
// ② Arithmetic chains and fractional-precision propagation (/3 then *10, fused
//    mul_add, mean, lerp).
// ③ sqrt/hypot/recip spectrum, including the 128-bit square root of U64F64::MAX and
//    the pure-fractional I0F16 case where sqrt(0.25)=0.5 leaves the value range, so
//    saturating_sqrt and wrapping_sqrt take their overflow branches.
// ④ checked_* overflow paths: MAX+DELTA, division by zero, MIN.checked_neg,
//    checked_from_num out of range or NaN, and the f64 boundary at 2^31.
// ⑤ saturating/wrapping/overflowing comparison over add/sub/mul/neg/div overflow and
//    from_num in both directions.
// ⑥ f64 <-> fixed-point rounding in both directions: the 0.1 truncation and roundtrip
//    bits for three storage types, lossless roundtrip, the ties-to-even half-LSB
//    spectrum, agreement between the two sources, +-0, and 1e19/MAX/MIN roundtrips
//    through the int256 software path.
// ⑦ wide_mul 128-bit widening multiplication.
// Deterministic: fixed-point values are always printed as Display plus to_bits(), which
// pins the bit pattern; no randomness, time, addresses or HashMap order; error text is a
// fixed string inside the crate; no IO.
use fixed::types::{I0F16, I16F16, I32F32, I4F12, I64F64, U32F32, U64F64};

/// Prints a fixed-point value: decimal expansion plus bit pattern.
macro_rules! pv {
    ($label:expr, $v:expr) => {
        println!("{} = {} bits={:#x}", $label, $v, $v.to_bits())
    };
}

/// Prints an Option<fixed-point>.
macro_rules! popt {
    ($label:expr, $v:expr) => {
        match $v {
            Some(x) => println!("{} = {} bits={:#x}", $label, x, x.to_bits()),
            None => println!("{} = none", $label),
        }
    };
}

/// Prints a parse result (Ok value / Err text).
macro_rules! pparse {
    ($label:expr, $v:expr) => {
        match $v {
            Ok(x) => println!("{} ok {} bits={:#x}", $label, x, x.to_bits()),
            Err(e) => println!("{} err {}", $label, e),
        }
    };
}

/// Prints the (value, overflow flag) pair returned by overflowing_*.
macro_rules! povf {
    ($label:expr, $v:expr) => {{
        let (x, o) = $v;
        println!("{} = {} bits={:#x} overflow={}", $label, x, x.to_bits(), o);
    }};
}

// ① parse spectrum
fn parse_spectrum() {
    println!("== 1 parse ==");
    let dec = [
        "3.14159265358979", "-2.5", "+7", "-0", "0.1", "1.5e-3", "6.25E+3",
        "123456789012345678901234567890.5", // >27 significant digits -> int256 scaling
        "1e999", "", "abc", "12.34.56", "0x10", "1e2e3",
    ];
    for s in dec {
        pparse!(format!("I32F32::{s:?}"), I32F32::from_str(s));
    }
    // Boundary overflow: I16F16 has 15 integer bits (16 bits of storage including the sign)
    for s in ["32767.5", "32768", "1e10", "-32768", "-32768.5"] {
        pparse!(format!("I16F16::{s:?}"), I16F16::from_str(s));
    }
    // Unsigned large-value boundary plus rejection of negatives
    for s in ["18446744073709551615.5", "18446744073709551616", "-1"] {
        pparse!(format!("U64F64::{s:?}"), U64F64::from_str(s));
    }
    // Other radices
    for s in ["1101.1011", "-10.1", "1.11e3", "101"] {
        pparse!(format!("I32F32::bin {s:?}"), I32F32::from_str_binary(s));
    }
    pparse!("I32F32::oct \"17.704\"", I32F32::from_str_octal("17.704"));
    pparse!("I32F32::hex \"ff.8\"", I32F32::from_str_hex("ff.8"));
    pparse!("I32F32::hex \"1.fp4\"", I32F32::from_str_hex("1.fp4"));
    pparse!("I32F32::hex \"dead.beef\"", I32F32::from_str_hex("dead.beef"));
    // Parse rounding, ties-to-even: I4F12 has LSB = 2^-12, half LSB = 2^-13
    for s in ["1.00048828125", "1.0009765625", "1.00146484375"] {
        pparse!(format!("I4F12::{s:?}"), I4F12::from_str(s));
    }
}

// ② arithmetic chains and fractional-precision propagation
fn arith_chains() {
    println!("== 2 arith ==");
    let a = I32F32::from_num(7.25);
    let b = I32F32::from_num(-2.5);
    let s1 = a + b;
    pv!("i32f32 7.25+(-2.5)", s1);
    let s2 = s1 * b;
    pv!("(..)*(-2.5)", s2);
    let s3 = s2 / 3;
    pv!("(..)/3", s3);
    let s4 = s3 - a;
    pv!("(..)-7.25", s4);
    let s5 = -s4;
    pv!("neg", s5);
    let s6 = s5.abs();
    pv!("abs", s6);

    let one = I32F32::from_num(1);
    let third = one / 3;
    pv!("i32f32 1/3", third);
    let back3 = third * 3;
    pv!("(1/3)*3", back3);
    println!("(1/3)*3 == 1 : {}", back3 == one);
    let tenth = one / 10;
    pv!("1/10", tenth);
    pv!("(1/10)*10", tenth * 10);

    // Fused multiply-add (full-precision intermediate, rounded once) vs stepwise
    let c = I32F32::from_num(0.375);
    pv!("mul_add 7.25*(-2.5)+0.375", a.mul_add(b, c));
    let step = a * b + c;
    pv!("a*b+c stepwise", step);
    println!("mul_add == stepwise : {}", a.mul_add(b, c) == step);
    pv!("mean(1/3, 1.5)", third.mean(I32F32::from_num(1.5)));

    // lerp: t.lerp(start, end)
    let t = I32F32::from_num(0.625);
    pv!(
        "lerp t=0.625 [2, 9.5]",
        t.lerp(I32F32::from_num(2), I32F32::from_num(9.5))
    );
    let t2 = I32F32::from_num(2);
    pv!(
        "lerp t=2 [-1.5, 7]",
        t2.lerp(I32F32::from_num(-1.5), I32F32::from_num(7))
    );

    // The 128-bit storage chain: U64F64 only takes non-negative values
    let u = U64F64::from_num(9.75);
    let v = U64F64::from_num(0.5);
    let u1 = u - v;
    pv!("u64f64 9.75-0.5", u1);
    let u2 = u1 * 4;
    pv!("(..)*4", u2);
    let u3 = u2 / 7;
    pv!("(..)/7", u3);
    let u4 = u3 + U64F64::from_num(1024);
    pv!("(..)+1024", u4);
    let uu = U64F64::from_num(1);
    let uthird = uu / 3;
    pv!("u64f64 1/3", uthird);
    pv!("u64f64 (1/3)*3", uthird * 3);

    // I64F64 (signed 128-bit): a mixed chain plus unary neg/abs, presented in the
    // same shape as the 32-bit chain.
    let w = I64F64::from_num(-123.125);
    let w1 = w * I64F64::from_num(0.0078125); // ×2^-7
    pv!("i64f64 -123.125*0.0078125", w1);
    let w2 = w1 / 11;
    pv!("(..)/11", w2);
    let w3 = w2 + I64F64::DELTA;
    pv!("(..)+DELTA", w3);
    let w4 = w3 * I64F64::from_num(-0.5);
    pv!("(..)*(-0.5)", w4);
    let w5 = -w4;
    pv!("i64f64 neg", w5);
    let w6 = w5.abs();
    pv!("i64f64 abs(neg)", w6);
    let w7 = w.abs();
    pv!("i64f64 abs(-123.125)", w7);
}

// ③ sqrt/recip spectrum (the crate has no trig/pow/log; see the file header)
fn sqrt_spectrum() {
    println!("== 3 sqrt/hypot/recip ==");
    for v in [0.0, 1.0, 2.0, 3.14159265358979, 65536.0] {
        pv!(format!("sqrt({v})"), I32F32::from_num(v).sqrt());
    }
    pv!("sqrt(u64f64 MAX)", U64F64::MAX.sqrt());
    pv!("sqrt(u64f64 2)", U64F64::from_num(2).sqrt());
    pv!("sqrt(i64f64 1e18)", I64F64::from_num(1_000_000_000_000_000_000u64).sqrt());
    popt!("checked_sqrt(-2.5)", I32F32::from_num(-2.5).checked_sqrt());
    // saturating/wrapping_sqrt panic on negative input just like sqrt, and that path is
    // covered by popt!/checked; instead the pure-fractional I0F16 type drives both modes
    // through a result overflow: sqrt(0.25)=0.5 leaves the I0F16 positive range -> sat=MAX.
    let q = I0F16::from_num(0.25);
    pv!("i0f16 saturating_sqrt(0.25)", q.saturating_sqrt());
    pv!("i0f16 wrapping_sqrt(0.25)", q.wrapping_sqrt());
    pv!(
        "hypot(3,4)",
        I32F32::from_num(3).hypot(I32F32::from_num(4))
    );
    pv!(
        "hypot(5e-9,1.2e-8)",
        I64F64::from_num(5e-9).hypot(I64F64::from_num(1.2e-8))
    );
    popt!(
        "checked_hypot(MAX,MAX)",
        I32F32::MAX.checked_hypot(I32F32::MAX)
    );
    pv!(
        "saturating_hypot(MAX,MAX)",
        I32F32::MAX.saturating_hypot(I32F32::MAX)
    );
    pv!("recip(7)", I32F32::from_num(7).recip());
    pv!("recip(-0.0625)", I32F32::from_num(-0.0625).recip());
    pv!("saturating_recip(DELTA)", I32F32::DELTA.saturating_recip());
    pv!("wrapping_recip(DELTA)", I32F32::DELTA.wrapping_recip());
}

// ④ checked_* overflow paths
fn checked_paths() {
    println!("== 4 checked ==");
    popt!("MAX.checked_add(DELTA)", I16F16::MAX.checked_add(I16F16::DELTA));
    popt!("MAX.checked_add(-1)", I16F16::MAX.checked_add(I16F16::from_num(-1)));
    popt!("MIN.checked_sub(DELTA)", I16F16::MIN.checked_sub(I16F16::DELTA));
    popt!(
        "100.checked_mul(1000)",
        I16F16::from_num(100).checked_mul(I16F16::from_num(1000))
    );
    popt!(
        "3.checked_div(0)",
        I16F16::from_num(3).checked_div(I16F16::ZERO)
    );
    popt!("MIN.checked_neg()", I16F16::MIN.checked_neg());
    popt!("checked_from_num(1e300)", I16F16::checked_from_num(1e300f64));
    popt!("checked_from_num(NaN)", I16F16::checked_from_num(f64::NAN));
    // f64 integer boundary: I32F32's range is [-2^31, 2^31 - 2^-32]
    popt!(
        "checked_from_num(2^31-1)",
        I32F32::checked_from_num(2_147_483_647.0f64)
    );
    popt!(
        "checked_from_num(2^31)",
        I32F32::checked_from_num(2_147_483_648.0f64)
    );
    popt!("u64 MAX.checked_add(DELTA)", U64F64::MAX.checked_add(U64F64::DELTA));
    popt!(
        "u64 0.checked_div(0)",
        U64F64::ZERO.checked_div(U64F64::ZERO)
    );
}

// ⑤ saturating / wrapping / overflowing comparison
fn modes() {
    println!("== 5 modes ==");
    let one = I16F16::from_num(1);
    pv!("sat MAX+1", I16F16::MAX.saturating_add(one));
    pv!("wrap MAX+1", I16F16::MAX.wrapping_add(one));
    povf!("ovf MAX+1", I16F16::MAX.overflowing_add(one));
    pv!("sat MIN-1", I16F16::MIN.saturating_sub(one));
    pv!("wrap MIN-1", I16F16::MIN.wrapping_sub(one));
    povf!("ovf MIN-1", I16F16::MIN.overflowing_sub(one));
    pv!("sat -MIN", I16F16::MIN.saturating_neg());
    pv!("wrap -MIN", I16F16::MIN.wrapping_neg());
    povf!("ovf -MIN", I16F16::MIN.overflowing_neg());
    let h = I16F16::from_num(100);
    let k = I16F16::from_num(1000);
    pv!("sat 100*1000", h.saturating_mul(k));
    pv!("wrap 100*1000", h.wrapping_mul(k));
    povf!("ovf 100*1000", h.overflowing_mul(k));
    // Division overflow with a non-zero divisor: fixed's sat/wrap/ovf div panic on a zero
    // divisor just like plain div (section 4 covers that path with checked_div(0)=none),
    // so 3/DELTA=196608, above the I16F16 upper bound, drives the three modes here.
    let three = I16F16::from_num(3);
    let mthree = I16F16::from_num(-3);
    pv!("sat 3/DELTA", three.saturating_div(I16F16::DELTA));
    pv!("sat -3/DELTA", mthree.saturating_div(I16F16::DELTA));
    pv!("wrap 3/DELTA", three.wrapping_div(I16F16::DELTA));
    povf!("ovf 3/DELTA", three.overflowing_div(I16F16::DELTA));
    // The three modes for out-of-range f64 conversion
    pv!("sat_from_num(1e300)", I16F16::saturating_from_num(1e300f64));
    pv!("wrap_from_num(1e300)", I16F16::wrapping_from_num(1e300f64));
    povf!("ovf_from_num(1e300)", I16F16::overflowing_from_num(1e300f64));
    pv!("sat_from_num(-1e300)", I16F16::saturating_from_num(-1e300f64));
    // Unsigned type fed a negative source
    pv!("u sat_from_num(-5)", U32F32::saturating_from_num(-5.0f64));
    pv!("u wrap_from_num(-1.5)", U32F32::wrapping_from_num(-1.5f64));
    povf!("u ovf_from_num(-1.5)", U32F32::overflowing_from_num(-1.5f64));
    // Unsigned saturation spectrum over both u64 and u128 storage; the u128
    // saturating_add/sub cases sit alongside the wrap/ovf modes in the same block.
    pv!("u sat MAX+1", U32F32::MAX.saturating_add(U32F32::from_num(1)));
    pv!("u sat 0-1", U32F32::ZERO.saturating_sub(U32F32::from_num(1)));
    pv!("u64 sat MAX+1", U64F64::MAX.saturating_add(U64F64::from_num(1)));
    pv!("u64 sat 0-1", U64F64::ZERO.saturating_sub(U64F64::from_num(1)));
    pv!("u64 wrap MAX+1", U64F64::MAX.wrapping_add(U64F64::from_num(1)));
    povf!("u64 ovf MAX+1", U64F64::MAX.overflowing_add(U64F64::from_num(1)));
}

// ⑥ f64 conversion rounding in both directions (fixed -> f64 goes through the Widest(u128) bit-manipulation channel)
fn f64_rounding() {
    println!("== 6 f64 rounding ==");
    // f64 -> fixed-point: the binary truncation of 0.1
    let d1 = I32F32::from_num(0.1f64);
    pv!("i32f32 from 0.1", d1);
    let d2 = U64F64::from_num(0.1f64);
    pv!("u64f64 from 0.1", d2);
    let d3 = I64F64::from_num(0.1f64);
    pv!("i64f64 from 0.1", d3);
    // fixed-point -> f64: roundtrip for three storage widths (bits pinned; to_num and From are one family)
    let r1: f64 = d1.to_num();
    println!("i32f32 0.1 -> f64 bits={:#x}", r1.to_bits());
    let r2: f64 = d2.to_num();
    println!("u64f64 0.1 -> f64 bits={:#x}", r2.to_bits());
    let r3: f64 = d3.to_num();
    println!("i64f64 0.1 -> f64 bits={:#x}", r3.to_bits());
    let r4: f64 = I32F32::from_num(2.5).to_num();
    println!("i32f32 2.5 -> f64 bits={:#x}", r4.to_bits());
    // Roundtrip consistency: fixed -> f64 -> fixed is bit-lossless within range
    println!("0.1 rt i32f32== : {}", I32F32::from_num(r1) == d1);
    println!("0.1 rt u64f64== : {}", U64F64::from_num(r2) == d2);
    println!("0.1 rt i64f64== : {}", I64F64::from_num(r3) == d3);
    // Half-LSB ties-to-even (I4F12, half LSB = 2^-13)
    let half = 1.0f64 / 8192.0;
    for k in [1.0f64, 3.0, 5.0, 7.0, 9.0] {
        pv!(format!("i4f12 from {k}*2^-13"), I4F12::from_num(k * half));
    }
    pv!("i4f12 from 0.1", I4F12::from_num(0.1f64));
    // Agreement between the sources: from_num (f64 binary source) and from_str (decimal parse)
    let ds = I32F32::from_str("0.1").unwrap();
    println!("0.1 from_num==from_str : {}", d1 == ds);
    let ds2 = I32F32::from_str("2.5").unwrap();
    println!("2.5 from_num==from_str : {}", I32F32::from_num(2.5f64) == ds2);
    let nz = I32F32::from_num(-0.0f64);
    pv!("i32f32 from -0.0", nz);
    println!("-0.0 bits==0 : {}", nz.to_bits() == 0);
    let rnz: f64 = nz.to_num();
    println!("-0.0 -> f64 bits={:#x}", rnz.to_bits());
    // NaN/+-Inf: fixed's sat/wrap/ovf_from_num always panic on a non-finite source
    // ("NaN"/"infinite"); the only safe entry is checked_from_num -> none, covered in section 4.
    // 128-bit f64 <-> fixed-point (from_num takes the int256 software path):
    pv!("u64f64 from 1e19", U64F64::from_num(1e19f64));
    let rb: f64 = U64F64::from_num(1e19f64).to_num();
    println!("u64f64 1e19 -> f64 bits={:#x}", rb.to_bits());
    let rmax: f64 = U64F64::MAX.to_num();
    println!("u64f64 MAX -> f64 bits={:#x}", rmax.to_bits());
    let rmin: f64 = I64F64::MIN.to_num();
    println!("i64f64 MIN -> f64 bits={:#x}", rmin.to_bits());
}

// ⑦ wide_mul: 64 -> 128-bit widening multiplication
fn wide_mul_section() {
    println!("== 7 wide_mul ==");
    let a = I32F32::MAX;
    let b = I32F32::from_num(3.5);
    let w = a.wide_mul(b); // FixedI128<U64>
    pv!("i32f32 MAX wide*3.5 -> i64f64", w);
    let c = I32F32::from_num(-2.25);
    let d = I32F32::from_num(-7.75);
    pv!("(-2.25) wide*(-7.75)", c.wide_mul(d));
    let e = U32F32::MAX;
    let f = U32F32::from_num(1.5);
    pv!("u32f32 MAX wide*1.5 -> u64f64", e.wide_mul(f));
    let g = I16F16::DELTA;
    pv!("i16f16 DELTA wide*DELTA -> i32f32", g.wide_mul(g));
}

fn main() {
    parse_spectrum();
    arith_chains();
    sqrt_spectrum();
    checked_paths();
    modes();
    f64_rounding();
    wide_mul_section();
}
