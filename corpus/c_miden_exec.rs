#!/usr/bin/env mirvm
---
[dependencies]
miden-assembly = "=0.25.5"
miden-processor = "=0.25.5"
---
// miden-vm 0.25.5（crates.io 2026-07-17 最新稳定线；批任务文本写 0.13/0.14/0.15，
// 上游实际已发到 0.25。按 task 精神取最新稳定钉到 patch）VM-in-VM 执行差分。
//
// 形态与绕行（execute-only，绝不开证明）：
//   * miden-vm umbrella crate 把 miden-prover 列为【非可选】依赖——证明器体量与
//     Fiat-Shamir 随机通道双双出批任务边界。绕行：直用 miden-assembly（MASM →
//     MAST 汇编）+ miden-processor（FastProcessor 同步执行）两个底层 crate
//     （任务文本明确授权 "miden_vm::execute（或 Processor 等价物）"）。umbrella /
//     prover / verifier / winterfell 一概不进闭包。
//   * features 全默认（std）；不开 miden-processor 的 `concurrent`：trace 构建
//     （trace::build_trace 把执行轨迹排成列矩阵，非 STARK 证明）走单线程，输出
//     确定性。物化闭包 ~176 个 normal crate（≤200 预算内）。
//
// 测试面（三个 MASM 小程序，经 miden_assembly::Assembler 汇编成 Package →
// unwrap_program → miden_processor 双路执行）：
//   ① fib(20)：repeat.20 定数循环（swap/dup.1/add），栈顶终值 [10946, 6765]，
//      断言 fib(21)/fib(20)。
//   ② 定值列表折叠乘加：acc=2 对 [3,5,7,11,13] 逐元 acc = acc*x + x（swap
//      dup.1 mul add 四元组），终值 51207。
//   ③ 深度递归求和 sum(14)=105：MASM 静态禁用递归（linker callgraph 环检测，
//      procref 自引用同样被拒）。走 dyncall 动态调用面：
//        - dyncall 从调用方上下文内存按地址读 callee 摘要 word，且新上下文
//          内存隔离（根上下文存的值 callee 看不到）。
//        - 摘要传递用两轮汇编：轮一 procref.sum_to 把过程摘要推上操作数栈，
//          Rust 读 StackOutputs 取回；轮二把摘要作为 adv_map 常量声明进程序
//          （汇编器写进 MastForest 的 advice map，执行期自动装入全局唯一的
//          advice provider——跨上下文共享，不随上下文切换切换）。
//        - 每帧 push.KEY adv.push_mapval adv_pushw（Pad4+AdvPopW 推送 word）
//          自取摘要写本帧 mem[100]，dropw 清场后 push.100 dyncall 下潜；n 与
//          累计链直接留在操作数栈上继承给 callee。
//        - 每帧结算后 dup.0 push.<EventId> emit 发射 S(k)，DefaultHost
//          ::register_handler 配一个 Rust 闭包（Arc<Mutex<Vec>>）按执行序回收
//          15 个发射值 [0,1,3,6,10,15,21,28,36,45,55,66,78,91,105]。
//        - 路径上压到 InvalidStackDepthOnReturn（call/dyncall 返回时 callee 栈
//          深必须恰 16）与 "stack should have at most 16 elements" 两条 VM
//          不变式检查面（调参阶段全部亲手触发过）。
//   每程序双路对拍：miden_processor::execute_sync（纯 FastProcessor）与
//   FastProcessor::execute_trace_inputs_sync + trace::build_trace（轨迹构建，
//   非证明），断言两路 StackOutputs 全等、rec 路两路 emit 序列全等；打印栈顶
//   与 trace_len/core/range/hash/bitwise/memory 各段轨迹长度（miden 处理机在
//   native 与 mirvm 上是同一套算法，轨迹长度属全确定量）。
//
// 确定性纪律：全部 MASM 源码与汇编配置定值化；无时间/随机/env/TLS 序；Felt 输出
// 一律 as_canonical_u64；emit 事件序 = VM 执行序；不注册任何 tracing subscriber
// （miden 的 tracing::instrument 事件全静默丢弃）；Error/Report 一律 panic 而不
// 打印。输出约 12 行。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_miden_exec.rs
//   B: d=$(grep -l 'name = "c_miden_exec"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_miden_exec.rs
//
// 调研期记录在案的 miden 栈语义坑（driver 已依正确语义写就）：MLoadW / AdvPopW
// 是先弹地址再原位复写 top-4（非推入）；adv_pushw = Pad 四零 + AdvPopW 的真推入
// 形式；swapdw 交换的是【两个】word（8 元素）而非双字；dyncall 只弹一个地址
// 元素，callee 继承调用方 top-16 但内存单开；drop 类指令在栈深 ≥16 的 floor
// 下从溢出区拉零回窗口。中期曾按错误模型绕过的死路（手工帧栈、栈上摘要互传、
// procref 自递归）均已剔除。
//
// 三维实测（2026-07-17，全绿）：A/C/B 三进程 stdout 逐字节一致（8 行：fib
// stack_top=[10946,6765,…] trace_len=128 core=75 range=45；fold [51207]
// trace_len=128 core=65；rec emits=[0,1,3,6,10,15,21,28,36,45,55,66,78,91,105]
// stack_top=[105] trace_len=1024 core=884 range=63 hash=560 bitwise=0 memory=46），
// stderr 全真空、exit 全 0。A 维（含 mirvm 首次构建依赖闭包）~38s，C 维 ~2s，
// B 维 ~31s（重构建：miden 系 crate 图大，记为重条目理由）。无 FRONTIER、
// 无引擎 bug 信号。

use std::sync::{Arc, Mutex};

use miden_assembly::debuginfo::{DefaultSourceManager, SourceManager};
use miden_assembly::Assembler;
use miden_processor::advice::{AdviceInputs, AdviceMutation};
use miden_processor::event::{EventError, EventName};
use miden_processor::trace::build_trace;
use miden_processor::{
    DefaultHost, ExecutionOptions, ExecutionOutput, FastProcessor, Program, StackInputs,
};

/// ① fib(20)：repeat 定数循环。栈终 [fib(21), fib(20), 0×14] = [10946, 6765, …]。
/// 开头 push.1 净 +1，结尾 movup.15 drop 恰好配平（miden 结束态要求栈深 ≤16）。
const FIB_SRC: &str = r"
begin
    push.1
    repeat.20
        swap dup.1 add
    end
    movup.15 drop
end
";

/// ② 折叠乘加：acc=2，对 x ∈ [3,5,7,11,13] 做 acc = acc*x + x。
/// 2→9→50→357→3938→51207。六次净 push，结尾六对 movup.15 drop 配平。
const FOLD_SRC: &str = r"
begin
    push.2
    push.3  swap dup.1 mul add
    push.5  swap dup.1 mul add
    push.7  swap dup.1 mul add
    push.11 swap dup.1 mul add
    push.13 swap dup.1 mul add
    movup.15 drop movup.15 drop movup.15 drop
    movup.15 drop movup.15 drop movup.15 drop
end
";

const EVT_NAME: &str = "mirvm::rec::sum";

/// adv_map 查表键对应的 MASM 立即数串（push.7.0.0.0 → 栈 word，即
/// copy_map_value_to_adv_stack 在 pos1..4 读到的键）。
const KEY_HEX: &str =
    "0x0000000000000000000000000000000000000000000000000700000000000000";

/// ③ sum_to 过程体：n≠0 时降一、自取摘要、下潜、回加；尾发射 S(n)。
/// FRAMELOAD 展开（净栈效应 0）：
///   push.7.0.0.0     键 word 上栈（末位元素在栈顶）
///   adv.push_mapval  系统事件节点：按栈上键把 map 值推进 advice 栈（栈中性）
///   dropw            丢掉键 word
///   adv_pushw        Pad4+AdvPopW：摘要 word 推上栈顶（+4）
///   mem_storew_le.100 存摘要到本帧局部内存 100（原值保留）
///   dropw            丢摘要（-4）
const SUM_BODY: &str = r"
proc sum_to
    dup.0 push.0 neq
    if.true
        dup.0 push.1 sub
        push.7.0.0.0 adv.push_mapval dropw adv_pushw mem_storew_le.100 dropw
        push.100 dyncall
        add
    end
    dup.0 push.{evt} emit drop drop
end
";

fn sum_body(evt: u64) -> String {
    SUM_BODY.replace("{evt}", &evt.to_string())
}

/// 完整 rec 源码：adv_map 带真实摘要（轮二注入）。
fn rec_src(vals: &[String; 4], evt: u64) -> String {
    format!(
        r"adv_map KEY({KEY_HEX}) = [0x{}, 0x{}, 0x{}, 0x{}]

begin
    procref.sum_to mem_storew_le.100 dropw
    push.14
    push.100 dyncall
    movup.15 drop
end

{}
",
        vals[0], vals[1], vals[2], vals[3],
        sum_body(evt),
    )
}

fn assemble(name: &str, source: &str) -> Program {
    let sm: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    Assembler::new(sm)
        .assemble_program(name, source)
        .unwrap_or_else(|e| panic!("assemble {name}: {e}"))
        .unwrap_program()
}

/// 事件 sink：DefaultHost + 注册 mirvm::rec::sum 处理器，把发射值按序收进 Vec。
fn run_with_host(
    program: &Program,
    tag: &str,
    sink: &Arc<Mutex<Vec<u64>>>,
) -> (Vec<u64>, Vec<u64>) {
    let mut host = DefaultHost::default();
    let cell = sink.clone();
    host.register_handler(
        EventName::new(EVT_NAME),
        Arc::new(move |state: &miden_processor::ProcessorState| {
            let v = state.get_stack_item(1).as_canonical_u64();
            cell.lock().unwrap().push(v);
            Ok(Vec::new()) as Result<Vec<AdviceMutation>, EventError>
        }),
    )
    .unwrap_or_else(|e| panic!("register handler {tag}: {e}"));
    let out = miden_processor::execute_sync(
        program,
        StackInputs::default(),
        AdviceInputs::default(),
        &mut host,
        ExecutionOptions::default(),
    )
    .unwrap_or_else(|e| panic!("exec {tag}: {e}"));
    let stack = out.stack.iter().map(|f| f.as_canonical_u64()).collect();
    (stack, sink.lock().unwrap().clone())
}

/// 轨迹路：execute_trace_inputs_sync + build_trace；返回 (栈, 各段轨迹长度)。
fn run_traced(
    program: &Program,
    tag: &str,
    sink: &Arc<Mutex<Vec<u64>>>,
) -> (Vec<u64>, Vec<u64>, (usize, usize, usize, usize, usize, usize)) {
    let mut host = DefaultHost::default();
    let cell = sink.clone();
    host.register_handler(
        EventName::new(EVT_NAME),
        Arc::new(move |state: &miden_processor::ProcessorState| {
            let v = state.get_stack_item(1).as_canonical_u64();
            cell.lock().unwrap().push(v);
            Ok(Vec::new()) as Result<Vec<AdviceMutation>, EventError>
        }),
    )
    .unwrap_or_else(|e| panic!("register handler {tag} traced: {e}"));
    let inputs = FastProcessor::new(StackInputs::default())
        .execute_trace_inputs_sync(program, &mut host)
        .unwrap_or_else(|e| panic!("trace-exec {tag}: {e}"));
    let trace = build_trace(inputs).unwrap_or_else(|e| panic!("build-trace {tag}: {e}"));
    let stack: Vec<u64> = trace.stack_outputs().iter().map(|f| f.as_canonical_u64()).collect();
    let sum = trace.trace_len_summary();
    let ch = sum.chiplets_trace_len();
    (
        stack,
        sink.lock().unwrap().clone(),
        (
            trace.length(),
            sum.core_trace_len(),
            sum.range_trace_len(),
            ch.hash_chiplet_len(),
            ch.bitwise_chiplet_len(),
            ch.memory_chiplet_len(),
        ),
    )
}

fn stack_str(s: &[u64]) -> String {
    format!("{:?}", &s[..8])
}

fn run_program(tag: &str, src: &str, want_stack: &[u64], want_emits: Option<&[u64]>) {
    let program = assemble(tag, src);

    let sink_plain = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (s_plain, e_plain) = run_with_host(&program, tag, &sink_plain);

    let sink_traced = Arc::new(Mutex::new(Vec::<u64>::new()));
    let (s_traced, e_traced, lens) = run_traced(&program, tag, &sink_traced);

    assert_eq!(s_plain, s_traced, "{tag}: execute_sync 与 trace 路栈输出不一致");
    assert_eq!(&s_plain[..want_stack.len()], want_stack, "{tag}: 栈顶终值不符");
    if let Some(want) = want_emits {
        assert_eq!(e_plain, want, "{tag}: execute_sync 路 emit 序列不符");
        assert_eq!(e_traced, want, "{tag}: trace 路 emit 序列不符");
        println!("{tag} emits={e_plain:?}");
    }
    println!("{tag} stack_top={}", stack_str(&s_plain));
    println!(
        "{tag} trace_len={} core={} range={} hash={} bitwise={} memory={}",
        lens.0, lens.1, lens.2, lens.3, lens.4, lens.5
    );
}

fn main() {
    run_program("fib", FIB_SRC, &[10946, 6765], None);
    run_program("fold", FOLD_SRC, &[51207], None);

    // ③ rec：轮一——裸 procref 程序取 sum_to 摘要（过程摘要只依赖自身体，
    // 与本程序 begin 块无关）。
    let evt = EventName::new(EVT_NAME).to_event_id().as_u64();
    let probe_src = format!(
        "begin procref.sum_to movup.15 drop movup.15 drop movup.15 drop movup.15 drop end\n\n{}\n",
        sum_body(evt)
    );
    let probe = assemble("rec_probe", &probe_src);
    let mut host0 = DefaultHost::default();
    let out0: ExecutionOutput = miden_processor::execute_sync(
        &probe,
        StackInputs::default(),
        AdviceInputs::default(),
        &mut host0,
        ExecutionOptions::default(),
    )
    .unwrap_or_else(|e| panic!("rec probe exec: {e}"));
    let vals: [String; 4] = out0
        .stack
        .iter()
        .take(4)
        .map(|f| format!("{:x}", f.as_canonical_u64()))
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();

    // 轮二——adv_map 注入真实摘要，全程 14 层 dyncall 递归。
    let src = rec_src(&vals, evt);
    run_program(
        "rec",
        &src,
        &[105],
        Some(&[0, 1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 66, 78, 91, 105]),
    );
}
