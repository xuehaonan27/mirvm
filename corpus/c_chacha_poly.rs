#!/usr/bin/env mirvm
---
[dependencies]
chacha20poly1305 = "0.10"
---
// chacha20poly1305 0.10（ChaCha20 流 cipher + Poly1305 MAC 的 AEAD）差分：
// ChaCha20-Poly1305 与 XChaCha20-Poly1305 两变体全 API 面对拍。
// 覆盖：RFC 8439 §2.8.2 与 draft-arciszewski-xchacha-03 A.1 已知答案自查
// （硬编码期望 ct/tag 双向比对：encrypt 对常量、decrypt 常量对明文）；
// 固定 key/nonce/aad 定向量（alloc encrypt/decrypt 打印 ct+tag hex）；
// in-place（encrypt_in_place/decrypt_in_place）与 detached-tag
// （encrypt_in_place_detached/decrypt_in_place_detached）同 alloc 逐字节一致性；
// 篡改 ct/tag、错 aad、缺 aad、错 key、错 nonce、截断 blob 的失败例（必须
// is_err）；明文谱系 0/1/63/64/65/127/128/129/255B（整块与跨 64B 块界，
// 每长度独立 nonce），输出 len+blob 长度+FNV-1a+roundtrip 布尔（短块附 hex）。
// 说明：chacha20 0.9 / poly1305 0.8 在 x86_64 运行期 cpuid 探测选 avx2
// backend；poly1305 avx2 的 llvm.x86.avx2.permd 已内建（2026-07-15），
// 本 driver 直接走硬件后端，无需 c_snow_noise 旧例的 poly1305_force_soft。
use chacha20poly1305::aead::{Aead, AeadInPlace, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag, XChaCha20Poly1305, XNonce};

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0);
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
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

/// 定种 xorshift64* PRNG（native/mirvm 同序列），谱系明文用。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        while v.len() < n {
            v.extend_from_slice(&self.next().to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

/// blob = ciphertext || tag(16B)，拆开打印；空 ct 以 `-` 占位（不留尾随空格）。
fn report(label: &str, blob: &[u8]) {
    let (ct, tag) = blob.split_at(blob.len() - 16);
    println!("{label} ct  = {}", if ct.is_empty() { "-".into() } else { hex(ct) });
    println!("{label} tag = {}", hex(tag));
}

// ---- RFC 8439 §2.8.2 常量 ----
const KAT_KEY: [u8; 32] = [
    0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e,
    0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d,
    0x9e, 0x9f,
];
const KAT_NONCE: [u8; 12] = [
    0x07, 0x00, 0x00, 0x00, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47,
];
const KAT_XNONCE: [u8; 24] = [
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e,
    0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57,
];
const KAT_AAD: [u8; 12] = [
    0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7,
];
const KAT_PT: &[u8] = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
const RFC8439_CT: &str = "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116";
const RFC8439_TAG: &str = "1ae10b594f09e26a7e902ecbd0600691";
const XCHACHA_CT: &str = "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b4522f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff921f9664c97637da9768812f615c68b13b52e";
const XCHACHA_TAG: &str = "c0875924c1c7987947deafd8780acf49";

fn main() {
    // ---- ① KAT：RFC 8439 §2.8.2（ChaCha20-Poly1305）----
    let key = Key::from_slice(&KAT_KEY);
    let c = ChaCha20Poly1305::new(key);
    let kat_blob = c
        .encrypt(
            Nonce::from_slice(&KAT_NONCE),
            Payload {
                msg: KAT_PT,
                aad: &KAT_AAD,
            },
        )
        .unwrap();
    report("kat/rfc8439", &kat_blob);
    let (ct, tag) = kat_blob.split_at(kat_blob.len() - 16);
    println!("kat/rfc8439 ct_ok  = {}", hex(ct) == RFC8439_CT);
    println!("kat/rfc8439 tag_ok = {}", hex(tag) == RFC8439_TAG);
    // 反向：decrypt 硬编码常量 blob → 必须还原明文
    let mut exp = unhex(RFC8439_CT);
    exp.extend_from_slice(&unhex(RFC8439_TAG));
    let back = c
        .decrypt(
            Nonce::from_slice(&KAT_NONCE),
            Payload {
                msg: exp.as_slice(),
                aad: &KAT_AAD,
            },
        )
        .unwrap();
    println!("kat/rfc8439 dec_ok = {}", back == KAT_PT);

    // ---- ② KAT：draft-arciszewski-xchacha-03 A.1（XChaCha20-Poly1305）----
    let xc = XChaCha20Poly1305::new(key);
    let xkat_blob = xc
        .encrypt(
            XNonce::from_slice(&KAT_XNONCE),
            Payload {
                msg: KAT_PT,
                aad: &KAT_AAD,
            },
        )
        .unwrap();
    report("kat/xchacha", &xkat_blob);
    let (xct, xtag) = xkat_blob.split_at(xkat_blob.len() - 16);
    println!("kat/xchacha ct_ok  = {}", hex(xct) == XCHACHA_CT);
    println!("kat/xchacha tag_ok = {}", hex(xtag) == XCHACHA_TAG);
    let mut xexp = unhex(XCHACHA_CT);
    xexp.extend_from_slice(&unhex(XCHACHA_TAG));
    let xback = xc
        .decrypt(
            XNonce::from_slice(&KAT_XNONCE),
            Payload {
                msg: xexp.as_slice(),
                aad: &KAT_AAD,
            },
        )
        .unwrap();
    println!("kat/xchacha dec_ok = {}", xback == KAT_PT);

    // ---- ③ 定向量：项目固定 key/nonce/aad，两变体 ----
    let dkey = Key::from_slice(&[
        0x42, 0x90, 0xbc, 0xb1, 0x54, 0x17, 0x35, 0x31, 0xf3, 0x14, 0xf5, 0x7c, 0x08, 0x8c,
        0x7e, 0x1e, 0x0e, 0x6d, 0x5c, 0xf7, 0x5e, 0x3c, 0x30, 0x81, 0x0b, 0xa9, 0xd6, 0x52,
        0x27, 0x60, 0x1f, 0x88,
    ]);
    let dnonce = Nonce::from_slice(&[
        0xcd, 0x7c, 0xf6, 0x7b, 0xe3, 0x9c, 0x79, 0x4a, 0x07, 0x92, 0x0e, 0xf1,
    ]);
    let dxnonce = XNonce::from_slice(&[
        0xcd, 0x7c, 0xf6, 0x7b, 0xe3, 0x9c, 0x79, 0x4a, 0x07, 0x92, 0x0e, 0xf1, 0x33, 0xa6,
        0x5d, 0x28, 0x4b, 0x19, 0x60, 0x8e, 0xf2, 0x71, 0x9c, 0x04,
    ]);
    let aad = b"mirvm/v1;lane=chacha;seq=7";
    let msg = b"mirvm differential probe: chacha20poly1305 AEAD!";
    println!("direct pt len = {}", msg.len());

    let dc = ChaCha20Poly1305::new(dkey);
    let dxc = XChaCha20Poly1305::new(dkey);
    let dblob = dc
        .encrypt(dnonce, Payload { msg, aad })
        .unwrap();
    report("direct/chacha", &dblob);
    let dback = dc
        .decrypt(
            dnonce,
            Payload {
                msg: dblob.as_slice(),
                aad,
            },
        )
        .unwrap();
    println!("direct/chacha roundtrip = {}", dback == msg);

    let dxblob = dxc
        .encrypt(dxnonce, Payload { msg, aad })
        .unwrap();
    report("direct/xchacha", &dxblob);
    let dxback = dxc
        .decrypt(
            dxnonce,
            Payload {
                msg: dxblob.as_slice(),
                aad,
            },
        )
        .unwrap();
    println!("direct/xchacha roundtrip = {}", dxback == msg);

    // ---- ④ in-place 与 detached-tag 同 alloc 逐字节一致 ----
    let mut buf = msg.to_vec();
    dc.encrypt_in_place(dnonce, aad, &mut buf).unwrap();
    println!("inplace enc eq alloc = {}", buf == dblob);
    dc.decrypt_in_place(dnonce, aad, &mut buf).unwrap();
    println!("inplace roundtrip    = {}", buf == msg);

    let mut dbuf = msg.to_vec();
    let dtag: Tag = dc
        .encrypt_in_place_detached(dnonce, aad, &mut dbuf)
        .unwrap();
    println!(
        "detached ct eq alloc  = {}",
        dbuf == dblob[..dblob.len() - 16]
    );
    println!(
        "detached tag eq alloc = {}",
        dtag.as_slice() == &dblob[dblob.len() - 16..]
    );
    dc.decrypt_in_place_detached(dnonce, aad, &mut dbuf, &dtag)
        .unwrap();
    println!("detached roundtrip    = {}", dbuf == msg);

    // XChaCha in-place 一致性
    let mut xbuf = msg.to_vec();
    dxc.encrypt_in_place(dxnonce, aad, &mut xbuf).unwrap();
    println!("x/inplace enc eq alloc = {}", xbuf == dxblob);
    let mut xdbuf = msg.to_vec();
    let xtag2: Tag = dxc
        .encrypt_in_place_detached(dxnonce, aad, &mut xdbuf)
        .unwrap();
    dxc.decrypt_in_place_detached(dxnonce, aad, &mut xdbuf, &xtag2)
        .unwrap();
    println!("x/detached roundtrip   = {}", xdbuf == msg);

    // ---- ⑤ 失败例（全部必须 is_err）----
    let mut t1 = dblob.clone();
    t1[7] ^= 0x01;
    println!(
        "fail tamper ct[7]      = {}",
        dc.decrypt(
            dnonce,
            Payload {
                msg: t1.as_slice(),
                aad
            }
        )
        .is_err()
    );
    let mut t2 = dblob.clone();
    let last = t2.len() - 1;
    t2[last] ^= 0x80;
    println!("fail tamper tag        = {}", dc.decrypt(dnonce, t2.as_slice()).is_err());
    println!(
        "fail wrong aad         = {}",
        dc.decrypt(
            dnonce,
            Payload {
                msg: dblob.as_slice(),
                aad: b"mirvm/v1;lane=chacha;seq=8"
            }
        )
        .is_err()
    );
    println!(
        "fail missing aad       = {}",
        dc.decrypt(dnonce, dblob.as_slice()).is_err()
    );
    let c_other = ChaCha20Poly1305::new(Key::from_slice(&[0x00; 32]));
    println!(
        "fail wrong key         = {}",
        c_other
            .decrypt(
                dnonce,
                Payload {
                    msg: dblob.as_slice(),
                    aad
                }
            )
            .is_err()
    );
    println!(
        "fail wrong nonce       = {}",
        dc.decrypt(
            Nonce::from_slice(&[0x11; 12]),
            Payload {
                msg: dblob.as_slice(),
                aad
            }
        )
        .is_err()
    );
    println!(
        "fail truncated blob    = {}",
        dc.decrypt(dnonce, &dblob[..8]).is_err()
    );
    let mut t3 = dblob.clone();
    t3[3] ^= 0xff;
    println!(
        "fail inplace tamper    = {}",
        dc.decrypt_in_place(dnonce, aad, &mut t3).is_err()
    );
    let mut t4 = dblob[..dblob.len() - 16].to_vec();
    let badtag = Tag::from_slice(&[0xaa; 16]);
    println!(
        "fail detached badtag   = {}",
        dc.decrypt_in_place_detached(dnonce, aad, &mut t4, badtag)
            .is_err()
    );
    // XChaCha 失败路径抽查
    let mut xt = dxblob.clone();
    xt[0] ^= 0x40;
    println!(
        "fail x/tamper ct[0]    = {}",
        dxc.decrypt(
            dxnonce,
            Payload {
                msg: xt.as_slice(),
                aad
            }
        )
        .is_err()
    );
    println!(
        "fail x/wrong xnonce    = {}",
        dxc.decrypt(
            XNonce::from_slice(&[0x77; 24]),
            Payload {
                msg: dxblob.as_slice(),
                aad
            }
        )
        .is_err()
    );

    // ---- ⑥ 明文谱系：0/1/整块 64B/跨界/多块，逐长度独立 nonce ----
    for len in [0usize, 1, 63, 64, 65, 127, 128, 129, 255] {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (len as u64).wrapping_mul(0x100_0000_01b3));
        let pt = rng.bytes(len);
        let mut n12 = [0xa5u8; 12];
        n12[..4].copy_from_slice(&(len as u32).to_le_bytes());
        let mut n24 = [0x5au8; 24];
        n24[..4].copy_from_slice(&(len as u32).to_le_bytes());

        let b1 = dc
            .encrypt(
                Nonce::from_slice(&n12),
                Payload {
                    msg: pt.as_slice(),
                    aad,
                },
            )
            .unwrap();
        let r1 = dc
            .decrypt(
                Nonce::from_slice(&n12),
                Payload {
                    msg: b1.as_slice(),
                    aad,
                },
            )
            .unwrap();
        println!(
            "spec chacha  len={len:3} blen={:3} fnv={:016x} rt={}",
            b1.len(),
            fnv1a(&b1),
            r1 == pt
        );
        if pt.len() <= 64 {
            report("spec chacha ", &b1);
        }

        let b2 = dxc
            .encrypt(
                XNonce::from_slice(&n24),
                Payload {
                    msg: pt.as_slice(),
                    aad,
                },
            )
            .unwrap();
        let r2 = dxc
            .decrypt(
                XNonce::from_slice(&n24),
                Payload {
                    msg: b2.as_slice(),
                    aad,
                },
            )
            .unwrap();
        println!(
            "spec xchacha len={len:3} blen={:3} fnv={:016x} rt={}",
            b2.len(),
            fnv1a(&b2),
            r2 == pt
        );
        if pt.len() <= 64 {
            report("spec xchacha", &b2);
        }
    }
}
