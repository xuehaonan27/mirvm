#!/usr/bin/env mirvm
---
[dependencies]
aes-gcm = "0.10"
---
// AES-128-GCM AEAD：固定 key/nonce/aad/明文的确定性加解密对拍。
// 覆盖：alloc API（encrypt/decrypt → Vec）、Payload AAD 变体、
// in-place API（encrypt_in_place/decrypt_in_place）、边界长度（0/1/跨块）、
// 篡改密文/tag/AAD 的错误路径（必须 is_err）。
// 注意：aes/ghash 在 x86_64 上运行期 cpuid 探测 AES-NI/PCLMULQDQ 选硬件路径。
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

/// blob = ciphertext || tag(16B)，拆开打印。
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

    // ---- 组 1：无 AAD，跨块明文（38B，跨 3 个 CTR 块）----
    let msg = b"mirvm differential test: AES-128-GCM!!";
    println!("pt len = {}", msg.len());
    let blob = cipher.encrypt(nonce, msg.as_ref()).unwrap();
    report("noaad", &blob);

    let back = cipher.decrypt(nonce, blob.as_slice()).unwrap();
    println!("noaad roundtrip = {}", back == msg);

    // 篡改一个密文字节 → 认证必须失败
    let mut tampered = blob.clone();
    tampered[7] ^= 0x01;
    println!("noaad tamper ct[7] is_err = {}", cipher.decrypt(nonce, tampered.as_slice()).is_err());
    // 篡改 tag 末字节 → 认证必须失败
    let mut badtag = blob.clone();
    let last = badtag.len() - 1;
    badtag[last] ^= 0x80;
    println!("noaad tamper tag is_err  = {}", cipher.decrypt(nonce, badtag.as_slice()).is_err());

    // ---- 组 2：AAD 变体 ----
    let aad = b"hdr:v1;seq=42;flags=0x5a";
    let blob2 = cipher
        .encrypt(nonce, Payload { msg, aad })
        .unwrap();
    report("aad", &blob2);
    let back2 = cipher
        .decrypt(nonce, Payload { msg: blob2.as_slice(), aad })
        .unwrap();
    println!("aad roundtrip = {}", back2 == msg);
    // 错误 AAD 解密必须失败
    let wrong = cipher.decrypt(
        nonce,
        Payload { msg: blob2.as_slice(), aad: b"hdr:v1;seq=43;flags=0x5a" },
    );
    println!("aad wrong-aad is_err = {}", wrong.is_err());
    // 缺 AAD 解密必须失败
    let no_aad = cipher.decrypt(nonce, blob2.as_slice());
    println!("aad missing-aad is_err = {}", no_aad.is_err());

    // ---- 组 3：边界长度（0B / 1B / 恰好 16B 一块）----
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
            // 空明文：blob 只有 tag
            println!("{label} tag = {}", hex(&b));
        }
    }

    // ---- 组 4：in-place API（Vec<u8> 作 Buffer）----
    let mut buf = msg.to_vec();
    cipher.encrypt_in_place(nonce, aad, &mut buf).unwrap();
    println!("inplace enc eq alloc = {}", buf == blob2);
    cipher.decrypt_in_place(nonce, aad, &mut buf).unwrap();
    println!("inplace roundtrip = {}", buf == msg);
    // in-place 篡改也必须失败
    let mut bad = blob2.clone();
    bad[3] ^= 0xff;
    println!("inplace tamper is_err = {}", cipher.decrypt_in_place(nonce, aad, &mut bad).is_err());

    // ---- 组 5：错误 key/nonce 解密已加数据必须失败 ----
    let other_key = Key::<Aes128Gcm>::from_slice(&[0x00; 16]);
    let c2 = Aes128Gcm::new(other_key);
    println!("wrong key is_err = {}", c2.decrypt(nonce, blob.as_slice()).is_err());
    let other_nonce = Nonce::from_slice(&[0x11; 12]);
    println!("wrong nonce is_err = {}", cipher.decrypt(other_nonce, blob.as_slice()).is_err());
}
