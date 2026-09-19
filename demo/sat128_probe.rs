//! Regression probe for the Bin128 with_overflow flag layout (the (u128,bool) flag is written
//! at dst+16): the frame slot range must cover 17 bytes. Under-covering promotes the flag slot
//! to SSA, so the JIT writes the physical frame while the reader takes the SSA zero -- a false
//! negative overflow flag (saturating_mul returns the wrapped value). The hot loop runs compiled.

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
    // Option<u128> discriminant read/write chain (checked_mul -> is_some/unwrap_or)
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
