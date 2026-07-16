#!/usr/bin/env mirvm
---
[dependencies]
k256 = { version = "0.13", features = ["ecdh"] }
# sha3 仅为复现 k256 内嵌的 ethereum RFC6979 定向量（Keccak256 摘要 +
# HMAC-DRBG-Keccak256 nonce）——该向量是 k256 src/ecdsa.rs 文档与测试
# 共同锚定的公开高层确定性签名向量。
sha3 = "0.10"
---
// k256 0.13（secp256k1，u128 域/标量算术 + precomputed-tables）差分：
// 固定标量 → SecretKey → 公钥 SEC1 压缩/非压缩；RFC6979 确定性 ECDSA
// 签名/正反 verify；ECDH 双固定密钥两路 shared；BIP340 schnorr 定向量。
//
// 定向量自查（全部来自 k256 0.13.4 内嵌测试/文档与 BIP340 官方 CSV，
// 常量逐字节抄录、大小写归一）：
// - src/test_vectors/ecdsa.rs：(d, q_x, q_y, k, m, r, s) —— 公钥坐标锚定
//   ①；hazmat SignPrimitive::try_sign_prehashed 显式 nonce 复现 r/s ④。
//   （注：嵌入 k 并非 RFC6979-SHA256(d, m) 的 nonce，故 ③ 的 sign_prehash
//   输出与之不同路径——两条路径各自独立锚定。）
// - src/ecdsa.rs 文档 ethereum 向量：sign_digest_recoverable(Keccak256)
//   复现已知签名 + recid=0 + recover roundtrip ③。
// - src/ecdsa.rs RECOVERY_TEST_VECTORS ×2：recover_from_digest(Sha256)
//   恢复压缩公钥逐字节比对 ③。
// - src/ecdsa.rs normalize 向量对：s_high.normalize_s() == s_low ④。
// - k256 特有：VerifyPrimitive 拒绝 high-s 签名（bitcoin 约定）④。
// - BIP340 CSV index 0-3 签名+验签 TRUE、index 5 公钥不在曲线上（解析
//   错误路径）、index 6 has_even_y(R)=false 验签 FALSE ⑥。
// 确定性：全部输入为固定字节；ECDSA 走 RFC6979、schnorr 用显式 aux_rand，
// 不碰 RNG；只打印 hex/布尔/计数。
use k256::{
    ecdh::diffie_hellman,
    ecdsa::{
        hazmat::SignPrimitive,
        signature::{
            hazmat::{PrehashSigner, PrehashVerifier},
            DigestSigner, DigestVerifier, Signer, Verifier,
        },
        RecoveryId, Signature, SigningKey, VerifyingKey,
    },
    elliptic_curve::{scalar::IsHigh, sec1::ToEncodedPoint},
    sha2::{Digest, Sha256},
    EncodedPoint, FieldBytes, NonZeroScalar, PublicKey, SecretKey,
};
use sha3::Keccak256;

fn hex(bytes: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(T[(b >> 4) as usize] as char);
        s.push(T[(b & 0x0f) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex len");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// k256 内嵌 ECDSA 测试向量（src/test_vectors/ecdsa.rs）。
struct EmbeddedVec;
impl EmbeddedVec {
    const D: &'static str = "ebb2c082fd7727890a28ac82f6bdf97bad8de9f5d7c9028692de1a255cad3e0f";
    const QX: &'static str = "779dd197a5df977ed2cf6cb31d82d43328b790dc6b3b7d4437a427bd5847dfcd";
    const QY: &'static str = "e94b724a555b6d017bb7607c3e3281daf5b1699d6ef4124975c9237b917d426f";
    const K: &'static str = "49a0d7b786ec9cde0d0721d72804befd06571c974b191efb42ecf322ba9ddd9a";
    const M: &'static str = "4b688df40bcedbe641ddb16ff0a1842d9c67ea1c3bf63f3e0471baa664531d1a";
    const R: &'static str = "241097efbf8b63bf145c8961dbdf10c310efbb3b2676bbc0f8b08505c9e2f795";
    const S: &'static str = "021006b7838609339e8b415a7f9acb1b661828131aef1ecbc7955dfb01f3ca0e";
}

/// BIP340 官方 CSV 向量（index 0-3 签名/验签 TRUE；5 公钥不在曲线上；
/// 6 has_even_y(R)=false 验签 FALSE）。
struct Bip340 {
    sk: &'static str,
    pk: &'static str,
    aux: &'static str,
    msg: &'static str,
    sig: &'static str,
}

const BIP340_SIGN: &[Bip340] = &[
    Bip340 {
        sk: "0000000000000000000000000000000000000000000000000000000000000003",
        pk: "f9308a019258c31049344f85f89d5229b531c845836f99b08601f113bce036f9",
        aux: "0000000000000000000000000000000000000000000000000000000000000000",
        msg: "0000000000000000000000000000000000000000000000000000000000000000",
        sig: "e907831f80848d1069a5371b402410364bdf1c5f8307b0084c55f1ce2dca821525f66a4a85ea8b71e482a74f382d2ce5ebeee8fdb2172f477df4900d310536c0",
    },
    Bip340 {
        sk: "b7e151628aed2a6abf7158809cf4f3c762e7160f38b4da56a784d9045190cfef",
        pk: "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659",
        aux: "0000000000000000000000000000000000000000000000000000000000000001",
        msg: "243f6a8885a308d313198a2e03707344a4093822299f31d0082efa98ec4e6c89",
        sig: "6896bd60eeae296db48a229ff71dfe071bde413e6d43f917dc8dcf8c78de33418906d11ac976abccb20b091292bff4ea897efcb639ea871cfa95f6de339e4b0a",
    },
    Bip340 {
        sk: "c90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b14e5c9",
        pk: "dd308afec5777e13121fa72b9cc1b7cc0139715309b086c960e18fd969774eb8",
        aux: "c87aa53824b4d7ae2eb035a2b5bbbccc080e76cdc6d1692c4b0b62d798e6d906",
        msg: "7e2d58d8b3bcdf1abadec7829054f90dda9805aab56c77333024b9d0a508b75c",
        sig: "5831aaeed7b44bb74e5eab94ba9d4294c49bcf2a60728d8b4c200f50dd313c1bab745879a5ad954a72c45a91c3a51d3c7adea98d82f8481e0e1e03674a6f3fb7",
    },
    // index 3：msg/aux 全 0xff——若实现把 msg 模 p/n 约减则复现不出此签名
    Bip340 {
        sk: "0b432b2677937381aef05bb02a66ecd012773062cf3fa2549e44f58ed2401710",
        pk: "25d1dff95105f5253c4022f628a996ad3a0d95fbf21d468a1b33f8c160d8f517",
        aux: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        msg: "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        sig: "7eb0509757e246f19449885651611cb965ecc1a187dd51b64fda1edc9637d5ec97582b9cb13db3933705b32ba982af5af25fd78881ebb32771fc5922efc66ea3",
    },
];

fn main() {
    // ---- ① 固定标量 → SecretKey → SEC1（嵌入向量 d 锚定公钥坐标）----
    let k1 = unhex(EmbeddedVec::D);
    let mut k2 = [0u8; 32];
    for (i, b) in k2.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(17).wrapping_add(3);
    }

    let sk1 = SecretKey::from_slice(&k1).unwrap();
    let sk2 = SecretKey::from_slice(&k2).unwrap();
    println!("sk1 scalar roundtrip = {}", sk1.to_bytes()[..] == k1[..]);
    println!("sk2 scalar = {}", hex(&sk2.to_bytes()));
    // 错误路径：零标量 / >= n 的标量必须被拒绝
    println!("zero scalar err = {}", SecretKey::from_slice(&[0u8; 32]).is_err());
    println!("ff..ff scalar err = {}", SecretKey::from_slice(&[0xffu8; 32]).is_err());

    let pk1 = sk1.public_key();
    let pk2 = sk2.public_key();
    let ep1u = pk1.to_encoded_point(false);
    let ep1c = pk1.to_encoded_point(true);
    println!("pk1 sec1 uncompressed = {}", hex(ep1u.as_bytes()));
    println!("pk1 sec1 compressed   = {}", hex(ep1c.as_bytes()));
    println!("pk2 sec1 compressed   = {}", hex(pk2.to_encoded_point(true).as_bytes()));
    let pk1_back = PublicKey::from_sec1_bytes(ep1c.as_bytes()).unwrap();
    println!("pk1 sec1 parse roundtrip = {}", pk1 == pk1_back);
    println!("pk1 x match embedded q_x = {}", hex(ep1u.x().unwrap()) == EmbeddedVec::QX);
    println!("pk1 y match embedded q_y = {}", hex(ep1u.y().unwrap()) == EmbeddedVec::QY);
    // 错误路径：非法 SEC1 前缀
    let mut bad_ep = [0u8; 65];
    bad_ep[0] = 0x05;
    println!("bad sec1 prefix err = {}", PublicKey::from_sec1_bytes(&bad_ep).is_err());

    // ---- ② RFC6979 高层 sign/verify 正反例 ----
    let signing1 = SigningKey::from_slice(&k1).unwrap();
    let verifying1 = VerifyingKey::from(&signing1);
    let msgs: [&[u8]; 3] = [
        b"sample",
        b"test",
        b"mirvm differential corpus: k256 ecdsa/rfc6979",
    ];
    for (i, msg) in msgs.iter().enumerate() {
        let sig: Signature = signing1.sign(msg);
        let sig_again: Signature = signing1.sign(msg);
        println!(
            "sig[{i}] r = {}\nsig[{i}] s = {}",
            hex(&sig.r().to_bytes()),
            hex(&sig.s().to_bytes())
        );
        println!("sig[{i}] deterministic = {}", sig == sig_again);
        println!("sig[{i}] der = {}", hex(sig.to_der().as_bytes()));
        println!("sig[{i}] verify ok = {}", verifying1.verify(msg, &sig).is_ok());
        // 反例 A：消息错
        println!(
            "sig[{i}] verify wrong-msg ok = {}",
            verifying1.verify(b"wrong message", &sig).is_ok()
        );
        // 反例 B：签名被篡改（翻转 s 末字节）
        let mut bad = sig.to_bytes();
        let n = bad.len();
        bad[n - 1] ^= 0x01;
        match Signature::from_slice(&bad) {
            Ok(bs) => println!(
                "sig[{i}] verify tampered ok = {}",
                verifying1.verify(msg, &bs).is_ok()
            ),
            Err(_) => println!("sig[{i}] tampered sig rejected at parse"),
        }
    }
    // 反例 C：别的公钥
    let signing2 = SigningKey::from_slice(&k2).unwrap();
    let verifying2 = VerifyingKey::from(&signing2);
    let sig1: Signature = signing1.sign(b"sample");
    println!(
        "verify with wrong key ok = {}",
        verifying2.verify(b"sample", &sig1).is_ok()
    );
    let vk1_back = VerifyingKey::from_sec1_bytes(ep1c.as_bytes()).unwrap();
    println!("vk1 sec1 roundtrip = {}", verifying1 == vk1_back);

    // ---- ③ RFC6979 定向量自查（k256 内嵌）----
    // ethereum 端到端向量（src/ecdsa.rs 文档+测试）：Keccak256 摘要，
    // RFC6979 HMAC-DRBG-Keccak256 nonce → 已知签名 + recid 0。
    let eth_sk = SigningKey::from_slice(&unhex(
        "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
    ))
    .unwrap();
    let eth_msg = unhex(
        "e9808504e3b29200831e848094f0109fc8df283027b6285cc889f5aa624eac1f55843b9aca0080018080",
    );
    let eth_digest = Keccak256::new_with_prefix(&eth_msg);
    let (eth_sig, eth_recid) = eth_sk.sign_digest_recoverable(eth_digest.clone()).unwrap();
    println!(
        "eth vec sig match = {}",
        hex(&eth_sig.to_bytes())
            == "c9cf86333bcb065d140032ecaab5d9281bde80f21b9687b3e94161de42d51895\
                727a108a0b8d101465414033c3f705a9c7b826e596766046ee1183dbc8aeaa68"
    );
    println!("eth vec recid = {}", eth_recid.to_byte());
    let eth_vk = VerifyingKey::recover_from_digest(eth_digest.clone(), &eth_sig, eth_recid).unwrap();
    println!("eth vec recover eq = {}", &eth_vk == eth_sk.verifying_key());
    println!("eth vec verify_digest = {}", eth_vk.verify_digest(eth_digest, &eth_sig).is_ok());

    // RECOVERY_TEST_VECTORS ×2（src/ecdsa.rs 内嵌，Sha256 摘要）
    for (i, (pk_hex, msg, sig_hex, recid_byte)) in [
        (
            "021a7a569e91dbf60581509c7fc946d1003b60c7dee85299538db6353538d59574",
            "example message",
            "ce53abb3721bafc561408ce8ff99c909f7f0b18a2f788649d6470162ab1aa032\
             3971edc523a6d6453f3fb6128d318d9db1a5ff3386feb1047d9816e780039d52",
            0u8,
        ),
        (
            "036d6caac248af96f6afa7f904f550253a0f3ef3f5aa2fe6838a95b216691468e2",
            "example message",
            "46c05b6368a44b8810d79859441d819b8e7cdc8bfd371e35c53196f4bcacdb51\
             35c7facce2a97b95eacba8a586d87b7958aaf8368ab29cee481f76e871dbd9cb",
            1u8,
        ),
    ]
    .iter()
    .enumerate()
    {
        let sig = Signature::from_slice(&unhex(sig_hex)).unwrap();
        let recid = RecoveryId::from_byte(*recid_byte).unwrap();
        let pk =
            VerifyingKey::recover_from_digest(Sha256::new_with_prefix(msg.as_bytes()), &sig, recid)
                .unwrap();
        println!(
            "recovery[{i}] pk match = {}",
            hex(EncodedPoint::from(&pk).as_bytes()) == *pk_hex
        );
    }

    // RFC6979-SHA256 prehash 路径（独立锚定；与 ④ 的显式 nonce 不同路径）
    let m = unhex(EmbeddedVec::M);
    let prehash_sig: Signature = signing1.sign_prehash(&m).unwrap();
    println!("rfc6979 prehash r = {}", hex(&prehash_sig.r().to_bytes()));
    println!("rfc6979 prehash s = {}", hex(&prehash_sig.s().to_bytes()));
    println!(
        "rfc6979 prehash verify ok = {}",
        verifying1.verify_prehash(&m, &prehash_sig).is_ok()
    );
    // DigestSigner 路径（RFC6979 HMAC-DRBG-SHA256 over Sha256(msg)）
    let dsig: Signature = signing1.sign_digest(Sha256::new_with_prefix(b"digest-signing"));
    println!("digest-sign r = {}", hex(&dsig.r().to_bytes()));
    println!(
        "digest-sign verify ok = {}",
        verifying1
            .verify_digest(Sha256::new_with_prefix(b"digest-signing"), &dsig)
            .is_ok()
    );

    // ---- ④ hazmat 复现 k256 内嵌向量（显式 nonce）+ high-s 谱系 ----
    let fb = |b: &[u8]| {
        let mut z = FieldBytes::default();
        z.copy_from_slice(b);
        z
    };
    let d_nz = NonZeroScalar::from_repr(fb(&k1)).unwrap();
    let k_nz: k256::Scalar = *NonZeroScalar::from_repr(fb(&unhex(EmbeddedVec::K)))
        .unwrap()
        .as_ref();
    let z: FieldBytes = fb(&m);
    let (hz_sig, hz_recid) = d_nz.as_ref().try_sign_prehashed(k_nz, &z).unwrap();
    println!("hazmat vec r match = {}", hex(&hz_sig.r().to_bytes()) == EmbeddedVec::R);
    println!("hazmat vec s match = {}", hex(&hz_sig.s().to_bytes()) == EmbeddedVec::S);
    println!("hazmat vec recid = {}", hz_recid.unwrap().to_byte());
    println!(
        "hazmat vec verify ok = {}",
        verifying1.verify_prehash(&m, &hz_sig).is_ok()
    );
    // k256 特有：VerifyPrimitive 拒绝 high-s（bitcoin 约定）。取上例签名
    // 的 s 取负（s' = n - s）得 high-s 签名——同一 (r, z) 下必须验签失败。
    let hi_sig = Signature::from_scalars(hz_sig.r(), -hz_sig.s()).unwrap();
    println!("orig s is_high = {}", bool::from(hz_sig.s().is_high()));
    println!("negated s is_high = {}", bool::from(hi_sig.s().is_high()));
    println!(
        "high-s verify ok = {}",
        verifying1.verify_prehash(&m, &hi_sig).is_ok()
    );
    println!("high-s normalize eq orig = {}", hi_sig.normalize_s().unwrap() == hz_sig);
    println!("low-s normalize none = {}", hz_sig.normalize_s().is_none());
    // 内嵌 normalize 向量对（src/ecdsa.rs，rust-secp256k1 生成）
    let emb_hi = Signature::from_slice(&unhex(
        "20c01a910ebb2610af2d763fa09b3b30923c8e408b11df2c61ad76d970a2f1bc\
         ee2f11ef8cb00a49617d1357f4d55641090a48f201e9b959c48f6f6bec6f938f",
    ))
    .unwrap();
    let emb_lo = Signature::from_slice(&unhex(
        "20c01a910ebb2610af2d763fa09b3b30923c8e408b11df2c61ad76d970a2f1bc\
         11d0ee10734ff5b69e82eca80b2aa9bdb1a493f4ad5ee6e1fb42ef20e3c6adb2",
    ))
    .unwrap();
    println!("embedded s_hi.is_high = {}", bool::from(emb_hi.s().is_high()));
    println!("embedded s_lo.is_high = {}", bool::from(emb_lo.s().is_high()));
    println!("embedded normalize eq = {}", emb_hi.normalize_s().unwrap() == emb_lo);

    // ---- ⑤ ECDH：双固定密钥两路 shared 相等 ----
    let ab = diffie_hellman(sk1.to_nonzero_scalar(), pk2.as_affine());
    let ba = diffie_hellman(sk2.to_nonzero_scalar(), pk1.as_affine());
    println!("ecdh ab = {}", hex(ab.raw_secret_bytes()));
    println!("ecdh two-way equal = {}", ab.raw_secret_bytes() == ba.raw_secret_bytes());
    let aa = diffie_hellman(sk1.to_nonzero_scalar(), pk1.as_affine());
    println!("ecdh self = {}", hex(aa.raw_secret_bytes()));
    println!("ecdh self != ab = {}", aa.raw_secret_bytes() != ab.raw_secret_bytes());

    // ---- ⑥ Schnorr BIP340 定向量 ----
    for (i, v) in BIP340_SIGN.iter().enumerate() {
        let sk = k256::schnorr::SigningKey::from_bytes(&unhex(v.sk)).unwrap();
        let aux: [u8; 32] = unhex(v.aux).try_into().unwrap();
        let msg = unhex(v.msg);
        println!(
            "bip340[{i}] pk match = {}",
            hex(&sk.verifying_key().to_bytes()) == v.pk
        );
        let sig = sk.sign_raw(&msg, &aux).unwrap();
        println!("bip340[{i}] sig = {}", hex(&sig.to_bytes()));
        println!("bip340[{i}] sig match = {}", hex(&sig.to_bytes()) == v.sig);
        let vk = k256::schnorr::VerifyingKey::from_bytes(&unhex(v.pk)).unwrap();
        println!("bip340[{i}] verify ok = {}", vk.verify_raw(&msg, &sig).is_ok());
        // 官方 CSV 验签 TRUE 也覆盖「签名者私钥与验签公钥分离解析」路径
        let expected_sig = k256::schnorr::Signature::try_from(unhex(v.sig).as_slice()).unwrap();
        println!(
            "bip340[{i}] verify expected-sig ok = {}",
            vk.verify_raw(&msg, &expected_sig).is_ok()
        );
    }
    // index 5：公钥不在曲线上 → 解析即失败
    println!(
        "bip340[5] bad pubkey parse err = {}",
        k256::schnorr::VerifyingKey::from_bytes(&unhex(
            "eefdea4cdb677750a420fee807eacf21eb9898ae79b9768766e4faa04a2d4a34"
        ))
        .is_err()
    );
    // index 6：has_even_y(R)=false → 验签 FALSE
    let vk6 = k256::schnorr::VerifyingKey::from_bytes(&unhex(
        "dff1d77f2a671c5f36183726db2341be58feae1da2deced843240f7b502ba659",
    ))
    .unwrap();
    let sig6 = k256::schnorr::Signature::try_from(
        unhex(
            "fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556\
             3cc27944640ac607cd107ae10923d9ef7a73c643e166be5ebeafa34b1ac553e2",
        )
        .as_slice(),
    )
    .unwrap();
    println!(
        "bip340[6] verify odd-R.y ok = {}",
        vk6.verify_raw(&unhex("243f6a8885a308d313198a2e03707344a4093822299f31d0082efa98ec4e6c89"), &sig6)
            .is_ok()
    );
    // 篡改负例：翻转 bip340[0] 签名 s 末字节
    let mut bad_schnorr = unhex(BIP340_SIGN[0].sig);
    let n = bad_schnorr.len();
    bad_schnorr[n - 1] ^= 0x01;
    match k256::schnorr::Signature::try_from(bad_schnorr.as_slice()) {
        Ok(bs) => {
            let vk0 =
                k256::schnorr::VerifyingKey::from_bytes(&unhex(BIP340_SIGN[0].pk)).unwrap();
            println!(
                "schnorr tamper verify ok = {}",
                vk0.verify_raw(&unhex(BIP340_SIGN[0].msg), &bs).is_ok()
            );
        }
        Err(_) => println!("schnorr tampered sig rejected at parse"),
    }
}
