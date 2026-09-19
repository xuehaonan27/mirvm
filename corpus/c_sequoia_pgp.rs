#!/usr/bin/env mirvm
---
[dependencies]
# sequoia-openpgp 2.4.1 (latest stable on crates.io at 2026-07-17; 2.2.0-pqc.1 is
# a prerelease, skipped). Large pure-Rust crate (171 src files, ~131k lines).
# default-features=false drops compression (deflate+bzip2; bzip2 is the C library
# libbz2) and crypto-nettle: the nettle path is not viable here -- only the
# libnettle.so.8 runtime is installed, no dev package (pkg-config nettle missing),
# and nettle-sys needs bindgen→libclang with clang missing. crypto-rust is the
# only pure-Rust backend, but its build.rs has three official gates that must be
# opted into in order: crypto-rust itself + allow-experimental-crypto
# (experimental backend informed consent) + allow-variable-time-crypto
# (variable-time crypto informed consent).
# The resolved closure is 238 crates (actual scaffold Cargo.lock count): the full
# 2.4 algorithm surface (aes/rsa(num-bigint-dig)/dalek/p256-384-521/ml-dsa/
# ml-kem/slh-dsa) compiles in, while at runtime only Ed25519+SHA512 is exercised.
sequoia-openpgp = { version = "=2.4.1", default-features = false, features = [
    "crypto-rust",
    "allow-experimental-crypto",
    "allow-variable-time-crypto",
] }
---
// sequoia-openpgp 2.4.1 (full OpenPGP RFC9580 implementation) differential driver.
// Covers cert parsing, detached signing/verification with a fixed private key, and
// the unknown-algorithm error surface.
//
// Embedded material (generated once offline, then pinned as constants):
// CertBuilder::new().add_userid("Mirvm Corpus <corpus@mirvm.invalid>")
//   .set_cipher_suite(Cv25519).set_creation_time(1735689600 = 2025-01-01)
//   .add_signing_subkey().generate() (no password), exporting cert/TSK armor
// (the armor carries Comment headers = fingerprint + userid; the text itself is
// the constant).
//
// Test surface:
//  (1) ASCII-armor dearmor (armor::Reader) → kind/rawlen/fnv;
//  (2) Cert field surface: cert fingerprint, primary key algorithm/creation time,
//      userid, signing subkey fingerprint/algorithm/creation time (raw Cert, no
//      policy iteration, fixed order);
//  (3) packet-layer type counts: PacketParser walks packet by packet, (tag number,
//      tag name) into a BTreeMap printed in order (PublicKey/UserID/Signature×3/
//      PublicSubkey);
//  (4) fixed private key detached signature: unpack the TSK → policy chain
//      (supported/alive/not-revoked/for_signing) selects the signing subkey →
//      hand-built v4 signature (see the salt pitfall below) → sig len/fnv anchor +
//      re-parsed fields (version/typ/hash/created/issuer_fps/issuers);
//  (5) DetachedVerifier streaming verification (StandardPolicy + self-supplied cert
//      helper), positive and negative: original layer0:good; flipping one message
//      byte layer0:bad:Bad signature;
//  (6) unknown-algorithm error surface, three samples, all injected by flipping bits
//      in the constant byte streams: E1 signature packet hash_algo→99 →
//      construction fails with "Unsupported hash algorithm: Unknown hash algorithm
//      99" (genuinely unknown algorithm); E2 signature packet pk_algo→18 (ElGamal)
//      → verification semantics error "Malformed signature: ... not a signature
//      algorithm" (valid algorithm but not a signing one); E3 TSK primary key
//      pk_algo→101 (private/experimental range) → Cert packaging rejects it with
//      "Unsupported Cert: Unsupported primary key: ...".
//      Note: the public-key packet algorithm field is tolerated during sequoia parse
//      (stored opaque, CERT-OK); the error surfaces only at the use/packaging layer,
//      and the driver records this contrast in the (6) output.
// Signature byte determinism [salt pitfall workaround]: for v4 keys sequoia injects
// a random 32-byte notation `salt@notations.sequoia-pgp.org` into the hashed area
// with no public opt-out (streaming Signer and SignerBuilder::sign_message/sign_hash
// both go through pre_sign; set_notation with the same name deletes then adds, so a
// pre-seeded fixed value is overwritten by the later salt; two consecutive runs
// really do give different sig fnv). The workaround hand-builds the RFC4880 v4 hash
// (msg || [04,typ,pk,hash,area_len,area] || [04,FF,len32]) and assembles a salt-free
// v4 detached signature via crypto::Signer::sign plus SubpacketArea/Signature4,
// byte-stable across runs. This is inherent crate behaviour (upstream deliberately
// non-reproducible), not an engine boundary.
// Determinism: no IO/time/env/rand reaches the output (the only rand consumption is
// at generation time, fixed offline); BTreeMap ordering; unix-secs times; sequoia's
// static error text; stderr empty; key constants anchored with assert_eq!.
use std::collections::BTreeMap;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use sequoia_openpgp as openpgp;
use openpgp::armor::{Kind as ArmorKind, Reader as ArmorReader, ReaderMode};
use openpgp::crypto::Signer as CryptoSigner;
use openpgp::packet::signature::subpacket::{Subpacket, SubpacketArea, SubpacketValue};
use openpgp::parse::stream::{
    DetachedVerifierBuilder, MessageLayer, MessageStructure, VerificationHelper,
};
use openpgp::parse::{Parse, PacketParserBuilder, PacketParserResult};
use openpgp::policy::StandardPolicy;
use openpgp::serialize::{Marshal, MarshalInto};
use openpgp::types::{HashAlgorithm, SignatureType, Timestamp};
use openpgp::{Cert, KeyHandle, Packet};

/// Embedded material ①: public certificate (Cv25519 + Ed25519 subkey, v4, fixed creation time).
const CERT_ARMOR: &str = r####"-----BEGIN PGP PUBLIC KEY BLOCK-----
Comment: 56C1 9B54 2C05 030E 891B  37B7 58EB CB97 123D 8C84
Comment: Mirvm Corpus <corpus@mirvm.invalid>

xjMEZ3SFgBYJKwYBBAHaRw8BAQdAjX8PYFqeu+TQc+PfdJgWtDjJYnw9ig6ubVbX
5OpsK0PCwAsEHxYKAH0Fgmd0hYADCwkHCRBY68uXEj2MhEcUAAAAAAAeACBzYWx0
QG5vdGF0aW9ucy5zZXF1b2lhLXBncC5vcmfs3hfLAkZQflDJ4W1+mZ4wRR1OIVgO
epQRUa4xmfLVgQMVCggCmwECHgkWIQRWwZtULAUDDokbN7dY68uXEj2MhAAABkEA
/3+mrGrKZrcuAnZX54ozHC3yfoBduXhQ4v9WxeN4FRTPAQC/5wXLS/5hs3TDryjt
TGLZguI5+ug7X4YnFrOv1uDcB80jTWlydm0gQ29ycHVzIDxjb3JwdXNAbWlydm0u
aW52YWxpZD7CwA4EExYKAIAFgmd0hYADCwkHCRBY68uXEj2MhEcUAAAAAAAeACBz
YWx0QG5vdGF0aW9ucy5zZXF1b2lhLXBncC5vcmcfBhut+zOeqPn+HwoO9edK/qXm
cchVMQATwAu+pYylJwMVCggCmQECmwECHgkWIQRWwZtULAUDDokbN7dY68uXEj2M
hAAAY5oA/1sP2Xd74rMvmBktqobTcXCABIHmhu77GkDpW/EuWPWPAQD/TzAnnN2D
lBWQ+ZXRld5Z+fQm2wyC/B4/OL5+YkL4Ac4zBGd0hYAWCSsGAQQB2kcPAQEHQP5v
mX1JsYoUGHX30gEsPvASLoKIxITigX7C0MQ2kbw5wsC/BBgWCgExBYJndIWACRBY
68uXEj2MhEcUAAAAAAAeACBzYWx0QG5vdGF0aW9ucy5zZXF1b2lhLXBncC5vcmeX
ovE6J5f6erJfJh7a4N1jyD2HFcqUcSWk61my927afwKbAr6gBBkWCgBvBYJndIWA
CRBcKfgesB5svkcUAAAAAAAeACBzYWx0QG5vdGF0aW9ucy5zZXF1b2lhLXBncC5v
cmehttfxzGf9QYPnLcQ+waYSA0Bt/SuXjs+ixg4KM+ft1hYhBGj7rNx2zBxQyT43
w1wp+B6wHmy+AAAkDAEAgDYI8YF7MG0/4u5D6KuUO5cnJ5weK3cD7DJV1jZ5Y5wB
APQzIdUiGhdQKtfU1lmlYvHg6O0f1I2nALY2Y9YAkpUIFiEEVsGbVCwFAw6JGze3
WOvLlxI9jIQAAGhTAP9pQkBL7S9fC4BpGPxV9bItgZr8nROXJS/pLH3+IGxZ3AD/
R6MpwP+UyIphYC707mT9iIiL2HAEtS/SZp3c6WteswU=
=q5QO
-----END PGP PUBLIC KEY BLOCK-----
"####;

/// Embedded material ②: private key half (unencrypted Ed25519 scalar; one-off test material).
const TSK_ARMOR: &str = r####"-----BEGIN PGP PRIVATE KEY BLOCK-----
Comment: 56C1 9B54 2C05 030E 891B  37B7 58EB CB97 123D 8C84
Comment: Mirvm Corpus <corpus@mirvm.invalid>

xVgEZ3SFgBYJKwYBBAHaRw8BAQdAjX8PYFqeu+TQc+PfdJgWtDjJYnw9ig6ubVbX
5OpsK0MAAP4skPQJJoK2CZg5mPzgLvoqO6Nv1iH5fowzwP+6OwHryRGYwsALBB8W
CgB9BYJndIWAAwsJBwkQWOvLlxI9jIRHFAAAAAAAHgAgc2FsdEBub3RhdGlvbnMu
c2VxdW9pYS1wZ3Aub3Jn7N4XywJGUH5QyeFtfpmeMEUdTiFYDnqUEVGuMZny1YED
FQoIApsBAh4JFiEEVsGbVCwFAw6JGze3WOvLlxI9jIQAAAZBAP9/pqxqyma3LgJ2
V+eKMxwt8n6AXbl4UOL/VsXjeBUUzwEAv+cFy0v+YbN0w68o7Uxi2YLiOfroO1+G
Jxazr9bg3AfNI01pcnZtIENvcnB1cyA8Y29ycHVzQG1pcnZtLmludmFsaWQ+wsAO
BBMWCgCABYJndIWAAwsJBwkQWOvLlxI9jIRHFAAAAAAAHgAgc2FsdEBub3RhdGlv
bnMuc2VxdW9pYS1wZ3Aub3JnHwYbrfsznqj5/h8KDvXnSv6l5nHIVTEAE8ALvqWM
pScDFQoIApkBApsBAh4JFiEEVsGbVCwFAw6JGze3WOvLlxI9jIQAAGOaAP9bD9l3
e+KzL5gZLaqG03FwgASB5obu+xpA6VvxLlj1jwEA/08wJ5zdg5QVkPmV0ZXeWfn0
JtsMgvwePzi+fmJC+AHHWARndIWAFgkrBgEEAdpHDwEBB0D+b5l9SbGKFBh199IB
LD7wEi6CiMSE4oF+wtDENpG8OQAA/iSAjHJeCwtMUCllHeMRoYdHAYrM39k2CbKh
IlzaunrYDsPCwL8EGBYKATEFgmd0hYAJEFjry5cSPYyERxQAAAAAAB4AIHNhbHRA
bm90YXRpb25zLnNlcXVvaWEtcGdwLm9yZ5ei8Tonl/p6sl8mHtrg3WPIPYcVypRx
JaTrWbL3btp/ApsCvqAEGRYKAG8Fgmd0hYAJEFwp+B6wHmy+RxQAAAAAAB4AIHNh
bHRAbm90YXRpb25zLnNlcXVvaWEtcGdwLm9yZ6G21/HMZ/1Bg+ctxD7BphIDQG39
K5eOz6LGDgoz5+3WFiEEaPus3HbMHFDJPjfDXCn4HrAebL4AACQMAQCANgjxgXsw
bT/i7kPoq5Q7lycnnB4rdwPsMlXWNnljnAEA9DMh1SIaF1Aq19TWWaVi8eDo7R/U
jacAtjZj1gCSlQgWIQRWwZtULAUDDokbN7dY68uXEj2MhAAAaFMA/2lCQEvtL18L
gGkY/FX1si2BmvydE5clL+ksff4gbFncAP9HoynA/5TIimFgLvTuZP2IiIvYcAS1
L9Jmndzpa16zBQ==
=AwUr
-----END PGP PRIVATE KEY BLOCK-----
"####;

const MSG: &[u8] = b"mirvm sequoia-openpgp differential sample, batch-8 wave-1.\n";
/// Signature creation time pin (2025-01-02, later than the key creation time 2025-01-01).
const SIG_TS: u32 = 1_735_780_000;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn dearmor(armored: &str) -> (Vec<u8>, ArmorKind) {
    let mut r = ArmorReader::from_bytes(armored.as_bytes(), ReaderMode::Tolerant(None));
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    (buf, r.kind().unwrap())
}

struct Helper {
    cert: Cert,
    verdicts: Vec<String>,
}

impl VerificationHelper for Helper {
    fn get_certs(&mut self, _ids: &[KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        Ok(vec![self.cert.clone()])
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        for (i, layer) in structure.into_iter().enumerate() {
            match layer {
                MessageLayer::SignatureGroup { results } => {
                    for r in results {
                        match r {
                            Ok(_) => self.verdicts.push(format!("layer{i}:good")),
                            Err(e) => self.verdicts.push(format!("layer{i}:bad:{e}")),
                        }
                    }
                }
                MessageLayer::Compression { .. } => {
                    self.verdicts.push(format!("layer{i}:compression"))
                }
                MessageLayer::Encryption { .. } => {
                    self.verdicts.push(format!("layer{i}:encryption"))
                }
            }
        }
        Ok(())
    }
}

/// Detached verification; returns (per-layer verdict lines, whether streaming completed cleanly).
fn verify_detached(p: &StandardPolicy, cert: &Cert, sig: &[u8], msg: &[u8]) -> (Vec<String>, bool) {
    let helper = Helper { cert: cert.clone(), verdicts: Vec::new() };
    let res = DetachedVerifierBuilder::from_bytes(sig)
        .and_then(|b| b.with_policy(p, None, helper))
        .and_then(|mut v| v.verify_bytes(msg).map(|_| v));
    match res {
        Ok(v) => (v.into_helper().verdicts, true),
        Err(e) => (vec![format!("err:{e}")], false),
    }
}

fn main() -> openpgp::Result<()> {
    let policy = &StandardPolicy::new();

    // ---- ① armor → binary ----
    let (cert_raw, ckind) = dearmor(CERT_ARMOR);
    println!("cert armor kind={ckind:?} rawlen={} fnv={:016x}", cert_raw.len(), fnv1a(&cert_raw));
    let (tsk_raw, tkind) = dearmor(TSK_ARMOR);
    println!("tsk armor kind={tkind:?} rawlen={} fnv={:016x}", tsk_raw.len(), fnv1a(&tsk_raw));
    assert_eq!(cert_raw.len(), 944);
    assert_eq!(fnv1a(&cert_raw), 0xac8392a95c7a522d);
    assert_eq!(fnv1a(&tsk_raw), 0xddc615bf946d6d5c);

    // ---- ② cert field surface ----
    let cert = Cert::from_bytes(&cert_raw)?;
    let cert_fp = cert.fingerprint().to_string();
    println!("cert fingerprint={cert_fp}");
    assert_eq!(cert_fp, "56C19B542C05030E891B37B758EBCB97123D8C84");
    let pk = cert.primary_key().key();
    println!("primary algo={:?} created={}", pk.pk_algo(), unix(pk.creation_time()));
    for (i, u) in cert.userids().enumerate() {
        println!("userid[{i}]={}", String::from_utf8_lossy(u.userid().value()));
    }
    for (i, ka) in cert.keys().subkeys().enumerate() {
        println!(
            "subkey[{i}] fp={} algo={:?} created={}",
            ka.key().fingerprint(),
            ka.key().pk_algo(),
            unix(ka.key().creation_time())
        );
    }

    // ---- ③ packet-layer type counts ----
    let mut ppo: PacketParserResult = PacketParserBuilder::from_bytes(&cert_raw)?.build()?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut total = 0usize;
    while let PacketParserResult::Some(pp) = ppo {
        let (packet, next) = pp.recurse()?;
        let tag = packet.tag();
        *counts.entry(format!("{:02} {:?}", u8::from(tag), tag)).or_default() += 1;
        total += 1;
        ppo = next;
    }
    println!("packets total={total} kinds={}", counts.len());
    for (k, n) in &counts {
        println!("pkttag {k} = {n}");
    }
    assert_eq!(total, 6);
    assert_eq!(counts.len(), 4);

    // ---- ④ fixed private key, hand-built v4 detached signature (no salt, see header) ----
    let tsk = Cert::from_bytes(&tsk_raw)?;
    let mut kp = tsk
        .keys()
        .unencrypted_secret()
        .with_policy(policy, None)
        .supported()
        .alive()
        .revoked(false)
        .for_signing()
        .next()
        .expect("signing subkey")
        .key()
        .clone()
        .into_keypair()?;
    let hashed_area = SubpacketArea::new(vec![
        Subpacket::new(
            SubpacketValue::SignatureCreationTime(Timestamp::from(SIG_TS)),
            true,
        )?,
        Subpacket::new(
            SubpacketValue::IssuerFingerprint(kp.public().fingerprint()),
            true,
        )?,
    ])?;
    let unhashed_area = SubpacketArea::new(vec![Subpacket::new(
        SubpacketValue::Issuer(kp.public().keyid()),
        false,
    )?])?;
    let mut hctx = HashAlgorithm::SHA512.context()?.for_signature(4);
    hctx.update(MSG);
    let halen = hashed_area.serialized_len();
    let mut header = [0u8; 6];
    header[0] = 4; // v4
    header[1] = u8::from(SignatureType::Binary);
    header[2] = u8::from(kp.public().pk_algo());
    header[3] = u8::from(HashAlgorithm::SHA512);
    header[4..6].copy_from_slice(&(halen as u16).to_be_bytes());
    hctx.update(&header);
    hashed_area.serialize(&mut hctx)?;
    hctx.update(&[4, 0xff]);
    hctx.update(&((6 + halen) as u32).to_be_bytes());
    let mut digest = vec![0u8; hctx.digest_size()];
    hctx.digest(&mut digest)?;
    let mpis = kp.sign(HashAlgorithm::SHA512, &digest)?;
    let sig4 = openpgp::packet::signature::Signature4::new(
        SignatureType::Binary,
        kp.public().pk_algo(),
        HashAlgorithm::SHA512,
        hashed_area,
        unhashed_area,
        [digest[0], digest[1]],
        mpis,
    );
    let sig: openpgp::packet::Signature = sig4.into();
    let pkt = Packet::from(sig.clone());
    let sig_bytes = pkt.to_vec()?;
    println!("sig len={} fnv={:016x}", sig_bytes.len(), fnv1a(&sig_bytes));
    assert_eq!(sig_bytes.len(), 119);
    assert_eq!(fnv1a(&sig_bytes), 0x290d1f34413dd221);
    println!(
        "sig version={} typ={:?} hash={:?} created={:?}",
        sig.version(),
        sig.typ(),
        sig.hash_algo(),
        sig.signature_creation_time().map(unix)
    );
    let fps: Vec<String> = sig.issuer_fingerprints().map(|f| f.to_string()).collect();
    let ids: Vec<String> = sig.get_issuers().iter().map(|h| h.to_string()).collect();
    println!("sig issuer_fps={fps:?}");
    println!("sig issuers={ids:?}");

    // ---- ⑤ streaming verification, positive and negative ----
    let (verdicts, ok) = verify_detached(policy, &cert, &sig_bytes, MSG);
    println!("verify-ok run-ok={ok} verdicts={verdicts:?}");
    assert_eq!(verdicts, ["layer0:good"]);
    let mut tampered = MSG.to_vec();
    tampered[10] ^= 0x01;
    let (verdicts2, ok2) = verify_detached(policy, &cert, &sig_bytes, &tampered);
    println!("verify-tampered run-ok={ok2} verdicts={verdicts2:?}");
    assert_eq!(verdicts2, ["layer0:bad:Bad signature: Message has been manipulated"]);

    // ---- ⑥ unknown-algorithm error surface ----
    // Contrast: the public-key packet algo field is tolerated at parse (opaque, fingerprint known).
    let mut mut_cert = cert_raw.clone();
    mut_cert[7] = 101; // first packet v4 primary key algo byte (ctb + len + ver + ctime → off7)
    let mc = Cert::from_bytes(&mut_cert)?;
    let mc_fp = mc.fingerprint().to_string();
    println!("mut-cert algo=101 parse=ok fp={mc_fp}");
    assert_eq!(mc_fp, "BE734E363361962465D1247B2795480BD5F9FF91");

    // E1: signature packet hash_algo→99, rejected at construction.
    let mut s1 = sig_bytes.clone();
    s1[5] = 99; // signature packet body: 1B ver+typ+pk_algo+hash_algo (2B header → offset 5)
    let (v1, ok1) = verify_detached(policy, &cert, &s1, MSG);
    println!("unk-sig-halgo run-ok={ok1} verdicts={v1:?}");
    assert_eq!(v1, ["err:Unsupported hash algorithm: Unknown hash algorithm 99"]);

    // E2: signature packet pk_algo→18 (ElGamal; a valid algorithm but not a signing one).
    let mut s2 = sig_bytes.clone();
    s2[4] = 18;
    let (v2, ok2) = verify_detached(policy, &cert, &s2, MSG);
    println!("unk-sig-pkalgo run-ok={ok2} verdicts={v2:?}");
    assert_eq!(
        v2,
        ["layer0:bad:Malformed signature: Malformed packet: not a signature algorithm"]
    );

    // E3: TSK primary key algo→101, rejected at the secret-key packaging layer.
    let mut mtsk = tsk_raw.clone();
    mtsk[7] = 101;
    match Cert::from_bytes(&mtsk) {
        Ok(_) => println!("unk-tsk unexpectedly ok"),
        Err(e) => println!("unk-tsk-cert err = {e}"),
    }

    Ok(())
}
