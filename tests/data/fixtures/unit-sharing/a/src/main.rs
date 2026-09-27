fn main() {
    let text = b"the quick brown fox";
    let at = memchr::memchr(b'q', text).expect("q is there");
    println!("unit_a: {at}");
}
