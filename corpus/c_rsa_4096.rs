#!/usr/bin/env mirvm
---
[dependencies]
# rsa 钉 patch =0.9.10（与 c_rsa_pss lock 同版）。default-features=false 去默认
# 三元组 [std, pem, u64_digit] 中的 u64_digit（i128 Neg 绕行，见下），显式回开
# std + sha2（Pkcs1v15Sign::new::<Sha256> 的 OID 关联）+ pem（本 driver 要 PEM
# 解析路径：pkcs1/pem + pkcs8/pem；pem 不拉 u64_digit，bitmap 与 c_rsa_pss 同构）。
rsa = { version = "=0.9.10", default-features = false, features = ["std", "sha2", "pem"] }
---
// rsa 0.9 @4096 位（纯 Rust 大数，num-bigint-dig 后端）差分。批7 波2「rsa 大
// 位宽加测」：c_rsa_pss 是 2048 位固定组件重建面，本 driver 走另一条输入形态
// ——内嵌一份 openssl genpkey 一次性生成的 4096 位 PKCS#8 PEM 私钥常量（生成后
// 固化，无 keygen rng），测 PEM 解析 + CRT 组件位长/同余校验 + v1.5 确定性签名
// 在 4096 位（CRT 两支 2048 位 modpow）下三维是否逐字节一致。
//
// 测试面：
//   ① RsaPrivateKey::from_pkcs8_pem 解析（PEM→base64→PKCS#8 DER→CRT 组件装载）
//      + 位长锚：size()=512B、n.bits()=4096、p/q.bits()=2048、e=65537（openssl
//      genpkey 硬保证）；d/dp/dq/qinv 实测位长打印；
//   ② CRT 数学校验：p*q==n、e*dp≡1 (mod p-1)、e*dq≡1 (mod q-1)、
//      qinv*crt*q≡1 (mod p)（crt_coefficient 语义）；
//   ③ v1.5 **确定性**签名（PKCS1v15Sign::new::<Sha256>，无 rng 通道）一次，
//      len=512 + FNV-1a64 锚；
//   ④ verify 双向：私钥就地 verify + 拆公钥（to_public_key）verify；
//   ⑤ 公钥导出/回灌：to_pkcs1_der（len+FNV）→ from_pkcs1_der == 原公钥 →
//      回灌公钥同步 verify 同一签名；
//   ⑥ 反例：篡改签名 1bit 拒验、错消息摘要拒验；
//   ⑦ PEM 错误路径：base64 字母表外字符注入 → from_pkcs8_pem 静态错误文案。
// 有意不做：keygen/OAEP/v1.5 加密——都带 rng 通道，违反 driver 确定性纪律；
// 不重签第二次——「v1.5 无盐随机」本身就是确定性方案，三维对拍即是确定性 oracle，
// 且 4096 位私钥 op 成本翻倍不划算（大数槽见下）。
//
// ── u64_digit 绕行（沿用 c_rsa_pss 头注记录）──
// rsa 默认 feature u64_digit 使 num-bigint-dig 以 u64 为 limb（SignedDoubleBigDigit
// =i128），modpow 必走的 inv_mod_alt 收尾 `-k0 as BigDigit` 是 i128 一元 Neg——
// M4 的 128 位族欠账（lower 期降 Trap exit 70）。default-features=false 退回
// u32 limb（SDouble=i64，标量全支持）。BigUint 值语义与 limb 宽无关，native
// 双 feature 构建实测 stdout 逐字节相同（c_rsa_pss 实证），对拍不受影响。
//
// ── 大数性能槽 ──
// 判4 rsa_pss（2048 位，十余次私钥 op）JIT=1 维 335s、tmo=400 先例。本 driver
// 只留一次 4096 位私钥 CRT sign（≈两支 2048 位 modpow）+ 三次公钥 verify（公钥
// op 极廉），解释器/JIT 运行时间预计数倍于 rsa_pss 单 op 量级，属既定大数槽，
// 非引擎 bug 信号；三维超时应按 rsa_pss 同级放宽。
//
// 三维复跑（A 首跑后按 name 定位 script 目录跑 B）：
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

/// openssl `genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096` 一次性生成后
/// `pkcs8 -topk8 -nocrypt` 固化的 PKCS#8 PEM（52 行）；此后即为常量，无 rng。
const PRIV_PKCS8_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
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
63oi9Ntb8YmkN81A7LpP56dI43lZ8w==
-----END PRIVATE KEY-----"#;

/// 被签固定消息（内容任意但固定；只进摘要，不进输出）。
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
    // ---- ① PKCS#8 PEM 解析 + 位长锚 ----
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

    // ---- ② CRT 数学同余校验（2048 位乘+模，无 modpow）----
    println!("p*q == n = {}", p * q == *pub_key.n());
    println!("e*dp mod (p-1) == 1 = {}", (pub_key.e() * dp) % (p - &one) == one);
    println!("e*dq mod (q-1) == 1 = {}", (pub_key.e() * dq) % (q - &one) == one);
    println!("qinv*q mod p == 1 = {}", (qinv * q) % p == one);

    // ---- ③ v1.5 确定性签名（唯一一次 4096 位私钥 op）----
    let digest = Sha256::digest(MSG);
    println!("digest fnv = {:016x}", fnv1a(&digest));
    let sig = priv_key.sign(Pkcs1v15Sign::new::<Sha256>(), &digest).unwrap();
    assert_eq!(sig.len(), 512);
    println!("sig len = {}", sig.len());
    println!("sig fnv = {:016x}", fnv1a(&sig));

    // ---- ④ verify 双向（私钥部件就地验 + 拆出的公钥验）----
    println!(
        "verify pub-from-priv = {}",
        pub_key.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &sig).is_ok()
    );

    // ---- ⑤ 公钥导出/回灌后同步 verify ----
    let pub_der = pub_key.to_pkcs1_der().unwrap();
    println!("pub der len={} fnv={:016x}", pub_der.as_bytes().len(), fnv1a(pub_der.as_bytes()));
    let pub_rt = RsaPublicKey::from_pkcs1_der(pub_der.as_bytes()).unwrap();
    println!("pub roundtrip eq = {}", pub_rt == pub_key);
    println!(
        "verify pub-rt = {}",
        pub_rt.verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &sig).is_ok()
    );

    // ---- ⑥ 反例：篡改签名 / 错消息 ----
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

    // ---- ⑦ PEM 错误路径（base64 字母表外字符注入）----
    let bad_pem = PRIV_PKCS8_PEM.replace("MIIJ", "M!IJ");
    match RsaPrivateKey::from_pkcs8_pem(&bad_pem) {
        Ok(_) => println!("bad-pem unexpectedly ok"),
        Err(err) => println!("bad-pem err = {err}"),
    }

    println!("rsa4096 ok");
}
