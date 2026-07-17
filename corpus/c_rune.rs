#!/usr/bin/env mirvm
---
[dependencies]
# rune 0.13.4（pin 最新 0.13 线；0.13.4 是 crates.io 上 rune 的最后一个发布版，
# 0.14 系列从未发布）。feature 按任务钉 default（emit/std/codespan-reporting/
# alloc/anyhow 五件套）：全部纯 Rust、无 C 绑定、无显式 SIMD/x86 intrinsic
# （rune-macros 走 syn 前端宏，编译期展开，运行期不碰 proc-macro）。
rune = "=0.13.4"
---
// rune 0.13.4：Rune 脚本 VM（VM-in-VM，批7 波1）。三段 Rune 脚本经
// rune::prepare().build() + Vm::call(["main"], ()) 编译求值；宿主经
// Module::function 注册 Rust 回调供脚本调用；打印各脚本返回值、宿主回调
// 调用次数与折叠态。
//
// API 适应记录（非引擎问题，不是 FRONTIER）：任务书测试面写「经 rune::run
// 求值」；rune::run 顶层 helper 存在于 0.12 线，0.13 线已移除（0.13.4 的
// lib.rs 无 pub fn run，docs.rs fn.run.html 404）。0.13 官方等价管线即
// docs.rs 首页示例：rune::prepare(&mut sources).with_context(&ctx)
//   .with_diagnostics(&mut diag).build() → Vm::new(Arc::new(
//   context.runtime()), Arc::new(unit)) → vm.call(["main"], ())。
// 本 driver 使用的就是这条 0.13 标准管线，语义覆盖面不变。
//
// 测试面：
//   S1 算术与字符串：整数混合/位运算/取模、while 循环、浮点乘法/除法
//     （位型经宿主 fbits() 以 to_bits 十六进制内嵌回文本，不经 Rune 浮点
//     格式化路径）、字符串数组 for 迭代 + 模板内插（${}）+ += 累加。
//   S2 结构体与迭代：struct Point/Acc + impl 动态实例函数（字段读写、
//     self 方法链）、结构体数组 while 索引迭代、inclusive range for
//     （1..=10）+ 条件累加、m% n 归零判定、最值搜索回读坐标。
//   S3 模式匹配：自定义 enum 带数据变体（tuple 变体构造与解构）、match 作
//     表达式、守卫（a > 0 / a == b）、向量 rest 模式（[1, ..]/[_, 2, ..]/
//     []）、tuple 模式、字符串字面量模式、`is i64` 类型守卫、兜底 `_`。
//   宿主回调：host_mix(x) 每次调用计数 +1、状态按 state*31+(x+0x9E3779B9)
//     wrapping 折叠、返回折叠态 rem_euclid 9973——脚本把返回值混回计算
//     （跨边界值交互双向验证）；三脚本合计调用次数与终态折叠值打印锚定。
//
// 确定性：打印量只含 i64/定值字符串/浮点 bits；脚本不返回 object/map
// （无哈希序进输出）；无时间/随机/环境/TLS 序；单线程顺序执行，回调计数
// 序唯一。stderr 真空。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_rune.rs
//   B: cd "$(grep -l 'name = "c_rune"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_rune.rs
use rune::{Context, Diagnostics, Module, Source, Sources, Vm};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// S1：算术与字符串。
const SCRIPT1: &str = r#"
pub fn main() {
    // 整数混合运算 + 位运算 + 取模；host_mix 回调混进累加器
    let acc = 40 + 2 * 3 - 17 % 5;
    let i = 0;
    while i < 4 {
        acc = ((acc << 1) ^ host_mix(acc)) % 100000;
        i += 1;
    }
    // 浮点：位型经宿主 fbits 锁定
    let f1 = 0.1 + 0.2;
    let f2 = -1.5 * 3.0 / 7.0;
    // 字符串：数组迭代 + 模板内插 + 累加拼接
    let s = "";
    for w in ["rune", "vm", "test"] {
        s += `[${w}]`;
    }
    let out = `acc=${acc} words=${s}`;
    out += ` f1=${fbits(f1)}`;
    out += ` f2=${fbits(f2)}`;
    out
}
"#;

/// S2：结构体与迭代。
const SCRIPT2: &str = r#"
struct Point { x, y }

impl Point {
    fn norm2(self) {
        self.x * self.x + self.y * self.y
    }
}

struct Acc { total, count }

impl Acc {
    fn add(self, v) {
        self.total += v;
        self.count += 1;
    }
}

pub fn main() {
    let pts = [
        Point { x: 3, y: 4 },
        Point { x: -1, y: 2 },
        Point { x: 5, y: -12 },
        Point { x: 7, y: 1 },
        Point { x: 0, y: -8 },
    ];
    let acc = Acc { total: 0, count: 0 };
    let best = pts[0];
    let i = 0;
    while i < pts.len() {
        let p = pts[i];
        let d = p.norm2();
        acc.add(host_mix(d));
        if d > best.norm2() {
            best = p;
        }
        i += 1;
    }
    // inclusive range 迭代 + 条件累加（继续打回调）
    let mix3 = 0;
    for n in 1..=10 {
        if n % 3 == 0 {
            mix3 += host_mix(n);
        }
    }
    `norm-sum=${acc.total} count=${acc.count} best=(${best.x},${best.y}) mix3=${mix3}`
}
"#;

/// S3：模式匹配。
const SCRIPT3: &str = r#"
enum Op {
    Add(a, b),
    Mul(a, b),
    Neg(a),
}

fn eval(op) {
    match op {
        Op::Add(a, b) if a > 0 => a + b,
        Op::Add(a, b) => host_mix(a) - b,
        Op::Mul(a, b) => a * b % 1000,
        Op::Neg(a) if a < 0 => 0 - a,
        Op::Neg(a) => a,
    }
}

fn classify(v) {
    match v {
        [1, ..] => "head-1",
        [_, 2, ..] => "second-2",
        [] => "empty",
        (a, b) if a == b => `pair-eq-${a}`,
        (a, _) => "pair-ne",
        "hit" => "string-hit",
        n if n is i64 => "int",
        _ => "other",
    }
}

pub fn main() {
    let ops = [Op::Add(3, 4), Op::Add(-2, 5), Op::Mul(6, 7), Op::Neg(-9), Op::Neg(3)];
    let total = 0;
    for op in ops {
        total += eval(op);
    }
    let parts = [
        classify([1, 9]),
        classify([5, 2, 8]),
        classify([]),
        classify((4, 4)),
        classify((4, 5)),
        classify("hit"),
        classify(4096 + host_mix(1)),
        classify(2.5),
    ];
    let s = `ops-total=${total}`;
    for p in parts {
        s += `|${p}`;
    }
    s
}
"#;

fn main() {
    // ---- 宿主回调（Module::function 闭包）----
    let calls = Arc::new(AtomicI64::new(0));
    let state = Arc::new(AtomicI64::new(0));
    let mut m = Module::new();
    {
        let calls = Arc::clone(&calls);
        let state = Arc::clone(&state);
        m.function("host_mix", move |x: i64| -> i64 {
            calls.fetch_add(1, Ordering::SeqCst);
            let next = state
                .load(Ordering::SeqCst)
                .wrapping_mul(31)
                .wrapping_add(x.wrapping_add(0x9E37_79B9));
            state.store(next, Ordering::SeqCst);
            next.rem_euclid(9973)
        })
        .build()
        .unwrap();
    }
    m.function("fbits", |x: f64| -> String { format!("{:016x}", x.to_bits()) })
        .build()
        .unwrap();

    let mut context = Context::with_default_modules().unwrap();
    context.install(m).unwrap();
    let runtime = Arc::new(context.runtime().unwrap());

    // 0.13 标准管线：prepare → build Unit → Vm::call（rune::run 已不存在）。
    let run = |script: &str| -> String {
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        assert!(diagnostics.is_empty());
        let mut vm = Vm::new(Arc::clone(&runtime), Arc::new(unit));
        let value = vm.call(["main"], ()).unwrap();
        rune::from_value::<String>(value).unwrap()
    };

    let c0 = calls.load(Ordering::SeqCst);
    let r1 = run(SCRIPT1);
    println!("r1 = {r1}");
    let c1 = calls.load(Ordering::SeqCst);
    println!("host.calls-s1 = {}", c1 - c0);

    let r2 = run(SCRIPT2);
    println!("r2 = {r2}");
    let c2 = calls.load(Ordering::SeqCst);
    println!("host.calls-s2 = {}", c2 - c1);

    let r3 = run(SCRIPT3);
    println!("r3 = {r3}");
    let c3 = calls.load(Ordering::SeqCst);
    println!("host.calls-s3 = {}", c3 - c2);

    println!("host.calls-total = {c3}");
    println!("host.state-final = {}", state.load(Ordering::SeqCst));

    // ---- 锚点 ----
    assert_eq!(c3, 14, "host_mix 总调用次数（4 + 8 + 2）");
}
