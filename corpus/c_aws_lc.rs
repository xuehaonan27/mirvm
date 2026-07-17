#!/usr/bin/env mirvm
---
[dependencies]
# aws-lc-rs =1.17.1（当前最新稳定 1.x，2026-07-17 crates.io 核实）+ default-features
# （aws-lc-sys + alloc + ring-io + ring-sig-verify；fips 非默认不开——任务口径
# 「非默认 provider 系可裁则裁」）。aws-lc-sys 显式钉 =0.42.0 且 default-features=false
# （与 aws-lc-rs 内部要求形一致——0.42.x 目前仅此一版，显式钉是防未来 0.42.1 漂移的
# 廉价保险；其 API 不直接使用）。aws-lc-sys 0.42.0 = AWS-LC C 巨物静态归档（~2000
# 文件）：本机 Linux x86_64 非 FIPS + 预制源路径走 CcBuilder（cc 直编 .c/.S，
# 无需 bindgen/perl/nasm；cmake 在场仅兜底），重 C 构建属任务明示预期。
# mirvm 侧加载通道 = native-archive「static .a → .so 闭包」（rusqlite/zstd/libgit2
# 先例）；aws-lc-sys 全符号带 `aws_lc_0_42_0_` BORINGSSL_PREFIX 前缀，与宿主
# OpenSSL 全域命名空间零碰撞（zstd 静默换库类风险不存在——已查
# generated-include/openssl/boringssl_prefix_symbols.h 实证）。guest 只做指针级
# FFI 调用（无按值聚合封送，不触 open-issues C1 结构边界；无 guest 回调传入 C，
# 不触 thunk 盲区）。
aws-lc-rs = "=1.17.1"
aws-lc-sys = { version = "=0.42.0", default-features = false }
---
// aws-lc-rs（AWS-LC C FFI 大物；批8 波1 重 FFI 条目）六族定向量三维差分。
// ★ 已修复（2026-07-17，三维全绿入册）：
//   A 维原在依赖降低期死于 native_archive lifecycle 拒装（aws-lc 全量
//   constructor/destructor section），放行后又在 EVP_AEAD* 调用链全非确定
//   撞死在 GCM 入口——三层根因与修法链（decision-history §7.8）：
//   ① constructor 分治解码：.init_array/.fini_array 一族由 loader 的
//      DT_INIT 语义原生执行（= native 进程启动期 constructor；aws-lc
//      do_library_init 意义完全一致），守卫从"全段拒"收窄为"旧式 `.init`/
//      `.fini` 裸注入段仍拒"（执行语义不可靠且真实 workload 不供养）。
//   ② `#[link_name = "\u{1}..."]`（aws-lc-sys BORINGSSL_PREFIX 全符号家族）
//      的 LLVM `\x01`=verbatim 前缀未剥除——dlsym 以加前缀名查找必然全域
//      未命中：lower 各 dlsym 口径统一 `canonical_link_name` 剥除。
//   ③ P2 GOT 对带前缀符号的键名去重度（`foreign_fn_slot`/`foreign_alloc_sym`
//      未同剥）→ fn-ptr 常量掉回烤 Imm（跨运行腐旧地址，Heisenberg 崩点）。
//   绕行遗留：无（driver 内嵌已知答案断言全量原样在跑，修复后断言成立）。
//
// 全部 key/nonce/plaintext/iki/sig 为源码内嵌固定常量；所用算法（SHA-2/HMAC/
// HKDF/AES-GCM/Ed25519/RSA-PKCS1v15 验签）均为零随机 API（不触 RAND），
// 输出跨进程完全确定。定向量真值来源（创建期宿主交叉核验）：
//   SHA-256/SHA-512 空串/abc/双多block向量 = FIPS 180-4 经典已知答案；
//   HMAC-SHA256 两例 = RFC 4231 TC1/TC2；HKDF-SHA256 = RFC 5869 TC1
//    （宿主 python3 hashlib/hmac 实算复核一致）；
//   AES-256-GCM = McGrew/Viega GCM 256-bit key 60B pt 向量（宿主 OpenSSL
//     3.x EVP 实算锚定 ct||tag，双跑 Δct==Δpt 密钥流一致性另证）；
//   Ed25519 = 固定种子 0x5A*32（RFC 8032 纯确定性签名；宿主 cryptography
//     库实算锚定 pk/sig 已知答案）；
//   RSA-2048 = 构建期 openssl CLI 生成的固定密钥（n/e 组件内嵌，
//     PKCS1v15+SHA-256 签名为确定性填充，签名人本体不进输出路径，只打印
//     验签布尔）。
// 测试面：
//   ① digest：SHA-256 空串/abc/56B 双 block 向量 hex 打印 + 已知答案
//      assert_eq；Context 分片流式（奇数切点，含 0 长 update）== one-shot
//      布尔；SHA-512 abc + 112B 双 block 向量 hex + 已知答案 assert_eq；
//      SHA-512 亦做分片流式一致性布尔；digest 输出长度断言。
//   ② hmac：RFC4231 TC1/TC2 Tag hex 打印 + 已知答案 assert_eq +
//      hmac::verify 正例布尔 + 篡改末字节反例布尔。
//   ③ hkdf：RFC5869 TC1 Salt::extract → Prk（opaque 不可打印）→ expand
//      （单段 info；KeyType 为公开 trait，本地实现 42B 定长类型——aws-lc-rs
//      无内建 42B 长度类型，内建 Algorithm 的 len 恒为 digest 输出长 32）
//      → fill 42B OKM hex 打印 + 已知答案 assert_eq；另测 expand 越界
//      （len > 255*HashLen 于 expand 期即 Err）布尔。
//   ④ aead：AES-256-GCM 固定 key/nonce/aad/pt 60B，
//      seal_in_place_append_tag → ct(60B) hex + tag(16B) hex 打印 +
//      已知答案 assert_eq；open_in_place roundtrip == pt 布尔；
//      篡改 ct 末字节 → open Err 布尔；错 aad → open Err 布尔；
//      UnboundKey 错 key 长（31B）→ Err 布尔。
//   ⑤ ed25519：from_seed_unchecked(0x5A*32) → pk hex + sign(msg) sig hex
//      打印 + 已知答案 assert_eq；UnparsedPublicKey verify 正例布尔 +
//      错消息反例布尔 + 篡改 sig 反例布尔。
//   ⑥ rsa：RsaPublicKeyComponents{n,e}（hex 内嵌、运行期解码）verify
//      PKCS1v15+SHA-256 正例布尔 + 篡改 sig 末字节反例布尔 + 错消息反例
//      布尔 + 篡改 n（末字节 xor 1，公钥-签名失配）反例布尔——全部经
//      aws-lc-rs 的 build_rsa → EVP 真实验签路径。
// 确定性：常量全内嵌；无 HashMap 序/RNG/时间/线程/env/路径入输出；错误一律
// 归约为布尔（aws-lc-rs 错误为 Unspecified 单态，无文案面）；stdout 28 行
// 全 hex/布尔，stderr 真空（driver 零 warning；B 维 cargo run -q 实测）。
// 三维复跑：
//   A: target/release/mirvm run corpus/c_aws_lc.rs
//   B: cd "$(grep -l 'name = "c_aws_lc"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_aws_lc.rs
use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use aws_lc_rs::signature::{
    Ed25519KeyPair, KeyPair, RsaPublicKeyComponents, UnparsedPublicKey, ED25519,
    RSA_PKCS1_2048_8192_SHA256,
};
use aws_lc_rs::{digest, hkdf, hmac};

fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unhex(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    assert_eq!(b.len() % 2, 0);
    (0..b.len() / 2)
        .map(|i| {
            let v = |c: u8| -> u8 {
                match c {
                    b'0'..=b'9' => c - b'0',
                    b'a'..=b'f' => c - b'a' + 10,
                    _ => panic!("bad hex"),
                }
            };
            v(b[2 * i]) << 4 | v(b[2 * i + 1])
        })
        .collect()
}

fn main() {
    // ---------- ① digest：SHA-256 / SHA-512 定向量 + 流式一致性 ----------
    let s256_empty = digest::digest(&digest::SHA256, b"");
    assert_eq!(
        hex(s256_empty.as_ref()),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    println!("sha256(\"\")   = {}", hex(s256_empty.as_ref()));

    let s256_abc = digest::digest(&digest::SHA256, b"abc");
    assert_eq!(
        hex(s256_abc.as_ref()),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    println!("sha256(abc)  = {}", hex(s256_abc.as_ref()));

    const M56: &[u8] = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    let s256_m56 = digest::digest(&digest::SHA256, M56);
    assert_eq!(
        hex(s256_m56.as_ref()),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    println!("sha256(m56)  = {}", hex(s256_m56.as_ref()));

    // 分片流式（奇数切点 + 0 长 update）必须等于 one-shot。
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(&M56[..1]);
    ctx.update(b"");
    ctx.update(&M56[1..40]);
    ctx.update(&M56[40..]);
    let s256_stream = ctx.finish();
    println!("sha256 stream eq = {}", s256_stream.as_ref() == s256_m56.as_ref());

    let s512_abc = digest::digest(&digest::SHA512, b"abc");
    assert_eq!(
        hex(s512_abc.as_ref()),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    println!("sha512(abc)  = {}", hex(s512_abc.as_ref()));

    const M112: &[u8] = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
    let s512_m112 = digest::digest(&digest::SHA512, M112);
    assert_eq!(
        hex(s512_m112.as_ref()),
        "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
    );
    println!("sha512(m112) = {}", hex(s512_m112.as_ref()));

    let mut ctx = digest::Context::new(&digest::SHA512);
    ctx.update(&M112[..7]);
    ctx.update(&M112[7..100]);
    ctx.update(&M112[100..]);
    let s512_stream = ctx.finish();
    println!("sha512 stream eq = {}", s512_stream.as_ref() == s512_m112.as_ref());

    assert_eq!(digest::SHA256.output_len(), 32);
    assert_eq!(digest::SHA512.output_len(), 64);

    // ---------- ② hmac：RFC 4231 TC1/TC2 ----------
    let k1 = hmac::Key::new(hmac::HMAC_SHA256, &[0x0b_u8; 20]);
    let t1 = hmac::sign(&k1, b"Hi There");
    assert_eq!(
        hex(t1.as_ref()),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    println!("hmac tc1     = {}", hex(t1.as_ref()));
    println!(
        "hmac tc1 verify = {}",
        hmac::verify(&k1, b"Hi There", t1.as_ref()).is_ok()
    );
    let mut bad1 = t1.as_ref().to_vec();
    *bad1.last_mut().unwrap() ^= 1;
    println!("hmac tamper reject = {}", hmac::verify(&k1, b"Hi There", &bad1).is_err());

    let k2 = hmac::Key::new(hmac::HMAC_SHA256, b"Jefe");
    let t2 = hmac::sign(&k2, b"what do ya want for nothing?");
    assert_eq!(
        hex(t2.as_ref()),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    println!("hmac tc2     = {}", hex(t2.as_ref()));

    // ---------- ③ hkdf：RFC 5869 TC1 ----------
    // aws-lc-rs 的 Okm 长度由 KeyType 绑定（无 42B 内建类型）；KeyType 是公开
    // trait，本地实现 42B 与越界 8161B（=255*32+1，HKDF_expand 必拒）两个类型。
    struct Okm42;
    impl hkdf::KeyType for Okm42 {
        fn len(&self) -> usize {
            42
        }
    }
    struct Okm8161;
    impl hkdf::KeyType for Okm8161 {
        fn len(&self) -> usize {
            32 * 255 + 1
        }
    }
    let hkdf_info = unhex("f0f1f2f3f4f5f6f7f8f9");
    let hkdf_infos: [&[u8]; 1] = [&hkdf_info[..]];
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, &unhex("000102030405060708090a0b0c"));
    let prk = salt.extract(&[0x0b_u8; 22]);
    let okm = prk.expand(&hkdf_infos, Okm42).unwrap();
    let mut out = [0u8; 42];
    okm.fill(&mut out).unwrap();
    assert_eq!(
        hex(&out),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
    println!("hkdf okm42   = {}", hex(&out));
    // 越界长度在 expand 期即拒（len > 255*HashLen → Unspecified）。
    println!("hkdf oversize err = {}", prk.expand(&hkdf_infos, Okm8161).is_err());

    // ---------- ④ aead：AES-256-GCM 固定向量 + roundtrip + 篡改 ----------
    let key = unhex("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308");
    let nonce = unhex("cafebabefacedbaddecaf888");
    let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
    let pt = unhex(concat!(
        "d9313225f88406e5a55909c5aff5269a",
        "86a7a9538534f7da1e4c303d2a318a72",
        "8c3c0c95156809539fcf0e2429a6b525",
        "b16aedf5aa0de657ba637b39"
    ));
    println!("gcm key err  = {}", UnboundKey::new(&AES_256_GCM, &key[..31]).is_err());
    let unbound = UnboundKey::new(&AES_256_GCM, &key).unwrap();
    let sealing = LessSafeKey::new(unbound);
    let mut in_out = pt.clone();
    sealing
        .seal_in_place_append_tag(
            Nonce::try_assume_unique_for_key(&nonce).unwrap(),
            Aad::from(&aad),
            &mut in_out,
        )
        .unwrap();
    let (ct, tag) = in_out.split_at(pt.len());
    assert_eq!(
        hex(ct),
        concat!(
            "522dc1f099567d07f47f37a32a8442",
            "7d643a8cdc2fe5c0c94598a2bd8555",
            "d1aa1cb08e48d90dbb3d17b08b1036",
            "828838c5f61e6393ba7a0abcc9f662"
        )
    );
    println!("gcm ct       = {}", hex(ct));
    assert_eq!(hex(tag), "d7ad50d72c894c3afb3a98304418cf1d");
    println!("gcm tag      = {}", hex(tag));

    let mut buf = in_out.clone();
    let opened = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).unwrap())
        .open_in_place(
            Nonce::try_assume_unique_for_key(&nonce).unwrap(),
            Aad::from(&aad),
            &mut buf,
        )
        .unwrap();
    println!("gcm roundtrip eq = {}", opened == &pt[..]);

    let mut tampered = in_out.clone();
    let n = tampered.len();
    tampered[n - 17] ^= 1; // 篡改 ct 末字节（tag 前一字节）
    println!(
        "gcm tamper reject = {}",
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).unwrap())
            .open_in_place(
                Nonce::try_assume_unique_for_key(&nonce).unwrap(),
                Aad::from(&aad),
                &mut tampered,
            )
            .is_err()
    );
    let mut wrong_aad = in_out.clone();
    println!(
        "gcm wrong-aad reject = {}",
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).unwrap())
            .open_in_place(
                Nonce::try_assume_unique_for_key(&nonce).unwrap(),
                Aad::from(b"wrong aad"),
                &mut wrong_aad,
            )
            .is_err()
    );

    // ---------- ⑤ ed25519：固定种子 sign/verify ----------
    let pair = Ed25519KeyPair::from_seed_unchecked(&[0x5a_u8; 32]).unwrap();
    let pk = pair.public_key();
    assert_eq!(
        hex(pk.as_ref()),
        "0d7550754e0800a5d237eef5826035766b9b3e5a15868a940ab289958788e3b0"
    );
    println!("ed25519 pk   = {}", hex(pk.as_ref()));

    const ED_MSG: &[u8] = b"mirvm aws-lc corpus: ed25519 fixed message\n";
    let sig = pair.sign(ED_MSG);
    assert_eq!(
        hex(sig.as_ref()),
        concat!(
            "6ce9b9cb1a242d9d041628d28467f13f8fee5d018bfda92a6b7bbeed97246f3f",
            "8c0bf3904558306f6e91dc88298d1d31ff8a58adca2a49bada3afaecdb15f502"
        )
    );
    println!("ed25519 sig  = {}", hex(sig.as_ref()));

    let pub_key = UnparsedPublicKey::new(&ED25519, pk.as_ref());
    println!("ed25519 verify = {}", pub_key.verify(ED_MSG, sig.as_ref()).is_ok());
    println!(
        "ed25519 wrong-msg reject = {}",
        pub_key.verify(b"mirvm aws-lc corpus: ed25519 WRONG message\n", sig.as_ref()).is_err()
    );
    let mut bad_sig = sig.as_ref().to_vec();
    *bad_sig.last_mut().unwrap() ^= 1;
    println!("ed25519 tamper reject = {}", pub_key.verify(ED_MSG, &bad_sig).is_err());

    // ---------- ⑥ rsa：固定公钥组件 PKCS1v15+SHA-256 验签 ----------
    let rsa_n = unhex(concat!(
        "ed2114c8d51696810dec038eea795019c485d983135cc98826924f25c474f4637e",
        "58375a77fd3c854f7dae47303ffe3996d6a2aacb23c4a0f02111077ecfa1597179",
        "f57e4e835874d35e90877310338b901e1eec9bed9edb67ddc46a4b413f2580798a",
        "922f120390fda37d78dbf79c78e4c0e20609b9b11c0b1731236e6ea32e4f11e67e",
        "9112dd635068a5164b79b6068896452bbedba08fae425e12b35a0f52e6ae28bc99",
        "477f9e3c875f3cf6d6c0c2a33113d9d05dab290f91da8bb0eee396c17cbb1c675e",
        "59c11ae629e4312632e761b733f908bf4d408ff1161e0702c32a02f7d256b38e0e",
        "480375f7d0805c989ca4e325c5b6aeb11c0c16df03fde518df"
    ));
    let rsa_sig = unhex(concat!(
        "48bbee05010e1600bdd1647b234e602e91aa96ced68f2bafa7f0e89f105849a5f0",
        "a8567dfd0216ad05ddccae3c4ceac5db587578bce79ec19dfb794dc59ff9c51e1f",
        "10f3fd934161eed159e1cf628ae4e5bec997777ef8aaad9d53355a2736a278a99f",
        "6da62b21b546aa50a40f0815a3a68186e8cbd324e9f2580863a8d881faae089d2c",
        "9e119cff26b73164bd2256e07a2927231f91687065000aec6c256732c5b25a676e",
        "6000fffd2d5ab15d5cec9e68564115997986dddf3c8f5f8e677c835a4afa595ac1",
        "a86305dddcb9802f7ffe0ac279bfc5b43d3af351010128f3ac26630f1742f1c664",
        "db2860193843ff00c311b48096b93c1acbb31e3d5617774823"
    ));
    const RSA_MSG: &[u8] = b"mirvm aws-lc corpus: fixed RSA-2048 PKCS1v15-SHA256 message\n";
    let components = RsaPublicKeyComponents {
        n: &rsa_n[..],
        e: &[0x01, 0x00, 0x01][..],
    };
    println!(
        "rsa verify = {}",
        components.verify(&RSA_PKCS1_2048_8192_SHA256, RSA_MSG, &rsa_sig).is_ok()
    );
    let mut bad_rsa_sig = rsa_sig.clone();
    *bad_rsa_sig.last_mut().unwrap() ^= 1;
    println!(
        "rsa tamper-sig reject = {}",
        components.verify(&RSA_PKCS1_2048_8192_SHA256, RSA_MSG, &bad_rsa_sig).is_err()
    );
    println!(
        "rsa wrong-msg reject = {}",
        components
            .verify(&RSA_PKCS1_2048_8192_SHA256, b"mirvm aws-lc corpus: WRONG message\n", &rsa_sig)
            .is_err()
    );
    let mut bad_n = rsa_n.clone();
    *bad_n.last_mut().unwrap() ^= 1;
    let bad_components = RsaPublicKeyComponents {
        n: &bad_n[..],
        e: &[0x01, 0x00, 0x01][..],
    };
    println!(
        "rsa wrong-key reject = {}",
        bad_components.verify(&RSA_PKCS1_2048_8192_SHA256, RSA_MSG, &rsa_sig).is_err()
    );
}
