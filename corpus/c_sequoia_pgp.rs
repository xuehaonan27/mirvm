#!/usr/bin/env mirvm
---
[dependencies]
# sequoia-openpgp 2.4.1（2026-07-17 时点 crates.io 最新 stable；2.2.0-pqc.1 系
# 预发布，跳过）。纯 Rust 大物（本体 171 文件 ~13.1 万行 src 实测），
# default-features=false 裁两道默认件：compression（deflate+bzip2，后者 C 库
# libbz2）、crypto-nettle——nettle 路机器侧不可行：系统只有 libnettle.so.8
# 运行库、无 dev 包（pkg-config nettle 缺席），且 nettle-sys 需
# bindgen→libclang，clang 缺席。crypto-rust 为唯一纯 Rust 后端，但其
# build.rs 有三道官方闸门，须逐级 opt-in：
#   crypto-rust 本体 + allow-experimental-crypto（实验后端知情同意）
#   + allow-variable-time-crypto（非常量时间知情同意）。
# 实收闭包 238 crates（scaffold Cargo.lock 实数，超 150 惯约上限——批8 重 FFI/C
# 大物批按实记录；闭包即 2.4 全算法面：aes/rsa(num-bigint-dig)/dalek/
# p256-384-521/ml-dsa/ml-kem/slh-dsa 编译期全在、运行期仅触 Ed25519+SHA512
# 路径）。
sequoia-openpgp = { version = "=2.4.1", default-features = false, features = [
    "crypto-rust",
    "allow-experimental-crypto",
    "allow-variable-time-crypto",
] }
---
// sequoia-openpgp 2.4.1（OpenPGP RFC9580 全实现）三维差分。
// 批8 波1：cert 解析 + 固定私钥 detached 签验 + 未知算法错误面。
//
// 内嵌素材（一次性离线生成后预埋常量，生成式如下）：同特性闭包跑
// CertBuilder::new().add_userid("Mirvm Corpus <corpus@mirvm.invalid>")
//   .set_cipher_suite(Cv25519).set_creation_time(1735689600=2025-01-01)
//   .add_signing_subkey().generate()（无 password），导出 cert/TSK armor
// （armor 自带 Comment 头 = 指纹+userid，文本本身即常量）。
//
// 测试面：
//  ① ASCII-armor dearmor（armor::Reader）→ kind/rawlen/fnv；
//  ② Cert 解析字段面：证书指纹、主钥算法/创建时间、userid、签名子钥指纹/算
//     法/创建时间（全 raw Cert 无 policy 迭代，定序）；
//  ③ packet 层类型计数：PacketParser 逐包遍历，(tag号,Tag名) 进 BTreeMap 定
//     序打印（PublicKey/UserID/Signature×3/PublicSubkey）；
//  ④ 固定私钥 detached 签：TSK 解包 → policy 链（supported/alive/not-revoked
//     /for_signing）择签名子钥 → 手工拼 v4 签（见下盐坑）→ sig len/fnv 锚 +
//     重解析字段（version/typ/hash/created/issuer_fps/issuers）；
//  ⑤ DetachedVerifier streaming 验证（StandardPolicy+自供 cert helper）正反
//     例：原文 layer0:good；翻转一消息字节 layer0:bad:Bad signature；
//  ⑥ 未知算法错误面三样例（全从常量字节流内位翻注入）：
//     E1 签名包 hash_algo→99 → 构造即错 "Unsupported hash algorithm:
//        Unknown hash algorithm 99"（正名未知算法）；
//     E2 签名包 pk_algo→18(ElGamal) → 验签语义错 "Malformed signature: ...
//        not a signature algorithm"（合法算法但不可签）；
//     E3 TSK 主钥 pk_algo→101(私用段) → Cert 打包拒收 "Unsupported Cert:
//        Unsupported primary key: ..."。
//     注：公钥包算法字段在 sequoia parse 期被容忍（opaque 存储、CERT-OK），
//     错误只在使用/打包层浮出——driver 已把这个对照记进 ⑥ 的输出。
//
// 签名字节确定性【盐坑绕行】：sequoia 对 v4 密钥的签名 **无公开 opt-out 地**
// 在 hashed area 注入随机 32B notation `salt@notations.sequoia-pgp.org`
// （streaming Signer 与 SignerBuilder::sign_message/sign_hash 同走 pre_sign，
// src/packet/signature.rs 1754-1759；同名片段 set_notation 先删后加，预塞固定
// 值必被后至的随机盐覆盖——实测双连跑 sig fnv 两两不同）。绕行 = 手工拼
// RFC4880 v4 hash 构成（msg ‖ [04,typ,pk,hash,area_len,area] ‖ [04,FF,len32]）
// 走公开件 crypto::Signer::sign + SubpacketArea/Signature4 组装（语义逐行照抄
// sequoia crypto/hash.rs 677-727），产出无盐 v4 detached 签，双连跑逐字节稳
// 定。性质判定：crate 既定行为（upstream 故意不可测性设计），非引擎边界。
//
// 确定性：无 IO/时间/env/rand 进输出（唯一 rand 消费点 = 生成期，已离线固化）；
// BTreeMap 定序；时间一律 unix secs；错误打印 sequoia 静态文案；stderr 真空；
// 关键常量 assert_eq! 锚定。FRONTIER：无（盐坑属 crate 层绕行，见上）。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_sequoia_pgp.rs
//   B: cd "$(grep -l 'name = "c_sequoia_pgp"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_sequoia_pgp.rs
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

/// 内嵌素材①：公开证书（Cv25519 主钥 + Ed25519 签名子钥，v4，定创建时间）。
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

/// 内嵌素材②：同证书的私钥部分（未加密 Ed25519 标量；一次性测试材料）。
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
/// 签名创建时间钉（2025-01-02，晚于密钥创建时间 2025-01-01）。
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

/// Detached 验签；返回 (各层判定行, streaming 是否无错走完)。
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

    // ---- ① armor → 二进制 ----
    let (cert_raw, ckind) = dearmor(CERT_ARMOR);
    println!("cert armor kind={ckind:?} rawlen={} fnv={:016x}", cert_raw.len(), fnv1a(&cert_raw));
    let (tsk_raw, tkind) = dearmor(TSK_ARMOR);
    println!("tsk armor kind={tkind:?} rawlen={} fnv={:016x}", tsk_raw.len(), fnv1a(&tsk_raw));
    assert_eq!(cert_raw.len(), 944);
    assert_eq!(fnv1a(&cert_raw), 0xac8392a95c7a522d);
    assert_eq!(fnv1a(&tsk_raw), 0xddc615bf946d6d5c);

    // ---- ② cert 字段面 ----
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

    // ---- ③ packet 层类型计数 ----
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

    // ---- ④ 固定私钥手工 v4 detached 签（无 salt notation，见头注盐坑）----
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

    // ---- ⑤ streaming 验签正反例 ----
    let (verdicts, ok) = verify_detached(policy, &cert, &sig_bytes, MSG);
    println!("verify-ok run-ok={ok} verdicts={verdicts:?}");
    assert_eq!(verdicts, ["layer0:good"]);
    let mut tampered = MSG.to_vec();
    tampered[10] ^= 0x01;
    let (verdicts2, ok2) = verify_detached(policy, &cert, &sig_bytes, &tampered);
    println!("verify-tampered run-ok={ok2} verdicts={verdicts2:?}");
    assert_eq!(verdicts2, ["layer0:bad:Bad signature: Message has been manipulated"]);

    // ---- ⑥ 未知算法错误面 ----
    // 对照：公钥包算法字段在 parse 期被容忍（opaque 存储，fingerprint 照常算）。
    let mut mut_cert = cert_raw.clone();
    mut_cert[7] = 101; // 首包 v4 主钥 algo 字节（ctb=c6 + 1B len + 1B ver + 4B ctime → off7）
    let mc = Cert::from_bytes(&mut_cert)?;
    let mc_fp = mc.fingerprint().to_string();
    println!("mut-cert algo=101 parse=ok fp={mc_fp}");
    assert_eq!(mc_fp, "BE734E363361962465D1247B2795480BD5F9FF91");

    // E1：签名包 hash_algo→99，构造即拒。
    let mut s1 = sig_bytes.clone();
    s1[5] = 99; // 签名包体：1B ver + 1B typ + 1B pk_algo + 1B hash_algo（2B 头 → 体偏移 5）
    let (v1, ok1) = verify_detached(policy, &cert, &s1, MSG);
    println!("unk-sig-halgo run-ok={ok1} verdicts={v1:?}");
    assert_eq!(v1, ["err:Unsupported hash algorithm: Unknown hash algorithm 99"]);

    // E2：签名包 pk_algo→18（ElGamal;合法算法但非签名算法）。
    let mut s2 = sig_bytes.clone();
    s2[4] = 18;
    let (v2, ok2) = verify_detached(policy, &cert, &s2, MSG);
    println!("unk-sig-pkalgo run-ok={ok2} verdicts={v2:?}");
    assert_eq!(
        v2,
        ["layer0:bad:Malformed signature: Malformed packet: not a signature algorithm"]
    );

    // E3：TSK 主钥 algo→101，secret-key 打包层拒收。
    let mut mtsk = tsk_raw.clone();
    mtsk[7] = 101;
    match Cert::from_bytes(&mtsk) {
        Ok(_) => println!("unk-tsk unexpectedly ok"),
        Err(e) => println!("unk-tsk-cert err = {e}"),
    }

    Ok(())
}
