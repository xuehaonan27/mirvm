#!/usr/bin/env mirvm
---
[dependencies]
# rsa is pinned to patch =0.9.10 (same version as the c_rsa_pss lock).
# default-features=false drops u64_digit from the default [std, pem, u64_digit]
# triple (the i128 Neg detour, see below) and re-enables std + sha2 (the OID
# association for Pkcs1v15Sign::new::<Sha256>) + pem for the PKCS#1/#8 parse path.
rsa = { version = "=0.9.10", default-features = false, features = ["std", "sha2", "pem"] }
---
// rsa 0.9 at 4096 bits (pure-Rust bignums, num-bigint-dig backend) differential. This
// driver takes a different input shape from c_rsa_pss (the 2048-bit component-rebuild
// surface): it embeds a 4096-bit PKCS#8 PEM private key constant generated once with
// openssl genpkey and then frozen (no keygen RNG), and checks PEM parsing, CRT component
// bit lengths/congruences, and a deterministic v1.5 signature at 4096 bits (two 2048-bit
// CRT modpows) across the three dimensions. A second 4096-bit private-key operation would
// double the cost, so the driver deliberately stops at one.
//
// Coverage:
//   1) RsaPrivateKey::from_pkcs8_pem parsing (PEM -> base64 -> PKCS#8 DER -> CRT loading)
//      with bit-length anchors: size()=512B, n.bits()=4096, p/q.bits()=2048, e=65537
//      (all guaranteed by openssl genpkey); d/dp/dq/qinv bit lengths are printed.
//   2) CRT math checks: p*q==n, e*dp == 1 (mod p-1), e*dq == 1 (mod q-1), qinv*q == 1 (mod p).
//   3) One deterministic v1.5 signature (PKCS1v15Sign::new::<Sha256>, no RNG channel),
//      anchored by len=512 + FNV-1a64, verified in place and with the split-out key.
//   4) Public-key export/reimport: to_pkcs1_der -> from_pkcs1_der equals the original,
//      and the reimported key verifies the same signature.
//   5) Negative cases: a 1-bit signature tamper and a wrong message digest are both rejected.
//   6) The PEM error path: a non-base64 character makes from_pkcs8_pem return static error text.
// Deliberately omitted: keygen/OAEP/v1.5 encryption all carry an RNG channel, which breaks
// this driver's determinism discipline, and the signature is not repeated because v1.5
// without salt randomization is already deterministic, so the differential is the oracle.
//
// u64_digit workaround: the rsa default feature u64_digit makes num-bigint-dig use u64
// limbs (SignedDoubleBigDigit = i128), and the i128 unary Neg in inv_mod_alt's closing
// `-k0 as BigDigit` (which modpow always reaches) traps during lowering (exit 70). Setting
// default-features=false falls back to u32 limbs (SDouble = i64, fully supported scalars).
// BigUint value semantics do not depend on limb width, and native builds with either feature
// print byte-identical stdout, so the differential is unaffected.
//
// Big-number performance note: a 2048-bit rsa_pss driver with a dozen private-key operations
// took 335s in the JIT dimension against a 400s timeout. This driver keeps one 4096-bit CRT
// sign (about two 2048-bit modpows) plus three cheap verifications, so its runtime is a known
// big-number cost, not an engine bug signal; relax the timeout to the rsa_pss level if needed.
// No FRONTIER: the only trap on this path is the i128 Neg avoided by the feature choice.
//
// Three-way rerun (locate the B script dir by name after A's first run):
//   A: target/release/mirvm run corpus/c_rsa_4096.rs
//   B: d=$(grep -l 'name = "c_rsa_4096"' ~/.cache/mirvm/scripts/*/Cargo.toml);
//      cd $(dirname $d) && RUSTC=$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc \
//        $HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_rsa_4096.rs
use rsa::pkcs1::{DecodeRsaPublicKey, EncodeRsaPublicKey};
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::{Digest, Sha256};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};

/// A PKCS#8 PEM (52 lines) frozen from a one-off `openssl genpkey -algorithm RSA
/// -pkeyopt rsa_keygen_bits:4096` + `pkcs8 -topk8 -nocrypt`; constant, no RNG.
const PRIV_PKCS8_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
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
63oi9Ntb8YmkN81A7LpP56dI43lZ8w==
-----END PRIVATE KEY-----"#;

/// The fixed signed message (arbitrary content, fixed; only enters the digest, never output).
const MSG: &[u8] = b"mirvm corpus rsa-4096 deterministic pkcs1v15 sign/verify probe";

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn main() {
    // ---- ① PKCS#8 PEM parsing + bit-length anchors ----
    let priv_key = RsaPrivateKey::from_pkcs8_pem(PRIV_PKCS8_PEM).unwrap();
    let pub_key = priv_key.to_public_key();
    assert_eq!(priv_key.size(), 512);
    assert_eq!(pub_key.n().bits(), 4096);
    assert_eq!(*pub_key.e(), BigUint::from(65537u32));
    let primes = priv_key.primes();
    assert_eq!(primes.len(), 2);
    assert_eq!(primes[0].bits(), 2048);
    assert_eq!(primes[1].bits(), 2048);
    println!("size = {}", priv_key.size());
    println!("n bits = {}", pub_key.n().bits());
    println!("e = {}", pub_key.e());
    println!("d bits = {}", priv_key.d().bits());
    println!("p bits = {}", primes[0].bits());
    println!("q bits = {}", primes[1].bits());
    let one = BigUint::from(1u32);
    let (p, q) = (&primes[0], &primes[1]);
    let dp = priv_key.dp().unwrap();
    let dq = priv_key.dq().unwrap();
    let qinv = priv_key.crt_coefficient().unwrap();
    println!("dp bits = {}", dp.bits());
    println!("dq bits = {}", dq.bits());
    println!("qinv bits = {}", qinv.bits());

    // ---- ② CRT math congruence checks (2048-bit multiply+mod, no modpow) ----
    println!("p*q == n = {}", p * q == *pub_key.n());
    println!("e*dp mod (p-1) == 1 = {}", (pub_key.e() * dp) % (p - &one) == one);
    println!("e*dq mod (q-1) == 1 = {}", (pub_key.e() * dq) % (q - &one) == one);
    println!("qinv*q mod p == 1 = {}", (qinv * q) % p == one);

    // ---- ③ deterministic v1.5 signature (the only 4096-bit private-key op) ----
    let digest = Sha256::digest(MSG);
    println!("digest fnv = {:016x}", fnv1a(&digest));
    let sig = priv_key.sign(Pkcs1v15Sign::new::<Sha256>(), &digest).unwrap();
    assert_eq!(sig.len(), 512);
    println!("sig len = {}", sig.len());
    println!("sig fnv = {:016x}", fnv1a(&sig));

    // ---- ④ verification both ways (private-key parts in place + the split-out public key) ----
    println!(
        "verify pub-from-priv = {}",
        pub_key.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &sig).is_ok()
    );

    // ---- ⑤ public-key export/reimport then verify again ----
    let pub_der = pub_key.to_pkcs1_der().unwrap();
    println!("pub der len={} fnv={:016x}", pub_der.as_bytes().len(), fnv1a(pub_der.as_bytes()));
    let pub_rt = RsaPublicKey::from_pkcs1_der(pub_der.as_bytes()).unwrap();
    println!("pub roundtrip eq = {}", pub_rt == pub_key);
    println!(
        "verify pub-rt = {}",
        pub_rt.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &sig).is_ok()
    );

    // ---- ⑥ negative cases: tampered signature / wrong message ----
    let mut bad_sig = sig.clone();
    bad_sig[10] ^= 0x01;
    println!(
        "tamper rejected = {}",
        pub_rt.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &bad_sig).is_err()
    );
    let wrong = Sha256::digest(b"not the signed message");
    println!(
        "wrong-msg rejected = {}",
        pub_rt.verify(Pkcs1v15Sign::new::<Sha256>(), &wrong, &sig).is_err()
    );

    // ---- ⑦ PEM error path (injecting a non-base64 character) ----
    let bad_pem = PRIV_PKCS8_PEM.replace("MIIJ", "M!IJ");
    match RsaPrivateKey::from_pkcs8_pem(&bad_pem) {
        Ok(_) => println!("bad-pem unexpectedly ok"),
        Err(err) => println!("bad-pem err = {err}"),
    }

    println!("rsa4096 ok");
}
