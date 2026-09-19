#!/usr/bin/env mirvm
---
[dependencies]
openssl = "0.10"
# Used only for the explicit dlopen preload in the workaround documented below; no other API is touched.
libc = "0.2"
---
// openssl 0.10 differential (heavy FFI over the system libssl/libcrypto; pkg-config
// --modversion openssl reports 3.0.2 and /usr/include/openssl is complete). openssl-sys
// links dynamically (-l ssl -l crypto), and no guest callback ever enters libcrypto (so the
// thunk blind spot is never touched). Rng::bytes is non-deterministic and unused.
//
// Known limitation: two failures with one root cause -- rlib metadata does not propagate -l to the final binary.
//   ① A bare run traps on the first foreign call (exit 70): mirvm[m4-engine]: foreign
//     `OpenSSL_version` symbol not found (neither the archive fallback table nor a full-domain dlsym matched; fn <caller>).
//   Root cause: cargo (nightly-2026-07-02) passes the build script's
//   `cargo:rustc-link-lib=ssl/crypto` only to the compilation of openssl-sys' own rlib
//   (recorded in the rlib metadata's native_libraries); the final bin's rustc command line
//   carries no `-l`/`-L native` (a native build does not care, because rustc adds the flags
//   from the rlib metadata at link time -- verified against `cargo build -vv`). mirvm's
//   dylib dlopen candidates, however, are collected only from `sess.opts.libs` (the `-l`
//   loop in src/lower/mod.rs), so module.native_libs stays empty, libssl/libcrypto never
//   enter the global namespace and a global dlsym misses. The static-archive path
//   (native_archive.rs) reads tcx.native_libraries and is unaffected.
//   Workaround: the guest main explicitly preloads `libc::dlopen(libcrypto.so.3 /
//   libssl.so.3, RTLD_NOW|RTLD_GLOBAL)`. That loads the same host and system library
//   version the native linker's DT_NEEDED would, so under native it is an idempotent no-op
//   and the semantics and output stay byte-identical.
//   ② With the preload in place the next call traps (exit 70) and there is no driver-level
//   workaround: mirvm[m4-engine]: TRAP: extern fn `EVP_EncryptInit_ex` taken as value address,
//   but symbol not found (neither archive fallback table nor global dlsym).
//   openssl 0.10.81 src/cipher_ctx.rs:133 passes `ffi::EVP_EncryptInit_ex` /
//   `EVP_DecryptInit_ex` as *values* to a generic cipher_init (whose parameter is an
//   `unsafe extern "C" fn`), so resolving a value-taken extern-fn pointer happens at
//   lowering time (foreign_fn_entry in src/lower/mod.rs, baked through dlsym(RTLD_DEFAULT))
//   -- when libcrypto is not yet in the mirvm process, because the guest preload is runtime
//   code. The baked TRAP aborts at the first cipher_init, and every EVP cipher path
//   (Crypter/encrypt/decrypt) goes through it, so this section is always red under mirvm.
//   The fix belongs on the mirvm side: lowering should collect the Dylib/RawDylib entries of
//   used_crates' tcx.native_libraries (plus `-L native` and `sess.opts.libs`) and preload
//   them with RTLD_GLOBAL before draining the worklist, exactly as required_native_libs does.
//
// Coverage (① ② ③ already run their differential under mirvm; ④ is green on native and
// aborts loudly under mirvm at its first cipher_init -- see limitation ② above):
// ① Six fixed EVP digest vectors against embedded known answers (sha256 x3 / sha512 / sha1
//    / md5, cross-checked with the host openssl CLI -- md5("abc") ends in e17f72, not
//    e17e72), plus the three openssl::sha one-shots, Hasher streaming with a split (and an
//    empty update), from_name hit/reject and size();
// ② RSA-2048 imported from fixed DER (never self-generated): private_key_from_der
//    (PKCS#1) / public_key_from_der_pkcs1 (PKCS#1) / Rsa::public_key_from_der (SPKI) /
//    PKey::public_key_from_der (SPKI) / from_public_components(BigNum) -- five paths (the
//    0.10.81 naming trap: public_key_from_der takes SPKI and only the _pkcs1 suffix takes
//    RSAPublicKey; feeding the wrong one is an asn1 wrong-tag error) -> Signer/Verifier
//    PKCS1v15+SHA-256 (deterministic padding) compared byte for byte against the known
//    answer; four public-key verify surfaces plus verification through the private key;
//    negative cases (changed message / changed last signature byte / another message's
//    signature); an Rsa Padding::NONE raw private-encrypt/public-decrypt roundtrip (256 B
//    block, msb=0 so the value is below n) with a short-input error; check_key; two DER
//    junk error paths;
// ③ pbkdf2-hmac-sha256 against a fixed vector (4096 iterations, cross-checked with python3
//    hashlib), base64 encode/decode roundtrip, memcmp::eq positive and negative, and the
//    library version surface;
// ④ EVP AES-128-CBC/ECB NIST SP800-38A F.2/F.1 vectors (Crypter pad(false) whole-block vs
//    13-byte split streaming, verified with openssl enc -nopad) plus the default pkcs7
//    padding roundtrip, a wrong key length, tampered ciphertext (padding error), and
//    from_nid hit / raw(-1) reject (cipher name lookup goes through Nid; from_name exists
//    only for MessageDigest).
//
// Deterministic: every key, vector and signature is an embedded hex constant (PKCS1v15
// signing is byte-deterministic); no HashMap order, time, addresses, threads or RNG; no temp
// files (everything is in memory); error paths print ErrorStack Display (error code +
// library/function/reason + file:line + data), all fixed strings inside the libcrypto build
// and therefore identical on native and mirvm, which share host and library; successful
// dimensions keep stderr empty (the driver emits no warnings).
use openssl::base64::{decode_block, encode_block};
use openssl::error::ErrorStack;
use openssl::hash::{Hasher, MessageDigest, hash};
use openssl::memcmp;
use openssl::nid::Nid;
use openssl::pkcs5::pbkdf2_hmac;
use openssl::pkey::{HasPublic, PKey, PKeyRef, Private, Public};
use openssl::rsa::{Padding, Rsa};
use openssl::sha;
use openssl::sign::{Signer, Verifier};
use openssl::symm::{Cipher, Crypter, Mode, decrypt, encrypt};
use openssl::version;

/// Fixed RSA-2048 private key (PKCS#1 DER, 1190 B; generated once, frozen, never regenerated).
const RSA_PRIV_PKCS1: &str = "308204a20201000282010100d8e1da9a60da804c7dc8f66c5fe408e6074e71e8ca110071057062ab38cd23c37ab31b2d7cbcdadaa4530cb3731b0d0bb4357183a5610598b85058e97c27ef07b95933b3a3b0e349c7f1182e4ed4de988045da7e3dbad1784e5efbb47ff61ace8be4f1c0f2286cf150628b0f1ebc087784f8663a8cd305d12af1dcafc1f14ccf81e176ae752c64ce8b350fd319cd5b4bd4a517c01306fcfb57d61df3986c10b480b73712b15116e55135f266eb272ed652738b8488f78ae1c09a025951273cbb8b898a0d2a05fb568f02d38ac7e35c36c2358d29d45bab960eab7c44110759dcdae902f1b5d3494c90e0754ca17cdbe654351472469f973d155939cf64efcdbb0203010001028201000100b2510a67983642529e890bbc732a151890c041c6e8589d6cd297f61044bd0fa12a5c007aa30c65bc188fefd54d2f23ab2796333a8a9b152711f22a67c8dc21483f4bf252cc0a38496e1230bd1d36594979642206ee16ff4dc31e27854cc46db32646fd17a0c150dcd3c5c31f9d6e6807f9f90553a8703a219a2299e3f2b5f4b6bca23a60ba3ce85d080a4c9c1f2e79df5dec2d73203f923451c75f9bf326dc3495fae4343f0d69f6ad563a45f14c61592746c46d6bddd6f5746ca070bfb668a28207edef49701b0368c8bc78843fb2a6c3bfdd2b5f7a45c12ecba0a49232c2eee1315b41baf900d1b6ca3532ec6a77f8170c84931995a3720798d33f143902818100f2ceac161130c427c18709a98ab0b801f9851b42207f73bb90677a012ad196474f44b97e378f58573f5d478adbf7f5dd0a766ba6a8bcdfe678e7bc4c708cebcd918c6979836b446fd5e93402ad8a441a1c2e6ec1aca93e15008ec51422fd180e5c1501a0877a517260bc4f27510ca00b1e68e494903e0aad1b08c430e4be852702818100e4aa93b51b5c44de0ac99071ab7b70ef685a74d9b9aa6e7800fa0f81bb0f463448ba262754772d859b7c523a2bc448c0f8f6aeb81b3631f149fb747338ff92b28a88d0c9c09cef2d4fe641a063a3ae51f5e6aca9479a396bbc9e5cf1e20b3c74d39eabc8c65a76cf616227e6cd5ed111cd67a1696fbbd13815a188544882d74d0281801ba18d4fcd9101218d1272f50a45660b437bf448282e98db0569e12674daf90110723fb1af5ceeaeaf154c68eef35ed552b57b36b2091c69bbe4933717afd1bdc90c738c527a488579905a4cdbb6da5d264bda6acbdd4ea55134ee14868ecac8078e946ad2400738beed6f0c885aa973da78115b1eb710bbf6519f11f955fd0d0281805de9c0a84d0864305d75d3211c30a27d70fa55ab66199d2d24198f6cd48abd6693c8000b7f21434cf042eaf2812f284238fdf75c1db0f06a0cdc7d432551b1ca2a236ebcada2c688719c3bafc7bc5dc7c39a6da748850ab838cb419906215f3f0bfacacab6cc48a77b7378b7cdf8f71cbca3a7234a8474b4f80d53946a0372b10281804e636e6eea9deebf9d2a9425a6cff6e8ce318c64b4e51f044030678bff4bbef6da8c832399ba0ad63f174b8701e12b412a02ac5875258b2cd95fead0e909f0288f446a025fda3ac5fbdbefa0d28e2412f1e331331a9d9bda9a250bf94209cf0aac768667a9de8f901e20001d124892a7557b8bf60be1473de710ef19c0c8e9ea";
/// Matching public key (PKCS#1 DER, 270 B).
const RSA_PUB_PKCS1: &str = "3082010a0282010100d8e1da9a60da804c7dc8f66c5fe408e6074e71e8ca110071057062ab38cd23c37ab31b2d7cbcdadaa4530cb3731b0d0bb4357183a5610598b85058e97c27ef07b95933b3a3b0e349c7f1182e4ed4de988045da7e3dbad1784e5efbb47ff61ace8be4f1c0f2286cf150628b0f1ebc087784f8663a8cd305d12af1dcafc1f14ccf81e176ae752c64ce8b350fd319cd5b4bd4a517c01306fcfb57d61df3986c10b480b73712b15116e55135f266eb272ed652738b8488f78ae1c09a025951273cbb8b898a0d2a05fb568f02d38ac7e35c36c2358d29d45bab960eab7c44110759dcdae902f1b5d3494c90e0754ca17cdbe654351472469f973d155939cf64efcdbb0203010001";
/// Matching public key (X.509 SPKI DER, 294 B).
const RSA_PUB_SPKI: &str = "30820122300d06092a864886f70d01010105000382010f003082010a0282010100d8e1da9a60da804c7dc8f66c5fe408e6074e71e8ca110071057062ab38cd23c37ab31b2d7cbcdadaa4530cb3731b0d0bb4357183a5610598b85058e97c27ef07b95933b3a3b0e349c7f1182e4ed4de988045da7e3dbad1784e5efbb47ff61ace8be4f1c0f2286cf150628b0f1ebc087784f8663a8cd305d12af1dcafc1f14ccf81e176ae752c64ce8b350fd319cd5b4bd4a517c01306fcfb57d61df3986c10b480b73712b15116e55135f266eb272ed652738b8488f78ae1c09a025951273cbb8b898a0d2a05fb568f02d38ac7e35c36c2358d29d45bab960eab7c44110759dcdae902f1b5d3494c90e0754ca17cdbe654351472469f973d155939cf64efcdbb0203010001";
/// Known-answer PKCS1v15+SHA-256 signature over SIGN_MSG (computed with the host openssl dgst, then frozen).
const RSA_SIG: &str = "3925ff092d2e623951611a5d36249246a631becd3f4a47dbfc29a4b01c19e2fc87ebf51222d39b1f3895242c440c63dd60147761a49aacd0a51a0eec8fc5a2ca4bccdd243379ca2566ecb0bd75bf43917667adea254ca1637c5e77f0e2de4aa6a0da5dbca74f6bd19c3dde631ff2bb8d3f760d809ca9e11fe00e9e394ba9b38bb5b74441060a4f54d99fbe8cdda0616e3683304c58f974d85ef8a843d2fc236ed9a68e1ca7b94783796e1f04758205357c1b5d691983d23a9ee9ebb1bbd0b7de7f1e1a7cd35ae8c5624beb6a76318ec865334eade33740738ced2c95bda34717dc6df91c8b610da9c34eb4908bf093c54d45fbb24a3c1f474a4b22603afcd673";

/// Fixed message for signing/verification (bound to the known answer; changing it changes the answer).
const SIGN_MSG: &[u8] = b"mirvm openssl_evp fixed message v1";

// NIST SP800-38A AES-128 fixed vectors.
const KEY128: &str = "2b7e151628aed2a6abf7158809cf4f3c";
const IV128: &str = "000102030405060708090a0b0c0d0e0f";
const PT_38A: &str = "6bc1bee22e409f96e93d7e117393172a\
                     ae2d8a571e03ac9c9eb76fac45af8e51\
                     30c81c46a35ce411e5fbc1191a0a52ef\
                     f69f2445df4f9b17ad2b417be66c3710";
const CT_CBC_38A: &str = "7649abac8119b246cee98e9b12e9197d\
                        5086cb9b507219ee95db113a917678b2\
                        73bed6b8e3c1743b7116e69e22229516\
                        3ff1caa1681fac09120eca307586e1a7";
const CT_ECB_38A: &str = "3ad77bb40d7a3660a89ecaf32466ef97\
                        f5d3d58503b9699de785895a96fdbaaf\
                        43b1cd7f598ece23881b00e3ed030688\
                        7b0c785e27e8ad3f8223207104725dd4";

fn unhex(s: &str) -> Vec<u8> {
    fn nib(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("bad hex char"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() % 2 == 0, "hex constant must have even length");
    b.chunks(2).map(|p| (nib(p[0]) << 4) | nib(p[1])).collect()
}

fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Inlined FNV-1a (anchors a binary blob).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// ErrorStack -> one line (lib::reason is a fixed string inside libcrypto, identical on both hosts).
fn fmt_stack(e: &ErrorStack) -> String {
    e.errors()
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn perr(label: &str, e: &ErrorStack) {
    println!("{label} err count={} {}", e.errors().len(), fmt_stack(e));
}

/// Feeds the Crypter in the given split sizes (pad always false, to match whole-block vectors).
fn crypter_run(
    cipher: Cipher,
    mode: Mode,
    key: &[u8],
    iv: Option<&[u8]>,
    input: &[u8],
    splits: &[usize],
) -> Vec<u8> {
    let mut c = Crypter::new(cipher, mode, key, iv).unwrap();
    c.pad(false);
    let mut out = Vec::new();
    let mut off = 0;
    for &sz in splits {
        let chunk = &input[off..off + sz];
        let mut buf = vec![0u8; chunk.len() + cipher.block_size()];
        let n = c.update(chunk, &mut buf).unwrap();
        out.extend_from_slice(&buf[..n]);
        off += sz;
    }
    assert_eq!(off, input.len(), "splits must consume the input exactly");
    let mut tail = vec![0u8; cipher.block_size()];
    let n = c.finalize(&mut tail).unwrap();
    out.extend_from_slice(&tail[..n]);
    out
}

/// Uniform shape for verify hits and misses: Ok(bool) or one error-stack line (same host and library).
fn verify_with<T: HasPublic>(pk: &PKeyRef<T>, msg: &[u8], sig: &[u8]) -> String {
    let mut v = Verifier::new(MessageDigest::sha256(), pk).unwrap();
    v.update(msg).unwrap();
    match v.verify(sig) {
        Ok(b) => format!("ok={b}"),
        Err(e) => format!("err count={} {}", e.errors().len(), fmt_stack(&e)),
    }
}

fn main() {
    // Workaround (root cause in the file header): explicitly preload the system libraries into
    // the global namespace, compensating for mirvm's lowering not collecting `-l` from rlib
    // metadata. crypto before ssl (dependency order); a native no-op on the same libraries.
    unsafe {
        for lib in ["libcrypto.so.3\0", "libssl.so.3\0"] {
            let h = libc::dlopen(lib.as_ptr().cast(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            assert!(!h.is_null(), "dlopen {lib} failed");
        }
    }

    println!(
        "lib = {} number = {:#010x}",
        version::version(),
        version::number()
    );

    // ---- ① EVP digest: fixed vectors + streaming + one-shot + from_name ----
    println!("== evp digest ==");
    let cases: &[(&str, &str, MessageDigest, &[u8], &str)] = &[
        ("sha256", "abc", MessageDigest::sha256(), b"abc",
         "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        ("sha256", "empty", MessageDigest::sha256(), b"",
         "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        ("sha256", "56B", MessageDigest::sha256(),
         b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
         "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"),
        ("sha512", "abc", MessageDigest::sha512(), b"abc",
         "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
          2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"),
        ("sha1", "abc", MessageDigest::sha1(), b"abc",
         "a9993e364706816aba3e25717850c26c9cd0d89d"),
        ("md5", "abc", MessageDigest::md5(), b"abc",
         "900150983cd24fb0d6963f7d28e17f72"),
    ];
    for (name, label, md, msg, want) in cases {
        let got = hex(&hash(*md, msg).unwrap());
        println!("dgst {name}({label}) = {got} known={}", got == *want);
    }
    println!(
        "sha-one-shot 256={} 512={} 1={}",
        hex(&sha::sha256(b"abc")) == cases[0].4,
        hex(&sha::sha512(b"abc")) == cases[3].4,
        hex(&sha::sha1(b"abc")) == cases[4].4
    );
    let mut h = Hasher::new(MessageDigest::sha256()).unwrap();
    h.update(b"a").unwrap();
    h.update(b"").unwrap();
    h.update(b"bc").unwrap();
    println!(
        "hasher split(1+0+2) == oneshot: {}",
        hex(&h.finish().unwrap()) == cases[0].4
    );
    println!(
        "md from_name sha256={} sha9999-zzz={}",
        MessageDigest::from_name("sha256").is_some(),
        MessageDigest::from_name("sha9999-zzz").is_none()
    );
    println!(
        "md size sha256={} sha512={} md5={}",
        MessageDigest::sha256().size(),
        MessageDigest::sha512().size(),
        MessageDigest::md5().size()
    );

    // ---- ② RSA: import fixed DER -> sign/verify hits and misses + NONE raw encryption ----
    println!("== rsa ==");
    let rsa = Rsa::private_key_from_der(&unhex(RSA_PRIV_PKCS1)).unwrap();
    println!(
        "rsa priv bits={} size={} check={:?}",
        rsa.n().num_bits(),
        rsa.size(),
        rsa.check_key()
    );
    let priv_n = rsa.n().to_vec();
    let priv_e = rsa.e().to_vec();
    println!("rsa e = {} n[0..8] = {}", hex(&priv_e), hex(&priv_n[..8]));
    let pkey: PKey<Private> = PKey::from_rsa(rsa).unwrap();

    let sig_expect = unhex(RSA_SIG);
    let mut s1 = Signer::new(MessageDigest::sha256(), &pkey).unwrap();
    s1.update(SIGN_MSG).unwrap();
    let sig1 = s1.sign_to_vec().unwrap();
    let mut s2 = Signer::new(MessageDigest::sha256(), &pkey).unwrap();
    s2.update(&SIGN_MSG[..10]).unwrap();
    s2.update(&SIGN_MSG[10..]).unwrap();
    let sig2 = s2.sign_to_vec().unwrap();
    println!(
        "sig len={} fnv={:016x} split-eq={} known={}",
        sig1.len(),
        fnv1a(&sig1),
        sig1 == sig2,
        sig1 == sig_expect
    );

    // Four public-key import paths (0.10.81 naming: public_key_from_der = SPKI, only the
    // _pkcs1 suffix takes RSAPublicKey) + from_public_components
    let rsa_pub1 = Rsa::public_key_from_der_pkcs1(&unhex(RSA_PUB_PKCS1)).unwrap();
    println!(
        "pub-pkcs1 bits={} n == priv n: {}",
        rsa_pub1.n().num_bits(),
        rsa_pub1.n().to_vec() == priv_n
    );
    let pkey_pub1: PKey<Public> = PKey::from_rsa(rsa_pub1).unwrap();
    let rsa_pub2 = Rsa::public_key_from_der(&unhex(RSA_PUB_SPKI)).unwrap();
    let pkey_pub2: PKey<Public> = PKey::from_rsa(rsa_pub2).unwrap();
    let pkey_pub2b: PKey<Public> = PKey::public_key_from_der(&unhex(RSA_PUB_SPKI)).unwrap();
    let rsa_pub3 = Rsa::from_public_components(
        openssl::bn::BigNum::from_slice(&priv_n).unwrap(),
        openssl::bn::BigNum::from_slice(&priv_e).unwrap(),
    )
    .unwrap();
    let pkey_pub3: PKey<Public> = PKey::from_rsa(rsa_pub3).unwrap();

    println!("verify pub-pkcs1  = {}", verify_with(&pkey_pub1, SIGN_MSG, &sig1));
    println!("verify pub-spki   = {}", verify_with(&pkey_pub2, SIGN_MSG, &sig1));
    println!("verify pkey-spki  = {}", verify_with(&pkey_pub2b, SIGN_MSG, &sig1));
    println!("verify pub-comp   = {}", verify_with(&pkey_pub3, SIGN_MSG, &sig1));
    println!("verify via-priv   = {}", verify_with(&pkey, SIGN_MSG, &sig1));
    // Three negative cases: changed message / changed last signature byte / another message's signature
    let mut tmsg = SIGN_MSG.to_vec();
    tmsg[3] ^= 0x01;
    println!("verify tampered-msg = {}", verify_with(&pkey_pub1, &tmsg, &sig1));
    let mut tsig = sig1.clone();
    *tsig.last_mut().unwrap() ^= 0xff;
    println!("verify tampered-sig = {}", verify_with(&pkey_pub1, SIGN_MSG, &tsig));
    let other: &[u8] = b"other message body v2";
    let mut s3 = Signer::new(MessageDigest::sha256(), &pkey).unwrap();
    s3.update(other).unwrap();
    let sig3 = s3.sign_to_vec().unwrap();
    println!("verify wrong-msg-sig = {}", verify_with(&pkey_pub2, SIGN_MSG, &sig3));
    println!("verify wrong-msg-sig own-msg = {}", verify_with(&pkey_pub2, other, &sig3));

    // Raw RSA op (Padding::NONE, deterministic): private-encrypt -> public-decrypt roundtrip.
    // The 256 B block's first byte is always 0, so the value stays below 2^2040 < n.
    let rsa_back = pkey.rsa().unwrap();
    let mut blk = [0u8; 256];
    for (i, b) in blk.iter_mut().enumerate().skip(1) {
        *b = (i as u8).wrapping_mul(37) ^ 0x5a;
    }
    let mut enc = vec![0u8; rsa_back.size() as usize];
    let n = rsa_back.private_encrypt(&blk, &mut enc, Padding::NONE).unwrap();
    let mut dec = vec![0u8; rsa_back.size() as usize];
    let m = rsa_back.public_decrypt(&enc[..n], &mut dec, Padding::NONE).unwrap();
    println!(
        "rsa raw n={n} m={m} fnv={:016x} roundtrip={}",
        fnv1a(&enc[..n]),
        dec[..m] == blk[..]
    );
    match rsa_back.private_encrypt(&[0x42u8; 100], &mut enc, Padding::NONE) {
        Ok(_) => println!("rsa raw short-input unexpectedly ok"),
        Err(e) => perr("rsa-short-input", &e),
    }
    match Rsa::private_key_from_der(&unhex("3082deadbeefcafe0000")) {
        Ok(_) => println!("priv junk der unexpectedly ok"),
        Err(e) => perr("priv-junk-der", &e),
    }
    match Rsa::public_key_from_der(&unhex("deadbeef")) {
        Ok(_) => println!("pub junk der unexpectedly ok"),
        Err(e) => perr("pub-junk-der", &e),
    }

    // ---- ③ misc: pbkdf2 / base64 / memcmp ----
    println!("== misc ==");
    let b64 = encode_block(b"hello, mirvm!");
    println!(
        "base64 enc = {b64} roundtrip={}",
        decode_block(&b64).unwrap() == b"hello, mirvm!"
    );
    let mut diff = sig1.clone();
    diff[7] ^= 0x01;
    println!(
        "memcmp eq-self={} eq-diff={}",
        memcmp::eq(&sig1, &sig1),
        memcmp::eq(&sig1, &diff)
    );
    let mut key32 = [0u8; 32];
    pbkdf2_hmac(b"mirvm-password", b"mirvm-salt", 4096, MessageDigest::sha256(), &mut key32)
        .unwrap();
    let pbk = hex(&key32);
    println!(
        "pbkdf2-hmac-sha256 = {pbk} known={}",
        pbk == "249313468e55625e35421824db264715c64d7b4ec2d29adb3589a88f6cbe966f"
    );

    // ---- ④ EVP cipher: AES-128 CBC/ECB fixed vectors + padding + error paths ----
    // This section hits the limitation from the file header: openssl's generic CipherContext
    // init passes `ffi::EVP_EncryptInit_ex`/`EVP_DecryptInit_ex` as *values*, so the extern-fn
    // pointer is resolved at lowering time (libcrypto not yet in the mirvm process) and the
    // TRAP aborts mirvm at the first Crypter/encrypt (exit 70). No guest-side workaround
    // preserves semantics, so this section comes last and ① ② ③ run first.
    println!("== evp cipher ==");
    let key = unhex(KEY128);
    let iv = unhex(IV128);
    let pt = unhex(&PT_38A.replace(char::is_whitespace, ""));
    let ct_cbc_want = unhex(&CT_CBC_38A.replace(char::is_whitespace, ""));
    let ct_ecb_want = unhex(&CT_ECB_38A.replace(char::is_whitespace, ""));
    let cbc = Cipher::aes_128_cbc();
    println!(
        "cipher aes-128-cbc klen={} ivlen={:?} bsize={}",
        cbc.key_len(),
        cbc.iv_len(),
        cbc.block_size()
    );

    let one = crypter_run(cbc, Mode::Encrypt, &key, Some(&iv), &pt, &[pt.len()]);
    println!("cbc enc = {}", hex(&one));
    println!("cbc known={}", one == ct_cbc_want);
    let stream = crypter_run(cbc, Mode::Encrypt, &key, Some(&iv), &pt, &[13, 13, 13, 13, 12]);
    println!("cbc split13 == oneshot: {}", stream == one);
    let back = crypter_run(cbc, Mode::Decrypt, &key, Some(&iv), &one, &[one.len()]);
    println!("cbc dec roundtrip={}", back == pt);

    let ecb = Cipher::aes_128_ecb();
    let e1 = crypter_run(ecb, Mode::Encrypt, &key, None, &pt, &[pt.len()]);
    println!("ecb enc = {}", hex(&e1));
    println!("ecb known={}", e1 == ct_ecb_want);
    let eb = crypter_run(ecb, Mode::Decrypt, &key, None, &e1, &[e1.len()]);
    println!("ecb dec roundtrip={}", eb == pt);

    // Default pkcs7 padding (11 B -> 16 B) with one-shot encrypt/decrypt
    let msg11 = b"hello world";
    let padded = encrypt(cbc, &key, Some(&iv), msg11).unwrap();
    let plain = decrypt(cbc, &key, Some(&iv), &padded).unwrap();
    println!(
        "cbc pad msg11 ctlen={} fnv={:016x} roundtrip={}",
        padded.len(),
        fnv1a(&padded),
        plain == msg11
    );
    // Name lookup: ciphers go through Nid (from_name exists only for MessageDigest)
    let by_nid = Cipher::from_nid(Nid::AES_128_CBC).unwrap();
    println!(
        "cipher from_nid sn={:?} hit={} raw(-1)={}",
        by_nid.nid().short_name().unwrap(),
        by_nid.nid() == cbc.nid(),
        Cipher::from_nid(Nid::from_raw(-1)).is_none()
    );
    match encrypt(cbc, &[0x11u8; 8], Some(&iv), b"x") {
        Ok(_) => println!("bad key len unexpectedly ok"),
        Err(e) => perr("bad-keylen", &e),
    }
    // NOTE: a too-short IV is not an Err -- openssl 0.10.81 cipher_ctx.rs asserts
    // `iv_len <= iv.len()` (the crate panics by design), so it is not used as an error case;
    // an over-long IV is silently truncated by libcrypto, giving no Err to check either.
    let mut tampered = padded.clone();
    *tampered.last_mut().unwrap() ^= 0x01;
    match decrypt(cbc, &key, Some(&iv), &tampered) {
        Ok(_) => println!("tampered ct unexpectedly ok"),
        Err(e) => perr("tampered-ct", &e),
    }
}
