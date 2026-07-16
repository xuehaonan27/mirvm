#!/usr/bin/env mirvm
---
[dependencies]
openssl = "0.10"
# 仅用于 FRONTIER 绕行里的显式 dlopen 预载（见下方 ★ 记录）；不碰其它 API。
libc = "0.2"
---
// openssl 0.10（FFI 大物：系统 libssl/libcrypto；pkg-config --modversion openssl =
// 3.0.2 实证存在，头文件 /usr/include/openssl 齐）。openssl-sys 为动态链接
// （-l ssl -l crypto），全程无 guest 回调传入 libcrypto（不触 thunk 盲区）。
// Rng::bytes 非确定，不用（任务指定）。
//
// ★ FRONTIER 记录（2026-07-16，同根两撞，新类别：rlib 元数据 `-l` 传播缺口）：
//   ① 裸跑撞（exit 70，首次 foreign 调用即失败）——可绕：
//     mirvm[m4-engine]: foreign `OpenSSL_version` 符号不存在（归档兜底表 /
//     dlsym 全域均未命中；fn _RNvNtCslFQvgr53rew_7openssl7version7version）
//   根因（已实证定位）：cargo（nightly-2026-07-02）对 build-script 的
//   `cargo:rustc-link-lib=ssl/crypto` 只把 `-l` 传给 openssl-sys 自己的 rlib
//   编译（落 rlib 元数据 native_libraries），**最终 bin 的 rustc 命令行不带
//   `-l`/`-L native`**（native 构建无碍：rustc 链接期自行从 rlib 元数据补
//   flag——已用 `cargo build -vv` 对照实证）。而 mirvm 的 dylib dlopen 候选
//   只收集 `sess.opts.libs`（src/lower/mod.rs 的 `-l` 循环）→
//   module.native_libs 为空 → libssl/libcrypto 从未进全局命名空间 → 全域
//   dlsym 未命中。Static 归档路径（native_archive.rs）走的是 tcx.native_
//   libraries 不受影响。
//   绕行 = guest 侧 main 开头显式 `libc::dlopen(libcrypto.so.3 / libssl.so.3,
//   RTLD_NOW|RTLD_GLOBAL)` 预载——与 native 链接器 DT_NEEDED 加载的是
//   同一宿主同一版本系统库，native 下为幂等 no-op，语义/输出逐字节不变；
//   纯 driver 源码层、无 env、无 harness 状态篡改（同 c_zip_arch 分块绕行类）。
//   ② 绕行后推进到第二撞（exit 70）——**driver 层无可绕**：
//     mirvm[m4-engine]: TRAP: extern fn `EVP_EncryptInit_ex` 被当作值取址，
//     但符号未命中（归档兜底表 / dlsym 全域均无）
//   openssl 0.10.81 src/cipher_ctx.rs:133 把 `ffi::EVP_EncryptInit_ex`/
//   `EVP_DecryptInit_ex` 当**值**传给通用 cipher_init（f: unsafe extern "C"
//   fn 形参）→ extern fn fn-ptr 取址的解析发生在 **lower 期**（src/lower/
//   mod.rs 的 foreign_fn_entry，经 dlsym(RTLD_DEFAULT) 烘焙真址），彼时
//   libcrypto 未进 mirvm 进程（guest 预载是运行期代码，够不着）→ 烘焙
//   TRAP 于执行到首个 cipher_init 时终止。所有 EVP cipher 路径（Crypter/
//   encrypt/decrypt）必经 cipher_init，此段在 mirvm 下必红。
//   mirvm 侧修法（产品票据，留给主线）：lower 把 used_crates 的
//   tcx.native_libraries 里 Dylib/RawDylib 项（连带 `-L native`/`sess.opts.
//   libs`）收成候选，并在排干 worklist **前** RTLD_GLOBAL 预载（与
//   required_native_libs 同点）——①②同源同修。修复后摘 dlopen 预载绕行，
//   本 driver 应三维全绿（解锁探针）。
//
// 覆盖（①②③ 段 mirvm 下已执行差分；④ cipher 段 native 全绿、mirvm 于
// 段内首个 cipher_init 响亮终止——谱系见上 ★②）：
// ① EVP digest 六定向量对拍内嵌已知答案（sha256×3 / sha512 / sha1 / md5，已用
//    宿主 openssl CLI 交叉核验——md5("abc") 末段是 e17f72 不是 e17e72）+
//    openssl::sha one-shot 三型 + Hasher 分片流式（含空 update）+ from_name
//    命中/拒绝 + size()；
// ② RSA-2048 从**固定 DER** 导入（不自生成）：private_key_from_der（PKCS#1）/
//    public_key_from_der_pkcs1（PKCS#1）/ Rsa::public_key_from_der（SPKI）/
//    PKey::public_key_from_der（SPKI）/ from_public_components(BigNum) 五条
//    路径（0.10.81 命名陷阱实证：public_key_from_der 吃 SPKI，_pkcs1 后缀
//    才吃 RSAPublicKey——喂错即 asn1 wrong tag）→ Signer/Verifier
//    PKCS1v15+SHA-256（确定性填充）签名与已知答案逐字节对拍；正例四公钥面 +
//    私钥验签面；反例：改消息 / 改签名末字节 / 他消息签名；
//    Rsa Padding::NONE 原始私加密/公解密
//    roundtrip（256B 块，msb=0 保证 < n）+ 短输入错误；check_key；DER 垃圾
//    两条错误路径；
// ③ pbkdf2-hmac-sha256 定向量（4096 iter，python3 hashlib 交叉核验）、
//    base64 encode/decode roundtrip、memcmp::eq 正反例、库版本面；
// ④ EVP aes-128-cbc/ecb NIST SP800-38A F.2/F.1 定向量（Crypter pad(false)
//    整块 vs 13B 分片流式；CLI openssl enc -nopad 核验）+ pkcs7 默认 padding
//    roundtrip + key 长度错误（过短 IV 是 crate 级 assert 而非 Err，不作
//    用例）+ 篡改密文 padding 错误 + from_nid 命中/
//    raw(-1) 拒绝（cipher 的名称查找走 Nid，from_name 只存在于 MessageDigest）。
//
// 确定性：全部密钥/向量/签名 hex 常量内嵌（签名为 PKCS1v15，逐字节确定）；
// 无 HashMap 序/时间/地址/线程/RNG；临时文件零（全内存）；错误路径打印
// ErrorStack Display（错误码 + library/function/reason + file:line + data），
// 全部为 libcrypto 构建内固定字符串——native/mirvm 同宿主同库，三维实测
// 逐字节一致；成功维度 stderr 为空（driver 零 warning）。
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

/// 固定 RSA-2048 私钥（PKCS#1 DER，1190B；生成一次后冻结，永不重roll）。
const RSA_PRIV_PKCS1: &str = "308204a20201000282010100d8e1da9a60da804c7dc8f66c5fe408e6074e71e8ca110071057062ab38cd23c37ab31b2d7cbcdadaa4530cb3731b0d0bb4357183a5610598b85058e97c27ef07b95933b3a3b0e349c7f1182e4ed4de988045da7e3dbad1784e5efbb47ff61ace8be4f1c0f2286cf150628b0f1ebc087784f8663a8cd305d12af1dcafc1f14ccf81e176ae752c64ce8b350fd319cd5b4bd4a517c01306fcfb57d61df3986c10b480b73712b15116e55135f266eb272ed652738b8488f78ae1c09a025951273cbb8b898a0d2a05fb568f02d38ac7e35c36c2358d29d45bab960eab7c44110759dcdae902f1b5d3494c90e0754ca17cdbe654351472469f973d155939cf64efcdbb0203010001028201000100b2510a67983642529e890bbc732a151890c041c6e8589d6cd297f61044bd0fa12a5c007aa30c65bc188fefd54d2f23ab2796333a8a9b152711f22a67c8dc21483f4bf252cc0a38496e1230bd1d36594979642206ee16ff4dc31e27854cc46db32646fd17a0c150dcd3c5c31f9d6e6807f9f90553a8703a219a2299e3f2b5f4b6bca23a60ba3ce85d080a4c9c1f2e79df5dec2d73203f923451c75f9bf326dc3495fae4343f0d69f6ad563a45f14c61592746c46d6bddd6f5746ca070bfb668a28207edef49701b0368c8bc78843fb2a6c3bfdd2b5f7a45c12ecba0a49232c2eee1315b41baf900d1b6ca3532ec6a77f8170c84931995a3720798d33f143902818100f2ceac161130c427c18709a98ab0b801f9851b42207f73bb90677a012ad196474f44b97e378f58573f5d478adbf7f5dd0a766ba6a8bcdfe678e7bc4c708cebcd918c6979836b446fd5e93402ad8a441a1c2e6ec1aca93e15008ec51422fd180e5c1501a0877a517260bc4f27510ca00b1e68e494903e0aad1b08c430e4be852702818100e4aa93b51b5c44de0ac99071ab7b70ef685a74d9b9aa6e7800fa0f81bb0f463448ba262754772d859b7c523a2bc448c0f8f6aeb81b3631f149fb747338ff92b28a88d0c9c09cef2d4fe641a063a3ae51f5e6aca9479a396bbc9e5cf1e20b3c74d39eabc8c65a76cf616227e6cd5ed111cd67a1696fbbd13815a188544882d74d0281801ba18d4fcd9101218d1272f50a45660b437bf448282e98db0569e12674daf90110723fb1af5ceeaeaf154c68eef35ed552b57b36b2091c69bbe4933717afd1bdc90c738c527a488579905a4cdbb6da5d264bda6acbdd4ea55134ee14868ecac8078e946ad2400738beed6f0c885aa973da78115b1eb710bbf6519f11f955fd0d0281805de9c0a84d0864305d75d3211c30a27d70fa55ab66199d2d24198f6cd48abd6693c8000b7f21434cf042eaf2812f284238fdf75c1db0f06a0cdc7d432551b1ca2a236ebcada2c688719c3bafc7bc5dc7c39a6da748850ab838cb419906215f3f0bfacacab6cc48a77b7378b7cdf8f71cbca3a7234a8474b4f80d53946a0372b10281804e636e6eea9deebf9d2a9425a6cff6e8ce318c64b4e51f044030678bff4bbef6da8c832399ba0ad63f174b8701e12b412a02ac5875258b2cd95fead0e909f0288f446a025fda3ac5fbdbefa0d28e2412f1e331331a9d9bda9a250bf94209cf0aac768667a9de8f901e20001d124892a7557b8bf60be1473de710ef19c0c8e9ea";
/// 同钥公钥（PKCS#1 DER，270B）。
const RSA_PUB_PKCS1: &str = "3082010a0282010100d8e1da9a60da804c7dc8f66c5fe408e6074e71e8ca110071057062ab38cd23c37ab31b2d7cbcdadaa4530cb3731b0d0bb4357183a5610598b85058e97c27ef07b95933b3a3b0e349c7f1182e4ed4de988045da7e3dbad1784e5efbb47ff61ace8be4f1c0f2286cf150628b0f1ebc087784f8663a8cd305d12af1dcafc1f14ccf81e176ae752c64ce8b350fd319cd5b4bd4a517c01306fcfb57d61df3986c10b480b73712b15116e55135f266eb272ed652738b8488f78ae1c09a025951273cbb8b898a0d2a05fb568f02d38ac7e35c36c2358d29d45bab960eab7c44110759dcdae902f1b5d3494c90e0754ca17cdbe654351472469f973d155939cf64efcdbb0203010001";
/// 同钥公钥（X.509 SPKI DER，294B）。
const RSA_PUB_SPKI: &str = "30820122300d06092a864886f70d01010105000382010f003082010a0282010100d8e1da9a60da804c7dc8f66c5fe408e6074e71e8ca110071057062ab38cd23c37ab31b2d7cbcdadaa4530cb3731b0d0bb4357183a5610598b85058e97c27ef07b95933b3a3b0e349c7f1182e4ed4de988045da7e3dbad1784e5efbb47ff61ace8be4f1c0f2286cf150628b0f1ebc087784f8663a8cd305d12af1dcafc1f14ccf81e176ae752c64ce8b350fd319cd5b4bd4a517c01306fcfb57d61df3986c10b480b73712b15116e55135f266eb272ed652738b8488f78ae1c09a025951273cbb8b898a0d2a05fb568f02d38ac7e35c36c2358d29d45bab960eab7c44110759dcdae902f1b5d3494c90e0754ca17cdbe654351472469f973d155939cf64efcdbb0203010001";
/// SIGN_MSG 的 PKCS1v15+SHA-256 签名已知答案（宿主 openssl dgst 计算后冻结）。
const RSA_SIG: &str = "3925ff092d2e623951611a5d36249246a631becd3f4a47dbfc29a4b01c19e2fc87ebf51222d39b1f3895242c440c63dd60147761a49aacd0a51a0eec8fc5a2ca4bccdd243379ca2566ecb0bd75bf43917667adea254ca1637c5e77f0e2de4aa6a0da5dbca74f6bd19c3dde631ff2bb8d3f760d809ca9e11fe00e9e394ba9b38bb5b74441060a4f54d99fbe8cdda0616e3683304c58f974d85ef8a843d2fc236ed9a68e1ca7b94783796e1f04758205357c1b5d691983d23a9ee9ebb1bbd0b7de7f1e1a7cd35ae8c5624beb6a76318ec865334eade33740738ced2c95bda34717dc6df91c8b610da9c34eb4908bf093c54d45fbb24a3c1f474a4b22603afcd673";

/// 签名/验签固定消息（与已知答案绑定，改动即换答案）。
const SIGN_MSG: &[u8] = b"mirvm openssl_evp fixed message v1";

// NIST SP800-38A AES-128 定向量。
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
    assert!(b.len() % 2 == 0, "hex 常量长度必须为偶");
    b.chunks(2).map(|p| (nib(p[0]) << 4) | nib(p[1])).collect()
}

fn hex(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 内联 FNV-1a（二进制块锚定）。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// ErrorStack → 单行（lib::reason 是 libcrypto 内固定字符串，宿主两侧一致）。
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

/// Crypter 按给定格点分片流式喂入（pad 一律 false，适配整块定向量）。
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
    assert_eq!(off, input.len(), "splits 必须恰好耗尽输入");
    let mut tail = vec![0u8; cipher.block_size()];
    let n = c.finalize(&mut tail).unwrap();
    out.extend_from_slice(&tail[..n]);
    out
}

/// 验签正反例统一形状：Ok(bool) 或错误栈单行（同宿主同库，两侧逐字节一致）。
fn verify_with<T: HasPublic>(pk: &PKeyRef<T>, msg: &[u8], sig: &[u8]) -> String {
    let mut v = Verifier::new(MessageDigest::sha256(), pk).unwrap();
    v.update(msg).unwrap();
    match v.verify(sig) {
        Ok(b) => format!("ok={b}"),
        Err(e) => format!("err count={} {}", e.errors().len(), fmt_stack(&e)),
    }
}

fn main() {
    // ★ FRONTIER 绕行（根因与诊断原文见文件头）：显式预载系统库进全局命名
    // 空间，补偿 mirvm lower 未收集 rlib 元数据 `-l` 的缺口。先 crypto 后
    // ssl（依赖序）；native 下是同一系统库上的幂等 no-op。
    unsafe {
        for lib in ["libcrypto.so.3\0", "libssl.so.3\0"] {
            let h = libc::dlopen(lib.as_ptr().cast(), libc::RTLD_NOW | libc::RTLD_GLOBAL);
            assert!(!h.is_null(), "dlopen {lib} 失败");
        }
    }

    println!(
        "lib = {} number = {:#010x}",
        version::version(),
        version::number()
    );

    // ---- ① EVP digest：定向量 + 流式 + one-shot + from_name ----
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

    // ---- ② RSA：固定 DER 导入 → sign/verify 正反例 + NONE 原始加解密 ----
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

    // 四条公钥导入路径（0.10.81 API 名义：public_key_from_der = SPKI，
    // _pkcs1 后缀才是 RSAPublicKey）+ from_public_components
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
    // 反例三连：改消息 / 改签名末字节 / 他消息签名充数
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

    // Rsa 原始 RSA op（Padding::NONE，确定性）：私加密 → 公解密 roundtrip。
    // 256B 块首字节恒 0 → 值 < 2^2040 < n。
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

    // ---- ③ misc：pbkdf2 / base64 / memcmp ----
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

    // ---- ④ EVP cipher：AES-128 CBC/ECB 定向量 + padding + 错误路径 ----
    // ★ FRONTIER 段（文件头 ★②）：openssl 的 CipherContext 通用初始化把
    // `ffi::EVP_EncryptInit_ex`/`EVP_DecryptInit_ex` 当**值**传参 → extern fn
    // fn-ptr 取址在 lower 期 dlsym（libcrypto 未进 mirvm 进程）→ 烘焙 TRAP，
    // mirvm 在执行到首个 Crypter/encrypt 时响亮终止（exit 70）。guest 侧无
    // 语义不变的绕行——故置末段，前 ①②③ 段先行完成差分。
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

    // pkcs7 默认 padding（11B → 16B）one-shot encrypt/decrypt
    let msg11 = b"hello world";
    let padded = encrypt(cbc, &key, Some(&iv), msg11).unwrap();
    let plain = decrypt(cbc, &key, Some(&iv), &padded).unwrap();
    println!(
        "cbc pad msg11 ctlen={} fnv={:016x} roundtrip={}",
        padded.len(),
        fnv1a(&padded),
        plain == msg11
    );
    // 名称查找面：cipher 走 Nid（from_name 只存在于 MessageDigest）
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
    // 注：过短 IV 不走 Err——openssl 0.10.81 cipher_ctx.rs 断言
    // `iv_len <= iv.len()`（crate 设计为 panic 而非 Err），故不作错误路径
    // 用例；过长 IV 被 libcrypto 静默截断亦无 Err 可验。
    let mut tampered = padded.clone();
    *tampered.last_mut().unwrap() ^= 0x01;
    match decrypt(cbc, &key, Some(&iv), &tampered) {
        Ok(_) => println!("tampered ct unexpectedly ok"),
        Err(e) => perr("tampered-ct", &e),
    }
}
