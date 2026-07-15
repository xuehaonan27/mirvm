#!/usr/bin/env mirvm
---
[dependencies]
argon2 = "0.6.0-rc.8"
---
// argon2：内存硬 KDF（m 个 1KiB block 上反复做 blake2b-long 压缩）。
// 固定 salt/password + 调小 Params → 全确定性；内部走 cpufeatures 的
// __cpuid 检测（与 sha2/blake3 同一类），SIMD/标量路径结果必须一致。
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
    // ① Params 构造 + 访问器
    let params = Params::new(8192, 1, 1, Some(32)).unwrap();
    println!(
        "params m={} t={} p={} out={:?}",
        params.m_cost(),
        params.t_cost(),
        params.p_cost(),
        params.output_len()
    );

    // ② 主哈希：Argon2id v0x13，m=8192 KiB × t=1 × p=1（内存硬主压力）
    let a2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let pwd = b"correct horse battery staple";
    let salt = b"mirvm-fixed-salt";
    let mut out = [0u8; 32];
    a2.hash_password_into(pwd, salt, &mut out).unwrap();
    println!("argon2id m=8192: {}", hex(&out));

    // ③ 算法 × 版本矩阵（小 m：压 d/i/id 分支与 v16 overwrite / v19 xor 差异）
    for alg in [Algorithm::Argon2d, Algorithm::Argon2i, Algorithm::Argon2id] {
        for ver in [Version::V0x10, Version::V0x13] {
            let p = Params::new(64, 2, 1, Some(16)).unwrap();
            let ctx = Argon2::new(alg, ver, p);
            let mut o = [0u8; 16];
            ctx.hash_password_into(b"pw", b"saltsalt", &mut o).unwrap();
            println!("{} v={} t2: {}", alg.as_str(), u32::from(ver), hex(&o));
        }
    }

    // ④ KAT 自校验：draft-irtf-cfrg-argon2 向量（secret + associated data +
    //    p=4 多 lane 同步；证明算得对，而不只是两边一致）
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

    // ⑤ PHC 字符串 API：固定 salt 哈希 → 编码串（对照参考向量）→ 解析 → verify 正反
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

    // ⑥ 错误路径（全部确定性 variant 名）
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
