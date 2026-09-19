#[inline(never)]
fn select_unsigned(value: u128) -> u8 {
    match value {
        0 => 10,
        0x1_0000_0000_0000_0000 => 20,
        u128::MAX => 40,
        _ => 30,
    }
}

#[inline(never)]
fn select_signed(value: i128) -> u8 {
    match value {
        -1 => 50,
        i128::MIN => 60,
        _ => 70,
    }
}

fn main() {
    let high = std::hint::black_box(1u64);
    let high_case = select_unsigned((high as u128) << 64);
    let max_case = select_unsigned(std::hint::black_box(u128::MAX));
    let negative_case = select_signed(std::hint::black_box(-1));
    let min_case = select_signed(std::hint::black_box(i128::MIN));
    println!("wide={high_case} max={max_case} negative={negative_case} min={min_case}");
}
