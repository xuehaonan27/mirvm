#!/usr/bin/env mirvm
---
[dependencies]
# rustls pinned to =0.23.42 (latest 0.23.x, pinned like c_rustls_cert): the default
# aws-lc-rs provider is off (aws-lc-sys is a huge C/C++ archive); only the official
# ring provider + std + tls12 + logging are enabled -- the ring archive is proven good.
rustls = { version = "=0.23.42", default-features = false, features = ["ring", "std", "tls12", "logging"] }
# rcgen pinned to =0.13.2 (latest 0.13.x): default-features=false keeps only ["ring"];
# the default trio's pem feature is unused (everything is DER, no PEM serialization),
# and x509-parser is off (no parsing; subject/issuer come straight from params).
rcgen = { version = "=0.13.2", default-features = false, features = ["ring"] }
---
// rcgen 0.13 + rustls 0.23 (ring provider) differential fixture: fixed-seed self-signed
// certificate + real in-process loopback TLS handshake + bidirectional echo. Where
// c_rustls_cert covers the static handshake-free surface (pemfile/verifier/config),
// this driver covers the dynamic surface on a real socket: handshake + round trips.
//
// Determinism: a handshake is full of randomness (ECDHE ephemeral keys, hello nonces,
// session tickets); only negotiated enums/booleans print, stable for a fixed config.
//   ① key algorithm Ed25519: RFC 8032 signatures are deterministic (no random nonce), so
//      the certificate DER is byte-stable across processes; ECDSA is excluded because
//      ring 0.17 signs with a random nonce, making the DER unanchorable.
//   ② fixed seed: splitmix64(SEED) expands to 32B as the Ed25519 seed, hand-wrapped in
//      the RFC 8410 PKCS#8 v1 prefix (302e020100300506032b657004220420||seed) and
//      imported into rcgen; KeyPair::serialize_der returns it unchanged, FNV-anchored.
//   ③ all certificate parameters are explicit: serial_number is a pinned literal (0.13.2's
//      None path derives it from the SPKI SHA-256 and is deterministic anyway, but explicit
//      is more stable); not_before/not_after use a fixed date_time_ymd (the default
//      1975..4096 range has no wall-clock channel either, but it is pinned); SAN/DN/
//      keyUsage/EKU are constants; webpki's validity check reads the real wall clock, but
//      the 2026..2046 window keeps the verdict constant. is_ca=NoCa (no BC extension): an
//      EE-role self-signed cert passes (missing BC counts as is_ca=false; a Ca would hit
//      CaUsedAsEndEntity); as a trust anchor webpki matches subject+SPKI only, ignoring BC.
// Test surface:
//   ① rcgen: fixed-seed keypair -> a fixed self-signed certificate, hard-asserting the DER
//      length + FNV-1a (the EXPECTED_CERT_FNV anchor); the key PKCS#8 FNV is printed.
//   ② subject/issuer: for a self-signed certificate the two must be equal, rendered from
//      cert.params()'s DistinguishedName (insertion order; rcgen preserves it with a Vec).
//   ③ rustls: in-process loopback (127.0.0.1:0, port never printed) two-sided handshake --
//      the server thread accepts + echoes, the client main thread initiates; it prints both
//      sides' handshake boolean, handshake_kind, protocol version, cipher suite and ALPN.
//   ④ bidirectional echo: payload A is a short string (client -> server -> client), and
//      payload B is 20KB (server -> client -> server; >16384 forces two TLS records),
//      with both directions checked for == plus len/FNV printing.
//   ⑤ thread scheduling never reaches the output: the server thread returns its report via
//      join, and all printing happens on the main thread in a fixed order.
// Three-way differential rerun:
//   A: target/release/mirvm run tests/scripts/c_rustls_shake.rs
//   B: d=$(dirname "$(grep -l 'name = "c_rustls_shake"' ~/.cache/mirvm/scripts/*/Cargo.toml | head -1)") && \
//      cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_rustls_shake.rs
// FRONTIER workaround: none. The ring archive (C+asm, -fvisibility=hidden) is already proven good
// in this repo; Ed25519 is a standard first-class path in both webpki and rustls.
// stderr must be empty; run with cargo run -q.
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

/// RFC 8410 Ed25519 PKCS#8 v1 prefix (OneAsymmetricKey, no publicKey field).
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
    0x20,
];

/// Certificate DER FNV-1a64 anchor, filled in after the first native run (see header note ①).
const EXPECTED_CERT_FNV: u64 = 0xc58c_0fdd_ae72_d6b4;

const PAYLOAD_A: &[u8] = b"mirvm c_rustls_shake: deterministic echo payload A";
const PAYLOAD_B_LEN: usize = 20_000; // >16384: forces TLS to split it into two records

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// splitmix64 stream: expands a fixed seed into an arbitrarily long deterministic byte string.
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

/// Renders a DN as "O=.., CN=.." (insertion order; only the two types this driver uses are covered).
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

/// Fixed-seed Ed25519 keypair: hand-made PKCS#8 v1 container, explicit sign algorithm.
fn make_keypair() -> KeyPair {
    let mut der = ED25519_PKCS8_PREFIX.to_vec();
    der.extend_from_slice(&Splitmix(SEED).fill(32));
    KeyPair::from_pkcs8_der_and_sign_algo(&PrivatePkcs8KeyDer::from(der), &PKCS_ED25519).unwrap()
}

/// Fixed-parameter self-signed certificate (all parameters explicit, no random channel; see header note ③).
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

/// Handshake driver: complete_io blocks on TCP reads and writes until the handshake ends;
/// reads are genuinely needed so there is no deadlock -- the peer already sent its data.
fn drive_handshake<D>(conn: &mut ConnectionCommon<D>, sock: &mut TcpStream, label: &str) {
    while conn.is_handshaking() {
        conn.complete_io(sock).unwrap_or_else(|e| panic!("{label} handshake io: {e}"));
    }
}

/// Pending-write flush: write_tls blocks on writes while wants_write holds, never touching reads.
fn flush_write<D>(conn: &mut ConnectionCommon<D>, sock: &mut TcpStream) {
    while conn.wants_write() {
        conn.write_tls(sock).unwrap();
    }
}

/// Read until full: on WouldBlock, complete_io blocks until the next record arrives.
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

/// server thread: accept -> handshake -> echo A -> send B -> verify the B echo -> return the report.
/// Returns (report, echo_a check, echo_b check). Never prints (scheduling order must not reach the output).
fn server_side(
    listener: TcpListener,
    scfg: Arc<ServerConfig>,
    payload_b: Vec<u8>,
) -> (EndReport, bool, bool) {
    let (mut sock, _peer) = listener.accept().unwrap();
    let mut conn = ServerConnection::new(scfg).unwrap();
    drive_handshake(&mut conn, &mut sock, "server");

    // A: receive -> check -> echo back verbatim
    let mut ea = vec![0u8; PAYLOAD_A.len()];
    recv_exact(&mut conn, &mut sock, &mut ea);
    let echo_a_ok = ea == PAYLOAD_A;
    send_all(&mut conn, &mut sock, PAYLOAD_A);

    // B: send -> receive the echo -> check
    send_all(&mut conn, &mut sock, &payload_b);
    let mut eb = vec![0u8; payload_b.len()];
    recv_exact(&mut conn, &mut sock, &mut eb);
    let echo_b_ok = eb == payload_b;

    (report(&conn), echo_a_ok, echo_b_ok)
}

fn main() {
    // ---- ① fixed-seed keypair + fixed self-signed certificate ----
    let kp = make_keypair();
    let key_der = kp.serialize_der();
    println!("key pkcs8 len={} fnv={:016x}", key_der.len(), fnv1a(&key_der));

    let cert = make_cert(&kp);
    let cert_der: &CertificateDer<'static> = cert.der();
    let cert_fnv = fnv1a(cert_der.as_ref());
    println!("cert der len={} fnv={:016x}", cert_der.as_ref().len(), cert_fnv);
    assert_eq!(cert_fnv, EXPECTED_CERT_FNV, "cert der fnv drift");

    // self-signed: subject == issuer == the params DN
    let dn = render_dn(&cert.params().distinguished_name);
    println!("cert subject = {dn}");
    println!("cert issuer  = {dn}");
    println!("cert serial  = {}", cert.params().serial_number.as_ref().unwrap());

    // ---- ② rustls config for both ends ----
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

    // ---- ③ loopback two-sided handshake (port 0 is allocated and never reaches the output) ----
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

    // ---- ④ bidirectional application-data echo ----
    send_all(&mut conn, &mut sock, PAYLOAD_A);
    let mut ea = vec![0u8; PAYLOAD_A.len()];
    recv_exact(&mut conn, &mut sock, &mut ea);
    let client_echo_a_ok = ea == PAYLOAD_A;

    let mut rb = vec![0u8; payload_b.len()];
    recv_exact(&mut conn, &mut sock, &mut rb);
    let client_echo_b_ok = rb == payload_b;
    send_all(&mut conn, &mut sock, &payload_b);

    let client_report = report(&conn);

    // ---- ⑤ summary (all printed in a fixed order on the main thread) ----
    let (server_report, server_echo_a_ok, server_echo_b_ok) = server.join().unwrap();
    print_report("client", &client_report);
    print_report("server", &server_report);
    println!("echo a client_ok={client_echo_a_ok} server_ok={server_echo_a_ok}");
    println!("echo b client_ok={client_echo_b_ok} server_ok={server_echo_b_ok}");
}
