#!/usr/bin/env mirvm
---
[dependencies]
pgp = "0.14"
---
// pgp 0.14（rpgp）OpenPGP packet 解析/构造面差分（不签名不加密）。
// 素材：crate 自带 tests 目录，内嵌两份 armor 原文——
//   ① tests/openpgp/pgp263-test.pub.asc 的 armor 块（pgp2.6.3 RSA-888 老公钥，
//      V2/V3 世代的旧包格式路径）；
//   ② tests/openpgp/samplemsgs/sig-1-key-1.asc 全文（GnuPG v2 风格独立签名包，
//      Issuer/IssuerFingerprint 子包路径）。
// 覆盖：SignedPublicKey / StandaloneSignature 的 from_armor_single 字段面
// （版本/PublicKeyAlgorithm/指纹 hex/KeyID/创建时间戳/过期间隔、RSA 模长、e、
// userid、direct/revocation 签名与子包计数、逐子包粗分类+部分载荷）；
// armor roundtrip（to_armored_string 再解析 PartialEq 相等 + 字节 fnv 锚定）；
// 二进制 roundtrip（ser::Serialize::to_bytes → from_bytes → 相等）；
// from_reader_single 的 is_binary 嗅探支路（headers=None）；
// 错误路径四条：坏 armor 头（无法识别 BEGIN 标记）、块类型错配
// （签名块当公钥解）、垃圾二进制 from_bytes、截断字节 from_bytes。
//
// 已知债绕行（语义同类，见 docs/m4-log.md「dyn trait 上溯 vtable 变换，
// M4.2+」）：armor 体（base64/CRC/footer）层的错误在 crate 内部一律被
// `Dearmor::read` 包成 `io::Error::new(Other, msg)`，再经 `Error::IOError`
// 的 `{source:?}` 或 io::Error 的 Display 走 `Box<dyn Error+Send+Sync>`
// → `dyn Debug/Display` 上溯——mirvm lower 期 TRAP（本 driver 初版亲测：
// 非法 base64 体在 from_armor_single 内部构造错误字符串时即炸，诊断原文
// `TRAP: dyn 上溯 vtable 变换（dyn std::error::Error + std::marker::Send +
// std::marker::Sync → dyn std::fmt::Debug，M4.2+）`）。改装甲头层错误输入：
// 头解析错误在 dearmor 流包装之前经 bail! 走 `Error::Message(String)`，
// Display/Debug 链不触 dyn——native 可跑、mirvm 可拍。
// 确定性：armor Headers 为 BTreeMap（键序保序）；时间一律 chrono
// .timestamp() 整数秒；指纹/KeyID/签名前缀/密钥位数据均本文件手写 hex
// 打印；错误打印 Display 固定串；不打印地址/线程序/HashMap 序；stderr 为空。
use pgp::armor::Headers;
use pgp::composed::{ArmorOptions, Deserializable, SignedPublicKey, StandaloneSignature};
use pgp::packet::SubpacketData;
use pgp::packet::Signature;
use pgp::ser::Serialize;
use pgp::types::{Mpi, PublicKeyTrait, PublicParams};

/// 素材① armor 块（原文件 armor 外还有一行统计头表，不属于 armor 数据，故略去）。
const PUB_KEY: &str = r#"-----BEGIN PGP PUBLIC KEY BLOCK-----
Version: 2.6.3a

mQB8AzvqRosAAAEDeNMKLJMJQeGC2RG5Nec6R2mzC12N1wGLiYYJCsmSQd1Y8mht
A2Sc+4k/q5+l6GHtfqUR/RTCIIudAZUzrQVIMhHDKF+5de9lsE5QxQS1u43QGVCb
/9IYrOLOizYQ2pkBtD9LCrf7W2DccMEkpQKD8QAFE7QRcGdwMi42LjMtdGVzdC1r
ZXmJAIQDBRA76kaL3HDBJKUCg/EBAZMoA3Yqqdix6B2RAzywi9bKSLqwAFVL+MMw
W+BnYeBXF9u+bPpQvtyxgi0vx8F9r84B3HAhZNEjBWODF6vctIQhXhAhXIniDTSj
HNzQ/+nbWnebQn18XUV2SdM1PzMOblD+nISte7+WUfWzlD7YUJPkFPw=
=b498
-----END PGP PUBLIC KEY BLOCK-----
"#;

/// 素材② 独立签名块全文。
const SIG: &str = r#"-----BEGIN PGP SIGNATURE-----
Version: GnuPG v2

iHsEABYIACMFAldqTEMcHHBhdHJpY2UubHVtdW1iYUBleGFtcGxlLm5ldAAKCRAT
lWNoKgINCu0XAQC6VSdsGyTbvFPp5e6BmkmBzPcb5Kex4ar722k0jzhLzgD+Js2q
Y1JIdjfW4GnFhdzqyUbuGTlk1wNY7Re1uNyD6gw=
=c0oW
-----END PGP SIGNATURE-----
"#;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 手写 hex（小写），替代 hex crate。
fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// MPI 有效位长（RSA 模长等）。
fn mpi_bits(m: &Mpi) -> usize {
    let b = m.as_bytes();
    match b.first() {
        None => 0,
        Some(&f) => (b.len() - 1) * 8 + (8 - f.leading_zeros() as usize),
    }
}

/// 公钥参数粗描：RSA 模长+e；ECC 曲线名；其余打形状。
fn params_desc(p: &PublicParams) -> String {
    match p {
        PublicParams::RSA { n, e } => format!("RSA n-bits={} e={}", mpi_bits(n), hex(e.as_bytes())),
        PublicParams::DSA { p, q, .. } => {
            format!("DSA p-bits={} q-bits={}", mpi_bits(p), mpi_bits(q))
        }
        PublicParams::Elgamal { p, .. } => format!("Elgamal p-bits={}", mpi_bits(p)),
        PublicParams::ECDSA(_) => "ECDSA".to_string(),
        PublicParams::ECDH(_) => "ECDH".to_string(),
        PublicParams::EdDSALegacy { curve, q } => {
            format!("EdDSALegacy curve={curve:?} q-bytes={}", q.as_bytes().len())
        }
        PublicParams::Ed25519 { .. } => "Ed25519".to_string(),
        PublicParams::X25519 { .. } => "X25519".to_string(),
        PublicParams::X448 { .. } => "X448".to_string(),
        PublicParams::Unknown { data } => format!("Unknown len={}", data.len()),
    }
}

/// PublicKeyTrait 字段面（主 key packet / subkey packet 通用）。
fn dump_key_pkt<K: PublicKeyTrait>(label: &str, k: &K) {
    println!(
        "{label} version={:?} alg={:?}",
        k.version(),
        k.algorithm()
    );
    println!("{label} fingerprint={}", hex(k.fingerprint().as_bytes()));
    println!("{label} key_id={:x}", k.key_id());
    println!(
        "{label} created={} expiration_days={:?} params={}",
        k.created_at().timestamp(),
        k.expiration(),
        params_desc(k.public_params())
    );
    println!(
        "{label} is_signing={} is_encryption={}",
        k.is_signing_key(),
        k.is_encryption_key()
    );
}

/// 子包粗分类名 + 常见载荷（确定性字段）。
fn sp_desc(d: &SubpacketData) -> String {
    match d {
        SubpacketData::SignatureCreationTime(t) => {
            format!("sig-creation time={}", t.timestamp())
        }
        SubpacketData::SignatureExpirationTime(dur) => {
            format!("sig-expiration secs={:?}", dur.num_seconds())
        }
        SubpacketData::KeyExpirationTime(dur) => {
            format!("key-expiration days={:?}", dur.num_days())
        }
        SubpacketData::Issuer(id) => format!("issuer id={id:x}"),
        SubpacketData::IssuerFingerprint(fp) => {
            format!("issuer-fp {}", hex(fp.as_bytes()))
        }
        SubpacketData::PreferredSymmetricAlgorithms(v) => format!("pref-sym {v:?}"),
        SubpacketData::PreferredHashAlgorithms(v) => format!("pref-hash {v:?}"),
        SubpacketData::PreferredCompressionAlgorithms(v) => format!("pref-comp {v:?}"),
        SubpacketData::KeyServerPreferences(b) => format!("keyserver-prefs {}", hex(b)),
        SubpacketData::KeyFlags(b) => format!("key-flags {}", hex(b)),
        SubpacketData::Features(b) => format!("features {}", hex(b)),
        SubpacketData::RevocationReason(code, r) => format!("revocation code={code:?} reason={r}"),
        SubpacketData::IsPrimary(b) => format!("is-primary={b}"),
        SubpacketData::Revocable(b) => format!("revocable={b}"),
        SubpacketData::EmbeddedSignature(_) => "embedded-sig".to_string(),
        SubpacketData::PreferredKeyServer(s) => format!("pref-keyserver {s}"),
        SubpacketData::Notation(n) => format!("notation {n:?}"),
        SubpacketData::RevocationKey(rk) => format!("revocation-key {rk:?}"),
        SubpacketData::SignersUserID(uid) => format!("signers-userid {uid}"),
        SubpacketData::PolicyURI(u) => format!("policy-uri {u}"),
        SubpacketData::TrustSignature(d, a) => format!("trust depth={d} amount={a}"),
        SubpacketData::RegularExpression(re) => format!("regexp {re}"),
        SubpacketData::ExportableCertification(b) => format!("exportable={b}"),
        SubpacketData::PreferredEncryptionModes(v) => format!("pref-modes {v:?}"),
        SubpacketData::IntendedRecipientFingerprint(fp) => {
            format!("intended-recipient {}", hex(fp.as_bytes()))
        }
        SubpacketData::PreferredAeadAlgorithms(v) => format!("pref-aead {v:?}"),
        SubpacketData::Experimental(t, b) => format!("experimental type={t} {}", hex(b)),
        SubpacketData::Other(t, b) => format!("other type={t} len={}", b.len()),
        SubpacketData::SignatureTarget(p, h, _) => format!("sig-target pub={p:?} hash={h:?}"),
    }
}

/// 签名包字段面 + 子包逐条。
fn dump_sig(label: &str, s: &Signature) {
    println!(
        "{label} sigver={:?} typ={:?} pub={:?} hash={:?} signed_prefix={}",
        s.config.version(),
        s.typ(),
        s.config.pub_alg,
        s.hash_alg(),
        hex(&s.signed_hash_value)
    );
    println!(
        "{label} subpackets hashed={} unhashed={}",
        s.config.hashed_subpackets.len(),
        s.config.unhashed_subpackets.len()
    );
    for (i, sp) in s.config.hashed_subpackets.iter().enumerate() {
        println!("  {label}[H{i}] critical={} {}", sp.is_critical, sp_desc(&sp.data));
    }
    for (i, sp) in s.config.unhashed_subpackets.iter().enumerate() {
        println!(
            "  {label}[U{i}] critical={} {}",
            sp.is_critical,
            sp_desc(&sp.data)
        );
    }
    println!(
        "{label} issuer_keys={:?}",
        s.issuer()
            .iter()
            .map(|k| format!("{k:x}"))
            .collect::<Vec<_>>()
    );
    println!(
        "{label} issuer_fps={:?}",
        s.issuer_fingerprint()
            .iter()
            .map(|f| hex(f.as_bytes()))
            .collect::<Vec<_>>()
    );
}

/// armor 头（BTreeMap 键序确定）。
fn dump_headers(h: &Headers) {
    println!("headers n={}", h.len());
    for (k, vs) in h {
        println!("  hdr {k} = {vs:?}");
    }
}

/// SignedPublicKey 全字段面。
fn dump_signed_key(key: &SignedPublicKey) {
    dump_key_pkt("primary", &key.primary_key);
    println!("expires_at={:?}", key.expires_at().map(|t| t.timestamp()));
    let d = &key.details;
    println!(
        "details users={} attrs={} direct_sigs={} revocation_sigs={}",
        d.users.len(),
        d.user_attributes.len(),
        d.direct_signatures.len(),
        d.revocation_signatures.len()
    );
    for (i, u) in d.users.iter().enumerate() {
        println!("user[{i}] id={} sigs={}", u.id, u.signatures.len());
        for (j, s) in u.signatures.iter().enumerate() {
            dump_sig(&format!("user[{i}].sig[{j}]"), s);
        }
    }
    for (i, s) in d.direct_signatures.iter().enumerate() {
        dump_sig(&format!("direct[{i}]"), s);
    }
    for (i, s) in d.revocation_signatures.iter().enumerate() {
        dump_sig(&format!("revocation[{i}]"), s);
    }
    println!("subkeys n={}", key.public_subkeys.len());
    for (i, sk) in key.public_subkeys.iter().enumerate() {
        dump_key_pkt(&format!("subkey[{i}]"), &sk.key);
        for (j, s) in sk.signatures.iter().enumerate() {
            dump_sig(&format!("subkey[{i}].sig[{j}]"), s);
        }
    }
}

fn main() {
    // ---- ① 公钥 armor 解析 + 字段面 ----
    let (key, kh) = SignedPublicKey::from_armor_single(PUB_KEY.as_bytes()).unwrap();
    println!("== signed public key ==");
    dump_headers(&kh);
    dump_signed_key(&key);

    // ---- ② 公钥 armor roundtrip ----
    let a1 = key.to_armored_string(ArmorOptions::default()).unwrap();
    println!("key armor len={} fnv={:016x}", a1.len(), fnv1a(a1.as_bytes()));
    let (key2, kh2) = SignedPublicKey::from_string(&a1).unwrap();
    println!("key armor roundtrip={} re-headers={}", key == key2, kh2.len());
    let a2 = key2.to_armored_string(ArmorOptions::default()).unwrap();
    println!("key armor stable={}", a1 == a2);

    // ---- ③ 公钥二进制 roundtrip + from_reader 嗅探支路 ----
    let kbin = key.to_bytes().unwrap();
    println!("key bin len={} fnv={:016x}", kbin.len(), fnv1a(&kbin));
    let key3 = SignedPublicKey::from_bytes(&kbin[..]).unwrap();
    println!("key bin roundtrip={}", key == key3);
    let (key4, kh4) = SignedPublicKey::from_reader_single(&kbin[..]).unwrap();
    println!("key reader roundtrip={} headers-is-none={}", key == key4, kh4.is_none());

    // ---- ④ 独立签名包解析 + 字段面 ----
    let (ssig, sh) = StandaloneSignature::from_armor_single(SIG.as_bytes()).unwrap();
    println!("== standalone signature ==");
    dump_headers(&sh);
    dump_sig("sig", &ssig.signature);

    // ---- ⑤ 签名 armor / 二进制 roundtrip ----
    let s1 = ssig.to_armored_string(ArmorOptions::default()).unwrap();
    println!("sig armor len={} fnv={:016x}", s1.len(), fnv1a(s1.as_bytes()));
    let (ssig2, _) = StandaloneSignature::from_armor_single(s1.as_bytes()).unwrap();
    println!("sig armor roundtrip={}", ssig == ssig2);
    let sbin = ssig.to_bytes().unwrap();
    println!("sig bin len={} fnv={:016x}", sbin.len(), fnv1a(&sbin));
    let ssig3 = StandaloneSignature::from_bytes(&sbin[..]).unwrap();
    println!("sig bin roundtrip={}", ssig == ssig3);

    // ---- ⑥ 错误路径（均避开 io-Custom 包装族，见文件头绕行注）----
    let bad = "definitely not an armor block, no BEGIN marker anywhere\n";
    match SignedPublicKey::from_armor_single(bad.as_bytes()) {
        Ok(_) => println!("bad-armor unexpectedly ok"),
        Err(e) => println!("bad-armor err: {e}"),
    }
    match SignedPublicKey::from_armor_single(SIG.as_bytes()) {
        Ok(_) => println!("wrong-type unexpectedly ok"),
        Err(e) => println!("wrong-type err: {e}"),
    }
    let junk = [0xffu8; 16];
    match SignedPublicKey::from_bytes(&junk[..]) {
        Ok(_) => println!("junk-bytes unexpectedly ok"),
        Err(e) => println!("junk-bytes err: {e}"),
    }
    let cut = &kbin[..kbin.len() / 3];
    match SignedPublicKey::from_bytes(cut) {
        Ok(_) => println!("truncated unexpectedly ok"),
        Err(e) => println!("truncated err: {e}"),
    }
}
