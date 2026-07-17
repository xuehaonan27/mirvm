#!/usr/bin/env mirvm
---
[dependencies]
# rustls 钉 =0.23.42（0.23.x 最新，与 c_rustls_cert 同钉法）：default-features=false
# 只启官方 ring provider（不走默认 aws-lc-rs：aws-lc-sys 巨型 C/C++ 归档，ring 归档
# 形状在本仓库已实证全绿，见 c_rustls_cert 头注）+ std + tls12 + logging。
rustls = { version = "=0.23.42", default-features = false, features = ["ring", "std", "tls12", "logging"] }
# rcgen 钉 =0.13.2（0.13.x 最新）：default-features=false 只留 ["ring"]——默认三元组
# crypto+pem+ring 里的 pem 用不到（全程 DER，不 PEM 序列化）；x509-parser 不开
# （不解析外部证书，subject/issuer 直接由 params 打印）。零 aws-lc-rs 面。
rcgen = { version = "=0.13.2", default-features = false, features = ["ring"] }
---
// rcgen 0.13 + rustls 0.23（ring provider）三维差分：定种子自签名证书 + 本进程内
// loopback TLS 真实握手 + 双向 echo。批7 波3「rustls 握手面」：c_rustls_cert 覆盖
// 无握手静态面（pemfile/verifier/config 构建），本 driver 补动态面——真实 socket
// 上的 Server/Client 双端全握手与应用数据往返。
//
// 确定性设计（TLS 握手含大量随机：ECDHE 临时密钥、ClientHello/ServerHello nonce、
// 会话票——全部不进输出；只打协商结果的枚举值与布尔，其在同一配置双端下必稳）：
//   ① 密钥算法选 Ed25519：RFC 8032 纯确定性签名（ring Ed25519KeyPair 无随机
//      nonce），证书 DER 跨进程逐字节稳定；ECDSA 被排除——ring 0.17 的 ECDSA
//      签名用随机 nonce（c_rustls_cert 头注实证），证书 DER 不可锚。
//   ② 定种子：splitmix64(SEED) 展开 32B 作 Ed25519 种子，手工套 RFC 8410
//      PKCS#8 v1 前缀（302e020100300506032b657004220420||seed）导入 rcgen；
//      KeyPair::serialize_der 依实现原样返回该输入，再锚一层 FNV。
//   ③ 证书参数全显式：serial_number 显式钉值（0.13.2 的 None 路径实为 SPKI
//      SHA-256 派生、本来也确定，显式更稳）；not_before/not_after 用
//      date_time_ymd 固定（默认 1975..4096 本就无 wall-clock 通道，显式钉死）；
//      SAN/DN/keyUsage/EKU 全常量；webpki 握手期有效期校验走真实壁钟但窗口
//      2026..2046 内结论恒定。is_ca=NoCa（无 BC 扩展）：同一张自签证书作 EE
//      被 server 出示时 webpki 以 Role::EndEntity 校验——BC 缺席按 is_ca=false
//      通过（Ca 则撞 CaUsedAsEndEntity，冒烟实锤）；作信任锚时 webpki 只按
//      subject+SPKI 匹配、不查 BC（verify_cert.rs 实证），自签钉根成立。
// 测试面：
//   ① rcgen：定种子 keypair → 固定自证书，证书 DER 长度 + FNV-1a 硬断言
//      （EXPECTED_CERT_FNV 常量锚）；key PKCS#8 FNV 打印。
//   ② subject/issuer：自签证书二者必同，由 cert.params() 的 DistinguishedName
//      渲染（插入序迭代，rcgen 用 Vec 保序）。
//   ③ rustls：本进程内 loopback（127.0.0.1:0，端口不进输出）双端握手——
//      server 线程 accept + echo，client 主线程发起；打印双端握手布尔、
//      handshake_kind、协议版本、cipher suite、ALPN。
//   ④ 应用数据双向 echo：payload A 短串（client→server→client），payload B
//      20KB（server→client→server；>16384 强制 TLS 分两条 record），双向
//      == 校验 + len/FNV 打印。
//   ⑤ 线程调度序不进输出：server 线程经 join 回传报告，全部打印在主线程
//      按固定序执行。
// 三维复跑：
//   A: target/release/mirvm run corpus/c_rustls_shake.rs
//   B: d=$(dirname "$(grep -l 'name = "c_rustls_shake"' ~/.cache/mirvm/scripts/*/Cargo.toml | head -1)") && \
//      cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_rustls_shake.rs
// FRONTIER 绕行：无。ring 归档（C+asm、-fvisibility=hidden）已实证全绿
// （docs/corpus.md ring bug② 修复记录）；Ed25519 在 webpki/rustls 均为标准
// 一级路径。stderr 真空；cargo run -q。
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use rcgen::{
    CertificateParams, DistinguishedName, DnType, DnValue, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, PKCS_ED25519, SerialNumber,
};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName,
};
use rustls::{
    ClientConfig, ClientConnection, ConnectionCommon, RootCertStore, ServerConfig,
    ServerConnection,
};

const SEED: u64 = 0x5EED_C0DE_55A4_E001;
const SEED_B: u64 = 0x5EED_C0DE_55A4_E002;

/// RFC 8410 Ed25519 PKCS#8 v1 前缀（OneAsymmetricKey，无 publicKey 字段）。
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
    0x20,
];

/// 首跑 native 后回填的证书 DER FNV-1a64 锚（见头注①）。
const EXPECTED_CERT_FNV: u64 = 0xc58c_0fdd_ae72_d6b4;

const PAYLOAD_A: &[u8] = b"mirvm c_rustls_shake: deterministic echo payload A";
const PAYLOAD_B_LEN: usize = 20_000; // >16384：强制分两条 TLS record

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// splitmix64 流：定种子展开任意长确定性字节串。
struct Splitmix(u64);
impl Splitmix {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn fill(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

/// DN 渲染成 "O=.., CN=.."（插入序；仅覆盖本 driver 用到的两种类型）。
fn render_dn(dn: &DistinguishedName) -> String {
    dn.iter()
        .map(|(ty, val)| {
            let t = match ty {
                DnType::OrganizationName => "O",
                DnType::CommonName => "CN",
                other => panic!("unexpected dn type {other:?}"),
            };
            let v = match val {
                DnValue::Utf8String(s) => s.as_str(),
                other => panic!("unexpected dn value {other:?}"),
            };
            format!("{t}={v}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 生成定种子 Ed25519 keypair（PKCS#8 v1 手工容器 + rcgen 指定算法导入）。
fn make_keypair() -> KeyPair {
    let mut der = ED25519_PKCS8_PREFIX.to_vec();
    der.extend_from_slice(&Splitmix(SEED).fill(32));
    KeyPair::from_pkcs8_der_and_sign_algo(&PrivatePkcs8KeyDer::from(der), &PKCS_ED25519).unwrap()
}

/// 固定参数自签证书（全量显式参数，无随机通道，见头注③）。
fn make_cert(kp: &KeyPair) -> rcgen::Certificate {
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "mirvm corpus");
    dn.push(DnType::CommonName, "mirvm.test");
    params.distinguished_name = dn;
    params.is_ca = IsCa::NoCa;
    params.serial_number = Some(SerialNumber::from(0x5155_A45E_0001u64));
    params.not_before = rcgen::date_time_ymd(2026, 1, 1);
    params.not_after = rcgen::date_time_ymd(2046, 1, 1);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.self_signed(kp).unwrap()
}

/// handshake 推进：complete_io 在握手未完成前阻塞读写 TCP（此时确实需要读，
/// 无死锁；数据在调用前已由对端发出）。
fn drive_handshake<D>(conn: &mut ConnectionCommon<D>, sock: &mut TcpStream, label: &str) {
    while conn.is_handshaking() {
        conn.complete_io(sock).unwrap_or_else(|e| panic!("{label} handshake io: {e}"));
    }
}

/// 纯冲刷待写（hs 后发数据：wants_write 时 write_tls 阻塞写，不碰读侧）。
fn flush_write<D>(conn: &mut ConnectionCommon<D>, sock: &mut TcpStream) {
    while conn.wants_write() {
        conn.write_tls(sock).unwrap();
    }
}

/// 读完为止：plaintext 未就绪（WouldBlock）→ complete_io 阻塞拉到下一条 record。
fn recv_exact<D>(conn: &mut ConnectionCommon<D>, sock: &mut TcpStream, buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        match conn.reader().read(&mut buf[filled..]) {
            Ok(0) => panic!("recv eof"),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                conn.complete_io(sock).unwrap();
            }
            Err(e) => panic!("recv: {e}"),
        }
    }
}

fn send_all<D>(conn: &mut ConnectionCommon<D>, sock: &mut TcpStream, data: &[u8]) {
    conn.writer().write_all(data).unwrap();
    flush_write(conn, sock);
}

struct EndReport {
    hs_done: bool,
    hs_kind: String,
    version: String,
    suite: String,
    alpn: String,
}

fn report<D>(conn: &ConnectionCommon<D>) -> EndReport {
    EndReport {
        hs_done: !conn.is_handshaking(),
        hs_kind: format!("{:?}", conn.handshake_kind()),
        version: format!("{:?}", conn.protocol_version()),
        suite: format!("{:?}", conn.negotiated_cipher_suite().map(|s| s.suite())),
        alpn: conn
            .alpn_protocol()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .unwrap_or_else(|| "<none>".to_string()),
    }
}

fn print_report(label: &str, r: &EndReport) {
    println!("{label} hs_done={} kind={} ver={} suite={} alpn={}", r.hs_done, r.hs_kind, r.version, r.suite, r.alpn);
}

/// server 线程：accept → 握手 → echo A → 发 B → 收 B 回波校验 → 回传报告。
/// 返回 (报告, echo_a 校验, echo_b 校验)。不打印（调度序不进输出）。
fn server_side(
    listener: TcpListener,
    scfg: Arc<ServerConfig>,
    payload_b: Vec<u8>,
) -> (EndReport, bool, bool) {
    let (mut sock, _peer) = listener.accept().unwrap();
    let mut conn = ServerConnection::new(scfg).unwrap();
    drive_handshake(&mut conn, &mut sock, "server");

    // A：收 → 校验 → 原样回
    let mut ea = vec![0u8; PAYLOAD_A.len()];
    recv_exact(&mut conn, &mut sock, &mut ea);
    let echo_a_ok = ea == PAYLOAD_A;
    send_all(&mut conn, &mut sock, PAYLOAD_A);

    // B：发 → 收回波 → 校验
    send_all(&mut conn, &mut sock, &payload_b);
    let mut eb = vec![0u8; payload_b.len()];
    recv_exact(&mut conn, &mut sock, &mut eb);
    let echo_b_ok = eb == payload_b;

    (report(&conn), echo_a_ok, echo_b_ok)
}

fn main() {
    // ---- ① 定种子 keypair + 固定自证书 ----
    let kp = make_keypair();
    let key_der = kp.serialize_der();
    println!("key pkcs8 len={} fnv={:016x}", key_der.len(), fnv1a(&key_der));

    let cert = make_cert(&kp);
    let cert_der: &CertificateDer<'static> = cert.der();
    let cert_fnv = fnv1a(cert_der.as_ref());
    println!("cert der len={} fnv={:016x}", cert_der.as_ref().len(), cert_fnv);
    assert_eq!(cert_fnv, EXPECTED_CERT_FNV, "cert der fnv drift");

    // 自签：subject == issuer == params 的 DN
    let dn = render_dn(&cert.params().distinguished_name);
    println!("cert subject = {dn}");
    println!("cert issuer  = {dn}");
    println!("cert serial  = {}", cert.params().serial_number.as_ref().unwrap());

    // ---- ② rustls 双端 config ----
    let mut scfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert_der.clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
        )
        .unwrap();
    scfg.alpn_protocols = vec![b"mirvm-echo/1".to_vec()];

    let mut roots = RootCertStore::empty();
    roots.add(cert_der.clone()).unwrap();
    let mut ccfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    ccfg.alpn_protocols = vec![b"mirvm-echo/1".to_vec()];

    // ---- ③ loopback 双端握手（端口 0 分配，不进输出）----
    let payload_b = Splitmix(SEED_B).fill(PAYLOAD_B_LEN);
    println!(
        "payload a len={} fnv={:016x} | b len={} fnv={:016x}",
        PAYLOAD_A.len(),
        fnv1a(PAYLOAD_A),
        payload_b.len(),
        fnv1a(&payload_b)
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    println!("bound loopback = {}", addr.ip().is_loopback());

    let scfg = Arc::new(scfg);
    let pb = payload_b.clone();
    let server = std::thread::spawn(move || server_side(listener, scfg, pb));

    let mut sock = TcpStream::connect(addr).unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut conn = ClientConnection::new(Arc::new(ccfg), name).unwrap();
    drive_handshake(&mut conn, &mut sock, "client");

    // ---- ④ 应用数据双向 echo ----
    send_all(&mut conn, &mut sock, PAYLOAD_A);
    let mut ea = vec![0u8; PAYLOAD_A.len()];
    recv_exact(&mut conn, &mut sock, &mut ea);
    let client_echo_a_ok = ea == PAYLOAD_A;

    let mut rb = vec![0u8; payload_b.len()];
    recv_exact(&mut conn, &mut sock, &mut rb);
    let client_echo_b_ok = rb == payload_b;
    send_all(&mut conn, &mut sock, &payload_b);

    let client_report = report(&conn);

    // ---- ⑤ 汇总（全在主线程固定序打印）----
    let (server_report, server_echo_a_ok, server_echo_b_ok) = server.join().unwrap();
    print_report("client", &client_report);
    print_report("server", &server_report);
    println!("echo a client_ok={client_echo_a_ok} server_ok={server_echo_a_ok}");
    println!("echo b client_ok={client_echo_b_ok} server_ok={server_echo_b_ok}");
}
