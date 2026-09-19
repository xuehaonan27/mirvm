#!/usr/bin/env mirvm
---
[dependencies]
sha2 = "0.10"
hex = "0.4"
---
// SHA-256 hashing. The RustCrypto crates use cpufeatures for SHA-NI runtime
// detection, which is expected to hit __cpuid inline asm just like blake3.
use sha2::{Digest, Sha256};

fn main() {
    let mut h = Sha256::new();
    h.update(b"hello world");
    let out = h.finalize();
    println!("sha256(hello world) = {}", hex::encode(out));

    // Known-answer vector: empty input
    println!("sha256() = {}", hex::encode(Sha256::digest(b"")));
}
