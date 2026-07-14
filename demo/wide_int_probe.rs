// M5.2 D8k 永久差分探针：128 位整数残余面（f↔wide 转换、128→128 cast、128 位
// bit intrinsic）与 native 对拍。
fn main() {
    let a: i128 = -170141183460469231731687303715884105727;
    let b: i128 = 12345678901234567890;
    let u: u128 = 0xDEAD_BEEF_CAFE_BABE_0123_4567_89AB_CDEF;

    // f↔wide 双向 + 饱和
    println!("f->i128: {} {} {}", 3.9e30f64 as i128, (-3.9e30f64) as i128, f64::NAN as i128);
    println!("f->u128: {} {}", 3.9e30f64 as u128, (-1.0f64) as u128);
    println!("i128->f: {} {}", (b as f64), (a as f64));
    println!("f32->i128: {}", 1.5e20f32 as i128);

    // 128->128 等宽 cast（位相同）
    println!("i128 as u128: {}", a as u128);
    println!("u128 as i128: {}", u as i128);

    // 128 位 bit intrinsic
    println!("count_ones: {} {}", u.count_ones(), (b as u128).count_ones());
    println!("lead/trail zeros: {} {}", u.leading_zeros(), u.trailing_zeros());
    println!("swap/reverse: {} {}", u.swap_bytes(), u.reverse_bits());
    println!("rotate: {} {}", b.rotate_left(5), u.rotate_right(13));

    // 混合链
    let r = ((u as i128).wrapping_add(b) as f64) as u128;
    println!("chain: {r}");
}
