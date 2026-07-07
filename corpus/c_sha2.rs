#!/usr/bin/env mirvm
---
[dependencies]
sha2 = "0.10"
hex = "0.4"
---
// SHA-256 哈希。RustCrypto 系用 cpufeatures 做 SHA-NI 运行时检测 →
// 预期与 blake3 同样撞 __cpuid 内联汇编（验证 §2.2 是一整类）。
use sha2::{Digest, Sha256};

fn main() {
    let mut h = Sha256::new();
    h.update(b"hello world");
    let out = h.finalize();
    println!("sha256(hello world) = {}", hex::encode(out));

    // 已知向量：空输入
    println!("sha256() = {}", hex::encode(Sha256::digest(b"")));
}
