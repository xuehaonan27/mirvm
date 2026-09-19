#!/usr/bin/env mirvm
---
[dependencies]
argon2 = "0.6.0-rc.8"
---
// argon2: memory-hard KDF, blake2b-long compression over m 1KiB blocks. A fixed
// salt/password plus small Params makes it deterministic. The crate probes __cpuid
// through cpufeatures; the SIMD and scalar paths must agree bit for bit.
use argon2::{
    Algorithm, Argon2, AssociatedData, Params, ParamsBuilder, PasswordHash, PasswordHasher,
    PasswordVerifier, Version,
};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

fn main() {
    // (1) Params construction and accessors
    let params = Params::new(8192, 1, 1, Some(32)).unwrap();
    println!(
        "params m={} t={} p={} out={:?}",
        params.m_cost(),
        params.t_cost(),
        params.p_cost(),
        params.output_len()
    );

    // (2) main hash: Argon2id v0x13, m=8192 KiB x t=1 x p=1
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let pwd = b"correct horse battery staple";
    let salt = b"mirvm-fixed-salt";
    let mut out = [0u8; 32];
    a2.hash_password_into(pwd, salt, &mut out).unwrap();
    println!("argon2id m=8192: {}", hex(&out));

    // (3) algorithm x version matrix (small m: d/i/id, v16 overwrite vs v19 xor)
    for alg in [Algorithm::Argon2d, Algorithm::Argon2i, Algorithm::Argon2id] {
        for ver in [Version::V0x10, Version::V0x13] {
            let p = Params::new(64, 2, 1, Some(16)).unwrap();
            let ctx = Argon2::new(alg, ver, p);
            let mut o = [0u8; 16];
            ctx.hash_password_into(b"pw", b"saltsalt", &mut o).unwrap();
            println!("{} v={} t2: {}", alg.as_str(), u32::from(ver), hex(&o));
        }
    }

    // (4) KAT self-check against the draft-irtf-cfrg-argon2 vector (secret +
    //     associated data + p=4 multi-lane sync): proves it right, not just equal
    let kat_params = ParamsBuilder::new()
        .m_cost(32)
        .t_cost(3)
        .p_cost(4)
        .data(AssociatedData::new(&[0x04; 12]).unwrap())
        .build()
        .unwrap();
    let kat_ctx =
        Argon2::new_with_secret(&[0x03; 8], Algorithm::Argon2id, Version::V0x13, kat_params)
            .unwrap();
    let mut tag = [0u8; 32];
    kat_ctx.hash_password_into(&[0x01; 32], &[0x02; 16], &mut tag).unwrap();
    println!("kat argon2id p=4: {}", hex(&tag));
    println!(
        "kat ok = {}",
        hex(&tag) == "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659"
    );

    // (5) PHC string API: hash -> encoded string -> parse -> verify both ways
    let phc_params = Params::new(256, 2, 1, None).unwrap();
    let phc_ctx = Argon2::new(Algorithm::Argon2id, Version::V0x13, phc_params);
    let hash = phc_ctx.hash_password_with_salt(b"password", b"somesalt").unwrap();
    let phc = hash.to_string();
    println!("phc: {phc}");
    println!(
        "phc match = {}",
        phc == "$argon2id$v=19$m=256,t=2,p=1$c29tZXNhbHQ$nf65EOgLrQMR/uIPnA4rEsF5h7TKyQwu9U1bMCHGi/4"
    );
    let parsed = PasswordHash::new(&phc).unwrap();
    println!(
        "parsed alg={} ver={:?} salt={} hash_len={}",
        parsed.algorithm.as_str(),
        parsed.version,
        hex(parsed.salt.unwrap().as_ref()),
        parsed.hash.unwrap().as_bytes().len()
    );
    let good = Argon2::default().verify_password(b"password", &parsed);
    println!("verify good = {}", good.is_ok());
    let bad = Argon2::default().verify_password(b"passwore", &parsed);
    println!("verify bad is_ok = {} err = {:?}", bad.is_ok(), bad.unwrap_err());

    // (6) error paths (all deterministic variant names)
    let small = Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(64, 1, 1, Some(16)).unwrap());
    let mut o16 = [0u8; 16];
    println!("short salt: {:?}", small.hash_password_into(b"pw", b"short", &mut o16));
    let mut o8 = [0u8; 8];
    println!("out too short: {:?}", small.hash_password_into(b"pw", b"saltsalt", &mut o8));
    println!("mem too little: {:?}", Params::new(4, 1, 1, None).unwrap_err());
    println!("t too small: {:?}", Params::new(64, 0, 1, None).unwrap_err());
    println!("p too few: {:?}", Params::new(64, 1, 0, None).unwrap_err());
    println!("out len too short: {:?}", Params::new(64, 1, 1, Some(2)).unwrap_err());
    println!("bad phc: {:?}", PasswordHash::new("$argon2id$garbage").unwrap_err());
}
