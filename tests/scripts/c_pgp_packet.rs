#!/usr/bin/env mirvm
---
[dependencies]
pgp = "0.14"
---
// pgp 0.14 (rpgp) OpenPGP packet parse/build differential (no signing, no encryption).
// Material: the crate's own tests directory, with two armored samples embedded --
//   1) the armor block from tests/openpgp/pgp263-test.pub.asc (pgp2.6.3 RSA-888 old
//      public key, the V2/V3-era legacy packet format path);
//   2) the full text of tests/openpgp/samplemsgs/sig-1-key-1.asc (GnuPG v2 style
//      standalone signature packet, the Issuer/IssuerFingerprint subpacket path).
// Coverage: SignedPublicKey / StandaloneSignature from_armor_single field surface
// (version/PublicKeyAlgorithm/fingerprint hex/KeyID/creation timestamp/expiration
// interval, RSA modulus bits, e, userid, direct/revocation signatures and subpacket
// counts, per-subpacket classification + partial payloads); armor roundtrip
// (to_armored_string re-parsed for PartialEq equality + byte fnv anchor); binary
// roundtrip (ser::Serialize::to_bytes -> from_bytes -> equality); is_binary sniff
// branch of from_reader_single (headers=None); four error paths: bad armor header
// (no BEGIN marker), sig block as public key, junk binary, truncated bytes (from_bytes).
//
// Known-debt workaround (same semantic class): armor body (base64/CRC/footer) layer
// errors are always wrapped by `Dearmor::read` as `io::Error::new(Other, msg)`,
// then travel via `Error::IOError`'s `{source:?}` or io::Error's Display up the
// `Box<dyn Error+Send+Sync>` -> `dyn Debug/Display` upcast, which TRAPs during
// mirvm lowering: an invalid base64 body blows up inside from_armor_single while
// it builds the error string (observed with the first version of this driver;
// diagnostic from the lowering's `dyn upcast ...` rejection family). So use armor-header-layer
// error inputs: a header parse error goes through bail! before the dearmor
// stream wrapper and becomes `Error::Message(String)`, whose Display/Debug
// chain never touches dyn -- native can run it, mirvm traps.
// Determinism: armor Headers is a BTreeMap (key order kept); times are chrono
// .timestamp() seconds; fingerprints/KeyIDs/sig-prefixes/key bits are hand-written
// hex; errors are fixed Display strings; no addr/thread-id/HashMap order; stderr empty.
use pgp::armor::Headers;
use pgp::composed::{ArmorOptions, Deserializable, SignedPublicKey, StandaloneSignature};
use pgp::packet::SubpacketData;
use pgp::packet::Signature;
use pgp::ser::Serialize;
use pgp::types::{Mpi, PublicKeyTrait, PublicParams};

/// Material (1) armor block (the stats header line outside the armor is omitted; not armor data).
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

/// Material (2) full standalone signature block.
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

/// Hand-written lowercase hex, replacing the hex crate.
fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// MPI effective bit length (RSA modulus bits, etc.).
fn mpi_bits(m: &Mpi) -> usize {
    let b = m.as_bytes();
    match b.first() {
        None => 0,
        Some(&f) => (b.len() - 1) * 8 + (8 - f.leading_zeros() as usize),
    }
}

/// Coarse public-key params: RSA modulus bits + e; ECC curve name; other shapes.
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

/// PublicKeyTrait field surface (shared by primary key packet / subkey packet).
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

/// Coarse subpacket class name + common payload (deterministic fields).
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

/// Signature packet field surface + per-subpacket dump.
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

/// Armor headers (BTreeMap key order is deterministic).
fn dump_headers(h: &Headers) {
    println!("headers n={}", h.len());
    for (k, vs) in h {
        println!("  hdr {k} = {vs:?}");
    }
}

/// SignedPublicKey full field surface.
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
    // ---- ① public key armor parse + field surface ----
    let (key, kh) = SignedPublicKey::from_armor_single(PUB_KEY.as_bytes()).unwrap();
    println!("== signed public key ==");
    dump_headers(&kh);
    dump_signed_key(&key);

    // ---- ② public key armor roundtrip ----
    let a1 = key.to_armored_string(ArmorOptions::default()).unwrap();
    println!("key armor len={} fnv={:016x}", a1.len(), fnv1a(a1.as_bytes()));
    let (key2, kh2) = SignedPublicKey::from_string(&a1).unwrap();
    println!("key armor roundtrip={} re-headers={}", key == key2, kh2.len());
    let a2 = key2.to_armored_string(ArmorOptions::default()).unwrap();
    println!("key armor stable={}", a1 == a2);

    // ---- ③ public key binary roundtrip + from_reader sniffing branch ----
    let kbin = key.to_bytes().unwrap();
    println!("key bin len={} fnv={:016x}", kbin.len(), fnv1a(&kbin));
    let key3 = SignedPublicKey::from_bytes(&kbin[..]).unwrap();
    println!("key bin roundtrip={}", key == key3);
    let (key4, kh4) = SignedPublicKey::from_reader_single(&kbin[..]).unwrap();
    println!("key reader roundtrip={} headers-is-none={}", key == key4, kh4.is_none());

    // ---- ④ standalone signature packet parse + field surface ----
    let (ssig, sh) = StandaloneSignature::from_armor_single(SIG.as_bytes()).unwrap();
    println!("== standalone signature ==");
    dump_headers(&sh);
    dump_sig("sig", &ssig.signature);

    // ---- ⑤ signature armor / binary roundtrip ----
    let s1 = ssig.to_armored_string(ArmorOptions::default()).unwrap();
    println!("sig armor len={} fnv={:016x}", s1.len(), fnv1a(s1.as_bytes()));
    let (ssig2, _) = StandaloneSignature::from_armor_single(s1.as_bytes()).unwrap();
    println!("sig armor roundtrip={}", ssig == ssig2);
    let sbin = ssig.to_bytes().unwrap();
    println!("sig bin len={} fnv={:016x}", sbin.len(), fnv1a(&sbin));
    let ssig3 = StandaloneSignature::from_bytes(&sbin[..]).unwrap();
    println!("sig bin roundtrip={}", ssig == ssig3);

    // ---- ⑥ error paths (all avoid the io-Custom wrapper family; see header note) ----
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
