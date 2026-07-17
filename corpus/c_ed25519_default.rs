#!/usr/bin/env mirvm
---
[dependencies]
# =2.2.0 patch 钉死（与批1 c_ed25519 已锁闭包同版，离线可重建）。不设任何
# backend env：默认（x86_64+nightly 自动 simd）backend 正是本条测试对象。
ed25519-dalek = "=2.2.0"
# 间接依赖显式 patch 钉死（4.1.3，与批1 锁定同值），防 registry 新 patch 漂移
# 导致闭包不可复现。
curve25519-dalek = "=4.1.3"
---
// ed25519-dalek 默认 simd backend 直跑——批1 c_ed25519（serial env 绕行）的解锁加测。
//
// 背景与钉选理由：curve25519-dalek 4.1.3 在 x86_64+nightly 下 auto 选 simd
// backend；Avx2/ifma 多版本函数以 #[target_feature] 编译、运行期 guest CPUID
// （直通宿主）派发。本机带 avx512ifma → 派发选中 ifma 路径 → 域乘法走
// llvm.x86.avx512.vpmadd52*——批1 时六宽未内建，以
// CARGO_CFG_CURVE25519_DALEK_BACKEND=serial 绕行（gate5 至今为 c_ed25519 注入
// 该 env，名键不命中本 driver）；2b4766b 起 vpmadd52 全内建 → 本 driver 不设
// env 直跑默认路径，验收解锁。
//
// 测试面：
//   ① RFC8032 §7.1 TEST 1/2/3：sk→pk 派生逐字节锚定、sign 逐字节锚定定向量、
//      verify / verify_strict 正例（assert + 稳定 print）；
//   ② 定种 keygen（两枚硬编码种子 × 3 条消息：空/短/175B 跨 SHA-512 双块）：
//      pk/sig hex 打印、同消息双签确定性、序列化 roundtrip、flip-msg/flip-sig
//      反例、跨密钥反例、fnv1a 汇总；
//   ③ backend 一致性（手工一次性，不进三维门禁）：本文件输出与 serial env 下
//      输出逐字节一致——canonical encoding 数学值不随 backend 变（见命令④）。
//      另覆盖压解/小阶边界：全零 y（二次剩余巧合 → 二阶点）、y=1 identity，
//      二者可解压但 strict 验签拒签。
//
// 确定性：无随机源/无时间/无 env 读取——种子与消息全部硬编码，ed25519 签名本身
// 无 nonce 熵（RFC8032 确定签名）；输出全为十六进制与布尔稳定行（约 60 行）。
// FRONTIER：无（vpmadd52 内建已落地， docs/corpus.md §5 M5.x 队列核销记录）。
//
// 复跑（仓库根）：
//   A: target/release/mirvm run corpus/c_ed25519_default.rs
//   B: cd $(dirname $(grep -l 'name = "c_ed25519_default"' ~/.cache/mirvm/scripts/*/Cargo.toml)) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_ed25519_default.rs
//   ④ backend 对比（手工）：CARGO_CFG_CURVE25519_DALEK_BACKEND=serial \
//      target/release/mirvm run corpus/c_ed25519_default.rs —— stdout 应与 A 逐字节一致。
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex len");
    let b = s.as_bytes();
    let nib = |c: u8| -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("bad hex char"),
        }
    };
    (0..s.len() / 2)
        .map(|i| (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]))
        .collect()
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// RFC8032 §7.1 测试向量（Ed25519，TEST 1/2/3）。
const RFC: [(&str, &str, &str, &str); 3] = [
    (
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
        "",
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    ),
    (
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
        "72",
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
         085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
    ),
    (
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
        "af82",
        "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
         18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
    ),
];

/// ① RFC8032 定向量：派生与签名结果硬锚定（assert），打印留三维对拍记录。
fn rfc_vectors() {
    for (i, (seed_h, pk_h, msg_h, sig_h)) in RFC.iter().enumerate() {
        let n = i + 1;
        let seed: [u8; 32] = unhex(seed_h).try_into().unwrap();
        let pk_expected: [u8; 32] = unhex(pk_h).try_into().unwrap();
        let msg = unhex(msg_h);
        let sig_expected: [u8; 64] = unhex(&sig_h.replace(char::is_whitespace, ""))
            .try_into()
            .unwrap();

        let sk = SigningKey::from_bytes(&seed);
        let pk = sk.verifying_key().to_bytes();
        println!("rfc{n} pk = {}", hex(&pk));
        assert_eq!(pk, pk_expected, "rfc{n} pk 派生偏离 RFC8032");

        let sig = sk.sign(&msg);
        let sig_bytes = sig.to_bytes();
        println!("rfc{n} sig = {}", hex(&sig_bytes));
        assert_eq!(sig_bytes, sig_expected, "rfc{n} 签名偏离 RFC8032");

        let vk = VerifyingKey::from_bytes(&pk_expected).unwrap();
        println!("rfc{n} verify = {}", vk.verify(&msg, &sig).is_ok());
        println!("rfc{n} strict = {}", vk.verify_strict(&msg, &sig).is_ok());
    }
}

/// ② 定种 keygen + 签验：一批稳定 print + 正反例。
fn round(label: &str, seed: [u8; 32], msgs: &[&[u8]], sink: &mut Vec<u8>) {
    let sk = SigningKey::from_bytes(&seed);
    let vk: VerifyingKey = sk.verifying_key();
    println!("{label} pk = {}", hex(vk.as_bytes()));

    println!(
        "{label} sk roundtrip = {}",
        SigningKey::from_bytes(&sk.to_bytes()).to_bytes() == seed
    );
    let vk2 = VerifyingKey::from_bytes(vk.as_bytes()).unwrap();
    println!("{label} vk roundtrip = {}", vk == vk2);

    for (i, msg) in msgs.iter().enumerate() {
        let sig: Signature = sk.sign(msg);
        let sig_bytes = sig.to_bytes();
        sink.extend_from_slice(&sig_bytes);
        println!("{label} sig{i} (len {}) = {}", msg.len(), hex(&sig_bytes));

        // ed25519 是确定性签名：同消息再签一次必须逐字节相等
        println!(
            "{label} sig{i} deterministic = {}",
            sig_bytes == sk.sign(msg).to_bytes()
        );
        println!("{label} sig{i} roundtrip = {}", Signature::from_bytes(&sig_bytes).to_bytes() == sig_bytes);
        println!("{label} sig{i} verify = {}", vk.verify(msg, &sig).is_ok());
        println!("{label} sig{i} strict = {}", vk.verify_strict(msg, &sig).is_ok());

        // 反例一：消息翻转一字节 → 必须失败
        let mut bad_msg = msg.to_vec();
        if bad_msg.is_empty() {
            bad_msg.push(0x01);
        } else {
            bad_msg[0] ^= 0x01;
        }
        println!("{label} sig{i} flip-msg err = {}", vk.verify(&bad_msg, &sig).is_err());

        // 反例二：签名翻转一字节 → 必须失败
        let mut bad_bytes = sig_bytes;
        bad_bytes[10] ^= 0x80;
        let bad_sig = Signature::from_bytes(&bad_bytes);
        println!("{label} sig{i} flip-sig err = {}", vk.verify(msg, &bad_sig).is_err());
    }
}

fn main() {
    rfc_vectors();

    // ② 两枚硬编码种子：一枚常数、一枚 xorshift 派生。
    let seed_a = [0x2a_u8; 32];
    let mut seed_b = [0u8; 32];
    let mut x = 0x9E37_79B9_7F4A_7C15_u64;
    for b in seed_b.iter_mut() {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *b = x.wrapping_mul(0x2545_F491_4F6C_DD1D) as u8;
    }

    // 三条消息：空 / 短 / 175B（跨 SHA-512 两个 128B 分块）。
    let mut long = Vec::new();
    for i in 0..175_u32 {
        long.push(i.wrapping_mul(7).wrapping_add(3) as u8);
    }
    let msgs: [&[u8]; 3] = [b"", b"mirvm ed25519 default-backend differential", long.as_slice()];

    let mut sink = Vec::new();
    round("seedA", seed_a, &msgs, &mut sink);
    round("seedB", seed_b, &msgs, &mut sink);

    // 跨密钥反例：seedA 的签名不能用 seedB 的公钥验证
    let sig_a = SigningKey::from_bytes(&seed_a).sign(msgs[2]);
    let vk_b = SigningKey::from_bytes(&seed_b).verifying_key();
    println!("cross-key verify err = {}", vk_b.verify(msgs[2], &sig_a).is_err());

    // 压解/小阶边界：全零字节 = y=0，x²=(y²-1)/(dy²+1)=-1 在模 p 下恰为二次
    // 剩余 → 解压成功（二阶点 (±√-1, 0)，属小阶点）；严格验签必须拒绝。
    // y=1（identity 弱公钥）解压成功、basic 验签自然失败、strict 明确拒绝。
    let vk_zero = VerifyingKey::from_bytes(&[0u8; 32]).unwrap();
    println!("vk all-zero decompress ok = true");
    let rfc_msg = unhex(RFC[0].2);
    let rfc_sig = Signature::from_bytes(
        &unhex(&RFC[0].3.replace(char::is_whitespace, ""))
            .try_into()
            .unwrap(),
    );
    println!("vk all-zero strict err = {}", vk_zero.verify_strict(&rfc_msg, &rfc_sig).is_err());
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let vk_id = VerifyingKey::from_bytes(&identity).unwrap();
    println!("vk identity basic = {}", vk_id.verify(&rfc_msg, &rfc_sig).is_ok());
    println!("vk identity strict err = {}", vk_id.verify_strict(&rfc_msg, &rfc_sig).is_err());

    println!("sigs total len = {}", sink.len());
    println!("sigs fnv1a = {:016x}", fnv1a(&sink));
}
