#!/usr/bin/env mirvm
---
[dependencies]
blake3 = "1"
---
// SIMD-heavy hashing: exercises the simd_* / platform intrinsic family.
// blake3 selects its SSE/AVX path from the target features.
fn main() {
    let mut hasher = blake3::Hasher::new();
    for i in 0..1000u32 {
        hasher.update(&i.to_le_bytes());
    }
    let hash = hasher.finalize();
    println!("blake3(0..1000 le) = {}", hash.to_hex());

    // Known vector: empty input
    let empty = blake3::hash(b"");
    println!("blake3(\"\") = {}", empty.to_hex());

    // Known vector: "hello"
    let h = blake3::hash(b"hello");
    println!("blake3(\"hello\") = {}", h.to_hex());
}
