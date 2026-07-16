#!/usr/bin/env mirvm
---
[dependencies]
# 只启官方 ring provider，不走默认 aws-lc-rs：aws-lc-sys 是巨型 C/C++ 归档
# （编译十分钟级，且静态归档闭包有 rusqlite 式 libm FRONTIER 风险）；ring 是
# rustls 0.23 官方双 provider 之一，其归档形状（C+asm 静态归档、
# -fvisibility=hidden 符号）在 mirvm 已实证全绿（docs/corpus.md 的 ring bug②
# 修复记录）。本 driver 覆盖的 rustls API 面与 provider 选择无关。
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12", "logging"] }
rustls-pemfile = "2"
---
// rustls 0.23 + rustls-pemfile 2：TLS 大物的无网络确定性面。不握手、不联网。
// ① pemfile：certs 迭代器 / private_key 通用入口 / 三种私钥专用迭代器 /
//    read_all Item 枚举 / 坏 base64 错误路径。
// ② RootCertStore：empty / add（好+坏 DER）/ add_parsable_certificates 计数 /
//    subjects / len / is_empty。
// ③ CryptoProvider 枚举：ring cipher_suites、kx_groups；ALL/DEFAULT_VERSIONS。
// ④ ServerConfig/ClientConfig 构建 + ALPN；两条构建错误路径（坏私钥 DER、
//    私钥与证书 SPKI 不匹配）。
// ⑤ WebPkiServerVerifier 全矩阵：正例（带/不带 intermediate、localhost SAN）
//    + 反例（错名 / 错根 / 过期 / 未生效 / 篡改签名 / 垃圾 EE 证书）+
//    空根 verifier 构建错误。
// ⑥ ECDSA 签名 → verify_tls13/12_signature 正反例。注意 ring 0.17 的签名
//    用随机 nonce（非 RFC6979，native 连跑两次签名即不同），签名本体不可
//    打印，只打确定性的验签结果。（DigitallySignedStruct 经 internal Codec
//    构造——rustls 自己的集成测试同款用法；internal 模块 semver-exempt，
//    但 0.23.41 已钉在本地缓存）。
// 证书材料（构建期 openssl CLI 生成，运行期全确定）：
//   openssl ecparam -genkey -name prime256v1 -out ca-key.pem
//   openssl req -new -x509 -key ca-key.pem -out ca-cert.pem -days 7300 \
//     -subj "/CN=mirvm-test-ca" -addext "basicConstraints=critical,CA:TRUE" \
//     -addext "keyUsage=critical,keyCertSign,cRLSign"
//   openssl req -new -key server-key.pem -out server.csr -subj "/CN=mirvm.test"
//   openssl x509 -req -in server.csr -CA ca-cert.pem -CAkey ca-key.pem \
//     -CAcreateserial -out server-cert.pem -days 7300 -extfile ext.cnf
//   # ext.cnf: basicConstraints=critical,CA:FALSE / keyUsage=critical,
//   # digitalSignature / extendedKeyUsage=serverAuth,clientAuth /
//   # subjectAltName=DNS:mirvm.test,DNS:localhost
// 有效期 2026-07-16..2046-07-11；验证时刻硬编码：1811808000（2027-06-01，
// 有效）/ 2524608000（2050-01-01，过期）/ 1577836800（2020-01-01，未生效）。
// 确定性：无时间/地址/HashMap 序/线程序；二进制只打 长度+FNV-1a；stderr 为空。
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::ServerCertVerifier;
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::ring;
use rustls::crypto::CryptoProvider;
use rustls::internal::msgs::codec::{Codec, Reader};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme,
    ALL_VERSIONS, DEFAULT_VERSIONS,
};

const NOW_VALID: u64 = 1_811_808_000; // 2027-06-01T00:00:00Z（窗口内）
const NOW_EXPIRED: u64 = 2_524_608_000; // 2050-01-01T00:00:00Z（过期后）
const NOW_TOO_EARLY: u64 = 1_577_836_800; // 2020-01-01T00:00:00Z（生效前）

const CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBlTCCATugAwIBAgIUORXYdf8V+fqWuWq/b+cv8khwAOUwCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNbWlydm0tdGVzdC1jYTAeFw0yNjA3MTYwNjAxMDFaFw00NjA3
MTEwNjAxMDFaMBgxFjAUBgNVBAMMDW1pcnZtLXRlc3QtY2EwWTATBgcqhkjOPQIB
BggqhkjOPQMBBwNCAARn67+kpwNDtu60X+LK+wfaBt9L9133jlqTpGfmiyE8XZ3C
CSgky6v1V+E+bH7LS+OuVf+Am7HJkgoUE+GBbKGNo2MwYTAdBgNVHQ4EFgQUbeCi
VovNvL3lWa/2ZU45iFG2ER0wHwYDVR0jBBgwFoAUbeCiVovNvL3lWa/2ZU45iFG2
ER0wDwYDVR0TAQH/BAUwAwEB/zAOBgNVHQ8BAf8EBAMCAQYwCgYIKoZIzj0EAwID
SAAwRQIgY7jUW7ZQdUSSQOq3sSCmR5IKXVTz89xxhqtHiu2MaGICIQC7CXL/2dAo
Anm3lT4sSb7/fHalHa3Tie01dX6lk1T0tQ==
-----END CERTIFICATE-----
";

const SERVER_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIB0jCCAXigAwIBAgIUVp5RIA89E/Qx65afqPOW4+msuQ8wCgYIKoZIzj0EAwIw
GDEWMBQGA1UEAwwNbWlydm0tdGVzdC1jYTAeFw0yNjA3MTYwNjAxMDFaFw00NjA3
MTEwNjAxMDFaMBUxEzARBgNVBAMMCm1pcnZtLnRlc3QwWTATBgcqhkjOPQIBBggq
hkjOPQMBBwNCAASnqCNfuHWyE27oOyBqZa57ua7e6fy0TqeEKb26OCqO35TXo0Bf
Myaq1U+LzPsSyvLgpgNUgT6NiKpgbTuVf6epo4GiMIGfMAwGA1UdEwEB/wQCMAAw
DgYDVR0PAQH/BAQDAgeAMB0GA1UdJQQWMBQGCCsGAQUFBwMBBggrBgEFBQcDAjAg
BgNVHREEGTAXggptaXJ2bS50ZXN0gglsb2NhbGhvc3QwHQYDVR0OBBYEFCmn6wEv
SMIy+eG3WS6RefezB+PfMB8GA1UdIwQYMBaAFG3golaLzby95Vmv9mVOOYhRthEd
MAoGCCqGSM49BAMCA0gAMEUCIQCn9sOXJkvtaFns0Zm9Er9b6cQOJ8Sd0dqeY2rM
EKX7ZQIgcU58jpGv2XRLTag1d1smnwe/JVt3XVgRYHL0iVM0XxE=
-----END CERTIFICATE-----
";

const OTHER_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBkzCCATmgAwIBAgIUD4viikCExBzweuNraemqXngmh78wCgYIKoZIzj0EAwIw
FzEVMBMGA1UEAwwMdW5yZWxhdGVkLWNhMB4XDTI2MDcxNjA2MDEwMVoXDTQ2MDcx
MTA2MDEwMVowFzEVMBMGA1UEAwwMdW5yZWxhdGVkLWNhMFkwEwYHKoZIzj0CAQYI
KoZIzj0DAQcDQgAE9WPhVTrRz/gjj8DJHYMLNIYP765hnqlGKXRULeKeyIj4aOwU
UimsXOKAvJAreFvqmlmRxablB4Pdtp946S9K2qNjMGEwHQYDVR0OBBYEFP8Fxj4+
djMlBiA+VbH2qgWjoFsDMB8GA1UdIwQYMBaAFP8Fxj4+djMlBiA+VbH2qgWjoFsD
MA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgEGMAoGCCqGSM49BAMCA0gA
MEUCIH3hv5SNhXJ8HbKHLS3aEqWyO0/h4MpFyqOqVg+oVWYPAiEAgt353F87ct+P
9wzl1unnARxvQXwRhN8uBWkHjG1jHdQ=
-----END CERTIFICATE-----
";

const SERVER_KEY_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg3QjwK/M5QlDk360/
7LBSAN6RxNzyJIdxSs9LXIDDHYOhRANCAASnqCNfuHWyE27oOyBqZa57ua7e6fy0
TqeEKb26OCqO35TXo0BfMyaq1U+LzPsSyvLgpgNUgT6NiKpgbTuVf6ep
-----END PRIVATE KEY-----
";

const SERVER_KEY_SEC1_PEM: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIN0I8CvzOUJQ5N+tP+ywUgDekcTc8iSHcUrPS1yAwx2DoAoGCCqGSM49
AwEHoUQDQgAEp6gjX7h1shNu6DsgamWue7mu3un8tE6nhCm9ujgqjt+U16NAXzMm
qtVPi8z7Esry4KYDVIE+jYiqYG07lX+nqQ==
-----END EC PRIVATE KEY-----
";

const CA_KEY_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgczroEuwTQUy8ioAc
ZNqy/THnXQISDL5sH/1pGi4py9KhRANCAARn67+kpwNDtu60X+LK+wfaBt9L9133
jlqTpGfmiyE8XZ3CCSgky6v1V+E+bH7LS+OuVf+Am7HJkgoUE+GBbKGN
-----END PRIVATE KEY-----
";

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn key_kind(k: &PrivateKeyDer<'_>) -> &'static str {
    match k {
        PrivateKeyDer::Pkcs1(_) => "pkcs1",
        PrivateKeyDer::Sec1(_) => "sec1",
        PrivateKeyDer::Pkcs8(_) => "pkcs8",
        _ => "other",
    }
}

fn alpn_join(v: &[Vec<u8>]) -> String {
    v.iter()
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

fn verify_case(
    label: &str,
    v: &WebPkiServerVerifier,
    ee: &CertificateDer<'_>,
    ints: &[CertificateDer<'_>],
    name: &ServerName<'_>,
    now_secs: u64,
) {
    let now = UnixTime::since_unix_epoch(Duration::from_secs(now_secs));
    match v.verify_server_cert(ee, ints, name, &[], now) {
        Ok(_) => println!("verify {label}: ok"),
        Err(e) => println!("verify {label}: err {e}"),
    }
}

fn main() {
    // ---- ① rustls-pemfile：证书/私钥解析全形态 ----
    let certs_pem = [CA_PEM, SERVER_PEM, OTHER_PEM].concat();
    let mut rd = BufReader::new(certs_pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut rd)
        .collect::<Result<_, _>>()
        .unwrap();
    println!("pem certs n={}", certs.len());
    for (i, c) in certs.iter().enumerate() {
        println!(
            "pem cert[{i}] len={} fnv={:016x}",
            c.as_ref().len(),
            fnv1a(c.as_ref())
        );
    }
    let (ca_cert, server_cert, other_cert) =
        (certs[0].clone(), certs[1].clone(), certs[2].clone());

    let server_key = rustls_pemfile::private_key(&mut BufReader::new(
        SERVER_KEY_PKCS8_PEM.as_bytes(),
    ))
    .unwrap()
    .expect("pkcs8 key");
    println!(
        "key {} len={} fnv={:016x}",
        key_kind(&server_key),
        server_key.secret_der().len(),
        fnv1a(server_key.secret_der())
    );
    let ca_key = rustls_pemfile::private_key(&mut BufReader::new(CA_KEY_PKCS8_PEM.as_bytes()))
        .unwrap()
        .expect("ca key");
    println!("ca key kind={}", key_kind(&ca_key));

    // 专用迭代器 × 匹配/不匹配标签
    let n_pkcs8 = rustls_pemfile::pkcs8_private_keys(&mut BufReader::new(
        SERVER_KEY_PKCS8_PEM.as_bytes(),
    ))
    .count();
    let n_rsa = rustls_pemfile::rsa_private_keys(&mut BufReader::new(
        SERVER_KEY_PKCS8_PEM.as_bytes(),
    ))
    .count();
    let n_ec_mismatch = rustls_pemfile::ec_private_keys(&mut BufReader::new(
        SERVER_KEY_PKCS8_PEM.as_bytes(),
    ))
    .count();
    let n_ec_sec1 = rustls_pemfile::ec_private_keys(&mut BufReader::new(
        SERVER_KEY_SEC1_PEM.as_bytes(),
    ))
    .count();
    println!("iters pkcs8={n_pkcs8} rsa={n_rsa} ec_on_pkcs8={n_ec_mismatch} ec_on_sec1={n_ec_sec1}");
    let sec1_key = rustls_pemfile::private_key(&mut BufReader::new(
        SERVER_KEY_SEC1_PEM.as_bytes(),
    ))
    .unwrap()
    .expect("sec1 key");
    println!("sec1 key kind={}", key_kind(&sec1_key));

    // read_all 混合流：Item 枚举形态
    let mixed = [
        CA_PEM,
        SERVER_KEY_PKCS8_PEM,
        SERVER_PEM,
        SERVER_KEY_SEC1_PEM,
        OTHER_PEM,
        CA_KEY_PKCS8_PEM,
    ]
    .concat();
    let items: Vec<_> = rustls_pemfile::read_all(&mut BufReader::new(mixed.as_bytes()))
        .collect::<Result<_, _>>()
        .unwrap();
    println!("read_all n={}", items.len());
    for it in &items {
        let tag = match it {
            rustls_pemfile::Item::X509Certificate(_) => "cert",
            rustls_pemfile::Item::Pkcs8Key(_) => "pkcs8key",
            rustls_pemfile::Item::Sec1Key(_) => "sec1key",
            rustls_pemfile::Item::Pkcs1Key(_) => "pkcs1key",
            _ => "other",
        };
        println!("item {tag}");
    }

    // 错误路径：坏 base64
    let garbage_pem = "-----BEGIN CERTIFICATE-----\nnot-valid-base64!!!\n-----END CERTIFICATE-----\n";
    match rustls_pemfile::certs(&mut BufReader::new(garbage_pem.as_bytes())).next() {
        Some(Err(e)) => println!("bad pem: err {e}"),
        Some(Ok(_)) => println!("bad pem: unexpected ok"),
        None => println!("bad pem: unexpected eof"),
    }

    // ---- ② RootCertStore ----
    let mut store = RootCertStore::empty();
    println!("store empty={} len={}", store.is_empty(), store.len());
    store.add(ca_cert.clone()).unwrap();
    let garbage_der = CertificateDer::from(vec![0x30, 0x03, 0x01, 0x01, 0xff]);
    match store.add(garbage_der.clone()) {
        Ok(()) => println!("store add garbage: unexpected ok"),
        Err(e) => println!("store add garbage: err {e}"),
    }
    let (ok_cnt, bad_cnt) = store.add_parsable_certificates([
        server_cert.clone(),
        garbage_der,
        other_cert.clone(),
    ]);
    let subj_fnv = store
        .subjects()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, dn| {
            let mut acc = h;
            for &b in dn.as_ref() {
                acc ^= b as u64;
                acc = acc.wrapping_mul(0x0000_0100_0000_01b3);
            }
            acc
        });
    println!("store parsable ok={ok_cnt} bad={bad_cnt} len={} subjects_fnv={subj_fnv:016x}", store.len());

    // 干净的两份信任库：好根 / 错根
    let mut store_good = RootCertStore::empty();
    store_good.add(ca_cert.clone()).unwrap();
    let mut store_wrong = RootCertStore::empty();
    store_wrong.add(other_cert.clone()).unwrap();

    // ---- ③ provider 与协议版本枚举 ----
    let provider = ring::default_provider();
    println!("suites n={}", provider.cipher_suites.len());
    for s in &provider.cipher_suites {
        println!("suite {:?}", s.suite());
    }
    println!("kx n={}", provider.kx_groups.len());
    for g in &provider.kx_groups {
        println!("kx {:?}", g.name());
    }
    println!("all vers n={}", ALL_VERSIONS.len());
    for v in ALL_VERSIONS {
        println!("ver {v:?}");
    }
    println!("default vers n={}", DEFAULT_VERSIONS.len());

    // ---- ④ ServerConfig / ClientConfig 构建 ----
    let mut scfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![server_cert.clone(), ca_cert.clone()],
            server_key.clone_key(),
        )
        .unwrap();
    scfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    println!("server alpn n={} {}", scfg.alpn_protocols.len(), alpn_join(&scfg.alpn_protocols));

    // 错误路径 A：私钥 DER 结构损坏（曲线 OID 区翻转）→ load 失败
    let mut bad_der = server_key.secret_der().to_vec();
    bad_der[20] ^= 0xff;
    match ServerConfig::builder().with_no_client_auth().with_single_cert(
        vec![server_cert.clone()],
        PrivateKeyDer::Pkcs8(bad_der.into()),
    ) {
        Ok(_) => println!("corrupt key: unexpected ok"),
        Err(e) => println!("corrupt key: err {e}"),
    }
    // 错误路径 B：CA 私钥 vs 服务器证书 → SPKI 不匹配
    match ServerConfig::builder().with_no_client_auth().with_single_cert(
        vec![server_cert.clone()],
        ca_key.clone_key(),
    ) {
        Ok(_) => println!("mismatch key: unexpected ok"),
        Err(e) => println!("mismatch key: err {e}"),
    }

    let mut ccfg = ClientConfig::builder()
        .with_root_certificates(store_good.clone())
        .with_no_client_auth();
    ccfg.alpn_protocols = vec![b"h2".to_vec()];
    println!("client alpn n={} {}", ccfg.alpn_protocols.len(), alpn_join(&ccfg.alpn_protocols));
    println!("provider default installed={}", CryptoProvider::get_default().is_some());

    // ---- ⑤ WebPkiServerVerifier 矩阵 ----
    match WebPkiServerVerifier::builder(Arc::new(RootCertStore::empty())).build() {
        Ok(_) => println!("empty roots verifier: unexpected ok"),
        Err(e) => println!("empty roots verifier: err {e}"),
    }
    let verifier = WebPkiServerVerifier::builder(Arc::new(store_good))
        .build()
        .unwrap();
    let schemes = verifier.supported_verify_schemes();
    println!("schemes n={}", schemes.len());
    for s in &schemes {
        println!("scheme {s:?}");
    }

    let name = ServerName::try_from("mirvm.test").expect("dns name");
    let name_local = ServerName::try_from("localhost").expect("dns name");
    let name_evil = ServerName::try_from("evil.test").expect("dns name");
    match ServerName::try_from("not a dns name") {
        Ok(_) => println!("bad server name: unexpected ok"),
        Err(e) => println!("bad server name: err {e}"),
    }

    let ints: Vec<CertificateDer<'static>> = vec![ca_cert.clone()];
    verify_case("good+int", &verifier, &server_cert, &ints, &name, NOW_VALID);
    verify_case("good-noint", &verifier, &server_cert, &[], &name, NOW_VALID);
    verify_case("good-localhost", &verifier, &server_cert, &ints, &name_local, NOW_VALID);
    verify_case("wrong-name", &verifier, &server_cert, &ints, &name_evil, NOW_VALID);
    verify_case("expired", &verifier, &server_cert, &ints, &name, NOW_EXPIRED);
    verify_case("not-yet-valid", &verifier, &server_cert, &ints, &name, NOW_TOO_EARLY);

    let verifier_wrong = WebPkiServerVerifier::builder(Arc::new(store_wrong))
        .build()
        .unwrap();
    verify_case("wrong-roots", &verifier_wrong, &server_cert, &ints, &name, NOW_VALID);

    // 篡改签名尾字节 → 链签名校验必须失败
    let mut t = server_cert.as_ref().to_vec();
    let last = t.len() - 1;
    t[last] ^= 0x01;
    let tampered = CertificateDer::from(t);
    verify_case("tampered-ee", &verifier, &tampered, &ints, &name, NOW_VALID);
    // 垃圾 DER 当 EE → BadEncoding
    let junk = CertificateDer::from(vec![0x30, 0x03, 0x01, 0x01, 0xff]);
    verify_case("junk-ee", &verifier, &junk, &ints, &name, NOW_VALID);

    // ---- ⑥ ECDSA 签名 → tls13/tls12 验签 ----
    // 注意：ring 0.17 的 ECDSA 签名用随机 nonce（signing.rs 原文 "using a
    // random nonce generated by rng"，native 连跑两次签名字节都不同），故
    // 签名本体不可打印；确定的是验签结果（随机有效签名照样过、篡改照样拒）。
    let sk = provider
        .key_provider
        .load_private_key(server_key.clone_key())
        .unwrap();
    let spki = sk.public_key().expect("spki");
    println!("spki len={} fnv={:016x}", spki.as_ref().len(), fnv1a(spki.as_ref()));
    let signer = sk
        .choose_scheme(&[SignatureScheme::ECDSA_NISTP256_SHA256])
        .expect("scheme supported");
    let msg: &[u8] = b"mirvm rustls_cert deterministic signature message";
    let sig = signer.sign(msg).unwrap();
    println!("sign scheme {:?}", signer.scheme());

    // DigitallySignedStruct::new 是 pub(crate)；走 rustls 集成测试同款的
    // internal Codec 解码路径构造（scheme u16 BE + PayloadU16 长度前缀 + sig）。
    let mut enc = Vec::with_capacity(sig.len() + 4);
    enc.extend_from_slice(&0x0403u16.to_be_bytes()); // ECDSA_NISTP256_SHA256 的 IANA 值
    enc.extend_from_slice(&(sig.len() as u16).to_be_bytes());
    enc.extend_from_slice(&sig);
    let dss = DigitallySignedStruct::read(&mut Reader::init(&enc)).unwrap();
    println!("dss scheme {:?}", dss.scheme);

    match verifier.verify_tls13_signature(msg, &server_cert, &dss) {
        Ok(_) => println!("tls13 sig: ok"),
        Err(e) => println!("tls13 sig: err {e}"),
    }
    match verifier.verify_tls12_signature(msg, &server_cert, &dss) {
        Ok(_) => println!("tls12 sig: ok"),
        Err(e) => println!("tls12 sig: err {e}"),
    }
    // 反例：消息被篡改 / 用错证书公钥
    match verifier.verify_tls13_signature(b"tampered message", &server_cert, &dss) {
        Ok(_) => println!("tls13 sig tampered: unexpected ok"),
        Err(e) => println!("tls13 sig tampered: err {e}"),
    }
    match verifier.verify_tls13_signature(msg, &ca_cert, &dss) {
        Ok(_) => println!("tls13 sig wrong-cert: unexpected ok"),
        Err(e) => println!("tls13 sig wrong-cert: err {e}"),
    }
}
