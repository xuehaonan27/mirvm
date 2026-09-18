fn main() {
    // Same dependency set as a2_one (S3′c: the second bin in the same workspace should get the same deps-image for free)
    let hay = b"two: pack my box with five dozen liquor jugs";
    let at = memchr::memmem::find(hay, b"box").expect("must find");
    println!("a2_two: found box at {at}");
}
