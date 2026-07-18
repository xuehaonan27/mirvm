#!/usr/bin/env mirvm
---
[dependencies]
miden-assembly = "=0.25.5"
miden-processor = "=0.25.5"
miden-prover = "=0.25.5"
miden-verifier = "=0.25.5"

# 上游破洞绕行（证据见头注「版本钉」第 3 条）：serde-wincode 0.1.2 的
# wincode>=0.4,<1  fresh 解析到 0.6.0，与 miden-* 0.25.5 自用的 wincode^0.5.5
# 撞 trait。把整个图的 wincode 钉到 0.5.5 的 git tag（内容与 registry 0.5.5 同源）。
[patch.crates-io]
wincode = { git = "https://github.com/anza-xyz/wincode", tag = "wincode@v0.5.5" }
---
// miden-vm 0.25.5 证明面（prove → verify → 篡改反锚）三维差分。与批8 c_miden_exec
// 同族（同一 MASM fib 程序、同一 assembler/processor 底座），但本 driver 走完整
// STARK 证明：miden-prover 出证明、miden-verifier 自验、篡改证明字节反向锚。
//
// 版本钉（相容组合证据）：
//   * miden-assembly / miden-processor / miden-prover / miden-verifier 全部钉
//     =0.25.5（crates.io 2026-07-18 最新稳定线，四者同 release train；上游
//     miden-vm 仓库同 tag 发布，processor 0.25.5 与 prover 0.25.5 的依赖约束
//     `version = "0.25"` 互相覆盖 0.25.5，无跨线错配）。
//   * miden-prover 0.25.5 的 STARK 后端已从 winterfell 换成 Plonky3 系的
//     miden-lifted-stark 0.28（经 miden-crypto 0.28 的 stark 模块 re-export），
//     ProvingOptions 只选哈希函数（默认 Blake3_256），FRI/安全参数由
//     miden-air::config 硬编码 96-bit——没有可调的证明参数面，默认即定值。
//   * 上游破洞（2026-07-18 实证，附绕行）：miden-prover/verifier 0.25.5 声明
//     `wincode = "0.5.5"` + `serde-wincode = "0.1"`；serde-wincode 0.1.2 的
//     `wincode >=0.4,<1` 在 fresh resolve 时取 0.6.0（cargo 按各自 req 取最大，
//     直挂 `wincode = "=0.5.5"` 不促成统一——/tmp 草项目实证两版本并存），
//     SerdeCompat<StarkProofData> 的 SchemaRead 与 miden 期望的 wincode 0.5.5
//     trait 错位 → miden-verifier lib.rs:220 E0277 编译失败（整系 0.25.3–0.25.5
//     同 req 全中）。miden-vm 仓库自带 lock 钉在 wincode 0.5.5 故上游 CI 无恙，
//     纯 fresh 消费者踩雷。绕行：frontmatter `[patch.crates-io]` 把 wincode 全图
//     钉到 anza-xyz/wincode 的 tag `wincode@v0.5.5`（registry 0.5.5 的同源发布
//     点），SerdeCompat 与 miden 两边 SchemaRead 归一。属「钉版本避上游破洞」
//     类合法绕行，未裁剪任何规格面。
//
// 确定性说明（证明字节可复现的证据链）：
//   * 证明器内部的 "randomness"（aux trace 随机挑战、FRI 挑战）全部来自
//     Fiat-Shamir channel（channel.sample_algebra_element，种子 = 协议参数 +
//     public values + main commitment），不触碰 OS 随机源；无 ZK 随机带。
//     源码证据：miden-lifted-stark-0.28.0/src/prover/mod.rs:347
//     `channel.sample_algebra_element::<EF>()`。
//   * features 全默认（std），不开 `concurrent`：p3_maybe_rayon 退化为顺序
//     迭代，证明构建单线程，字节级可复现。
//   * 实证：native 同 driver 连跑两次，proof fnv 完全一致（见下「三维实测」）。
//   * tracing::instrument 事件无 subscriber 全静默丢弃，stderr 真空。
//
// 覆盖清单：
//   ① 定值 MASM fib(20)（repeat 定数循环，栈顶终值 [10946, 6765]）经
//      miden_prover::prove_sync 执行 + 出证明：打印栈顶、证明字节长度、
//      FNV-1a/64 指纹、security_level。
//   ② 自验：miden_verifier::verify(ProgramInfo, StackInputs, StackOutputs,
//      proof) 必须 Ok——打印 verify=true 与返回的安全位数。
//   ③ 反向锚：把 STARK 证明体正中间一个字节 XOR 0x01（深度在 FRI/承诺数据
//      区，改任何值都只会得到确定性的验证失败），verify 必须 Err——
//      打印 verify_tampered=false。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_miden_prove.rs
//   B: d=$(grep -l 'name = "c_miden_prove"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_miden_prove.rs
//
// 三维实测（2026-07-18，全绿）：A/B/C 三进程 stdout 逐字节一致（4 行：
// fib stack_top=[10946, 6765, 0, 0]；proof len=37599 fnv=449fe979d1395d47
// security=96；verify=true security=96；verify_tampered=false），stderr 全真空
// （0 字节）、exit 全 0。证明字节跨实现（mirvm 解释/JIT vs native）与跨进程
// 复跑（A/A2、B/B2 各自双跑）指纹完全一致——Fiat-Shamir 全确定，无 ZK 随机带
// 实证成立。时长：A 冷跑（含 245 crate 依赖闭包首次构建）6m19s、热跑 2m45s；
// B 复跑 2.7s（运行本体）；C（JIT=1）2m44.7s（<180s 预算内，为最慢维）。
// 依赖闭包 245 crate（miden 系 0.25.5 四件 + Plonky3 系 p3-* 15 件 + git
// wincode 0.5.5）。无 FRONTIER、无引擎 bug 信号。

use std::sync::Arc;

use miden_assembly::Assembler;
use miden_assembly::debuginfo::{DefaultSourceManager, SourceManager};
use miden_processor::{DefaultHost, ExecutionOptions, Program, StackInputs};
use miden_prover::{AdviceInputs, ExecutionProof, ProvingOptions, prove_sync};
use miden_verifier::{ProgramInfo, verify};

/// fib(20)：repeat 定数循环。栈终 [fib(21), fib(20), 0×14] = [10946, 6765, …]。
/// 与批8 c_miden_exec 同一定值程序（栈深配平已在批8验证）。
const FIB_SRC: &str = r"
begin
    push.1
    repeat.20
        swap dup.1 add
    end
    movup.15 drop
end
";

/// FNV-1a 64：证明字节指纹（无外部依赖，位确定）。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn assemble(name: &str, source: &str) -> Program {
    let sm: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    Assembler::new(sm)
        .assemble_program(name, source)
        .unwrap_or_else(|e| panic!("assemble {name}: {e}"))
        .unwrap_program()
}

fn main() {
    let program = assemble("fib", FIB_SRC);
    let program_info = ProgramInfo::from(program.clone());
    let stack_inputs = StackInputs::default();

    // ① 执行 + 出证明（默认 ProvingOptions = Blake3_256，96-bit 参数硬编码）。
    let mut host = DefaultHost::default();
    let (stack_outputs, proof) = prove_sync(
        &program,
        stack_inputs.clone(),
        AdviceInputs::default(),
        &mut host,
        ExecutionOptions::default(),
        ProvingOptions::default(),
    )
    .unwrap_or_else(|e| panic!("prove fib: {e}"));

    let outs: Vec<u64> = stack_outputs.iter().map(|f| f.as_canonical_u64()).collect();
    assert_eq!(&outs[..2], &[10946, 6765], "fib 栈顶终值不符");
    let proof_bytes = proof.to_bytes();
    println!("fib stack_top={:?}", &outs[..4]);
    println!(
        "proof len={} fnv={:016x} security={}",
        proof_bytes.len(),
        fnv1a64(&proof_bytes),
        proof.security_level()
    );

    // ② 自验正锚。
    let security = verify(
        program_info.clone(),
        stack_inputs.clone(),
        stack_outputs.clone(),
        proof.clone(),
    )
    .unwrap_or_else(|e| panic!("verify good proof: {e}"));
    println!("verify=true security={security}");

    // ③ 篡改反向锚：证明体正中字节 XOR 0x01，verify 必须 Err。
    let mut tampered: ExecutionProof = proof;
    let mid = tampered.proof.len() / 2;
    tampered.proof[mid] ^= 0x01;
    let ok = verify(program_info, stack_inputs, stack_outputs, tampered).is_ok();
    assert!(!ok, "篡改后的证明不应通过验证");
    println!("verify_tampered={ok}");
}
