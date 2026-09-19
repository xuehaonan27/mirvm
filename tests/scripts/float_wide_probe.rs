// Permanent differential probe for f16/f128: the scalar lane (f16) and the 16-byte wide lane
// (f128) compared across arithmetic/comparison/conversion/math/fma. The hex to_bits must match
// bit-for-bit: rustc lowers both sides to the same compiler-builtins/__*tf* + *f128 libm.
#![feature(f16, f128)]

fn f16_lane() {
    let a: f16 = 1.5;
    let b: f16 = -2.25;
    println!("f16 arith = {} {} {} {} {}",
        (a + b) as f64, (a - b) as f64, (a * b) as f64, (a / b) as f64, (a % b) as f64);
    println!("f16 cmp = {} {} {}", a > b, a == a, b <= a);
    println!("f16 neg/abs/copysign = {} {} {}",
        (-a) as f64, b.abs() as f64, a.copysign(b) as f64);
    println!("f16 math = {} {} {} {}",
        (2.0f16).sqrt() as f64, (0.5f16).sin() as f64,
        (2.7f16).floor() as f64, (1.5f16).round_ties_even() as f64);
    println!("f16 fma = {}", a.mul_add(a, b) as f64);
    println!("f16 min/max = {} {}", a.min(b) as f64, a.max(b) as f64);
    // Conversions: f16↔f32/f64, int↔f16, saturating
    println!("f16 casts = {} {} {} {}",
        (a as f32), (b as f64), (300i32 as f16) as f64, (a as i8));
    println!("f16 sat = {} {}", (f16::INFINITY as u8), (f16::NAN as i16));
    println!("f16 bits = {:#06x} {:#06x}", a.to_bits(), (a + b).to_bits());
    // subnormal / boundary
    let tiny = f16::from_bits(1);
    println!("f16 tiny = {:e} {:#06x}", tiny as f64, (tiny + tiny).to_bits());
}

fn f128_lane() {
    let a: f128 = 3.5;
    let b: f128 = 1.25;
    println!("f128 arith = {} {} {} {} {}",
        (a + b) as f64, (a - b) as f64, (a * b) as f64, (a / b) as f64, (a % b) as f64);
    let nan = 0.0f128 / 0.0; // runtime NaN (sidesteps the invalid_nan_comparisons lint)
    println!("f128 cmp = {} {} {}", a > b, a == a, nan == nan);
    println!("f128 neg/abs/copysign = {} {} {}",
        (-a) as f64, (-b).abs() as f64, a.copysign(-b) as f64);
    println!("f128 math = {} {} {} {} {}",
        (2.0f128).sqrt() as f64, (1.0f128).exp() as f64, (100.0f128).ln() as f64,
        a.powi(3) as f64, a.powf(0.5) as f64);
    println!("f128 floor/trunc/round = {} {} {}",
        (2.7f128).floor() as f64, (-2.7f128).trunc() as f64, (2.5f128).round_ties_even() as f64);
    println!("f128 fma = {}", a.mul_add(b, -a) as f64);
    println!("f128 min/max = {} {}", a.min(b) as f64, a.max(f128::NAN) as f64);
    // Precision proof: a difference too small for f64 is visible in the f128 domain
    let eps = f128::EPSILON;
    println!("f128 eps sum = {} {:e}", (1.0f128 + eps) > 1.0f128, eps as f64);
    // Conversion matrix
    println!("f128 casts = {} {} {} {}",
        (a as f32), (a as f16) as f64, (7u64 as f128) as f64, (-9i32 as f128) as f64);
    println!("f128 to int = {} {} {}", a as i64, a as u8, (-a) as u32);
    println!("f128 wide int = {} {}",
        (u128::MAX as f128) as f64, ((1i128 << 100) as f128) as f64);
    println!("f128 from wide back = {}", ((1u128 << 90) as f128) as u128);
    println!("f128 bits = {:#034x}", (a + b).to_bits());
    println!("f128 div edge = {} {}", (1.0f128 / 0.0) as f64, (0.0f128 / 0.0).is_nan());
}

fn main() {
    f16_lane();
    f128_lane();
}
