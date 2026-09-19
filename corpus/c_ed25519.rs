#!/usr/bin/env mirvm
---
[dependencies]
ed25519-dalek = "2"
---
// ed25519-dalek: fixed seeds -> SigningKey, public-key/signature hex, verify positive and
// negative cases plus a cross-key case. Field arithmetic uses curve25519-dalek's u128 serial backend.
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn round(label: &str, seed: [u8; 32], msgs: &[&[u8]], sink: &mut Vec<u8>) {
    let sk = SigningKey::from_bytes(&seed);
    let vk: VerifyingKey = sk.verifying_key();
    println!("{label} pk = {}", hex(&vk.to_bytes()));

    // secret/public key serialization round-trip
    println!("{label} sk roundtrip = {}", SigningKey::from_bytes(&sk.to_bytes()).to_bytes() == seed);
    let vk2 = VerifyingKey::from_bytes(&vk.to_bytes()).unwrap();
    println!("{label} vk roundtrip = {}", vk == vk2);

    for (i, msg) in msgs.iter().enumerate() {
        let sig: Signature = sk.sign(msg);
        let sig_bytes = sig.to_bytes();
        sink.extend_from_slice(&sig_bytes);
        println!("{label} sig{i} (msg len {}) = {}", msg.len(), hex(&sig_bytes));

        // ed25519 signatures are deterministic: signing the same message again must be byte-identical
        let sig_again: Signature = sk.sign(msg);
        println!("{label} sig{i} deterministic = {}", sig_bytes == sig_again.to_bytes());

        // signature serialization round-trip
        let sig_rt = Signature::from_bytes(&sig_bytes);
        println!("{label} sig{i} roundtrip = {}", sig_rt.to_bytes() == sig_bytes);

        // positive case: the original message and signature must verify
        println!("{label} sig{i} verify ok = {}", vk.verify(msg, &sig).is_ok());
        println!("{label} sig{i} verify_strict ok = {}", vk.verify_strict(msg, &sig).is_ok());

        // negative case 1: flip one message byte, which must fail
        let mut bad_msg = msg.to_vec();
        if bad_msg.is_empty() {
            bad_msg.push(0x01);
        } else {
            bad_msg[0] ^= 0x01;
        }
        println!("{label} sig{i} verify flipped-msg err = {}", vk.verify(&bad_msg, &sig).is_err());

        // negative case 2: flip one signature byte, which must fail
        let mut bad_bytes = sig_bytes;
        bad_bytes[10] ^= 0x80;
        let bad_sig = Signature::from_bytes(&bad_bytes);
        println!("{label} sig{i} verify flipped-sig err = {}", vk.verify(msg, &bad_sig).is_err());
    }
}

fn main() {
    let msgs: [&[u8]; 2] = [
        b"",
        "mirvm ↔ native 对拍：ed25519 域算术 \u{0}\u{fe}\u{ff}".as_bytes(),
    ];

    let seed0 = [0x2a_u8; 32];
    let mut seed1 = [0u8; 32];
    for (i, b) in seed1.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(1);
    }

    let mut sink = Vec::new();
    round("seed0", seed0, &msgs, &mut sink);
    round("seed1", seed1, &msgs, &mut sink);

    // cross-seed negative case: seed0's signature must not verify under seed1's key
    let sig0 = SigningKey::from_bytes(&seed0).sign(msgs[1]);
    let vk1 = SigningKey::from_bytes(&seed1).verifying_key();
    println!("cross-key verify err = {}", vk1.verify(msgs[1], &sig0).is_err());

    // summary: total signature length and an FNV-1a checksum
    println!("sigs total len = {}", sink.len());
    println!("sigs fnv1a = {:016x}", fnv1a(&sink));
}
