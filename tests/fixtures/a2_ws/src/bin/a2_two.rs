fn main() {
    // 与 a2_one 同依赖集（S3′c：同 workspace 第二 bin 应白拿同一张 deps-image）
    let hay = b"two: pack my box with five dozen liquor jugs";
    let at = memchr::memmem::find(hay, b"box").expect("must find");
    println!("a2_two: found box at {at}");
}
