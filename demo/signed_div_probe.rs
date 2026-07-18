// C 维 corpus c_resvg_svg 实锤回归探针（jit_compile int_bin signed Div/Rem
// 未按槽零扩不变量先 sext：窄宽负值被当大正数做 sdiv/srem；另 b=-1 特判支
// 错回被除数而非 -x）。tiny-skia fdot16::fast_div（(a<<16)/b，a 可为负 fdot6
// 差）在 JIT 下算出巨正 slope，hairline_aa 的 |slope|≤ONE 断言炸（exit 101）。
// 修复后逢调即编维与 native 逐位一致。
use std::hint::black_box;

#[inline(never)]
fn sdiv8(a: i8, b: i8) -> i8 {
    a / b
}
#[inline(never)]
fn srem16(a: i16, b: i16) -> i16 {
    a % b
}
#[inline(never)]
fn sdiv32(a: i32, b: i32) -> i32 {
    a / b
}
#[inline(never)]
fn srem32(a: i32, b: i32) -> i32 {
    a % b
}
#[inline(never)]
fn sdiv64(a: i64, b: i64) -> i64 {
    a / b
}
#[inline(never)]
fn srem64(a: i64, b: i64) -> i64 {
    a % b
}
/// tiny-skia fixed_point::fdot16::fast_div 同形：left_shift(a,16)/b
#[inline(never)]
fn fast_div_like(a: i32, b: i32) -> i32 {
    ((a as u32) << 16) as i32 / b
}

fn main() {
    let mut acc = 0i64;
    for _ in 0..1_000_000 {
        acc += sdiv8(black_box(-100), black_box(3)) as i64; // -33
        acc += srem16(black_box(-30000), black_box(7)) as i64; // -5
        acc += sdiv32(black_box(-5), black_box(2)) as i64; // -2
        acc += srem32(black_box(-5), black_box(2)) as i64; // -1
        acc += sdiv32(black_box(i32::MIN), black_box(2)) as i64; // -1073741824
        acc += sdiv64(black_box(10), black_box(-1)); // -10（b=-1 特判支）
        acc += srem64(black_box(10), black_box(-1)); // 0
        acc += fast_div_like(black_box(-96), black_box(64)) as i64; // -98304
    }
    println!("acc={acc}");
}
