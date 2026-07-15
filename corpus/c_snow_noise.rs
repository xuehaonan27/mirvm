#!/usr/bin/env mirvm
---
[dependencies]
snow = "0.9"
---
// snow 0.9（Noise 协议框架）差分：Noise_XX 双向认证握手（3 消息）+ Noise_NKpsk0
// （psk 位置 0 + 预置对端静态公钥）。静态密钥固定字节；ephemeral 用
// Builder::fixed_ephemeral_key_for_testing_only 锁定（设后握手状态机不再触碰
// OsRng）→ 全部 handshake transcript 逐字节确定。
// 覆盖：XX 三消息交换（payload 逐跳加密状态 was_write_payload_encrypted）、
// handshake hash 双侧一致、远端静态公钥互见、into_transport_mode 双向加密消息
// roundtrip、篡改密文/tag 错误路径、rekey 对（rekey_outgoing/incoming）、
// NKpsk0 的 psk 正/误两例、乱序 write_message 与未完成握手转 transport 的
// StateProblem 错误路径。
//
// 已知 FRONTIER 绕行记录（语义不变，换 cipher 特性）：
// 原定套件 Noise_XX_25519_AESGCM_SHA256 在 mirvm 下于 XX 第二跳（响应方首次
// AEAD 加密静态公钥）撞 aes 族运行期 cpuid 探测选中的 AES-NI 硬件路径，
// 诊断原文（exit 70）：
//   mirvm[m4-engine]: TRAP: foreign `llvm.x86.aesni.aeskeygenassist`（LLVM 内部
//   符号，按需内建）（fn _RINvNtNtNtCs…_4core9core_arch3x863aes25__mm_aeskeygenassist_si128…）
// 即 docs/corpus.md 欠账队列中的 `llvm.x86.aesni.*`（aes-gcm 无 force-soft 退路，
// c_aes_gcm 同款 expected-red）。绕行 = 换 Noise 自带的 ChaChaPoly cipher
// （chacha20poly1305 纯软件路径），协议状态机/DH/哈希覆盖面不变。
// chacha20poly1305 0.10 传递依赖的 poly1305 0.8 其 avx2 backend 撞
// `llvm.x86.avx2.permd`，诊断原文（exit 70）：
//   mirvm[m4-engine]: TRAP: foreign `llvm.x86.avx2.permd`（LLVM 内部符号，按需
//   内建）（fn _RNvNtNtNtCs…_4core9core_arch3x864avx227__mm256_permutevar8x32_epi32…）
// 绕行 = poly1305 0.8 自带的 `--cfg poly1305_force_soft` 开关（只换后端实现，
// Poly1305 为精确整数算术，输出逐比特相同）。故 mirvm 两维需带：
//   RUSTFLAGS='--cfg poly1305_force_soft'
// （cargo 指纹跟踪 RUSTFLAGS，加/去 flag 自动重编；native 维可裸跑——宿主
// 真 CPU 吃得下 avx2，两 backend 输出逐字节一致。）
// 附记：curve25519-dalek 4.1 默认 simd backend 本例全程（XX/NK 共 8 次 X25519）
// 在 mirvm 下安然通过——其 avx2 向量域算术全走普通 IR 内建（_mm256_mul_epu32
// 等），未撞 c_ed25519 的 vpmadd52 FRONTIER（ifma 需编译期 target_feature，
// 本工具链未开）——故本 driver 无需 ed25519 的 serial-backend env。
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, TransportState};

const SUITE: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
const SUITE_NK: &str = "Noise_NKpsk0_25519_ChaChaPoly_SHA256";

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 固定字节派生 32B 密钥材料（x25519 内部 clamp，任意 32B 均合法）。
fn key(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = seed ^ (i as u8).wrapping_mul(0x9d);
    }
    k
}

/// 一跳握手消息：写方 write_message → 读方 read_message，打印 transcript
/// hex / 写方 payload 是否加密 / payload 内容，累计进 fnv sink。
fn hop(
    label: &str,
    w: &mut HandshakeState,
    r: &mut HandshakeState,
    payload: &[u8],
    sink: &mut Vec<u8>,
) {
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 512];
    let n = w.write_message(payload, &mut tx).unwrap();
    sink.extend_from_slice(&tx[..n]);
    println!(
        "{label} len={n} payload_enc={} msg={}",
        w.was_write_payload_encrypted(),
        hex(&tx[..n])
    );
    let m = r.read_message(&tx[..n], &mut rx).unwrap();
    println!(
        "{label} payload len={m} ok={} text={:?}",
        rx[..m] == *payload,
        std::str::from_utf8(&rx[..m]).unwrap()
    );
}

/// transport 一跳：加密 → 对端解密 roundtrip，打印密文 hex。
fn thop(label: &str, w: &mut TransportState, r: &mut TransportState, payload: &[u8]) {
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 512];
    let n = w.write_message(payload, &mut tx).unwrap();
    println!("{label} ct len={n} = {}", hex(&tx[..n]));
    let m = r.read_message(&tx[..n], &mut rx).unwrap();
    println!("{label} roundtrip = {}", rx[..m] == *payload);
}

fn main() {
    let is = key(0x11); // initiator static
    let ie = key(0x33); // initiator ephemeral
    let rs = key(0x22); // responder static
    let re = key(0x44); // responder ephemeral
    let prologue = b"mirvm-snow-prologue-v1";

    // ---- ① Noise_XX 双向认证握手（双方互不知道对方静态公钥）----
    let params: NoiseParams = SUITE.parse().unwrap();
    let mut init = Builder::new(params.clone())
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .prologue(prologue)
        .build_initiator()
        .unwrap();
    let mut resp = Builder::new(params)
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .prologue(prologue)
        .build_responder()
        .unwrap();
    println!("xx pre finished i={} r={}", init.is_handshake_finished(), resp.is_handshake_finished());
    println!("xx pre turn i={} r={}", init.is_my_turn(), resp.is_my_turn());

    let mut sink = Vec::new();
    // XX: -> e | <- e, ee, s, es | -> s, se
    hop("xx m1 (->e)", &mut init, &mut resp, b"xx-payload-1-plain", &mut sink);
    hop("xx m2 (<-e,ee,s,es)", &mut resp, &mut init, b"xx-payload-2-enc", &mut sink);
    hop("xx m3 (->s,se)", &mut init, &mut resp, b"xx-payload-3-enc", &mut sink);
    println!("xx transcripts fnv = {:016x}", fnv1a(&sink));

    println!("xx post finished i={} r={}", init.is_handshake_finished(), resp.is_handshake_finished());
    let hh_i = init.get_handshake_hash().to_vec();
    let hh_r = resp.get_handshake_hash().to_vec();
    println!("xx hh i = {}", hex(&hh_i));
    println!("xx hh r = {}", hex(&hh_r));
    println!("xx hh eq = {}", hh_i == hh_r);
    // XX 之后双方互见对方静态公钥；存下供 NKpsk0 当 remote_public_key 用
    let mut is_pub = [0u8; 32];
    let mut rs_pub = [0u8; 32];
    is_pub.copy_from_slice(resp.get_remote_static().unwrap());
    rs_pub.copy_from_slice(init.get_remote_static().unwrap());
    println!("xx r sees is_pub = {}", hex(&is_pub));
    println!("xx i sees rs_pub = {}", hex(&rs_pub));

    // ---- ② transport：双向加密消息 + 篡改 + rekey ----
    let mut it = init.into_transport_mode().unwrap();
    let mut rt = resp.into_transport_mode().unwrap();
    thop("tx i->r #0", &mut it, &mut rt, b"transport msg zero");
    thop("tx i->r #1", &mut it, &mut rt, b"");
    thop("tx r->i #0", &mut rt, &mut it, "reply with unicode 汉字 🎉".as_bytes());
    thop("tx r->i #1", &mut rt, &mut it, &[0xde, 0xad, 0xbe, 0xef]);

    // 篡改密文一字节 → AEAD 必须失败。snow 语义：失败的解密不自增接收方
    // nonce（cipherstate.rs 的 `decrypt(...)?` 提前返回），但发送方 nonce 已 +1
    // → 应用层必须用 receiving_nonce/set_receiving_nonce 显式对齐才能继续会话。
    let mut tx = [0u8; 512];
    let mut rx = [0u8; 512];
    let n = it.write_message(b"will be tampered", &mut tx).unwrap();
    tx[3] ^= 0x01;
    match rt.read_message(&tx[..n], &mut rx) {
        Ok(_) => println!("tx tamper unexpectedly ok"),
        Err(e) => println!("tx tamper err = {e}"),
    }
    let desync = rt.receiving_nonce();
    println!("tx recv nonce after failed read = {desync}");
    rt.set_receiving_nonce(desync + 1);
    println!("tx recv nonce resynced = {}", rt.receiving_nonce());

    // rekey 对：一端 rekey_outgoing ↔ 对端 rekey_incoming，之后继续 roundtrip
    it.rekey_outgoing();
    rt.rekey_incoming();
    thop("tx post-rekey i->r", &mut it, &mut rt, b"after rekey one");
    rt.rekey_outgoing();
    it.rekey_incoming();
    thop("tx post-rekey r->i", &mut rt, &mut it, b"after rekey two");

    // ---- ③ 错误路径：乱序写 / 未完成握手转 transport / 篡改握手消息 ----
    let params: NoiseParams = SUITE.parse().unwrap();
    let mut early_i = Builder::new(params.clone())
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .build_initiator()
        .unwrap();
    let mut early_r = Builder::new(params)
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .build_responder()
        .unwrap();
    // 响应方第一跳只能读：乱序 write 必须 err
    let mut tmp = [0u8; 512];
    match early_r.write_message(b"out of turn", &mut tmp) {
        Ok(_) => println!("xx out-of-turn write unexpectedly ok"),
        Err(e) => println!("xx out-of-turn write err = {e}"),
    }
    // 未完成握手 into_transport_mode 必须 err
    let early_i2 = Builder::new(SUITE.parse().unwrap())
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .build_initiator()
        .unwrap();
    match early_i2.into_transport_mode() {
        Ok(_) => println!("xx early transport unexpectedly ok"),
        Err(e) => println!("xx early transport err = {e}"),
    }
    // 篡改握手第二跳（含加密静态公钥）→ 读方 AEAD err
    let n = early_i.write_message(b"m1", &mut tmp).unwrap();
    let mut rbuf = [0u8; 512];
    let _ = early_r.read_message(&tmp[..n], &mut rbuf).unwrap();
    let n2 = early_r.write_message(b"m2", &mut tmp).unwrap();
    tmp[40] ^= 0x80;
    match early_i.read_message(&tmp[..n2], &mut rbuf) {
        Ok(_) => println!("xx tamper hs-m2 unexpectedly ok"),
        Err(e) => println!("xx tamper hs-m2 err = {e}"),
    }

    // ---- ④ Noise_NKpsk0：预置对端静态公钥 + psk(0) ----
    let psk = key(0x5a);
    let params_nk: NoiseParams = SUITE_NK.parse().unwrap();
    let mut nk_i = Builder::new(params_nk.clone())
        .remote_public_key(&rs_pub)
        .local_private_key(&is)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .psk(0, &psk)
        .build_initiator()
        .unwrap();
    let mut nk_r = Builder::new(params_nk.clone())
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .psk(0, &psk)
        .build_responder()
        .unwrap();
    let mut sink2 = Vec::new();
    hop("nk m1 (->psk,e,es)", &mut nk_i, &mut nk_r, b"nk-init", &mut sink2);
    hop("nk m2 (<-e,ee)", &mut nk_r, &mut nk_i, b"nk-resp", &mut sink2);
    println!("nk transcripts fnv = {:016x}", fnv1a(&sink2));
    println!("nk finished i={} r={}", nk_i.is_handshake_finished(), nk_r.is_handshake_finished());
    let mut nit = nk_i.into_transport_mode().unwrap();
    let mut nrt = nk_r.into_transport_mode().unwrap();
    thop("nk tx i->r", &mut nit, &mut nrt, b"nk transport");

    // psk 错误 → 第一跳解密即失败
    let bad_psk = key(0xa5);
    let mut bi = Builder::new(params_nk.clone())
        .remote_public_key(&rs_pub)
        .fixed_ephemeral_key_for_testing_only(&ie)
        .psk(0, &bad_psk)
        .build_initiator()
        .unwrap();
    let mut br = Builder::new(params_nk)
        .local_private_key(&rs)
        .fixed_ephemeral_key_for_testing_only(&re)
        .psk(0, &psk)
        .build_responder()
        .unwrap();
    let n = bi.write_message(b"bad-psk payload", &mut tmp).unwrap();
    match br.read_message(&tmp[..n], &mut rbuf) {
        Ok(_) => println!("nk bad-psk unexpectedly ok"),
        Err(e) => println!("nk bad-psk err = {e}"),
    }
}
