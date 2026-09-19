fn main() {
    // Deterministic output (the gate's well-defined oracle): hit index from memchr::memmem::find
    let hay = b"one: the quick brown fox";
    let at = memchr::memmem::find(hay, b"quick").expect("must find");
    println!("a2_one: found quick at {at}");
}
