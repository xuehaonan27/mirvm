---
[dependencies]
itoa = "1"
memchr = "2"
---

// D15 P2 切① 对拍夹具（tests/diff_cless.sh）：frontmatter 脚本形态。
// 输出确定性文本——cargo 腿（MIRVM_DEPS=cargo）与 self 腿（MIRVM_DEPS=self）
// 逐字节对拍。
fn main() {
    let mut buf = itoa::Buffer::new();
    let printed = buf.format(12345);
    let pos = memchr::memchr(b'o', b"hello world").unwrap();
    println!("cless-script itoa={printed} memchr={pos}");
    std::process::exit(4);
}
