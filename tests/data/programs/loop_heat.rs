#[inline(never)]
fn short_loop() -> u64 {
    let mut sum = 0;
    for value in 0..128 {
        sum += std::hint::black_box(value);
    }
    sum
}

fn main() {
    let mut sum = 0;
    for _ in 0..600 {
        sum += short_loop();
    }
    println!("{sum}");
}
