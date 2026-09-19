#!/usr/bin/env mirvm
---
[dependencies]
# Pinned feature set: default-features=false drops u64_digit/pem (keeping std + the implicit
# sha2 feature that Pkcs1v15Sign::new::<Sha256>'s OID association needs). The FRONTIER note below covers it.
rsa = { version = "0.9", default-features = false, features = ["std", "sha2"] }
---
// rsa 0.9 (pure-Rust big-integer RSA on a num-bigint-dig backend) differential. Keys are not
// generated (that needs an rng): the fixed p/q/n/d components are rebuilt through
// RsaPrivateKey::from_components. The components were derived offline by a deterministic script
// (xorshift candidates + 32-base Miller-Rabin, openssl-confirmed primes), e=65537, d=e^-1 mod lcm(p-1,q-1).
//
// Coverage: from_components/from_p_q/the empty-primes SP800-56B recovery/bad-component error
// paths; the traits surface (PublicKeyParts/PrivateKeyParts n/e/d/primes/dp/dq/qinv/
// crt_coefficient) + CRT congruence checks; public-key export (PKCS#1 DER, SPKI DER) and
// private-key PKCS#8/PKCS#1 DER roundtrips plus a tampered-DER error path; PKCS1v15
// sign/verify vectors (Sha256 x2 incl. the empty message, Sha384, Sha512, new_unprefixed,
// deterministic re-signing; counterexamples: wrong message/tampered signature/wrong public
// key/digest length InputNotHashed); encryption vectors (offline hand EME-PKCS1v15 fixed
// ciphertext CT_VEC decrypted, seeded-ChaCha re-encryption byte-identical twice,
// decrypt_blinded agreement, tampered-ciphertext/over-long-message errors, empty plaintext);
// PSS (below); OAEP (seeded Sha256 encryption byte-identical + roundtrip, label, tamper).
//
// -- PSS random salt: the rsa API accepts an injected rng (the core probe result here) --
// RsaPrivateKey::sign_with_rng(&mut rng, Pss::new::<D>(), digest) accepts any
// R: CryptoRngCore (rand_core 0.6's RngCore+CryptoRng); Pss's SignatureScheme::sign
// returns Error::InvalidPaddingScheme outright for a None rng (printed by this driver).
// pss::SigningKey/VerifyingKey offer the same injection channel through
// signature::RandomizedSigner/Verifier. So a fixed-seed hand-written ChaCha20 (RFC 8439
// block function implementing rand_core 0.6 RngCore+CryptoRng through the rsa::rand_core
// re-export, so the version matches exactly) is injected: the same seed gives byte-identical
// signatures, different seeds give valid but distinct salts, and zero salt ignores the seed.
//
// -- Known FRONTIER bypass (semantics unchanged, only the crate feature set changes) --
// rsa 0.9's default features = ["std", "pem", "u64_digit"]; u64_digit makes num-bigint-dig
// 0.8.6 use u64 limbs (its build.rs sets has_i128 unconditionally): DoubleBigDigit=u128,
// SignedDoubleBigDigit=i128. Every modpow of the RSA keys (moduli n/p/q are always odd)
// goes monty_modpow -> MontyReducer::new -> inv_mod_alt, whose trailing `-k0 as BigDigit`
// (k0: i128) is a unary Neg on i128. mirvm's 128-bit integer family already has
// Bin128/Cmp128, bidirectional IntToInt casts and SwitchInt widening (sbb's i128 +=/-/as
// u64/>>=64 all pass), but the 128-bit UnOp::Neg/Not forms are not wired
// (lower_operand_scalar errors on a Bytes aggregate), so lowering degrades to Stmt::Trap
// and execution reaching inv_mod_alt exits 70 with the diagnostic:
//   mirvm[m4-engine]: TRAP: non-scalar operand (aggregate, ty=i128, M4.1+)
// Minimal repro (single pure-std file): calling `fn f(x: i128) -> i128 { -x }` triggers it;
// `-k0 as u64` and `(k0 as u64).wrapping_neg()` are bit-identical. The bypass is
// default-features=false (drop u64_digit; pem was never needed since this uses DER), so
// limbs fall back to u32 and SignedDoubleBigDigit=i64, where i64 Neg is scalar and fully
// supported. BigUint value semantics do not depend on limb width (from_bytes_be/to_bytes_be/
// arithmetic byte-identical), confirmed by a native dual-feature build whose u64_digit on/off
// versions print byte-identical stdout; the native dimension matches this frontmatter exactly.
use rsa::pkcs1::{
    DecodeRsaPrivateKey, DecodeRsaPublicKey, EncodeRsaPrivateKey, EncodeRsaPublicKey,
};
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey};
use rsa::rand_core::{CryptoRng, RngCore};
use rsa::sha2::{Digest, Sha256, Sha384, Sha512};
use rsa::signature::{RandomizedSigner, SignatureEncoding, Verifier};
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{BigUint, Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};

const P: [u8; 128] = [
    0xca, 0xfb, 0x2b, 0x10, 0x9a, 0x82, 0x20, 0x41, 0xba, 0xa6, 0xb4, 0x5e, 0x77, 0xb5, 0x14, 0x41,
    0x2c, 0x4e, 0x29, 0x73, 0x18, 0xc5, 0x4e, 0x29, 0xfd, 0x4c, 0x46, 0x3b, 0xbb, 0x88, 0x6e, 0xf5,
    0x3b, 0x30, 0x61, 0x5c, 0xe5, 0xea, 0x63, 0x68, 0xa3, 0x41, 0x1f, 0xb2, 0xb1, 0x88, 0x6d, 0xae,
    0xcf, 0x97, 0x51, 0x9b, 0x16, 0xaf, 0xd6, 0xf5, 0xd3, 0xbf, 0x98, 0x7b, 0x59, 0x99, 0x94, 0x18,
    0x05, 0xac, 0x65, 0x37, 0xb9, 0xac, 0xa1, 0x30, 0x86, 0x54, 0xd4, 0x85, 0x41, 0x35, 0xb4, 0x72,
    0xf4, 0x48, 0x94, 0xf3, 0xb7, 0x60, 0x83, 0x9a, 0x78, 0xac, 0x7f, 0x60, 0x50, 0x67, 0xe4, 0x1d,
    0x1d, 0xa2, 0x1e, 0xfd, 0x6c, 0x97, 0x8c, 0x95, 0xce, 0xd3, 0xcc, 0x77, 0x0f, 0xa7, 0x26, 0xcc,
    0x99, 0x82, 0x4e, 0x88, 0x5b, 0xaa, 0x5b, 0x81, 0x17, 0x1e, 0x36, 0x81, 0x74, 0x17, 0xd1, 0xc5,
];
const Q: [u8; 128] = [
    0xca, 0xfb, 0x2b, 0x10, 0x5b, 0x04, 0x40, 0x82, 0x8a, 0xa6, 0x77, 0x54, 0x63, 0xb4, 0x28, 0x83,
    0x81, 0x6d, 0x24, 0x06, 0xa3, 0x4d, 0x00, 0x12, 0x72, 0xb9, 0x91, 0x1c, 0x60, 0x6d, 0xde, 0x92,
    0x12, 0x4c, 0xd9, 0x3a, 0x3b, 0x56, 0xe1, 0xaf, 0x1a, 0xa1, 0xc4, 0x37, 0xc9, 0xf5, 0xc7, 0xac,
    0xc4, 0x85, 0xb4, 0xb7, 0xed, 0xa4, 0x47, 0x23, 0xad, 0xd5, 0xa8, 0x84, 0x2d, 0x50, 0xa7, 0x6d,
    0x56, 0x1c, 0x6c, 0xe8, 0x9f, 0x08, 0x7d, 0x63, 0x21, 0xd9, 0x7a, 0xfd, 0x29, 0x37, 0x55, 0x59,
    0xaa, 0x53, 0x3a, 0x52, 0xef, 0x7d, 0x4d, 0xb3, 0x66, 0x67, 0x2d, 0xff, 0x4b, 0x96, 0xbb, 0xe8,
    0x68, 0xd6, 0x7a, 0xa0, 0x5e, 0xec, 0x6c, 0x9f, 0x4d, 0xf3, 0x24, 0x2d, 0xcf, 0xd5, 0x73, 0x86,
    0xc8, 0xfd, 0xee, 0x21, 0x3e, 0xa4, 0xf8, 0xe1, 0x01, 0x1f, 0x2f, 0x91, 0x18, 0x5b, 0xaa, 0x5b,
];
const N: [u8; 256] = [
    0xa0, 0xf1, 0x56, 0x63, 0x7b, 0x4b, 0x4d, 0x68, 0xe0, 0xb4, 0xad, 0x7c, 0x74, 0xdb, 0x8a, 0xb6,
    0xca, 0x96, 0x81, 0x42, 0x4c, 0xbb, 0x72, 0x27, 0x9e, 0xf5, 0xb1, 0x55, 0x63, 0xec, 0xf6, 0x3c,
    0x4b, 0xc6, 0xb1, 0x4e, 0x27, 0x02, 0xfc, 0x32, 0x29, 0xae, 0x0f, 0xbc, 0xfc, 0x8b, 0x41, 0xc7,
    0x01, 0x79, 0xcb, 0x5a, 0x27, 0x83, 0x46, 0x84, 0x71, 0x95, 0x1b, 0x37, 0xf4, 0x00, 0x62, 0x32,
    0x58, 0x1b, 0xf1, 0xb1, 0xce, 0xc6, 0x76, 0x75, 0x97, 0xe0, 0xea, 0x74, 0xcb, 0xad, 0x56, 0x69,
    0xa7, 0xfb, 0x65, 0x1a, 0x08, 0x86, 0x0f, 0x66, 0x8e, 0x1c, 0x2f, 0x73, 0x1a, 0x42, 0x6d, 0x49,
    0xa1, 0x09, 0xf5, 0x64, 0xe0, 0x57, 0x9d, 0xd3, 0x5b, 0xd4, 0x5d, 0x67, 0x33, 0x9b, 0x98, 0x10,
    0x60, 0x39, 0x3d, 0x99, 0xf2, 0xe8, 0xe5, 0xd1, 0x2d, 0x29, 0x59, 0xee, 0xbb, 0x50, 0xa3, 0x1a,
    0x03, 0x54, 0x70, 0x47, 0xd0, 0x6e, 0x25, 0x12, 0xae, 0xbc, 0x3e, 0xab, 0x80, 0x27, 0x21, 0x41,
    0x72, 0x59, 0xd6, 0xd7, 0x2a, 0x9f, 0x18, 0x0f, 0x04, 0x0b, 0x1c, 0x8f, 0x1f, 0x62, 0x42, 0xf1,
    0xc5, 0xcd, 0x10, 0x0e, 0x25, 0xb2, 0x4e, 0x61, 0xb9, 0x4c, 0xa4, 0xd2, 0x0f, 0x0a, 0x40, 0x37,
    0xfa, 0x92, 0x80, 0x25, 0xdf, 0x95, 0x5c, 0x86, 0x82, 0x00, 0xb4, 0x76, 0xe1, 0x4a, 0x67, 0x0b,
    0xc2, 0xc0, 0x05, 0x2f, 0xcb, 0xce, 0x24, 0xed, 0x7f, 0xaf, 0x5c, 0xa1, 0x1f, 0x18, 0xd4, 0x45,
    0x0e, 0x52, 0xbb, 0xc6, 0xc6, 0x0b, 0x2e, 0x6c, 0x85, 0x76, 0x01, 0xe2, 0x98, 0x87, 0x49, 0x55,
    0xb7, 0xbc, 0x46, 0xd0, 0x90, 0xd4, 0xed, 0xbe, 0xdb, 0x6c, 0x17, 0x6c, 0x4c, 0xe1, 0x58, 0xf7,
    0xc3, 0xbc, 0x1c, 0x69, 0x4a, 0xfa, 0x4d, 0x93, 0x17, 0xb1, 0xd2, 0xd3, 0x1e, 0xcb, 0x63, 0x07,
];
const D: [u8; 256] = [
    0x07, 0x65, 0x75, 0x44, 0x11, 0x77, 0x51, 0x4c, 0x3d, 0x16, 0x15, 0xea, 0x4e, 0xcf, 0xda, 0x17,
    0x9d, 0x71, 0x7e, 0x3e, 0x3c, 0x45, 0x84, 0x5b, 0xad, 0x72, 0xa5, 0x74, 0x2d, 0xa9, 0x47, 0x05,
    0xad, 0x5b, 0xd3, 0xc1, 0x2a, 0x34, 0xce, 0x69, 0xf0, 0x6a, 0xc1, 0xe9, 0x67, 0x02, 0x70, 0x02,
    0xee, 0x73, 0xee, 0x49, 0xab, 0x35, 0x88, 0x13, 0x9a, 0x74, 0x06, 0x47, 0x5e, 0x2c, 0xcd, 0xab,
    0xcf, 0xa0, 0x75, 0x22, 0x92, 0x99, 0xf7, 0x4e, 0xb4, 0x81, 0xf9, 0xc9, 0xba, 0xa4, 0x86, 0xd5,
    0xf3, 0x34, 0xda, 0x9d, 0x70, 0x2a, 0x82, 0x61, 0xad, 0xad, 0x3e, 0x41, 0xaa, 0xc1, 0x71, 0x16,
    0xa8, 0x52, 0x76, 0x01, 0xc2, 0xa2, 0x49, 0x81, 0x5c, 0x4a, 0x83, 0x02, 0x5f, 0xa7, 0x9f, 0xbc,
    0xe7, 0x97, 0x29, 0xd8, 0x13, 0x31, 0x95, 0xd7, 0x8a, 0x47, 0x9e, 0x0c, 0xef, 0xc9, 0xd1, 0xbc,
    0xfe, 0x7b, 0x0b, 0xcb, 0xa1, 0x00, 0x1e, 0xba, 0xeb, 0x1d, 0xd7, 0x80, 0x30, 0x1d, 0xfe, 0xa1,
    0x51, 0xed, 0xe6, 0x95, 0x31, 0xce, 0x96, 0xe5, 0xd9, 0x0c, 0x9c, 0x4a, 0x33, 0x70, 0xe3, 0x85,
    0xec, 0x42, 0x8a, 0xfb, 0xb3, 0x16, 0x0e, 0x1d, 0xba, 0xc7, 0xf8, 0xe8, 0x0b, 0x95, 0x57, 0x83,
    0x1a, 0x7d, 0xa7, 0xf1, 0x6d, 0xf1, 0xc3, 0xd0, 0x81, 0x96, 0x43, 0xe7, 0xb1, 0x1d, 0x13, 0x52,
    0xfe, 0xbf, 0x64, 0x8c, 0x2a, 0x0c, 0xca, 0x88, 0x6d, 0x74, 0x23, 0xaa, 0x0c, 0xc1, 0x35, 0x07,
    0x04, 0xe7, 0x4c, 0xb0, 0xc9, 0xfe, 0x02, 0x8b, 0x5f, 0xe2, 0x76, 0xa2, 0x25, 0xc0, 0x92, 0xbb,
    0x64, 0x90, 0x33, 0x7b, 0xa3, 0xaa, 0xc7, 0xef, 0x89, 0x4b, 0x3b, 0x60, 0x2b, 0x37, 0x02, 0x8b,
    0x39, 0x0c, 0x10, 0x89, 0x2a, 0xdc, 0x28, 0xa3, 0x3c, 0xfc, 0xd8, 0xe9, 0xf4, 0x05, 0x1b, 0x25,
];
/// Fixed ciphertext vector for the offline hand-built EME-PKCS1v15 encoding
/// (00 02 <fixed nonzero PS> 00 <01..20>) with c = m^e mod n; the private-key path recovers it.
const CT_VEC: [u8; 256] = [
    0x1f, 0x52, 0xb1, 0x65, 0x3d, 0xa7, 0xa0, 0xd0, 0x07, 0xb2, 0x73, 0x2f, 0xdf, 0x57, 0x3b, 0x38,
    0x50, 0x58, 0x1e, 0x73, 0x9d, 0x50, 0x03, 0x8b, 0xcd, 0x61, 0x48, 0x0c, 0xa7, 0x33, 0xa9, 0x41,
    0x2d, 0xf2, 0xf1, 0x07, 0x01, 0x93, 0x9a, 0x97, 0x1c, 0x04, 0x66, 0xea, 0xee, 0xa7, 0x31, 0x21,
    0x53, 0x9b, 0x06, 0xae, 0x5a, 0x54, 0xc0, 0xc9, 0x91, 0x8e, 0xea, 0x77, 0x76, 0xde, 0xce, 0x9d,
    0xaa, 0x09, 0x8f, 0x2d, 0x3c, 0x22, 0xd9, 0x61, 0x0a, 0xf6, 0xb2, 0xff, 0xdf, 0x3f, 0xa3, 0x38,
    0x6e, 0x8a, 0xef, 0x71, 0x79, 0xee, 0x21, 0x8f, 0x96, 0x78, 0xbf, 0x1c, 0x81, 0xca, 0x8d, 0xd0,
    0xda, 0x54, 0x54, 0x0d, 0xd8, 0xfc, 0xf5, 0xf2, 0x00, 0x1e, 0x16, 0xfe, 0x61, 0x4b, 0x22, 0x2a,
    0x29, 0xbd, 0x98, 0x0d, 0x35, 0xea, 0xac, 0xb0, 0x89, 0xfe, 0x6a, 0xf1, 0x7f, 0x1d, 0x16, 0x53,
    0x15, 0xc3, 0x22, 0xf7, 0xff, 0x13, 0xe2, 0xfe, 0xd6, 0xa3, 0x14, 0x5b, 0xda, 0x5b, 0x13, 0x94,
    0x47, 0xbf, 0x7a, 0x60, 0xb0, 0xdc, 0xaf, 0xd3, 0x90, 0x50, 0xf6, 0x1d, 0x88, 0x2e, 0xdb, 0x18,
    0x9c, 0xf0, 0x39, 0xb8, 0xb8, 0x4f, 0xbe, 0xa7, 0x94, 0xf0, 0xd8, 0xc1, 0x55, 0xee, 0xff, 0x16,
    0x54, 0xc2, 0xb8, 0xa4, 0xb4, 0xfc, 0x5c, 0x84, 0xb7, 0x61, 0xe4, 0xcf, 0x84, 0xa4, 0xaf, 0x2f,
    0xa0, 0x2e, 0x17, 0x23, 0xab, 0x95, 0xd5, 0x83, 0x1f, 0x33, 0x00, 0xe4, 0xc1, 0x64, 0x33, 0xc4,
    0x20, 0xaf, 0xf1, 0x72, 0x16, 0xa3, 0xf6, 0xd0, 0x64, 0x18, 0x82, 0x2f, 0xab, 0xaa, 0x41, 0x30,
    0xa6, 0xe4, 0x57, 0x0b, 0x1c, 0x10, 0x1e, 0x34, 0x66, 0x7e, 0xbf, 0x9d, 0x99, 0x1d, 0xc7, 0x5d,
    0xaf, 0x4c, 0xe7, 0xcf, 0xbc, 0xc8, 0x83, 0x09, 0x64, 0xbf, 0xb4, 0x4f, 0x0e, 0x2f, 0x43, 0x91,
];
const E: u64 = 65537;

fn hex(b: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(T[(x >> 4) as usize] as char);
        s.push(T[(x & 0x0f) as usize] as char);
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

fn big(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

/// Seeded byte-stream seed (distinct tags keep each injection point's stream independent).
fn seed(tag: u8) -> [u8; 32] {
    let mut s = [0u8; 32];
    for (i, b) in s.iter_mut().enumerate() {
        *b = tag ^ (i as u8).wrapping_mul(0x9d);
    }
    s
}

/// Hand-written RFC 8439 ChaCha20 block function implementing rand_core 0.6 RngCore+CryptoRng
/// (CryptoRngCore comes from the blanket impl), injected into every rng parameter rsa takes:
/// nonce fixed at "mirvm-rsa-pss", counter from 0, so the output stream is fully deterministic.
struct ChaCha20 {
    state: [u32; 16],
    buf: [u8; 64],
    pos: usize,
}

impl ChaCha20 {
    fn new(seed: [u8; 32]) -> Self {
        let mut state = [0u32; 16];
        state[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
        for i in 0..8 {
            state[4 + i] = u32::from_le_bytes(seed[4 * i..4 * i + 4].try_into().unwrap());
        }
        state[12] = 0;
        let nonce = *b"mirvm-rsapss"; // 12-byte fixed nonce
        for i in 0..3 {
            state[13 + i] = u32::from_le_bytes(nonce[4 * i..4 * i + 4].try_into().unwrap());
        }
        let mut this = Self { state, buf: [0u8; 64], pos: 64 };
        this.refill();
        this
    }

    fn refill(&mut self) {
        let mut w = self.state;
        for _ in 0..10 {
            qr(&mut w, 0, 4, 8, 12);
            qr(&mut w, 1, 5, 9, 13);
            qr(&mut w, 2, 6, 10, 14);
            qr(&mut w, 3, 7, 11, 15);
            qr(&mut w, 0, 5, 10, 15);
            qr(&mut w, 1, 6, 11, 12);
            qr(&mut w, 2, 7, 8, 13);
            qr(&mut w, 3, 4, 9, 14);
        }
        for i in 0..16 {
            w[i] = w[i].wrapping_add(self.state[i]);
            self.buf[4 * i..4 * i + 4].copy_from_slice(&w[i].to_le_bytes());
        }
        self.state[12] = self.state[12].wrapping_add(1);
        self.pos = 0;
    }
}

fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

impl RngCore for ChaCha20 {
    fn next_u32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.fill_bytes(&mut b);
        u32::from_le_bytes(b)
    }
    fn next_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.fill_bytes(&mut b);
        u64::from_le_bytes(b)
    }
    fn fill_bytes(&mut self, mut dest: &mut [u8]) {
        while !dest.is_empty() {
            if self.pos == 64 {
                self.refill();
            }
            let n = (64 - self.pos).min(dest.len());
            dest[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            dest = &mut dest[n..];
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rsa::rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for ChaCha20 {}

fn main() {
    let n = big(&N);
    let e = BigUint::from(E);
    let d = big(&D);
    let p = big(&P);
    let q = big(&Q);
    let one = BigUint::from(1u32);

    // ---- ① component rebuild + traits surface + CRT math checks ----
    let priv_key =
        RsaPrivateKey::from_components(n.clone(), e.clone(), d.clone(), vec![p.clone(), q.clone()])
            .unwrap();
    let pub_key = priv_key.to_public_key();
    println!("key size = {}", priv_key.size());
    println!("pub n = {}", hex(&pub_key.n().to_bytes_be()));
    println!("pub e = {}", pub_key.e());
    println!("priv d len={} fnv={:016x}", d.to_bytes_be().len(), fnv1a(&d.to_bytes_be()));
    println!("priv p fnv={:016x}", fnv1a(&p.to_bytes_be()));
    println!("priv q fnv={:016x}", fnv1a(&q.to_bytes_be()));
    let dp = priv_key.dp().unwrap();
    let dq = priv_key.dq().unwrap();
    println!("dp len={} fnv={:016x}", dp.to_bytes_be().len(), fnv1a(&dp.to_bytes_be()));
    println!("dq len={} fnv={:016x}", dq.to_bytes_be().len(), fnv1a(&dq.to_bytes_be()));
    let pm1 = &p - &one;
    let qm1 = &q - &one;
    println!("e*d  mod (p-1) == 1 = {}", (&e * &d) % &pm1 == one);
    println!("e*dp mod (p-1) == 1 = {}", (&e * dp) % &pm1 == one);
    println!("e*dq mod (q-1) == 1 = {}", (&e * dq) % &qm1 == one);
    let qinv = priv_key.crt_coefficient().unwrap();
    println!("qinv*q mod p == 1 = {}", (&qinv * &q) % &p == one);
    println!(
        "qinv trait == crt_coefficient = {}",
        priv_key.qinv().unwrap().to_biguint().unwrap() == qinv
    );
    let primes = priv_key.primes();
    println!("primes[0]==p primes[1]==q = {}", primes.len() == 2 && primes[0] == p && primes[1] == q);

    // ---- ② rebuild-path variants + error paths ----
    let priv_pq = RsaPrivateKey::from_p_q(p.clone(), q.clone(), e.clone()).unwrap();
    println!("from_p_q d eq = {}", priv_pq.d() == priv_key.d());
    let priv_rec = RsaPrivateKey::from_components(n.clone(), e.clone(), d.clone(), vec![]).unwrap();
    println!("recover primes eq = {}", priv_rec.primes() == priv_key.primes());
    // primes out of order [q, p]: CRT internals differ but RSA math is equivalent, so the signature must match
    let priv_sw =
        RsaPrivateKey::from_components(n.clone(), e.clone(), d.clone(), vec![q.clone(), p.clone()])
            .unwrap();
    let d_probe = Sha256::digest(b"mirvm rsa differential: pkcs1v15 sign vector #1");
    let sig_main = priv_key.sign(Pkcs1v15Sign::new::<Sha256>(), &d_probe).unwrap();
    let sig_sw = priv_sw.sign(Pkcs1v15Sign::new::<Sha256>(), &d_probe).unwrap();
    println!("swapped-primes sig eq = {}", sig_main == sig_sw);
    let bad_n = &n + BigUint::from(2u32);
    match RsaPrivateKey::from_components(bad_n, e.clone(), d.clone(), vec![p.clone(), q.clone()]) {
        Ok(_) => println!("bad-n unexpectedly ok"),
        Err(err) => println!("bad-n err = {err}"),
    }
    match RsaPrivateKey::from_components(n.clone(), e.clone(), d.clone(), vec![p.clone()]) {
        Ok(_) => println!("one-prime unexpectedly ok"),
        Err(err) => println!("one-prime err = {err}"),
    }
    match RsaPublicKey::new(n.clone(), BigUint::from(4u32)) {
        Ok(_) => println!("even-e unexpectedly ok"),
        Err(err) => println!("even-e err = {err}"),
    }

    // ---- ③ public/private DER export + parse roundtrip + tamper ----
    let pkcs1_pub = pub_key.to_pkcs1_der().unwrap();
    println!("pkcs1 pub der len={} fnv={:016x}", pkcs1_pub.as_bytes().len(), fnv1a(pkcs1_pub.as_bytes()));
    let pub_rt = RsaPublicKey::from_pkcs1_der(pkcs1_pub.as_bytes()).unwrap();
    println!("pkcs1 pub roundtrip = {}", pub_rt == pub_key);
    let spki = pub_key.to_public_key_der().unwrap();
    println!("spki der len={} fnv={:016x}", spki.as_bytes().len(), fnv1a(spki.as_bytes()));
    let pub_rt2 = RsaPublicKey::from_public_key_der(spki.as_bytes()).unwrap();
    println!("spki roundtrip = {}", pub_rt2 == pub_key);
    let pkcs8_priv = priv_key.to_pkcs8_der().unwrap();
    println!("pkcs8 priv der len={} fnv={:016x}", pkcs8_priv.as_bytes().len(), fnv1a(pkcs8_priv.as_bytes()));
    let priv_rt = RsaPrivateKey::from_pkcs8_der(pkcs8_priv.as_bytes()).unwrap();
    println!(
        "pkcs8 priv roundtrip = {}",
        priv_rt.n() == priv_key.n() && priv_rt.d() == priv_key.d() && priv_rt.primes() == priv_key.primes()
    );
    let pkcs1_priv = priv_key.to_pkcs1_der().unwrap();
    println!("pkcs1 priv der len={} fnv={:016x}", pkcs1_priv.as_bytes().len(), fnv1a(pkcs1_priv.as_bytes()));
    let priv_rt2 = RsaPrivateKey::from_pkcs1_der(pkcs1_priv.as_bytes()).unwrap();
    println!(
        "pkcs1 priv roundtrip = {}",
        priv_rt2.n() == priv_key.n() && priv_rt2.d() == priv_key.d() && priv_rt2.primes() == priv_key.primes()
    );
    // Break the DER outer SEQUENCE tag (0x30 -> 0xcf): parsing must fail regardless of key content
    let mut bad_der = spki.as_bytes().to_vec();
    bad_der[0] ^= 0xff;
    match RsaPublicKey::from_public_key_der(&bad_der) {
        Ok(_) => println!("spki tamper unexpectedly ok"),
        Err(err) => println!("spki tamper err = {err}"),
    }

    // ---- ④ PKCS1v15 sign/verify vectors (sha2 digests) ----
    let msgs: [&[u8]; 2] = [
        b"mirvm rsa differential: pkcs1v15 sign vector #1",
        b"",
    ];
    for (i, m) in msgs.iter().enumerate() {
        let digest = Sha256::digest(m);
        let sig = priv_key.sign(Pkcs1v15Sign::new::<Sha256>(), &digest).unwrap();
        println!("v15 sig[{i}] = {}", hex(&sig));
        println!("v15 verify[{i}] = {}", pub_key.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &sig).is_ok());
        let wrong = Sha256::digest(b"not the signed message");
        println!("v15 wrong-msg[{i}] = {}", pub_key.verify(Pkcs1v15Sign::new::<Sha256>(), &wrong, &sig).is_ok());
        let mut bad = sig.clone();
        bad[10] ^= 0x01;
        println!("v15 tamper[{i}] = {}", pub_key.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &bad).is_ok());
    }
    let sig_again = priv_key.sign(Pkcs1v15Sign::new::<Sha256>(), &d_probe).unwrap();
    println!("v15 deterministic = {}", sig_again == sig_main);
    let wrong_pub = RsaPublicKey::new(&n + BigUint::from(2u32), e.clone()).unwrap();
    println!("v15 wrong-key = {}", wrong_pub.verify(Pkcs1v15Sign::new::<Sha256>(), &d_probe, &sig_main).is_ok());
    let d384 = Sha384::digest(msgs[0]);
    let sig384 = priv_key.sign(Pkcs1v15Sign::new::<Sha384>(), &d384).unwrap();
    println!("v15 sha384 fnv={:016x} verify={}", fnv1a(&sig384), pub_key.verify(Pkcs1v15Sign::new::<Sha384>(), &d384, &sig384).is_ok());
    let d512 = Sha512::digest(msgs[0]);
    let sig512 = priv_key.sign(Pkcs1v15Sign::new::<Sha512>(), &d512).unwrap();
    println!("v15 sha512 fnv={:016x} verify={}", fnv1a(&sig512), pub_key.verify(Pkcs1v15Sign::new::<Sha512>(), &d512, &sig512).is_ok());
    // new_unprefixed: no DigestInfo prefix and no digest-length limit -- signs 3 raw bytes
    let sig_unp = priv_key.sign(Pkcs1v15Sign::new_unprefixed(), b"\x01\x02\x03").unwrap();
    println!("v15 unprefixed fnv={:016x} verify={}", fnv1a(&sig_unp), pub_key.verify(Pkcs1v15Sign::new_unprefixed(), b"\x01\x02\x03", &sig_unp).is_ok());
    match priv_key.sign(Pkcs1v15Sign::new::<Sha256>(), b"short") {
        Ok(_) => println!("v15 unhashed unexpectedly ok"),
        Err(err) => println!("v15 unhashed err = {err}"),
    }

    // ---- ⑤ encryption vectors + byte-identical re-encryption ----
    let msg_enc: Vec<u8> = (1u8..=32).collect();
    let pt = priv_key.decrypt(Pkcs1v15Encrypt, &CT_VEC).unwrap();
    println!("dec vec ok = {}", pt == msg_enc);
    println!("dec vec pt = {}", hex(&pt));
    let ct2 = pub_key.encrypt(&mut ChaCha20::new(seed(0xe1)), Pkcs1v15Encrypt, &msg_enc).unwrap();
    let ct2b = pub_key.encrypt(&mut ChaCha20::new(seed(0xe1)), Pkcs1v15Encrypt, &msg_enc).unwrap();
    println!("reenc deterministic = {}", ct2 == ct2b);
    println!("reenc ct len={} fnv={:016x}", ct2.len(), fnv1a(&ct2));
    println!("reenc != vec = {}", ct2[..] != CT_VEC[..]);
    let pt2 = priv_key.decrypt(Pkcs1v15Encrypt, &ct2).unwrap();
    println!("reenc roundtrip = {}", pt2 == msg_enc);
    let pt3 = priv_key.decrypt_blinded(&mut ChaCha20::new(seed(0xb1)), Pkcs1v15Encrypt, &CT_VEC).unwrap();
    println!("blinded dec eq = {}", pt3 == pt);
    let mut bad_ct = ct2.clone();
    bad_ct[3] ^= 0x01;
    match priv_key.decrypt(Pkcs1v15Encrypt, &bad_ct) {
        Ok(_) => println!("dec tamper unexpectedly ok"),
        Err(err) => println!("dec tamper err = {err}"),
    }
    let long_msg = vec![0x55u8; 246]; // beyond the k-11 = 245 limit
    match pub_key.encrypt(&mut ChaCha20::new(seed(0xe2)), Pkcs1v15Encrypt, &long_msg) {
        Ok(_) => println!("enc too-long unexpectedly ok"),
        Err(err) => println!("enc too-long err = {err}"),
    }
    let ct_empty = pub_key.encrypt(&mut ChaCha20::new(seed(0xe3)), Pkcs1v15Encrypt, b"").unwrap();
    println!("enc empty roundtrip = {}", priv_key.decrypt(Pkcs1v15Encrypt, &ct_empty).unwrap().is_empty());

    // ---- ⑥ PSS: fixed-seed ChaCha rng injection ----
    let sig_p1 = priv_key.sign_with_rng(&mut ChaCha20::new(seed(0x51)), Pss::new::<Sha256>(), &d_probe).unwrap();
    println!("pss sig1 = {}", hex(&sig_p1));
    let sig_p1b = priv_key.sign_with_rng(&mut ChaCha20::new(seed(0x51)), Pss::new::<Sha256>(), &d_probe).unwrap();
    println!("pss deterministic = {}", sig_p1 == sig_p1b);
    let sig_p2 = priv_key.sign_with_rng(&mut ChaCha20::new(seed(0x52)), Pss::new::<Sha256>(), &d_probe).unwrap();
    println!("pss salt-random = {}", sig_p1 != sig_p2);
    println!("pss verify1 = {}", pub_key.verify(Pss::new::<Sha256>(), &d_probe, &sig_p1).is_ok());
    println!("pss verify2 = {}", pub_key.verify(Pss::new::<Sha256>(), &d_probe, &sig_p2).is_ok());
    let wrong_d = Sha256::digest(b"not the signed message");
    println!("pss wrong-msg = {}", pub_key.verify(Pss::new::<Sha256>(), &wrong_d, &sig_p1).is_ok());
    let mut bad_ps = sig_p1.clone();
    bad_ps[200] ^= 0x80;
    println!("pss tamper = {}", pub_key.verify(Pss::new::<Sha256>(), &d_probe, &bad_ps).is_ok());
    println!("pss saltlen-mismatch = {}", pub_key.verify(Pss::new_with_salt::<Sha256>(20), &d_probe, &sig_p1).is_ok());
    // Zero salt: PSS becomes fully deterministic, so different seeds must still match byte for byte
    let z1 = priv_key.sign_with_rng(&mut ChaCha20::new(seed(0x51)), Pss::new_with_salt::<Sha256>(0), &d_probe).unwrap();
    let z2 = priv_key.sign_with_rng(&mut ChaCha20::new(seed(0x99)), Pss::new_with_salt::<Sha256>(0), &d_probe).unwrap();
    println!("pss zero-salt seed-independent = {}", z1 == z2);
    println!("pss zero-salt verify = {}", pub_key.verify(Pss::new_with_salt::<Sha256>(0), &d_probe, &z1).is_ok());
    let d384b = Sha384::digest(msgs[0]);
    let s384 = priv_key.sign_with_rng(&mut ChaCha20::new(seed(0x51)), Pss::new::<Sha384>(), &d384b).unwrap();
    println!("pss sha384 fnv={:016x} verify={}", fnv1a(&s384), pub_key.verify(Pss::new::<Sha384>(), &d384b, &s384).is_ok());
    // Pss with no rng -> InvalidPaddingScheme (the counter-proof that the injection point exists)
    match priv_key.sign(Pss::new::<Sha256>(), &d_probe) {
        Ok(_) => println!("pss no-rng unexpectedly ok"),
        Err(err) => println!("pss no-rng err = {err}"),
    }
    // pss::SigningKey/VerifyingKey: the same injection channel through the signature traits
    let sk = rsa::pss::SigningKey::<Sha256>::new_with_salt_len(priv_key.clone(), 16);
    let sig_t: rsa::pss::Signature = sk.sign_with_rng(&mut ChaCha20::new(seed(0x53)), msgs[0]);
    println!("pss trait sig = {}", hex(&sig_t.to_bytes()));
    let vk = rsa::pss::VerifyingKey::<Sha256>::new_with_salt_len(pub_key.clone(), 16);
    println!("pss trait verify = {}", vk.verify(msgs[0], &sig_t).is_ok());
    println!("pss trait wrong-msg = {}", vk.verify(b"not the signed message", &sig_t).is_ok());

    // ---- ⑦ OAEP ----
    let ct_o = pub_key.encrypt(&mut ChaCha20::new(seed(0xa1)), Oaep::new::<Sha256>(), &msg_enc).unwrap();
    let ct_ob = pub_key.encrypt(&mut ChaCha20::new(seed(0xa1)), Oaep::new::<Sha256>(), &msg_enc).unwrap();
    println!("oaep deterministic = {}", ct_o == ct_ob);
    println!("oaep ct len={} fnv={:016x}", ct_o.len(), fnv1a(&ct_o));
    let pt_o = priv_key.decrypt(Oaep::new::<Sha256>(), &ct_o).unwrap();
    println!("oaep roundtrip = {}", pt_o == msg_enc);
    let ct_l = pub_key.encrypt(&mut ChaCha20::new(seed(0xa2)), Oaep::new_with_label::<Sha256, _>("mirvm-label"), b"labelled secret").unwrap();
    let pt_l = priv_key.decrypt(Oaep::new_with_label::<Sha256, _>("mirvm-label"), &ct_l).unwrap();
    println!("oaep label roundtrip = {}", pt_l == b"labelled secret");
    match priv_key.decrypt(Oaep::new_with_label::<Sha256, _>("other-label"), &ct_l) {
        Ok(_) => println!("oaep wrong-label unexpectedly ok"),
        Err(err) => println!("oaep wrong-label err = {err}"),
    }
    let mut bad_o = ct_o.clone();
    bad_o[100] ^= 0x02;
    match priv_key.decrypt(Oaep::new::<Sha256>(), &bad_o) {
        Ok(_) => println!("oaep tamper unexpectedly ok"),
        Err(err) => println!("oaep tamper err = {err}"),
    }
}
