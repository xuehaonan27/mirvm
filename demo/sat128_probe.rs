//! T1-d 回归探针：Bin128 with_overflow 的旗标布局（(u128,bool) 旗标写
//! dst+16）——frame 落帧区间必须覆盖 17 字节，欠覆盖时旗标槽 SSA 提升、
//! JIT 写物理帧而读侧取 SSA 零值 = 溢出旗假阴性（saturating_mul 给包绕值）。
//! 热循环越阈后以编译码执行，digest 与 native 逐字节一致。

#[inline(never)]
fn sat_u(a: u128, b: u128) -> (u128, u128, u128) {
    (a.saturating_mul(b), a.saturating_add(b), a.saturating_sub(b))
}

#[inline(never)]
fn sat_s(a: i128, b: i128) -> (i128, i128, i128) {
    (a.saturating_mul(b), a.saturating_add(b), a.saturating_sub(b))
}

#[inline(never)]
fn cm_u(a: u128, b: u128) -> u128 {
    // Option<u128> 判别位读写链（checked_mul → is_some/unwrap_or）
    a.checked_mul(b).unwrap_or(0xdead)
}

fn main() {
    let mut acc: u128 = 0;
    for i in 0..30000u128 {
        let (m, a, s) = sat_u(5 ^ i, u128::MAX / 3);
        acc ^= m ^ a.rotate_left(7) ^ s.rotate_left(13);
        let (m, a, s) = sat_s(-5 ^ (i as i128), i128::MAX / 3);
        acc ^= (m as u128) ^ (a as u128).rotate_left(3) ^ (s as u128).rotate_left(11);
        acc = acc.rotate_left(1) ^ cm_u(7 ^ i, u128::MAX / 5);
    }
    println!("digest={acc:032x}");
}
