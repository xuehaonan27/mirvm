fn main() {
    // 确定性输出（gate 的 well-defined oracle）：memchr::memmem::find 的命中下标
    let hay = b"one: the quick brown fox";
    let at = memchr::memmem::find(hay, b"quick").expect("must find");
    println!("a2_one: found quick at {at}");
}
