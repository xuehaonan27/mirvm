#!/usr/bin/env mirvm
---
[dependencies]
p256 = { version = "0.13", features = ["ecdh"] }
---
// p256 (NIST P-256): fixed scalar -> SecretKey -> SEC1 public key; RFC6979 deterministic
// ECDSA (same key+msg signs identically) with both verify outcomes; ECDH two-way secret.
use p256::{
    ecdh::diffie_hellman,
    ecdsa::{
        signature::{Signer, Verifier},
        Signature, SigningKey, VerifyingKey,
    },
    elliptic_curve::sec1::ToEncodedPoint,
    PublicKey, SecretKey,
};

fn hex(bytes: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(T[(b >> 4) as usize] as char);
        s.push(T[(b & 0x0f) as usize] as char);
    }
    s
}

fn main() {
    // ① Fixed scalar -> SecretKey. k1 is the x from RFC6979 appendix A.2.5,
    //    so its signature can be checked against the public test vector.
    let k1: [u8; 32] = [
        0xc9, 0xaf, 0xa9, 0xd8, 0x45, 0xba, 0x75, 0x16,
        0x6b, 0x5c, 0x21, 0x57, 0x67, 0xb1, 0xd6, 0x93,
        0x4e, 0x50, 0xc3, 0xdb, 0x36, 0xe8, 0x9b, 0x12,
        0x7b, 0x8a, 0x62, 0x2b, 0x12, 0x0f, 0x67, 0x21,
    ];
    let mut k2 = [0u8; 32];
    for (i, b) in k2.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(17).wrapping_add(3);
    }

    let sk1 = SecretKey::from_slice(&k1).unwrap();
    let sk2 = SecretKey::from_slice(&k2).unwrap();
    println!("sk1 scalar roundtrip = {}", sk1.to_bytes()[..] == k1[..]);
    println!("sk2 scalar = {}", hex(&sk2.to_bytes()));
    // Error path: a zero scalar and scalars >= n must be rejected
    println!("zero scalar err = {}", SecretKey::from_slice(&[0u8; 32]).is_err());
    println!("ff..ff scalar err = {}", SecretKey::from_slice(&[0xffu8; 32]).is_err());

    // ② SEC1 public-key encoding (65B uncompressed / 33B compressed) + parse roundtrip
    let pk1 = sk1.public_key();
    let pk2 = sk2.public_key();
    let ep1u = pk1.to_encoded_point(false);
    let ep1c = pk1.to_encoded_point(true);
    println!("pk1 sec1 uncompressed = {}", hex(ep1u.as_bytes()));
    println!("pk1 sec1 compressed   = {}", hex(ep1c.as_bytes()));
    println!("pk2 sec1 compressed   = {}", hex(pk2.to_encoded_point(true).as_bytes()));
    let pk1_back = PublicKey::from_sec1_bytes(ep1c.as_bytes()).unwrap();
    println!("pk1 sec1 parse roundtrip = {}", pk1 == pk1_back);
    println!("pk1 x = {}", hex(ep1u.x().unwrap()));
    println!("pk1 y = {}", hex(ep1u.y().unwrap()));

    // ③ RFC6979 deterministic ECDSA: signing one key+msg twice must be byte-for-byte equal
    let signing1 = SigningKey::from_slice(&k1).unwrap();
    let verifying1 = VerifyingKey::from(&signing1);
    let msgs: [&[u8]; 3] = [
        b"sample",
        b"test",
        b"mirvm differential corpus: p256 ecdsa/rfc6979",
    ];
    for (i, msg) in msgs.iter().enumerate() {
        let sig: Signature = signing1.sign(msg);
        let sig_again: Signature = signing1.sign(msg);
        println!(
            "sig[{i}] r = {}\nsig[{i}] s = {}",
            hex(&sig.r().to_bytes()),
            hex(&sig.s().to_bytes())
        );
        println!("sig[{i}] deterministic = {}", sig == sig_again);
        println!("sig[{i}] der = {}", hex(sig.to_der().as_bytes()));
        println!("sig[{i}] verify ok = {}", verifying1.verify(msg, &sig).is_ok());
        // Negative case A: wrong message
        println!(
            "sig[{i}] verify wrong-msg ok = {}",
            verifying1.verify(b"wrong message", &sig).is_ok()
        );
        // Negative case B: tampered signature (flip the last byte of s)
        let mut bad = sig.to_bytes();
        let n = bad.len();
        bad[n - 1] ^= 0x01;
        match Signature::from_slice(&bad) {
            Ok(bs) => println!(
                "sig[{i}] verify tampered ok = {}",
                verifying1.verify(msg, &bs).is_ok()
            ),
            Err(_) => println!("sig[{i}] tampered sig rejected at parse"),
        }
    }
    // Negative case C: a different public key
    let signing2 = SigningKey::from_slice(&k2).unwrap();
    let verifying2 = VerifyingKey::from(&signing2);
    let sig1: Signature = signing1.sign(b"sample");
    println!(
        "verify with wrong key ok = {}",
        verifying2.verify(b"sample", &sig1).is_ok()
    );
    // VerifyingKey SEC1 roundtrip
    let vk1_back = VerifyingKey::from_sec1_bytes(ep1c.as_bytes()).unwrap();
    println!("vk1 sec1 roundtrip = {}", verifying1 == vk1_back);

    // ④ ECDH: each side uses its own private key x the peer public key; both shared secrets must match
    let ab = diffie_hellman(sk1.to_nonzero_scalar(), pk2.as_affine());
    let ba = diffie_hellman(sk2.to_nonzero_scalar(), pk1.as_affine());
    println!("ecdh ab = {}", hex(ab.raw_secret_bytes()));
    println!("ecdh two-way equal = {}", ab.raw_secret_bytes() == ba.raw_secret_bytes());
    // Self-exchange (a different path from the peer secret, so the value should differ)
    let aa = diffie_hellman(sk1.to_nonzero_scalar(), pk1.as_affine());
    println!("ecdh self = {}", hex(aa.raw_secret_bytes()));
    println!("ecdh self != ab = {}", aa.raw_secret_bytes() != ab.raw_secret_bytes());
}
