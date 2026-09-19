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
// c_ethers_evm -- alloy's no-network surface: a three-way differential over the ABI
// encode/decode matrix, transaction RLP signing (legacy EIP-155 + EIP-1559 type-2,
// k256 RFC6979 deterministic signing with recover roundtrip), block-header parsing
// and keccak hash fingerprints.
//
// Version pins (compatible-combination evidence):
//   * alloy-consensus =2.2.0, alloy-sol-types =1.6.1, alloy-primitives =1.6.1,
//     alloy-rlp =0.3.16, alloy-eips =2.2.0, k256 =0.13.4 (the latest on each line).
//     consensus 2.2.0 declares alloy-primitives ^1.6.0 and alloy-eips ^2.2.0 (same
//     release train) plus alloy-rlp ^0.3.14; sol-types 1.6.1 declares
//     alloy-primitives ^1.6.1, and since the primitives 1.x top is 1.6.1 (no 2.x),
//     pinning =1.6.1 keeps the whole graph single-versioned; k256 =0.13.4 matches the
//     green c_k256_ecdsa version and is the top of consensus's ^0.13 k256 feature.
//   * Feature cut: consensus defaults to std only; c-kzg/blst/secp256k1-sys/sha3-asm
//     stay in the lock graph but not the build graph (cargo tree -e normal,build shows
//     96 packages, zero C/FFI/assembly), and keccak uses the pure-Rust sha3 path. The
//     `k256` feature only enables Signed's recover_signer (pure-Rust k256 recovery).
//
// Determinism:
//   * Every input is a file constant; signing uses k256's RFC6979 HMAC-DRBG
//     (sign_prehash_recoverable) with zero RNG; keccak/RLP/ABI/recover are pure
//     functions; no time/thread/HashMap iteration/network/TTY/environment surface.
//   * External public anchors (third-party evidence independent of the
//     implementation): the ERC20 transfer selector 0xa9059cbb; the EIP-155 spec
//     example transaction fields (nonce 9 / 20gwei / 21000 / to=0x3535...3535 /
//     1ETH / chain 1 -> v=37); the Ethereum mainnet genesis header keccak
//     0xd4e56740f876aef8...1cb8fa3; and the RLP string-length 55/56 head bytes b7/b8.
//
// Known FRONTIER workaround (semantics unchanged, same trap as c_revm_evm): const-hex
// 1.x probes ssse3/avx2 at runtime and takes the SIMD `_mm_lddqu_si128` path, an x86
// intrinsic mirvm does not have (the trap reads `foreign `llvm.x86.sse3.ldu.dq``).
// alloy-primitives' hex module and FixedBytes/B256/Address LowerHex/Display all hit
// it. Every runtime hex operation here therefore uses hand-written byte-wise
// hex()/unhex(); address!/b256! are compile-time const parses (evaluated by the host
// rustc, outside mirvm's runtime). The printed content is equivalent, so the
// three-way comparison is unaffected.
//
// Coverage:
//   1) Static ABI matrix: (U256,bool,i64,Address,FixedBytes<32>,[u16;4]) encode plus
//      the validate decode roundtrip.
//   2) Dynamic ABI matrix: (String with multi-byte UTF-8, Bytes, Vec<u64>) encode
//      plus the decode roundtrip.
//   3) Nested ABI matrix: ((u64,String),Vec<(u16,bool)>,(Address,U256)) nested tuple
//      plus a dynamic tuple array. (Upstream alloy-sol-types deliberately does not
//      implement SolValue for u8 -- Vec<u8>/[u8;N] specialize to Bytes/FixedBytes --
//      so the array elements use u16.)
//   4) The non-standard abi_encode_packed surface.
//   5) Decode negative paths: a truncated half-packet and a wrong-typed decode must Err.
//   6) Hand-written function selector: keccak("transfer(address,uint256)")[..4] plus
//      the argument encoding gives the full calldata anchor.
//   7) Signing: a fixed private key (k256's embedded test vector D) -> address anchor;
//      TxLegacy (EIP-155 chain 1) and TxEip1559 (type-2, 36B input) each go through
//      sighash -> RFC6979 deterministic signing -> Signed -> eip2718 encoding (len+hex
//      anchors) -> recover_signer == address -> TxEnvelope decode/re-encode roundtrip.
//   8) Block headers: the mainnet genesis header's 15-field RLP (len+fnv+head96) with
//      hash_slow == the public genesis hash anchor and a decode roundtrip; a London-style
//      header (base_fee=Some, all fields non-empty) with RLP/hash/roundtrip.
//   9) Raw RLP edges: empty string (0x80), the 55/56B length boundary (b7/b8), a single
//      byte as-is (0x7f), a nested list, and a truncated decode that must Err.
//
// Three-way rerun:
//   A: target/release/mirvm run corpus/c_ethers_evm.rs
//   B: d=$(grep -l 'name = "c_ethers_evm"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)
//      && cd "$d" && cargo +nightly-2026-07-02 run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_ethers_evm.rs
//
// The measured green baseline: A/B/C agree byte-for-byte on 36 stdout lines with exit 0;
// A/C stderr is empty and B's stderr carries only cargo build lines. Key anchors:
// abi1.static.enc len=288 fnv=f5af2825935bf254; abi6.selector=0xa9059cbb;
// signer=0xdf4abd97183d56aa7fdf00e349a2aa633a2bb86f; legacy.signed len=110 (EIP-155
// v=0x25=37); eip1559.signed len=158 ty=2; genesis.rlp len=535 fnv=69e31c2d5fa07df1;
// genesis.hash matches the mainnet anchor; rlp.s55_head=b7 rlp.s56_head=b8.
//

use alloy_consensus::{
    Header as EthHeader, SignableTransaction, TxEip1559, TxEnvelope, TxLegacy,
};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{
    address, b256, keccak256, Address, Bytes, FixedBytes, Signature, TxKind, B256, U256,
};
use alloy_sol_types::SolValue;
use k256::ecdsa::SigningKey;

/// FNV-1a 64: the inline fingerprint for binary output.
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

/// Print a byte anchor: full hex up to 160B, otherwise len+fnv+the first 96 hex chars.
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

    // ① purely static matrix
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

    // ② dynamic matrix: String (multi-byte UTF-8) / Bytes / Vec<u64>
    let t2 = (
        "hello 以太坊".to_string(),
        Bytes::from(vec![0xcau8, 0xfe, 0xba, 0xbe, 0x00, 0x11]),
        vec![7u64, 8, 9, 10, 11],
    );
    let e2 = t2.abi_encode();
    show_bytes("abi2.dynamic.enc", &e2);
    let d2 = <(String, Bytes, Vec<u64>)>::abi_decode_validate(&e2).unwrap();
    println!("abi2.dynamic.roundtrip={}", t2 == d2);

    // ③ nested tuple + dynamic tuple array
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

    // ④ packed encoding (a non-standard but deterministic ABI surface)
    let p = (U256::from(123456789u64), "packed".to_string(), vec![5u16, 6u16]);
    let ep = p.abi_encode_packed();
    show_bytes("abi4.packed.enc", &ep);

    // ⑤ negative paths: truncation and wrong-type decoding must Err
    let bad = <(String, Bytes, Vec<u64>)>::abi_decode(&e2[..e2.len() / 2]);
    println!("abi5.trunc_err={}", bad.is_err());
    let wrong = <(U256, bool, i64, Address, FixedBytes<32>, [u16; 4])>::abi_decode(&e2);
    println!("abi5.wrongtype_err={}", wrong.is_err());

    // ⑥ hand-written function selector + calldata (no sol! macro surface)
    let selector = &keccak256(b"transfer(address,uint256)")[..4];
    let args = (addr, U256::from(1000000000000000000u128));
    let mut calldata = selector.to_vec();
    calldata.extend_from_slice(&args.abi_encode());
    println!("abi6.selector=0x{}", hex(selector));
    show_bytes("abi6.calldata", &calldata);
}

/// Fixed private key (k256's embedded test vector D) -> address anchor + legacy/eip1559 sign/recover.
fn sign_txs() {
    let d = hex_bytes32("ebb2c082fd7727890a28ac82f6bdf97bad8de9f5d7c9028692de1a255cad3e0f");
    let sk = SigningKey::from_slice(&d).unwrap();
    let ep = sk.verifying_key().to_encoded_point(false);
    let addr = Address::from_raw_public_key(&ep.as_bytes()[1..]);
    println!("signer=0x{}", hex(addr.as_slice()));

    // ① legacy: EIP-155 chain_id=1 (the spec example transaction fields)
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

    // ② eip1559: type-2, 36B input (selector + fixed arguments)
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
    // ① Ethereum mainnet genesis: every field a public constant, keccak as the external anchor.
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

    // ② London-style header: base_fee_per_gas=Some + all fields non-empty, parse/re-encode/fingerprint.
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

    // ③ raw RLP edge matrix: empty string / 55/56B length boundary / single byte / list / truncated negative path.
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
