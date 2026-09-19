#!/usr/bin/env mirvm
---
[dependencies]
snow = "0.9"
---
// snow 0.9 (Noise protocol framework) differential: Noise_XX mutual-auth handshake (3
// messages) + Noise_NKpsk0 (psk at index 0 + pre-shared peer static public key). Static keys
// are fixed bytes; the ephemeral is pinned with Builder::fixed_ephemeral_key_for_testing_only,
// after which the handshake state machine never touches OsRng -> every handshake transcript is
// byte-exact. Covers: XX three-message exchange (per-hop payload encryption via
// was_write_payload_encrypted), matching handshake hash on both sides, mutual remote static
// public key, into_transport_mode round-trip of encrypted messages both ways, tampered
// ciphertext/tag errors, the rekey pair (rekey_outgoing/incoming), NKpsk0 psk correct/wrong,
// out-of-order write_message, and unfinished-handshake StateProblem error paths.
//
// Known FRONTIER workaround (same semantics, different cipher feature):
// The suite Noise_XX_25519_AESGCM_SHA256 hits the AES-NI hardware path selected by the aes
// family's runtime cpuid probe on the second XX hop (the responder's first AEAD encryption of
// a static public key); diagnostic text (exit 70):
//   mirvm[m4-engine]: TRAP: foreign `llvm.x86.aesni.aeskeygenassist` (LLVM internal
//   symbol, built on demand) (fn _RINvNtNtNtCs…_4core9core_arch3x863aes25__mm_aeskeygenassist_si128…)
// The `llvm.x86.aesni.*` hazard: aes-gcm has no force-soft fallback; the workaround is Noise's
// ChaChaPoly cipher (pure-software chacha20poly1305) -- protocol/DH/hash coverage unchanged.
// chacha20poly1305 0.10's transitive poly1305 0.8 has an avx2 backend that hits
// `llvm.x86.avx2.permd`; diagnostic text (exit 70):
//   mirvm[m4-engine]: TRAP: foreign `llvm.x86.avx2.permd` (LLVM internal symbol, built
//   on demand) (fn _RNvNtNtNtCs…_4core9core_arch3x864avx227__mm256_permutevar8x32_epi32…)
// Workaround: the `--cfg poly1305_force_soft` switch built into poly1305 0.8 swaps only the
// backend implementation; Poly1305 is exact integer arithmetic, so the output is
// bit-identical. Both mirvm dimensions therefore need:
//   RUSTFLAGS='--cfg poly1305_force_soft'
// (cargo fingerprints RUSTFLAGS, so toggling the flag rebuilds automatically; the native
// dimension runs bare -- the host CPU handles avx2 and both backends agree byte-for-byte.)
// Note: curve25519-dalek 4.1's default simd backend passes the whole run (8 X25519 ops across
// XX/NK) under mirvm: its avx2 vector field arithmetic uses plain IR intrinsics
// (_mm256_mul_epu32 etc.), never the vpmadd52 FRONTIER (ifma needs a compile-time
// target_feature the toolchain lacks), so no ed25519 serial-backend env is needed.
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, TransportState};

const SUITE: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
const SUITE_NK: &str = "Noise_NKpsk0_25519_ChaChaPoly_SHA256";

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
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

/// Derive 32B key material from fixed bytes (x25519 clamps internally; any 32B is valid).
fn key(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = seed ^ (i as u8).wrapping_mul(0x9d);
    }
    k
}

/// One handshake hop: writer's write_message -> reader's read_message, printing the transcript
/// hex / writer payload-encrypted flag / payload, accumulated into an fnv sink.
fn hop(
    label: &str,
    w: &mut HandshakeState,
    r: &mut HandshakeState,
    payload: &[u8],
    sink: &mut Vec<u8>,
) {
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 512];
    let n = w.write_message(payload, &mut tx).unwrap();
    sink.extend_from_slice(&tx[..n]);
    println!(
        "{label} len={n} payload_enc={} msg={}",
        w.was_write_payload_encrypted(),
        hex(&tx[..n])
    );
    let m = r.read_message(&tx[..n], &mut rx).unwrap();
    println!(
        "{label} payload len={m} ok={} text={:?}",
        rx[..m] == *payload,
        std::str::from_utf8(&rx[..m]).unwrap()
    );
}

/// One transport hop: encrypt -> peer decrypt round-trip, printing the ciphertext hex.
fn thop(label: &str, w: &mut TransportState, r: &mut TransportState, payload: &[u8]) {
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 512];
    let n = w.write_message(payload, &mut tx).unwrap();
    println!("{label} ct len={n} = {}", hex(&tx[..n]));
    let m = r.read_message(&tx[..n], &mut rx).unwrap();
    println!("{label} roundtrip = {}", rx[..m] == *payload);
}

fn main() {
    let is = key(0x11); // initiator static
    let ie = key(0x33); // initiator ephemeral
    let rs = key(0x22); // responder static
    let re = key(0x44); // responder ephemeral
    let prologue = b"mirvm-snow-prologue-v1";

    // ---- (1) Noise_XX mutual-auth handshake (neither side knows the other's static public key) ----
    let params: NoiseParams = SUITE.parse().unwrap();
    let mut init = Builder::new(params.clone())
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .prologue(prologue)
        .build_initiator()
        .unwrap();
    let mut resp = Builder::new(params)
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .prologue(prologue)
        .build_responder()
        .unwrap();
    println!("xx pre finished i={} r={}", init.is_handshake_finished(), resp.is_handshake_finished());
    println!("xx pre turn i={} r={}", init.is_my_turn(), resp.is_my_turn());

    let mut sink = Vec::new();
    // XX: -> e | <- e, ee, s, es | -> s, se
    hop("xx m1 (->e)", &mut init, &mut resp, b"xx-payload-1-plain", &mut sink);
    hop("xx m2 (<-e,ee,s,es)", &mut resp, &mut init, b"xx-payload-2-enc", &mut sink);
    hop("xx m3 (->s,se)", &mut init, &mut resp, b"xx-payload-3-enc", &mut sink);
    println!("xx transcripts fnv = {:016x}", fnv1a(&sink));

    println!("xx post finished i={} r={}", init.is_handshake_finished(), resp.is_handshake_finished());
    let hh_i = init.get_handshake_hash().to_vec();
    let hh_r = resp.get_handshake_hash().to_vec();
    println!("xx hh i = {}", hex(&hh_i));
    println!("xx hh r = {}", hex(&hh_r));
    println!("xx hh eq = {}", hh_i == hh_r);
    // After XX both sides see the other's static public key; keep them as NKpsk0's remote_public_key
    let mut is_pub = [0u8; 32];
    let mut rs_pub = [0u8; 32];
    is_pub.copy_from_slice(resp.get_remote_static().unwrap());
    rs_pub.copy_from_slice(init.get_remote_static().unwrap());
    println!("xx r sees is_pub = {}", hex(&is_pub));
    println!("xx i sees rs_pub = {}", hex(&rs_pub));

    // ---- (2) transport: encrypted messages both ways + tampering + rekey ----
    let mut it = init.into_transport_mode().unwrap();
    let mut rt = resp.into_transport_mode().unwrap();
    thop("tx i->r #0", &mut it, &mut rt, b"transport msg zero");
    thop("tx i->r #1", &mut it, &mut rt, b"");
    thop("tx r->i #0", &mut rt, &mut it, "reply with unicode 汉字 🎉".as_bytes());
    thop("tx r->i #1", &mut rt, &mut it, &[0xde, 0xad, 0xbe, 0xef]);

    // Tampering one ciphertext byte -> AEAD must fail. snow semantics: a failed decrypt does not
    // advance the receiver nonce (`decrypt(...)?` in cipherstate.rs returns early), but the sender
    // nonce already advanced -> realign with receiving_nonce/set_receiving_nonce to continue the session.
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 512];
    let n = it.write_message(b"will be tampered", &mut tx).unwrap();
    tx[3] ^= 0x01;
    match rt.read_message(&tx[..n], &mut rx) {
        Ok(_) => println!("tx tamper unexpectedly ok"),
        Err(e) => println!("tx tamper err = {e}"),
    }
    let desync = rt.receiving_nonce();
    println!("tx recv nonce after failed read = {desync}");
    rt.set_receiving_nonce(desync + 1);
    println!("tx recv nonce resynced = {}", rt.receiving_nonce());

    // rekey pair: one side's rekey_outgoing <-> the peer's rekey_incoming, then keep round-tripping
    it.rekey_outgoing();
    rt.rekey_incoming();
    thop("tx post-rekey i->r", &mut it, &mut rt, b"after rekey one");
    rt.rekey_outgoing();
    it.rekey_incoming();
    thop("tx post-rekey r->i", &mut rt, &mut it, b"after rekey two");

    // ---- (3) error paths: out-of-order write, unfinished handshake, tampered handshake msg ----
    let params: NoiseParams = SUITE.parse().unwrap();
    let mut early_i = Builder::new(params.clone())
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .build_initiator()
        .unwrap();
    let mut early_r = Builder::new(params)
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .build_responder()
        .unwrap();
    // The responder's first hop can only read: an out-of-order write must err
    let mut tmp = [0u8; 512];
    match early_r.write_message(b"out of turn", &mut tmp) {
        Ok(_) => println!("xx out-of-turn write unexpectedly ok"),
        Err(e) => println!("xx out-of-turn write err = {e}"),
    }
    // into_transport_mode on an unfinished handshake must err
    let early_i2 = Builder::new(SUITE.parse().unwrap())
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .build_initiator()
        .unwrap();
    match early_i2.into_transport_mode() {
        Ok(_) => println!("xx early transport unexpectedly ok"),
        Err(e) => println!("xx early transport err = {e}"),
    }
    // Tamper the second handshake hop (it carries the encrypted static public key) -> reader AEAD err
    let n = early_i.write_message(b"m1", &mut tmp).unwrap();
    let mut rbuf = [0u8; 512];
    let _ = early_r.read_message(&tmp[..n], &mut rbuf).unwrap();
    let n2 = early_r.write_message(b"m2", &mut tmp).unwrap();
    tmp[40] ^= 0x80;
    match early_i.read_message(&tmp[..n2], &mut rbuf) {
        Ok(_) => println!("xx tamper hs-m2 unexpectedly ok"),
        Err(e) => println!("xx tamper hs-m2 err = {e}"),
    }

    // ---- (4) Noise_NKpsk0: pre-shared peer static public key + psk(0) ----
    let psk = key(0x5a);
    let params_nk: NoiseParams = SUITE_NK.parse().unwrap();
    let mut nk_i = Builder::new(params_nk.clone())
        .remote_public_key(&rs_pub)
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .psk(0, &psk)
        .build_initiator()
        .unwrap();
    let mut nk_r = Builder::new(params_nk.clone())
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .psk(0, &psk)
        .build_responder()
        .unwrap();
    let mut sink2 = Vec::new();
    hop("nk m1 (->psk,e,es)", &mut nk_i, &mut nk_r, b"nk-init", &mut sink2);
    hop("nk m2 (<-e,ee)", &mut nk_r, &mut nk_i, b"nk-resp", &mut sink2);
    println!("nk transcripts fnv = {:016x}", fnv1a(&sink2));
    println!("nk finished i={} r={}", nk_i.is_handshake_finished(), nk_r.is_handshake_finished());
    let mut nit = nk_i.into_transport_mode().unwrap();
    let mut nrt = nk_r.into_transport_mode().unwrap();
    thop("nk tx i->r", &mut nit, &mut nrt, b"nk transport");

    // Wrong psk -> the first hop already fails to decrypt
    let bad_psk = key(0xa5);
    let mut bi = Builder::new(params_nk.clone())
        .remote_public_key(&rs_pub)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .psk(0, &bad_psk)
        .build_initiator()
        .unwrap();
    let mut br = Builder::new(params_nk)
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .psk(0, &psk)
        .build_responder()
        .unwrap();
    let n = bi.write_message(b"bad-psk payload", &mut tmp).unwrap();
    match br.read_message(&tmp[..n], &mut rbuf) {
        Ok(_) => println!("nk bad-psk unexpectedly ok"),
        Err(e) => println!("nk bad-psk err = {e}"),
    }
}
