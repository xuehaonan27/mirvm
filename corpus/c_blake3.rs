#!/usr/bin/env mirvm
---
[dependencies]
blake3 = "1"
---
// SIMD 密集哈希：压 simd_* / 平台 intrinsic 家族。
// blake3 会按 target-feature 走 SSE/AVX 路径。
fn main() {
    let mut hasher = blake3::Hasher::new();
    for i in 0..1000u32 {
        hasher.update(&i.to_le_bytes());
    }
    let hash = hasher.finalize();
    println!("blake3(0..1000 le) = {}", hash.to_hex());

    // 已知向量：空输入
    let empty = blake3::hash(b"");
    println!("blake3(\"\") = {}", empty.to_hex());

    // 已知向量："hello"
    let h = blake3::hash(b"hello");
    println!("blake3(\"hello\") = {}", h.to_hex());
}
