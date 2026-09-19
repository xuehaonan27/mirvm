#!/usr/bin/env mirvm
---
[dependencies]
revm = { version = "19", default-features = false }
---
// revm 19 (resolves to 19.7.0, the newest on the 1x line; the third VM-in-VM case):
// a pure-Rust EVM interpreter executing fixed bytecode contracts inside mirvm.
// default-features=false cuts the three C/cc native bindings c-kzg/secp256k1/blst
// (they only serve precompiles, unused here), leaving ruint U256, tiny-keccak,
// hashbrown, bitvec and alloy-primitives -- all pure Rust, no FFI, no x86 intrinsic.
// The reduced graph is what keeps this fixture runnable without a C toolchain.
//
// API surface: the Evm::builder chain (with_db / modify_db via insert_account_info /
// with_spec_id / modify_block_env / modify_tx_env / build), transact/transact_commit, the
// three ExecutionResult states (Success{reason,gas,logs,output} / Revert / Halt), InMemoryDB,
// Database::basic readback, Bytecode::new_raw + hash_slow (the tiny-keccak chain), Log topics.
//
// Seven fixed bytecode cases (address/balance/gas limit/block/timestamp/gas price fixed):
//   arith    PUSH1 x2/ADD/PUSH0/MSTORE/RETURN -> 32B holding 5
//   storage  SSTORE x3 (0->nz, 0->nz, nz->0 triggering refund) + SLOAD + RETURN
//   revert   SSTORE then REVERT (storage rolled back) + a 2B output 0xbeef
//   logs     LOG1(32B, topic 0xdead) + LOG0(empty) + STOP
//   badjump  JUMP to a non-JUMPDEST -> Halt(InvalidJump), all gas consumed
//   create   CREATE: initcode SSTOREs 7@0 (on the derived contract's storage) then
//            CODECOPY + RETURN deploys a 5B runtime; address derived from caller+nonce
//   transfer a plain transfer (transact_commit + Database::basic readback proves commit)
// Determinism: state (a HashMap) is printed sorted by address and slots sorted by key;
// every byte output is len+hex+FNV-1a; no time/random/thread/address randomization, and
// the EVM reads no wall clock or environment.
//
// Known FRONTIER workaround (semantics unchanged): const-hex 1.19's encode probes
// ssse3/avx2 at runtime and takes the SIMD `_mm_lddqu_si128` path, an x86 intrinsic mirvm
// does not have (the trap reads `TRAP: foreign `llvm.x86.sse3.ldu.dq``). alloy's
// FixedBytes LowerHex (B256/Address with {:#x}) hits it, so all hex output here goes
// through this file's byte-wise hex() (FixedBytes::as_slice / U256::to_be_bytes); the
// printed content is equivalent, so the native/mirvm byte comparison is unaffected.
use revm::{
    primitives::{
        address, AccountInfo, Address, Bytecode, Bytes, EvmState, ExecutionResult, Output,
        SpecId, TxKind, U256,
    },
    Database, Evm, InMemoryDB,
};

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

/// Shared builder: fix the spec/block/tx environment, then run one Call or Create.
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

/// Pre-seed the contract account -> Call transaction -> print result + state.
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

/// CREATE transaction: initcode deploys the runtime; print the derived address + new contract storage.
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

/// Plain transfer: transact_commit persists, then Database::basic reads it back to verify.
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

    // ③ SSTORE 0x42@7 (rolled back); MSTORE 0xbeef@0; REVERT 2@30 -> output 0xbeef
    run_call(
        "revert",
        CONTRACT_REVERT,
        &[
            0x60, 0x42, 0x60, 0x07, 0x55, 0x61, 0xbe, 0xef, 0x5f, 0x52, 0x60, 0x02, 0x60, 0x1e,
            0xfd,
        ],
        1_000_000,
    );

    // ④ MSTORE 42@0; LOG1 topic=0xdead 32B@0; LOG0 empty; STOP
    run_call(
        "logs",
        CONTRACT_LOGS,
        &[
            0x60, 0x2a, 0x5f, 0x52, 0x61, 0xde, 0xad, 0x60, 0x20, 0x5f, 0xa1, 0x5f, 0x5f, 0xa0,
            0x00,
        ],
        1_000_000,
    );

    // ⑤ PUSH1 2; JUMP -> target 0x56 is not a JUMPDEST -> Halt(InvalidJump)
    run_call(
        "badjump",
        CONTRACT_BADJUMP,
        &[0x60, 0x02, 0x56, 0x5b, 0x00],
        1_000_000,
    );

    // ⑥ initcode: SSTORE 7@0 (on the derived contract); CODECOPY the 5B runtime; RETURN deploys.
    //    The runtime is PUSH1 7; PUSH0; SSTORE; STOP (not run at deploy time, only on a call).
    run_create(
        "create",
        &[
            0x60, 0x07, 0x5f, 0x55, 0x60, 0x05, 0x60, 0x0e, 0x5f, 0x39, 0x60, 0x05, 0x5f, 0xf3,
            0x60, 0x07, 0x5f, 0x55, 0x00,
        ],
        200_000,
    );

    // ⑦ plain transfer of 10^15 wei (a codeless target), 21000 gas + commit
    run_transfer("transfer", TRANSFER_TO, 1_000_000_000_000_000, 100_000);
}
