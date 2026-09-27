fn main() {
    let text = b"the quick brown fox";
    let at = memchr::memchr(b'q', text).expect("q is there");
    let mut buffer = itoa::Buffer::new();
    println!("unit_b: {at} {}", buffer.format(at));
}
