// M5.2 D8c：f16/f128 真实用法画像——f16 量化/反量化（ML 权重惯用），
// f128 高精度累加（Kahan 不需要的场合）。断言内联（corpus oracle = exit 0 + stdout）。
#![feature(f16, f128)]

fn quantize(xs: &[f32]) -> Vec<f16> {
    xs.iter().map(|&x| x as f16).collect()
}

fn main() {
    // f16 量化误差有界
    let xs: Vec<f32> = (0..64).map(|i| (i as f32) * 0.113 - 3.0).collect();
    let q = quantize(&xs);
    let max_err = xs
        .iter()
        .zip(&q)
        .map(|(&x, &h)| (x - h as f32).abs())
        .fold(0.0f32, f32::max);
    assert!(max_err < 0.004, "f16 量化误差越界 {max_err}");
    let s16: f16 = q.iter().fold(0.0f16, |acc, &h| acc + h / 64.0);
    println!("f16 quant err<{:.4} mean = {}", 0.004, s16 as f64);

    // f128 精确累加：f64 域会丢的低位在 f128 域保住
    let mut acc: f128 = 0.0;
    for i in 1..=1000u32 {
        acc += 1.0f128 / (i as f128);
    }
    println!("f128 harmonic(1000) = {:.20}", acc as f64);
    let big: f128 = 1.0e30;
    let small: f128 = 1.0;
    assert!((big + small) - big == small, "f128 精度丢失");
    println!("f128 keeps 1e30+1 = true, sqrt2 = {:.18}", (2.0f128).sqrt() as f64);

    // 位往返 + 判别
    let h: f16 = 0.1;
    assert_eq!(f16::from_bits(h.to_bits()), h);
    println!("f16 bits(0.1) = {:#06x} f128 eps = {:e}", h.to_bits(), f128::EPSILON as f64);
}
