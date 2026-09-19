// Regression probe for signed Div/Rem in int_bin: the slot zero-extension invariant must sign
// extend first, because a narrow negative value is otherwise treated as a large positive for
// sdiv/srem, and the b=-1 special case must return -x rather than the dividend. In tiny-skia
// fdot16::fast_div ((a<<16)/b, where a can be a negative fdot6 delta) that produced a huge
// positive slope under JIT, tripping hairline_aa's |slope|<=ONE assertion. Must match native.
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
/// Same shape as tiny-skia fixed_point::fdot16::fast_div: left_shift(a,16)/b
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
        acc += sdiv64(black_box(10), black_box(-1)); // -10 (the b=-1 special case)
        acc += srem64(black_box(10), black_box(-1)); // 0
        acc += fast_div_like(black_box(-96), black_box(64)) as i64; // -98304
    }
    println!("acc={acc}");
}
