#!/usr/bin/env mirvm
---
[dependencies]
aes-gcm = "0.10"
---
// AES-128-GCM AEAD over a fixed key/nonce/aad/plaintext, compared byte-for-byte
// with native. Covers: alloc API (encrypt/decrypt -> Vec), Payload AAD variants,
// in-place API, boundary lengths (0/1/block spanning), tampered ciphertext/tag/AAD.
// Error paths must return is_err.
// NOTE: on x86_64 aes/ghash pick the AES-NI/PCLMULQDQ path via runtime cpuid.
use aes_gcm::aead::{Aead, AeadInPlace, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, Key, Nonce};

const KEY: [u8; 16] = [
    0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6,
    0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c,
];
const NONCE: [u8; 12] = [
    0xf0, 0xe1, 0xd2, 0xc3, 0xb4, 0xa5, 0x96, 0x87, 0x78, 0x69, 0x5a, 0x4b,
];

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// `blob = ciphertext || tag(16B)`; split apart before printing.
fn report(label: &str, blob: &[u8]) {
    let (ct, tag) = blob.split_at(blob.len() - 16);
    println!("{label} ct   = {}", hex(ct));
    println!("{label} tag  = {}", hex(tag));
    println!("{label} len  = {} (ct={} tag={})", blob.len(), ct.len(), tag.len());
}

fn main() {
    let key = Key::<Aes128Gcm>::from_slice(&KEY);
    let cipher = Aes128Gcm::new(key);
    let nonce = Nonce::from_slice(&NONCE);

    // ---- group 1: no AAD, plaintext spanning blocks (38B over 3 CTR blocks) ----
    let msg = b"mirvm differential test: AES-128-GCM!!";
    println!("pt len = {}", msg.len());
    let blob = cipher.encrypt(nonce, msg.as_ref()).unwrap();
    report("noaad", &blob);

    let back = cipher.decrypt(nonce, blob.as_slice()).unwrap();
    println!("noaad roundtrip = {}", back == msg);

    // flipping one ciphertext byte must fail authentication
    let mut tampered = blob.clone();
    tampered[7] ^= 0x01;
    println!("noaad tamper ct[7] is_err = {}", cipher.decrypt(nonce, tampered.as_slice()).is_err());
    // flipping the last tag byte must fail authentication
    let mut badtag = blob.clone();
    let last = badtag.len() - 1;
    badtag[last] ^= 0x80;
    println!("noaad tamper tag is_err  = {}", cipher.decrypt(nonce, badtag.as_slice()).is_err());

    // ---- group 2: AAD variants ----
    let aad = b"hdr:v1;seq=42;flags=0x5a";
    let blob2 = cipher
        .encrypt(nonce, Payload { msg, aad })
        .unwrap();
    report("aad", &blob2);
    let back2 = cipher
        .decrypt(nonce, Payload { msg: blob2.as_slice(), aad })
        .unwrap();
    println!("aad roundtrip = {}", back2 == msg);
    // decrypting with the wrong AAD must fail
    let wrong = cipher.decrypt(
        nonce,
        Payload { msg: blob2.as_slice(), aad: b"hdr:v1;seq=43;flags=0x5a" },
    );
    println!("aad wrong-aad is_err = {}", wrong.is_err());
    // decrypting without the AAD must fail
    let no_aad = cipher.decrypt(nonce, blob2.as_slice());
    println!("aad missing-aad is_err = {}", no_aad.is_err());

    // ---- group 3: boundary lengths (0B / 1B / exactly one 16B block) ----
    for (label, m) in [
        ("empty", &b""[..]),
        ("one", &b"x"[..]),
        ("block", b"sixteen bytes!!!".as_slice()),
    ] {
        let b = cipher.encrypt(nonce, Payload { msg: m, aad }).unwrap();
        let r = cipher.decrypt(nonce, Payload { msg: b.as_slice(), aad }).unwrap();
        println!("{label}: blob_len={} roundtrip={}", b.len(), r == m);
        if label != "empty" {
            println!("{label} tag = {}", hex(&b[b.len() - 16..]));
        } else {
            // empty plaintext: the blob is the tag alone
            println!("{label} tag = {}", hex(&b));
        }
    }

    // ---- group 4: in-place API (Vec<u8> as the buffer) ----
    let mut buf = msg.to_vec();
    cipher.encrypt_in_place(nonce, aad, &mut buf).unwrap();
    println!("inplace enc eq alloc = {}", buf == blob2);
    cipher.decrypt_in_place(nonce, aad, &mut buf).unwrap();
    println!("inplace roundtrip = {}", buf == msg);
    // in-place tampering must fail too
    let mut bad = blob2.clone();
    bad[3] ^= 0xff;
    println!("inplace tamper is_err = {}", cipher.decrypt_in_place(nonce, aad, &mut bad).is_err());

    // ---- group 5: wrong key/nonce must fail to decrypt sealed data ----
    let other_key = Key::<Aes128Gcm>::from_slice(&[0x00; 16]);
    let c2 = Aes128Gcm::new(other_key);
    println!("wrong key is_err = {}", c2.decrypt(nonce, blob.as_slice()).is_err());
    let other_nonce = Nonce::from_slice(&[0x11; 12]);
    println!("wrong nonce is_err = {}", cipher.decrypt(other_nonce, blob.as_slice()).is_err());
}
