---
[dependencies]
itoa = "1"
memchr = "2"
---

// differential.cargoless frontmatter script fixture.
// Deterministic output — byte-identical between cargo leg (MIRVM_DEPS=cargo) and self leg (MIRVM_DEPS=self).
fn main() {
    let mut buf = itoa::Buffer::new();
    let printed = buf.format(12345);
    let pos = memchr::memchr(b'o', b"hello world").unwrap();
    println!("cless-script itoa={printed} memchr={pos}");
    std::process::exit(4);
}
