#!/usr/bin/env mirvm
---
[dependencies]
# starlark 0.13.0（crates.io 0.13 线唯一且最高的发布版；任务书限定 0.12/0.13，
# 0.14.x 拉 blake3 默认 feature 族——cpuid/SIMD 派发高风险面，不取）。
# crate 无 feature 表（0.13.0 features={}）：default 即全集，rustyline/
# debugserver-types 进闭包但运行期不触发。
starlark = "=0.13.0"
# #[starlark_module] 宏展开在宿主代码生成 anyhow::Result 引用，宿主必须自带
# anyhow（starlark 文档与 buck2 惯例）；钉 patch。
anyhow = "=1.0.100"
# 绕上游 semver 破洞：starlark_map 0.13.0 以 allocative[hashbrown] 的 RawTable
# Allocative 实现为基础类型；allocative 0.3.6 把该 optional 依赖私下从
# hashbrown 0.14 升到 0.16（未 bump minor），与 starlark_map 自建 hashbrown
# 0.14 分叉 → RawTable: Allocative 不满足，编译期失败。0.3.5 已 yank；钉
# =0.3.4（其 hashbrown optional 仍 ^0.14.3 + "raw" feature）。
allocative = "=0.3.4"
---
// starlark 0.13.0：Bazel/Buck 的 Starlark 方言求值器（字节码解释器，VM-in-VM，
// 批8 波2）。三段脚本经 AstModule::parse + Evaluator::eval_module 顺序在同一
// Module 上求值；宿主经 #[starlark_module] 注入 bump() 回调（调用计数 + fold
// 状态经 Evaluator.extra 回传），经 GlobalsBuilder::set 与 Module::set 两通道
// 注入四个值供脚本回读；EvalModule 返回值（list of tuple/混合）逐一 repr 打印；
// 宿主反向 eval_function 调脚本函数（位置参数 + 命名参数两通道）；freeze 后
// FrozenModule 命名空间导出（BTreeSet 排序名 + 各导出值 repr）；两个评估错误
// 的 ErrorKind 类别固定打印。
//
// 测试面：
//   S1 递归 fib：def/if/return/列表推导/range，fib(0..15) repr；脚本调用宿主
//     bump(fib(10))（跨边界值回环）。
//   S2 数据结构：dict 计数（get 默认值、keys）、sorted 键序、list.index 搜索、
//     最值搜索循环回读；任意精度整数（starlark int = num-bigint 支撑）：
//     1<<100 的 % 与 // 整除——num-bigint 0.4.8 的 u64 按量除法走 div_wide
//     内联 `div` 指令 asm（旧 tier-0 时代的红点条目，本批验证当前引擎 asm
//      lowering 回归面）。
//   S3 字符串与模板：join/str.format/% 格式化、上界切片、replace、find、
//     len；globals 注入（host_mul=7 / host_suffix）与 module 注入
//     （injected_base=1000 / injected_label）全部经脚本计算回读。
//   错误面：脚本 fail("boom-42") → ErrorKind::Fail 整段 Traceback 打印；宿主
//     eval_function 错参 → ErrorKind::Function。kind 经 match 映射为固定串。
//   终态：冻结模块导出 7 名（build_strings/fib/greet/两注入值/run/search）
//     + 宿主回调调用次数 5 与 fold 折叠值。
//
// C 维实锤引擎 bug（批8 波2 本 driver 首撞；expected-red，引擎侧未修）：
// MIRVM_JIT_THRESHOLD=1 下，JIT 编译 starlark ChunkChain::drop（堆 teardown
// 必经，任何 starlark 求值都躲不开）时 jit worker 线程 panic「取址 offset 必落帧
// （analyze_frame 全集）」（src/vm/engine/jit_compile.rs:1276，worker 死后其余
// 函数全部回退解释——stdout/exit code 与 A/B 维逐字节一致，唯 stderr 被 panic
// 文本污染[内嵌 pid]，三跑三现）。机制链：
//   ChunkChain::drop 的 MIR 含「&mut ZST local 作泛型实参且该 ZST 是帧布最后
//   local」——src/lower/frame.rs:113-129 的帧冻结给 ZST 分 size=0、offset 可待
//   于 off==frame_size；analyze_frame::scan_place（jit_compile.rs:3089-3114）
//   对它 clamp(0,fsz)+Escape 产生退化区间 (fsz,fsz)，被 FrameMap::add 的
//   `if a<b` 丢弃（3063）→ 该函数落帧集整个为空 → frame_ss=None → 翻译同一条
//   Ref 时 addr_of_local（1273）expect 炸。同形态无处不在：本 driver 的
//   ChunkChain::drop、以及下附最小复现。
// 最小复现（21 行，无 crate 依赖；负对照：直接调用闭包不炸，因为被取址的
//   ZST 不再是帧末 local，区间非退化）：
//   struct Chain(Option<u8>);
//   impl Chain {
//       fn clear_with(&mut self, chunk_drop: &mut impl FnMut(Option<u8>)) {
//           if let Some(c) = std::mem::take(&mut self.0) { chunk_drop(Some(c)); }
//       }
//       fn run(&mut self) { self.clear_with(&mut |_| {}); }
//   }
//   fn main() {
//       for _ in 0..200000u64 {
//           let mut c = Chain(Some(1u8));
//           c.run();
//           std::thread::sleep(std::time::Duration::ZERO);
//       }
//       println!("done");
//   }
// 跑法：MIRVM_JIT_THRESHOLD=1 target/release/mirvm run <file>
//   → thread 'mirvm-jit' panicked at jit_compile.rs:1276（done 仍打印、rc=0）。
//   Drop::drop 版（drop 本体调泛型方法 + &mut 闭包字面量，同态于
//   ChunkChain::drop）同样必炸。
//
// 确定性：打印量只含 int/bigint repr、定值字符串、dict/list 按 starlark 规范
// 插入序（插入序为该语言语义本身，跨进程稳定）、函数 repr（限定名）、错误串
// （固定文件名/行号/源码行）；导出名 BTreeSet 排序；无浮点、无时间/随机/env/
// TLS 序；单线程顺序执行，bump 计数序唯一。stderr 真空（cargo run -q）。
//
// 闭包 ~165 crate（重型，按预算标注：VM-in-VM 旗舰类条目，starlark 本体 +
// rustyline/lalrpop-util 遗产链 + lsp-types/serde_json 全家；冷构建单独计）。
//
// 三维复跑（当前口径：A 绿 / B 绿 / C expected-red[上节 JIT bug]，stdout/exit
// 三维逐字节一致，唯 C 维 stderr 带一条 jit worker panic 文本）：
//   A: target/release/mirvm run corpus/c_starlark_eval.rs
//   B: cd "$(grep -l 'name = "c_starlark_eval"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_starlark_eval.rs
use std::cell::Cell;
use std::collections::BTreeSet;

use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::ErrorKind;

/// 跨边界状态：宿主函数 bump 的调用计数与折叠值（单线程顺序执行，序唯一）。
#[derive(Debug, ProvidesStaticType, Default)]
struct HostState {
    calls: Cell<u64>,
    fold: Cell<u64>,
}

#[starlark::starlark_module]
fn host_fns(builder: &mut GlobalsBuilder) {
    fn bump(x: i32, eval: &mut Evaluator) -> anyhow::Result<i32> {
        let s = eval.extra.unwrap().downcast_ref::<HostState>().unwrap();
        s.calls.set(s.calls.get() + 1);
        let f = s
            .fold
            .get()
            .wrapping_mul(31)
            .wrapping_add((x as u64).wrapping_add(0x9E37_79B9));
        s.fold.set(f);
        Ok((f % 9973) as i32)
    }
}

const S1: &str = r#"
def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

bump(fib(10))
[fib(x) for x in range(15)]
"#;

const S2: &str = r#"
def search():
    words = "banana apple bandana apple banana apple cherry bandana banana".split()
    counts = {}
    for w in words:
        counts[w] = counts.get(w, 0) + 1
    ks = sorted(counts.keys())
    best = ""
    best_n = 0
    for k in ks:
        if counts[k] > best_n:
            best = k
            best_n = counts[k]
    big = 1 << 100
    bump(best_n)
    bump(len(ks))
    return [(k, counts[k]) for k in ks] + [("best", best), ("best_n", best_n), ("idx", [ks.index(w) for w in ["apple", "cherry"]]), ("big_mod", big % 97), ("big_div", big // 123456789)]

search()
"#;

const S3: &str = r#"
def build_strings():
    parts = ["alpha", "beta", "gamma"]
    joined = "|".join(parts)
    t1 = "{}-{}-{}".format(joined, host_mul, injected_base)
    t2 = "n=%d tail=%s" % (host_mul * 6, host_suffix.upper())
    sliced = t1[2:10]
    replaced = joined.replace("a", "A")
    n_find = joined.find("beta")
    injected_echo = [injected_base + 7, host_mul * host_mul, len(host_suffix)]
    bump(n_find)
    bump(injected_echo[0])
    return [t1, t2, sliced, replaced, n_find, injected_echo, injected_label]

build_strings()
"#;

fn kind_label(e: &starlark::Error) -> &'static str {
    match e.kind() {
        ErrorKind::Fail(_) => "Fail",
        ErrorKind::StackOverflow(_) => "StackOverflow",
        ErrorKind::Value(_) => "Value",
        ErrorKind::Function(_) => "Function",
        ErrorKind::Scope(_) => "Scope",
        ErrorKind::Parser(_) => "Parser",
        ErrorKind::Freeze(_) => "Freeze",
        ErrorKind::Internal(_) => "Internal",
        ErrorKind::Native(_) => "Native",
        ErrorKind::Other(_) => "Other",
        _ => "Unknown",
    }
}

fn main() {
    let mut globals_builder = GlobalsBuilder::standard().with(host_fns);
    globals_builder.set("host_mul", 7);
    globals_builder.set("host_suffix", "-fin".to_owned());
    let globals = globals_builder.build();
    let dialect = Dialect::Standard;
    let module = Module::new();
    let state = HostState::default();
    {
        let heap = module.heap();
        module.set("injected_base", heap.alloc(1000));
        module.set("injected_label", heap.alloc_str("LBL-3").to_value());
    }
    let mut eval = Evaluator::new(&module);
    eval.extra = Some(&state);

    for (label, src) in [("S1", S1), ("S2", S2), ("S3", S3)] {
        let ast = AstModule::parse("case.star", src.to_owned(), &dialect).unwrap();
        let v: Value = eval.eval_module(ast, &globals).unwrap();
        println!("{label} => {}", v.to_str());
    }

    // 宿主回调脚本函数：位置参数 + 命名参数两通道。
    let heap = module.heap();
    let fib = module.get("fib").unwrap();
    let r = eval.eval_function(fib, &[heap.alloc(12)], &[]).unwrap();
    println!("host-call fib(12) = {}", r.to_str());

    // 命名参数通道：新建带可选参数的函数现场定义再回调。
    let ast = AstModule::parse(
        "greet.star",
        "def greet(name, punct):\n    return \"hi \" + name + punct\ngreet".to_owned(),
        &dialect,
    )
    .unwrap();
    let greet = eval.eval_module(ast, &globals).unwrap();
    let r = eval
        .eval_function(
            greet,
            &[heap.alloc_str("star").to_value()],
            &[("punct", heap.alloc_str("!!").to_value())],
        )
        .unwrap();
    println!("host-call named = {}", r.to_str());

    // 评估错误面（kind 固定类别打印）。
    let ast = AstModule::parse(
        "fail.star",
        "def run():\n    fail(\"boom-42\")\nrun()".to_owned(),
        &dialect,
    )
    .unwrap();
    match eval.eval_module(ast, &globals) {
        Ok(v) => println!("fail-case: unexpectedly ok {}", v.to_str()),
        Err(e) => println!("fail-case kind={} msg={}", kind_label(&e), e),
    }

    // eval_function 错参错误（kind 打印）。
    match eval.eval_function(fib, &[], &[]) {
        Ok(v) => println!("arity-case: unexpectedly ok {}", v.to_str()),
        Err(e) => println!("arity-case kind={} msg={}", kind_label(&e), e),
    }

    drop(eval);

    // 模块命名空间导出：名字排序 + 各值表示。
    let frozen = module.freeze().unwrap();
    let names: BTreeSet<String> = frozen.names().map(|n| n.as_str().to_owned()).collect();
    println!("module names = {names:?}");
    for n in &names {
        let v = frozen.get(n).unwrap();
        println!("export {n} = {}", v.value().to_str());
    }

    println!(
        "host calls = {} fold = {}",
        state.calls.get(),
        state.fold.get()
    );
}
