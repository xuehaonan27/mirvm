// M5.2 D8b：std::simd（portable SIMD）真实用法画像——点积/归一化/字节扫描/统计。
// 断言全部内联（corpus oracle = exit 0 + 输出逐字对齐 gate 预期）。
#![feature(portable_simd)]

use std::simd::StdFloat;
use std::simd::prelude::*;

fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = f32x8::splat(0.0);
    let chunks = a.len() / 8;
    for i in 0..chunks {
        let x = f32x8::from_slice(&a[i * 8..]);
        let y = f32x8::from_slice(&b[i * 8..]);
        acc = x.mul_add(y, acc);
    }
    let mut s = acc.reduce_sum();
    for i in chunks * 8..a.len() {
        s += a[i] * b[i];
    }
    s
}

fn count_byte(hay: &[u8], needle: u8) -> usize {
    let n = u8x16::splat(needle);
    let mut total = 0usize;
    let chunks = hay.len() / 16;
    for i in 0..chunks {
        let v = u8x16::from_slice(&hay[i * 16..]);
        total += v.simd_eq(n).to_bitmask().count_ones() as usize;
    }
    for &b in &hay[chunks * 16..] {
        total += (b == needle) as usize;
    }
    total
}

fn main() {
    let a: Vec<f32> = (0..67).map(|i| (i as f32) * 0.25).collect();
    let b: Vec<f32> = (0..67).map(|i| 1.0 - (i as f32) * 0.125).collect();
    println!("dot = {}", dot(&a, &b));

    let v = f64x4::from_array([3.0, -4.0, 12.0, 0.0]);
    let norm = (v * v).reduce_sum().sqrt();
    let unit = v / f64x4::splat(norm);
    println!("norm = {norm} unit0 = {} sum = {}", unit[0], unit.reduce_sum());

    let text = b"the quick brown fox jumps over the lazy dog; the end...".repeat(9);
    println!("count 'e' = {} 'q' = {}", count_byte(&text, b'e'), count_byte(&text, b'q'));

    let xs = i32x8::from_array([9, -3, 40, 7, -25, 0, 13, 6]);
    let clamped = xs.simd_clamp(i32x8::splat(-10), i32x8::splat(10));
    println!("clamp = {:?} min = {} max = {}", clamped.to_array(),
        xs.reduce_min(), xs.reduce_max());

    let bytes = u8x16::from_array(*b"0123456789abcdef");
    let digits = bytes.simd_lt(u8x16::splat(b'a'));
    println!("digits = {} rotate = {:?}", digits.to_bitmask(),
        bytes.rotate_elements_left::<3>().to_array());

    let f = f32x4::from_array([1.5, -2.5, 3.5, -4.5]);
    println!("round/abs = {:?} {:?}", f.round_ties_even().to_array(), f.abs().to_array());
    println!("as i32 = {:?}", f.cast::<i32>().to_array());
}
