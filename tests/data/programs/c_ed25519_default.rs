#!/usr/bin/env mirvm
---
[dependencies]
# =2.2.0 patch-pinned so the closure stays rebuildable offline. No backend env is
# set: the default (x86_64+nightly auto simd) backend is what this case exercises.
ed25519-dalek = "=2.2.0"
# Indirect dependency pinned with an explicit patch (4.1.3) so a new registry patch
# cannot drift the closure into something unreproducible.
curve25519-dalek = "=4.1.3"
---
// Runs ed25519-dalek on its default simd backend, with no backend env set.
//
// Pinning rationale: curve25519-dalek 4.1.3 auto-selects a simd backend on
// x86_64+nightly; the Avx2/ifma multi-version functions are compiled with
// #[target_feature] and dispatched at runtime from guest CPUID (passed straight
// through from the host). With avx512ifma present, dispatch selects the ifma path
// so field multiplication uses llvm.x86.avx512.vpmadd52*; those intrinsics are
// built in, so this driver sets no env and runs the default path. The gate injects
// CARGO_CFG_CURVE25519_DALEK_BACKEND=serial for c_ed25519 only.
//
// Coverage:
//   ① RFC8032 §7.1 TEST 1/2/3: sk→pk derivation and sign anchored byte-for-byte to
//      the vectors, plus verify / verify_strict positives (assert + stable print);
//   ② fixed-seed keygen (two hardcoded seeds × 3 messages: empty / short / 175B
//      spanning two SHA-512 blocks): pk/sig hex prints, re-sign determinism,
//      serialization roundtrip, flip-msg / flip-sig and cross-key negatives,
//      fnv1a summary;
//   ③ decompression / small-order edges: all-zero y (a quadratic-residue
//      coincidence → order-2 point) and y=1 identity both decompress; strict
//      verification rejects them. Backend consistency is checked manually (see ④).
//
// Determinism: no randomness, time or env reads; seeds and messages are hardcoded,
// ed25519 signing has no nonce entropy (RFC8032), and output is hex/boolean lines (~60).
// FRONTIER: none (the vpmadd52 intrinsics are built in).
//
// Re-run (from the repo root):
//   A: target/release/mirvm run tests/data/programs/c_ed25519_default.rs
//   B: cd $(dirname $(grep -l 'name = "c_ed25519_default"' ~/.cache/mirvm/scripts/*/Cargo.toml)) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_ed25519_default.rs
//   ④ backend comparison (manual): CARGO_CFG_CURVE25519_DALEK_BACKEND=serial \
//      target/release/mirvm run tests/data/programs/c_ed25519_default.rs -- stdout must match A byte-for-byte.
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex len");
    let b = s.as_bytes();
    let nib = |c: u8| -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("bad hex char"),
        }
    };
    (0..s.len() / 2)
        .map(|i| (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]))
        .collect()
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// RFC8032 §7.1 test vectors (Ed25519, TEST 1/2/3).
const RFC: [(&str, &str, &str, &str); 3] = [
    (
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
         085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
         18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
];

/// ① RFC8032 vectors: derivation and signature hard-anchored by assert; prints are the record.
fn rfc_vectors() {
    for (i, (seed_h, pk_h, msg_h, sig_h)) in RFC.iter().enumerate() {
        let n = i + 1;
        let seed: [u8; 32] = unhex(seed_h).try_into().unwrap();
        let pk_expected: [u8; 32] = unhex(pk_h).try_into().unwrap();
        let msg = unhex(msg_h);
        let sig_expected: [u8; 64] = unhex(&sig_h.replace(char::is_whitespace, ""))
            .try_into()
            .unwrap();

        let sk = SigningKey::from_bytes(&seed);
        let pk = sk.verifying_key().to_bytes();
        println!("rfc{n} pk = {}", hex(&pk));
        assert_eq!(pk, pk_expected, "rfc{n} pk derivation deviates from RFC8032");

        let sig = sk.sign(&msg);
        let sig_bytes = sig.to_bytes();
        println!("rfc{n} sig = {}", hex(&sig_bytes));
        assert_eq!(sig_bytes, sig_expected, "rfc{n} signature deviates from RFC8032");

        let vk = VerifyingKey::from_bytes(&pk_expected).unwrap();
        println!("rfc{n} verify = {}", vk.verify(&msg, &sig).is_ok());
        println!("rfc{n} strict = {}", vk.verify_strict(&msg, &sig).is_ok());
    }
}

/// ② Fixed-seed keygen and sign/verify: stable prints plus positives and negatives.
fn round(label: &str, seed: [u8; 32], msgs: &[&[u8]], sink: &mut Vec<u8>) {
    let sk = SigningKey::from_bytes(&seed);
    let vk: VerifyingKey = sk.verifying_key();
    println!("{label} pk = {}", hex(vk.as_bytes()));

    println!(
        "{label} sk roundtrip = {}",
        SigningKey::from_bytes(&sk.to_bytes()).to_bytes() == seed
    );
    let vk2 = VerifyingKey::from_bytes(vk.as_bytes()).unwrap();
    println!("{label} vk roundtrip = {}", vk == vk2);

    for (i, msg) in msgs.iter().enumerate() {
        let sig: Signature = sk.sign(msg);
        let sig_bytes = sig.to_bytes();
        sink.extend_from_slice(&sig_bytes);
        println!("{label} sig{i} (len {}) = {}", msg.len(), hex(&sig_bytes));

        // ed25519 signing is deterministic: re-signing the same message must be byte-identical
        println!(
            "{label} sig{i} deterministic = {}",
            sig_bytes == sk.sign(msg).to_bytes()
        );
        println!("{label} sig{i} roundtrip = {}", Signature::from_bytes(&sig_bytes).to_bytes() == sig_bytes);
        println!("{label} sig{i} verify = {}", vk.verify(msg, &sig).is_ok());
        println!("{label} sig{i} strict = {}", vk.verify_strict(msg, &sig).is_ok());

        // Negative case 1: flip one message byte → must fail
        let mut bad_msg = msg.to_vec();
        if bad_msg.is_empty() {
            bad_msg.push(0x01);
        } else {
            bad_msg[0] ^= 0x01;
        }
        println!("{label} sig{i} flip-msg err = {}", vk.verify(&bad_msg, &sig).is_err());

        // Negative case 2: flip one signature byte → must fail
        let mut bad_bytes = sig_bytes;
        bad_bytes[10] ^= 0x80;
        let bad_sig = Signature::from_bytes(&bad_bytes);
        println!("{label} sig{i} flip-sig err = {}", vk.verify(msg, &bad_sig).is_err());
    }
}

fn main() {
    rfc_vectors();

    // ② Two hardcoded seeds: one constant, one derived by xorshift.
    let seed_a = [0x2a_u8; 32];
    let mut seed_b = [0u8; 32];
    let mut x = 0x9E37_79B9_7F4A_7C15_u64;
    for b in seed_b.iter_mut() {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *b = x.wrapping_mul(0x2545_F491_4F6C_DD1D) as u8;
    }

    // Three messages: empty / short / 175B (spanning two 128B SHA-512 blocks).
    let mut long = Vec::new();
    for i in 0..175_u32 {
        long.push(i.wrapping_mul(7).wrapping_add(3) as u8);
    }
    let msgs: [&[u8]; 3] = [b"", b"mirvm ed25519 default-backend differential", long.as_slice()];

    let mut sink = Vec::new();
    round("seedA", seed_a, &msgs, &mut sink);
    round("seedB", seed_b, &msgs, &mut sink);

    // Cross-key negative: seedA's signature must not verify under seedB's public key
    let sig_a = SigningKey::from_bytes(&seed_a).sign(msgs[2]);
    let vk_b = SigningKey::from_bytes(&seed_b).verifying_key();
    println!("cross-key verify err = {}", vk_b.verify(msgs[2], &sig_a).is_err());

    // Decompression / small-order edges: all-zero bytes = y=0 gives x²=(y²-1)/(dy²+1)=-1,
    // exactly a quadratic residue mod p → decompression succeeds (order-2 point (±√-1, 0),
    // small order); strict rejects. y=1 identity decompresses; basic verify fails, strict rejects.
    let vk_zero = VerifyingKey::from_bytes(&[0u8; 32]).unwrap();
    println!("vk all-zero decompress ok = true");
    let rfc_msg = unhex(RFC[0].2);
    let rfc_sig = Signature::from_bytes(
        &unhex(&RFC[0].3.replace(char::is_whitespace, ""))
            .try_into()
            .unwrap(),
    );
    println!("vk all-zero strict err = {}", vk_zero.verify_strict(&rfc_msg, &rfc_sig).is_err());
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let vk_id = VerifyingKey::from_bytes(&identity).unwrap();
    println!("vk identity basic = {}", vk_id.verify(&rfc_msg, &rfc_sig).is_ok());
    println!("vk identity strict err = {}", vk_id.verify_strict(&rfc_msg, &rfc_sig).is_err());

    println!("sigs total len = {}", sink.len());
    println!("sigs fnv1a = {:016x}", fnv1a(&sink));
}
