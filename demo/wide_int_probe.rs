// M5.2 D8k permanent differential probe: 128-bit integer residual surface (f↔wide conversion, 128→128 cast, 128-bit
// bit intrinsics) compared against native.
fn main() {
    let a: i128 = -170141183460469231731687303715884105727;
    let b: i128 = 12345678901234567890;
    let u: u128 = 0xDEAD_BEEF_CAFE_BABE_0123_4567_89AB_CDEF;

    // f↔wide bidirectional + saturation
    println!("f->i128: {} {} {}", 3.9e30f64 as i128, (-3.9e30f64) as i128, f64::NAN as i128);
    println!("f->u128: {} {}", 3.9e30f64 as u128, (-1.0f64) as u128);
    println!("i128->f: {} {}", (b as f64), (a as f64));
    println!("f32->i128: {}", 1.5e20f32 as i128);

    // 128->128 same-width cast (bits unchanged)
    println!("i128 as u128: {}", a as u128);
    println!("u128 as i128: {}", u as i128);

    // 128-bit bit intrinsics
    println!("count_ones: {} {}", u.count_ones(), (b as u128).count_ones());
    println!("lead/trail zeros: {} {}", u.leading_zeros(), u.trailing_zeros());
    println!("swap/reverse: {} {}", u.swap_bytes(), u.reverse_bits());
    println!("rotate: {} {}", b.rotate_left(5), u.rotate_right(13));

    // mixed chain
    let r = ((u as i128).wrapping_add(b) as f64) as u128;
    println!("chain: {r}");
}
