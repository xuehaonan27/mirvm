#!/usr/bin/env mirvm
---
[dependencies]
alloy-primitives = { version = "=1.6.1", default-features = false, features = ["std"] }
alloy-sol-types = "=1.6.1"
alloy-consensus = { version = "=2.2.0", default-features = false, features = ["std", "k256"] }
alloy-eips = { version = "=2.2.0", default-features = false }
alloy-rlp = "=0.3.16"
k256 = "=0.13.4"
---
// c_ethers_evm —— alloy 无网面（ethers-core 语义面的现行 alloy 接棒；批10 波2）：
// ABI encode/decode 矩阵、交易 RLP 签名（legacy EIP-155 + EIP-1559 type-2，k256
// RFC6979 定签 + 恢复回环）、区块头解析与 keccak 哈希指纹的三维差分。
//
// 版本钉（相容组合证据）：
//   * alloy-consensus =2.2.0、alloy-sol-types =1.6.1、alloy-primitives =1.6.1、
//     alloy-rlp =0.3.16、alloy-eips =2.2.0、k256 =0.13.4（2026-07-18 crates.io
//     sparse index 各线顶）。相容性：consensus 2.2.0 声明 alloy-primitives ^1.6.0
//     与 alloy-eips ^2.2.0（同 release train）、alloy-rlp ^0.3.14；sol-types
//     1.6.1 声明 alloy-primitives ^1.6.1——primitives 1.x 线顶即 1.6.1（无 2.x），
//     =1.6.1 使全图单版本无错配。k256 =0.13.4 与 c_k256_ecdsa 已绿同版，且正是
//     consensus `k256` feature 的 ^0.13 约束顶。
//   * features 裁剪：consensus 默认仅 std；c-kzg/blst/secp256k1-sys/sha3-asm
//     虽在 lock 图但不进构建图（cargo tree -e normal,build 实证 96 包、零 C/FFI/
//     零汇编），keccak 走 sha3 纯 Rust 路径。`k256` feature 只为 Signed 的
//     recover_signer（纯 Rust k256 恢复），不引 secp256k1 C 绑定。
//
// 确定性说明：
//   * 全部输入为文件内常量；签名走 k256 RFC6979 HMAC-DRBG
//     （sign_prehash_recoverable）零 RNG；keccak/RLP/ABI/recover 全为纯函数；
//     无时间/线程/HashMap 迭代/网络/TTY/系统环境面。
//   * 外部公开定值锚（与实现无关的第三者证据）：ERC20 transfer 选择子
//     0xa9059cbb；EIP-155 规范例交易字段（nonce 9 / 20gwei / 21000 /
//     to=0x3535…3535 / 1ETH / chain 1 → v=37 形态）；以太坊主网 genesis 头
//     keccak = 0xd4e56740f876aef8…1cb8fa3；RLP 串长 55/56 边界头字节 b7/b8。
//   * B 维 native 同 driver 连跑两次 stdout 逐字节一致（见下「三维实测」）。
//
// 已知 FRONTIER 绕行（语义不变，与 c_revm_evm 同一坑）：const-hex 1.x 运行期
// 探测 ssse3/avx2 后走 SIMD `_mm_lddqu_si128`——mirvm 未内建该 x86 intrinsic
// （TRAP 原文：`foreign `llvm.x86.sse3.ldu.dq``）。alloy-primitives 的 hex 模块、
// FixedBytes/B256/Address 的 LowerHex/Display 均会命中。故本 driver 所有运行时
// hex 走自写逐字节 hex()/unhex()；address!/b256! 宏为编译期 const 解析（宿主
// rustc 求值，不进 mirvm 运行面）。打印内容等价，三维对拍不受影响。
//
// 覆盖清单：
//   ① ABI 静态矩阵：(U256,bool,i64,Address,FixedBytes<32>,[u16;4]) 编码 +
//      validate 解码回环。
//   ② ABI 动态矩阵：(String[含多字节 UTF-8],Bytes,Vec<u64>) 编码 + 回环。
//   ③ ABI 嵌套矩阵：((u64,String),Vec<(u16,bool)>,(Address,U256)) 嵌套 tuple +
//      tuple 动态数组。（上游 alloy-sol-types 有意不为 u8 实现 SolValue——
//      Vec<u8>/[u8;N] 已特化为 Bytes/FixedBytes，故数组元素用 u16。）
//   ④ abi_encode_packed 非标准打包面。
//   ⑤ 解码负路径：截断半包必 Err、错型解码必 Err。
//   ⑥ 手写函数选择子：keccak("transfer(address,uint256)")[..4] + 参数编码
//      = 完整 calldata 锚。
//   ⑦ 签名面：固定私钥（k256 内嵌测试向量 D）→ 地址锚；TxLegacy(EIP-155
//      chain 1) 与 TxEip1559(type-2，36B input) 各走 sighash → RFC6979 定签
//      → Signed → eip2718 编码（len+hex 全锚）→ recover_signer == 地址 →
//      TxEnvelope 解码重编码回环。
//   ⑧ 区块头：主网 genesis 头 15 字段 RLP（len+fnv+head96）+ hash_slow ==
//      公开 genesis 哈希外部锚 + decode 回环；London 风格头（base_fee=Some，
//      全非空字段）RLP/哈希/回环。
//   ⑨ 裸 RLP 边界：空串(0x80)、55/56B 串长边界(b7/b8)、单字节原样(0x7f)、
//      嵌套 list、截断解码必 Err。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_ethers_evm.rs
//   B: d=$(grep -l 'name = "c_ethers_evm"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && cargo +nightly-2026-07-02 run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_ethers_evm.rs
//
// 三维实测（2026-07-19，全绿）：A/B/C 三进程 stdout 逐字节一致（36 行，md5
// 9c56d7fd2b7a0e0f31ff878ce02887ab），exit 全 0；A/C stderr 真空（0 字节），
// B stderr 仅 cargo 构建行。关键锚：abi1.static.enc len=288 fnv=f5af2825935bf254；
// abi6.selector=0xa9059cbb（ERC20 transfer 规范选择子）；signer=
// 0xdf4abd97183d56aa7fdf00e349a2aa633a2bb86f；legacy.signed len=110（v=0x25=37
// 的 EIP-155 形态）；eip1559.signed len=158 ty=2；genesis.rlp len=535
// fnv=69e31c2d5fa07df1；genesis.hash=0xd4e56740f876aef8c010b86a40d5f567
// 45a118d0906a34e69aec8c0db1cb8fa3（主网 genesis 外部锚，hash_ok=true）；
// rlp.s55_head=b7 rlp.s56_head=b8。时长：A（含 script 首次构建，依赖共享缓存
// 已热）real 17.2s；B（script dir 原生构建+跑）13.2s、B 复跑 <1s（B 维双跑
// stdout 逐字节一致）；C（JIT=1，缓存热）0.68s。依赖：lock 265 条，实际编译
// 图 96 包（cargo tree -e normal,build），零 C/FFI/零汇编。无 FRONTIER、无
// 引擎 bug 信号。

use alloy_consensus::{
    Header as EthHeader, SignableTransaction, TxEip1559, TxEnvelope, TxLegacy,
};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{
    address, b256, keccak256, Address, Bytes, FixedBytes, Signature, TxKind, B256, U256,
};
use alloy_sol_types::SolValue;
use k256::ecdsa::SigningKey;

/// FNV-1a 64：二进制输出的内联指纹。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn hex(data: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(data.len() * 2);
    for &b in data {
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

fn hex_bytes32(s: &str) -> [u8; 32] {
    let v = unhex(s);
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    out
}

/// 打印字节锚：<=160B 全 hex，否则 len+fnv+前 96hex。
fn show_bytes(tag: &str, data: &[u8]) {
    if data.len() <= 160 {
        println!("{tag} len={} hex={}", data.len(), hex(data));
    } else {
        println!(
            "{tag} len={} fnv={:016x} head96={}",
            data.len(),
            fnv1a(data),
            hex(&data[..96])
        );
    }
}

fn show_hash(tag: &str, h: &B256) {
    println!("{tag} 0x{}", hex(h.as_slice()));
}

fn abi_matrix() {
    let addr: Address = address!("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
    let f32b: FixedBytes<32> =
        b256!("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    // ① 纯静态矩阵
    let t1 = (
        U256::from(0xdeadbeefu64),
        true,
        -42i64,
        addr,
        f32b,
        [1u16, 2u16, 3u16, 4u16],
    );
    let e1 = t1.abi_encode();
    show_bytes("abi1.static.enc", &e1);
    let d1 =
        <(U256, bool, i64, Address, FixedBytes<32>, [u16; 4])>::abi_decode_validate(&e1).unwrap();
    println!("abi1.static.roundtrip={}", t1 == d1);

    // ② 动态矩阵：String（多字节 UTF-8）/Bytes/Vec<u64>
    let t2 = (
        "hello 以太坊".to_string(),
        Bytes::from(vec![0xcau8, 0xfe, 0xba, 0xbe, 0x00, 0x11]),
        vec![7u64, 8, 9, 10, 11],
    );
    let e2 = t2.abi_encode();
    show_bytes("abi2.dynamic.enc", &e2);
    let d2 = <(String, Bytes, Vec<u64>)>::abi_decode_validate(&e2).unwrap();
    println!("abi2.dynamic.roundtrip={}", t2 == d2);

    // ③ 嵌套 tuple + tuple 动态数组
    let t3 = (
        (1u64, "nested".to_string()),
        vec![(2u16, true), (3u16, false)],
        (addr, U256::from(9u64)),
    );
    let e3 = t3.abi_encode();
    show_bytes("abi3.nested.enc", &e3);
    let d3 = <((u64, String), Vec<(u16, bool)>, (Address, U256))>::abi_decode_validate(&e3)
        .unwrap();
    println!("abi3.nested.roundtrip={}", t3 == d3);

    // ④ packed 编码（非标准但确定的 ABI 面）
    let p = (U256::from(123456789u64), "packed".to_string(), vec![5u16, 6u16]);
    let ep = p.abi_encode_packed();
    show_bytes("abi4.packed.enc", &ep);

    // ⑤ 负路径：截断与错型必须 Err
    let bad = <(String, Bytes, Vec<u64>)>::abi_decode(&e2[..e2.len() / 2]);
    println!("abi5.trunc_err={}", bad.is_err());
    let wrong = <(U256, bool, i64, Address, FixedBytes<32>, [u16; 4])>::abi_decode(&e2);
    println!("abi5.wrongtype_err={}", wrong.is_err());

    // ⑥ 手写函数选择子 + calldata（无 sol! 宏面）
    let selector = &keccak256(b"transfer(address,uint256)")[..4];
    let args = (addr, U256::from(1000000000000000000u128));
    let mut calldata = selector.to_vec();
    calldata.extend_from_slice(&args.abi_encode());
    println!("abi6.selector=0x{}", hex(selector));
    show_bytes("abi6.calldata", &calldata);
}

/// 固定私钥（k256 内嵌测试向量 D）→ 地址锚 + legacy/eip1559 签名恢复回环。
fn sign_txs() {
    let d = hex_bytes32("ebb2c082fd7727890a28ac82f6bdf97bad8de9f5d7c9028692de1a255cad3e0f");
    let sk = SigningKey::from_slice(&d).unwrap();
    let ep = sk.verifying_key().to_encoded_point(false);
    let addr = Address::from_raw_public_key(&ep.as_bytes()[1..]);
    println!("signer=0x{}", hex(addr.as_slice()));

    // ① legacy：EIP-155 chain_id=1（规范例交易字段）
    let legacy = TxLegacy {
        chain_id: Some(1),
        nonce: 9,
        gas_price: 20_000_000_000u128,
        gas_limit: 21_000,
        to: TxKind::Call(address!("3535353535353535353535353535353535353535")),
        value: U256::from(1_000_000_000_000_000_000u128),
        input: Bytes::new(),
    };
    let sighash = legacy.signature_hash();
    show_hash("legacy.sighash", &sighash);
    let (sig, recid) = sk.sign_prehash_recoverable(sighash.as_slice()).unwrap();
    let psig = Signature::from_scalars_and_parity(
        B256::from_slice(&sig.r().to_bytes()),
        B256::from_slice(&sig.s().to_bytes()),
        recid.is_y_odd(),
    );
    let signed = legacy.into_signed(psig);
    let mut enc = Vec::new();
    signed.encode_2718(&mut enc);
    show_bytes("legacy.signed", &enc);
    let rec = signed.recover_signer().unwrap();
    println!("legacy.recover_ok={}", rec == addr);
    let env = TxEnvelope::decode_2718_exact(&enc).unwrap();
    let mut enc2 = Vec::new();
    env.encode_2718(&mut enc2);
    println!("legacy.env_roundtrip={}", enc == enc2);

    // ② eip1559：type-2，36B input（选择子 + 定值参数）
    let mut input =
        unhex("a9059cbb0000000000000000000000004646464646464646464646464646464646464646");
    input.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    let e1559 = TxEip1559 {
        chain_id: 1,
        nonce: 10,
        gas_limit: 50_000,
        max_fee_per_gas: 30_000_000_000u128,
        max_priority_fee_per_gas: 2_000_000_000u128,
        to: TxKind::Call(address!("4646464646464646464646464646464646464646")),
        value: U256::from(500_000_000_000_000_000u128),
        access_list: Default::default(),
        input: Bytes::from(input),
    };
    let sighash2 = e1559.signature_hash();
    show_hash("eip1559.sighash", &sighash2);
    let (sig2, recid2) = sk.sign_prehash_recoverable(sighash2.as_slice()).unwrap();
    let psig2 = Signature::from_scalars_and_parity(
        B256::from_slice(&sig2.r().to_bytes()),
        B256::from_slice(&sig2.s().to_bytes()),
        recid2.is_y_odd(),
    );
    let signed2 = e1559.into_signed(psig2);
    let mut enc3 = Vec::new();
    signed2.encode_2718(&mut enc3);
    show_bytes("eip1559.signed", &enc3);
    println!("eip1559.ty={}", enc3[0]);
    let rec2 = signed2.recover_signer().unwrap();
    println!("eip1559.recover_ok={}", rec2 == addr);
    let env2 = TxEnvelope::decode_2718_exact(&enc3).unwrap();
    let mut enc4 = Vec::new();
    env2.encode_2718(&mut enc4);
    println!("eip1559.env_roundtrip={}", enc3 == enc4);
}

fn headers() {
    // ① 以太坊主网 genesis：全字段公开定值，keccak 外部锚。
    let genesis = EthHeader {
        parent_hash: B256::ZERO,
        ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
        beneficiary: Address::ZERO,
        state_root: b256!("d7f8974fb5ac78d9ac099b9ad5018bedc2ce0a72dad1827a1709da30580f0544"),
        transactions_root: b256!(
            "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
        ),
        receipts_root: b256!(
            "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
        ),
        logs_bloom: Default::default(),
        difficulty: U256::from(17_179_869_184u64),
        number: 0,
        gas_limit: 5_000,
        gas_used: 0,
        timestamp: 0,
        extra_data: Bytes::from(unhex(
            "11bbe8db4e347b4e8c937c1c8370e4b5ed33adb3db69cbdb7a38e1e50b1b82fa",
        )),
        mix_hash: B256::ZERO,
        nonce: FixedBytes::<8>::from([0, 0, 0, 0, 0, 0, 0, 0x42]),
        base_fee_per_gas: None,
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
        block_access_list_hash: None,
        slot_number: None,
    };
    let rlp1 = alloy_rlp::encode(&genesis);
    show_bytes("genesis.rlp", &rlp1);
    let h1 = genesis.hash_slow();
    show_hash("genesis.hash", &h1);
    println!(
        "genesis.hash_ok={}",
        h1 == b256!("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3")
    );
    let back: EthHeader = alloy_rlp::decode_exact(&rlp1).unwrap();
    println!("genesis.rlp_roundtrip={}", back == genesis);

    // ② London 风格头：base_fee_per_gas=Some + 全非空字段，解析/回编/指纹。
    let london = EthHeader {
        parent_hash: b256!("aa36a7f9f8e59c9b77f4b28bdab8e1c9b02a13b6b8b05e1b7f06b81e7f23a2c3"),
        ommers_hash: b256!("1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"),
        beneficiary: address!("95222290DD7278Aa3Ddd389Cc1E1d165CC4BAfe5"),
        state_root: b256!("f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb4b"),
        transactions_root: b256!(
            "c51c8e7f5e0c68c0d05d2d9c5c1d3b0e3e8b8f6a4d2c1b0a9f8e7d6c5b4a3928"
        ),
        receipts_root: b256!(
            "bc614e2f8e59c9b77f4b28bdab8e1c9b02a13b6b8b05e1b7f06b81e7f23a2c3d"
        ),
        logs_bloom: Default::default(),
        difficulty: U256::ZERO,
        number: 12_965_000,
        gas_limit: 30_000_000,
        gas_used: 1_500_000,
        timestamp: 1_628_000_000,
        extra_data: Bytes::from(vec![0x62, 0x75, 0x69, 0x6c, 0x64, 0x65, 0x72]),
        mix_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
        nonce: FixedBytes::<8>::ZERO,
        base_fee_per_gas: Some(51_000_000_000),
        withdrawals_root: None,
        blob_gas_used: None,
        excess_blob_gas: None,
        parent_beacon_block_root: None,
        requests_hash: None,
        block_access_list_hash: None,
        slot_number: None,
    };
    let rlp2 = alloy_rlp::encode(&london);
    show_bytes("london.rlp", &rlp2);
    let h2 = london.hash_slow();
    show_hash("london.hash", &h2);
    let back2: EthHeader = alloy_rlp::decode_exact(&rlp2).unwrap();
    println!("london.rlp_roundtrip={}", back2 == london);

    // ③ 裸 RLP 边界小矩阵：空串 / 55/56B 串长边界 / 单字节 / list / 截断负路径。
    let e0 = alloy_rlp::encode(&Bytes::new());
    show_bytes("rlp.empty", &e0);
    let e55 = alloy_rlp::encode(&Bytes::from(vec![0x61u8; 55]));
    let e56 = alloy_rlp::encode(&Bytes::from(vec![0x61u8; 56]));
    show_bytes("rlp.s55", &e55);
    show_bytes("rlp.s56", &e56);
    println!("rlp.s55_head={:02x} rlp.s56_head={:02x}", e55[0], e56[0]);
    let one = alloy_rlp::encode(&0x7fu8);
    show_bytes("rlp.single", &one);
    let list: Vec<U256> = vec![U256::from(1u64), U256::from(1024u64)];
    let elist = alloy_rlp::encode(&list);
    show_bytes("rlp.list", &elist);
    let dlist: Vec<U256> = alloy_rlp::decode_exact(&elist).unwrap();
    println!("rlp.list_roundtrip={}", dlist == list);
    let bad = alloy_rlp::decode_exact::<Bytes>(&e56[..e56.len() - 1]);
    println!("rlp.trunc_err={}", bad.is_err());
}

fn main() {
    abi_matrix();
    sign_txs();
    headers();
}
