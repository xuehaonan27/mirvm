#!/usr/bin/env mirvm
---
[dependencies]
revm = { version = "19", default-features = false }
---
// revm 19（解析为 19.7.0，1x 线最新；VM-in-VM 第三弹）：纯 Rust EVM 解释器
// 在 mirvm 里解释执行固定字节码小合约。default-features=false 裁掉
// c-kzg/secp256k1/blst 三个 C/cc 原生绑定（只服务 precompile，本 driver 不调），
// 剩余依赖（ruint U256、tiny-keccak、hashbrown、bitvec、alloy-primitives）
// 全为纯 Rust——无 FFI、无 x86 intrinsic。
//
// API 覆盖面：Evm::builder 链（with_db / modify_db 经由 insert_account_info /
// with_spec_id / modify_block_env / modify_tx_env / build）、transact /
// transact_commit、ExecutionResult 三态（Success{reason,gas,logs,output} /
// Revert / Halt）、InMemoryDB、Database::basic 读回、Bytecode::new_raw +
// hash_slow（tiny-keccak 全链路）、Output::Call/Create、Log topics。
//
// 七个固定字节码用例（地址/余额/gas limit/区块号/时间戳/gas price 全固定）：
//   arith    PUSH1×2/ADD/PUSH0/MSTORE/RETURN → 32B 的 5
//   storage  SSTORE×3（0→nz、0→nz、nz→0 触发 refund）+ SLOAD + RETURN
//   revert   SSTORE 后 REVERT（存储回滚）+ 2B 输出 0xbeef
//   logs     LOG1(32B, topic 0xdead) + LOG0(空) + STOP
//   badjump  JUMP 到非 JUMPDEST → Halt(InvalidJump)，gas 全耗
//   create   CREATE：initcode 先 SSTORE 7@0（挂在派生合约存储上）再 CODECOPY
//            + RETURN 部署 5B runtime，caller+nonce 派生地址
//   transfer 纯转账（transact_commit + Database::basic 读回验证落库）
// 确定性：state（HashMap）按地址排序、slot 按键排序后打印；所有字节输出
// len+hex+FNV-1a；无时间/随机/线程/地址随机化。
//
// 已知 FRONTIER 绕行（语义不变）：const-hex 1.19 的 encode 在运行期检测
// ssse3/avx2 后走 SIMD 路径 `_mm_lddqu_si128`——mirvm 未内建的 x86 intrinsic
// （TRAP 原文：`TRAP: foreign `llvm.x86.sse3.ldu.dq`，触发 fn 为
// core_arch::x86::sse3::__mm_lddqu_si128，单态化自 c_revm_evm）。alloy 的
// FixedBytes LowerHex（B256/Address 的 {:#x}）会命中该路径。故所有 hex 输出
// 改走本文件的逐字节 hex()（FixedBytes::as_slice / U256::to_be_bytes），
// 打印内容等价，native/mirvm 逐字节对拍不受影响。
use revm::{
    primitives::{
        address, AccountInfo, Address, Bytecode, Bytes, EvmState, ExecutionResult, Output,
        SpecId, TxKind, U256,
    },
    Database, Evm, InMemoryDB,
};

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
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn show_bytes(tag: &str, data: &[u8]) {
    println!(
        "{tag} len={} hex={} fnv={:016x}",
        data.len(),
        hex(data),
        fnv1a(data)
    );
}

const CALLER: Address = address!("1000000000000000000000000000000000000001");
const COINBASE: Address = address!("c01ba5e000000000000000000000000000000001");
const CONTRACT_ARITH: Address = address!("2000000000000000000000000000000000000001");
const CONTRACT_STORAGE: Address = address!("2000000000000000000000000000000000000002");
const CONTRACT_REVERT: Address = address!("2000000000000000000000000000000000000003");
const CONTRACT_LOGS: Address = address!("2000000000000000000000000000000000000004");
const CONTRACT_BADJUMP: Address = address!("2000000000000000000000000000000000000005");
const TRANSFER_TO: Address = address!("3000000000000000000000000000000000000001");

/// 10^18 wei。
const CALLER_BALANCE: u64 = 1_000_000_000_000_000_000;
/// 10 wei/gas。
const GAS_PRICE: u64 = 10;

fn seed_caller(db: &mut InMemoryDB) {
    db.insert_account_info(
        CALLER,
        AccountInfo {
            balance: U256::from(CALLER_BALANCE),
            ..Default::default()
        },
    );
}

fn print_result(label: &str, result: &ExecutionResult) {
    match result {
        ExecutionResult::Success {
            reason,
            gas_used,
            gas_refunded,
            logs,
            output,
        } => {
            let (odata, created): (&Bytes, Option<Address>) = match output {
                Output::Call(d) => (d, None),
                Output::Create(d, a) => (d, *a),
            };
            println!(
                "{label} result=success reason={reason:?} gas={gas_used} refunded={gas_refunded}"
            );
            if let Some(a) = created {
                println!("{label} created=0x{}", hex(a.as_slice()));
            }
            show_bytes(&format!("{label} output"), odata);
            for (i, lg) in logs.iter().enumerate() {
                let topics = lg
                    .topics()
                    .iter()
                    .map(|t| hex(t.as_slice()))
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{label} log{i} addr=0x{} topics=[{topics}]",
                    hex(lg.address.as_slice())
                );
                show_bytes(&format!("{label} log{i} data"), &lg.data.data);
            }
        }
        ExecutionResult::Revert { gas_used, output } => {
            println!("{label} result=revert gas={gas_used}");
            show_bytes(&format!("{label} output"), output);
        }
        ExecutionResult::Halt { reason, gas_used } => {
            println!("{label} result=halt reason={reason:?} gas={gas_used}");
        }
    }
}

fn print_state(label: &str, state: &EvmState) {
    let mut accounts: Vec<_> = state.iter().collect();
    accounts.sort_by(|a, b| a.0.cmp(b.0));
    println!("{label} state accounts={}", accounts.len());
    for (addr, acct) in accounts {
        println!(
            "{label} acct 0x{} balance={} nonce={} status={:?}",
            hex(addr.as_slice()),
            acct.info.balance,
            acct.info.nonce,
            acct.status
        );
        let mut slots: Vec<_> = acct.storage.iter().collect();
        slots.sort_by(|a, b| a.0.cmp(b.0));
        for (k, slot) in slots {
            println!(
                "{label} slot 0x{} orig=0x{} -> now=0x{}",
                hex(&k.to_be_bytes::<32>()),
                hex(&slot.original_value.to_be_bytes::<32>()),
                hex(&slot.present_value.to_be_bytes::<32>())
            );
        }
    }
}

/// 公共 builder：固定 spec/区块环境/交易环境后执行一笔 Call 或 Create。
fn build_evm(
    db: InMemoryDB,
    to: TxKind,
    data: Bytes,
    value: u64,
    gas_limit: u64,
) -> Evm<'static, (), InMemoryDB> {
    Evm::builder()
        .with_db(db)
        .with_spec_id(SpecId::CANCUN)
        .modify_block_env(|b| {
            b.number = U256::from(15_000_000u64);
            b.timestamp = U256::from(1_700_000_000u64);
            b.coinbase = COINBASE;
        })
        .modify_tx_env(|tx| {
            tx.caller = CALLER;
            tx.gas_limit = gas_limit;
            tx.gas_price = U256::from(GAS_PRICE);
            tx.transact_to = to;
            tx.value = U256::from(value);
            tx.data = data;
            tx.nonce = Some(0);
        })
        .build()
}

/// 预置合约账户 → Call 交易 → 打印 result + state。
fn run_call(label: &str, contract: Address, raw: &'static [u8], gas_limit: u64) {
    println!("== {label} ==");
    let code = Bytecode::new_raw(Bytes::from_static(raw));
    let code_hash = code.hash_slow();
    show_bytes(&format!("{label} code"), raw);
    println!("{label} code_hash=0x{}", hex(code_hash.as_slice()));
    let mut db = InMemoryDB::default();
    seed_caller(&mut db);
    db.insert_account_info(contract, AccountInfo::new(U256::ZERO, 1, code_hash, code));
    let mut evm = build_evm(
        db,
        TxKind::Call(contract),
        Bytes::new(),
        0,
        gas_limit,
    );
    let out = evm.transact().unwrap();
    print_result(label, &out.result);
    print_state(label, &out.state);
}

/// CREATE 交易：initcode 部署 runtime，打印派生地址 + 新合约存储。
fn run_create(label: &str, initcode: &'static [u8], gas_limit: u64) {
    println!("== {label} ==");
    show_bytes(&format!("{label} initcode"), initcode);
    let mut db = InMemoryDB::default();
    seed_caller(&mut db);
    let mut evm = build_evm(
        db,
        TxKind::Create,
        Bytes::from_static(initcode),
        0,
        gas_limit,
    );
    let out = evm.transact().unwrap();
    print_result(label, &out.result);
    print_state(label, &out.state);
}

/// 纯转账：transact_commit 落库后用 Database::basic 读回验证。
fn run_transfer(label: &str, to: Address, value: u64, gas_limit: u64) {
    println!("== {label} ==");
    let mut db = InMemoryDB::default();
    seed_caller(&mut db);
    let mut evm = build_evm(db, TxKind::Call(to), Bytes::new(), value, gas_limit);
    let result = evm.transact_commit().unwrap();
    print_result(label, &result);
    let db = evm.db_mut();
    let caller = db.basic(CALLER).unwrap().unwrap();
    let recv = db.basic(to).unwrap().unwrap();
    let miner = db.basic(COINBASE).unwrap().unwrap();
    println!(
        "{label} committed caller balance={} nonce={}",
        caller.balance, caller.nonce
    );
    println!(
        "{label} committed recv balance={} nonce={}",
        recv.balance, recv.nonce
    );
    println!(
        "{label} committed coinbase balance={} nonce={}",
        miner.balance, miner.nonce
    );
}

fn main() {
    // ① PUSH1 2; PUSH1 3; ADD; PUSH0; MSTORE; PUSH1 32; PUSH0; RETURN → 5
    run_call(
        "arith",
        CONTRACT_ARITH,
        &[
            0x60, 0x02, 0x60, 0x03, 0x01, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ],
        1_000_000,
    );

    // ② SSTORE 42@5; SSTORE 7@6; SSTORE 0@6（refund）; SLOAD 5; MSTORE; RETURN → 42
    run_call(
        "storage",
        CONTRACT_STORAGE,
        &[
            0x60, 0x2a, 0x60, 0x05, 0x55, 0x60, 0x07, 0x60, 0x06, 0x55, 0x5f, 0x60, 0x06, 0x55,
            0x60, 0x05, 0x54, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ],
        1_000_000,
    );

    // ③ SSTORE 0x42@7（回滚）; MSTORE 0xbeef@0; REVERT 2@30 → 输出 0xbeef
    run_call(
        "revert",
        CONTRACT_REVERT,
        &[
            0x60, 0x42, 0x60, 0x07, 0x55, 0x61, 0xbe, 0xef, 0x5f, 0x52, 0x60, 0x02, 0x60, 0x1e,
            0xfd,
        ],
        1_000_000,
    );

    // ④ MSTORE 42@0; LOG1 topic=0xdead 32B@0; LOG0 空; STOP
    run_call(
        "logs",
        CONTRACT_LOGS,
        &[
            0x60, 0x2a, 0x5f, 0x52, 0x61, 0xde, 0xad, 0x60, 0x20, 0x5f, 0xa1, 0x5f, 0x5f, 0xa0,
            0x00,
        ],
        1_000_000,
    );

    // ⑤ PUSH1 2; JUMP → 目标 0x56 非 JUMPDEST → Halt(InvalidJump)
    run_call(
        "badjump",
        CONTRACT_BADJUMP,
        &[0x60, 0x02, 0x56, 0x5b, 0x00],
        1_000_000,
    );

    // ⑥ initcode：SSTORE 7@0（挂在派生合约上）; CODECOPY 5B runtime; RETURN 部署。
    //    runtime = PUSH1 7; PUSH0; SSTORE; STOP（部署时不执行，调用才跑）。
    run_create(
        "create",
        &[
            0x60, 0x07, 0x5f, 0x55, 0x60, 0x05, 0x60, 0x0e, 0x5f, 0x39, 0x60, 0x05, 0x5f, 0xf3,
            0x60, 0x07, 0x5f, 0x55, 0x00,
        ],
        200_000,
    );

    // ⑦ 纯转账 10^15 wei（无代码目标），21000 gas + commit 落库
    run_transfer("transfer", TRANSFER_TO, 1_000_000_000_000_000, 100_000);
}
