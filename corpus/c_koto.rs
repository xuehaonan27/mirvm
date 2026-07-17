#!/usr/bin/env mirvm
---
[dependencies]
# koto =0.15.3：0.15 线最新（2025-04-07；0.16.x 超出「最新 0.15」分工钉法）。
# features = default（default = ["rc"]——Rc 内存管理；b 线内存策略 arc/rc 二选一
# 冲突，保持 crate 默认）。依赖闭包约 30 crate：koto_bytecode/koto_parser/
# koto_runtime/koto_lexer/koto_memory/koto_derive(proc-macro，仅编译期)/
# indexmap/rustc-hash/smallvec/saturating_cast/downcast-rs/thiserror/
# unicode-segmentation/chrono/instant/equivalent/hashbrown 等，无 C 依赖、无 SIMD。
koto = "=0.15.3"
---
// koto 0.15.3 脚本语言（VM-in-VM：字节码编译 + 树走 VM）三维差分 driver。
//
// 测试面（分工条目 c_koto）：
//   ① 脚本 A「map/filter 管道」：each/keep/sum/fold/zip/count/min_max/to_list/
//      to_tuple 迭代器适配器与消费器链；宿主注入值 fbits 的浮点桩。
//   ② 脚本 B「闭包与捕获」：koto 捕获=创建期拷贝（重绑外部变量不影响闭包），
//      经捕获容器的可变状态（state.count）、闭包工厂（make_adder）、
//      递归闭包 fib、脚本内回调宿主函数（host_mul_add / bump / mark）。
//   ③ 脚本 C「for-while 循环」：for 区间+continue、for 列表、while+break 值、
//      until、loop+break 值、嵌套 for、循环内宿主回调副作用（bump）。
//   宿主注入：prelude 值（host_seed i64 / host_scale f64 / host_name str）
//   与四个回调（bump 副作用计数、mark 日志、host_mul_add 纯函数、
//   fbits f64.to_bits 十六进制）。
//   输出 = 脚本内 print（单参数经 @display / 多参数打 tuple）+
//          宿主侧 value_to_string(各脚本返回值) + 副作用计数与日志。
//
// 确定性依据：koto 的 ValueMap = IndexMap + FxHasher（固定种子，非
// RandomState），map 遍历 = 插入序，跨进程确定；浮点一律经 fbits 打
// to_bits()；不调任何时钟/os/io 输入函数；每脚本独立 Koto 实例，互不串
// exports。stderr 真空依赖：依赖无编译期 warning，native 用 cargo run -q。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_koto.rs
//   B: cd "$(grep -l 'name = "c_koto"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | xargs dirname)" && RUSTC=~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc \
//        ~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_koto.rs
//
// FRONTIER：无（纯 Rust 依赖闭包，无已知绕行）。
use koto::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

type Counter = Rc<RefCell<i64>>;
type Log = Rc<RefCell<Vec<String>>>;

/// 在一个新 Koto 实例上注册宿主注入（值 + 回调）并运行脚本；
/// 返回脚本结果，由调用方打印。
fn run_script(
    label: &str,
    script: &str,
    bump_count: &Counter,
    marks: &Log,
) -> Result<String, String> {
    let mut koto = Koto::new();
    let prelude = koto.prelude();

    // ---- 宿主注入值 ----
    prelude.insert("host_seed", 7);
    prelude.insert("host_scale", 2.5);
    prelude.insert("host_name", "koto-host");

    // ---- 宿主注入回调 ----
    // 副作用计数：bump(n) 累加并返回新累计值。
    {
        let bump_count = Rc::clone(bump_count);
        prelude.add_fn("bump", move |ctx| match ctx.args() {
            [KValue::Number(KNumber::I64(n))] => {
                let mut g = bump_count.borrow_mut();
                *g += n;
                Ok(KValue::Number(KNumber::I64(*g)))
            }
            unexpected => unexpected_args("|Number|", unexpected),
        });
    }
    // 日志：mark(tag) 记录并返回当前条数。
    {
        let marks = Rc::clone(marks);
        prelude.add_fn("mark", move |ctx| match ctx.args() {
            [KValue::Str(tag)] => {
                let mut g = marks.borrow_mut();
                g.push(tag.to_string());
                Ok(KValue::Number(KNumber::I64(g.len() as i64)))
            }
            unexpected => unexpected_args("|String|", unexpected),
        });
    }
    // 纯函数：host_mul_add(a, b, c) = a * b + c。
    prelude.add_fn("host_mul_add", |ctx| match ctx.args() {
        [KValue::Number(a), KValue::Number(b), KValue::Number(c)] => Ok((*a * *b + *c).into()),
        unexpected => unexpected_args("|Number, Number, Number|", unexpected),
    });
    // 浮点锁位：fbits(x) = f64.to_bits() 的十六进制串。
    prelude.add_fn("fbits", |ctx| match ctx.args() {
        [KValue::Number(KNumber::F64(x))] => Ok(format!("0x{:016x}", x.to_bits()).into()),
        unexpected => unexpected_args("|Float|", unexpected),
    });

    match koto.compile_and_run(script) {
        Ok(result) => koto
            .value_to_string(result)
            .map_err(|e| format!("{label}: display failed: {e}")),
        Err(e) => Err(format!("{label}: {e}")),
    }
}

// ---- 脚本 A：map/filter 迭代器管道 ----
const SCRIPT_A: &str = r#"
pipe1 = [1, 2, 3, 4, 5, 6, 7, 8, 9]
  .each |n| n * n
  .keep |n| n % 2 == 0
  .to_list()
print 'pipe1: {pipe1}'

pipe2 = [1, 2, 3, 4, 5, 6, 7, 8, 9]
  .keep |n| n % 2 == 1
  .each |n| n + host_seed
  .sum()
print 'pipe2 sum: {pipe2}'

folded = [10, 20, 30, 40]
  .fold 'acc', |acc, n| '{acc}/{n}'
print 'fold: {folded}'

pairs = (1, 2, 3)
  .zip ('a', 'b', 'c')
  .to_list()
print 'zip: {pairs}'

evens = (3, 8, 5, 12, 7, 22)
  .keep |n| n % 2 == 0
  .to_tuple()
print 'evens: {evens} count: {size(evens)}'

mm = (-3, 0, 7, 2, 9).min_max()
print 'min_max: {mm}'

big = (10, 25, 41, 9, 33)
  .keep |n| n % 2 == 1
  .count()
print 'odd count: {big}'

print 'host_name: {host_name}'
print 'float bits: {fbits(host_scale * 3.0 + 0.5)}'
print 'neg float bits: {fbits(-1.5 / 7.0)}'

host_seed * 2
"#;

// ---- 脚本 B：闭包与捕获（koto 捕获 = 创建期拷贝）----
const SCRIPT_B: &str = r#"
x = 5
f = |n| n + x
x = 100
print 'capture-copy: {f(2)}'

state = {count: 0}
inc = |n|
  state.count += n
  state.count
print 'container-capture: {inc(5)}, {inc(7)}, {inc(-2)}, {state.count}'

make_adder = |n| |m| m + n
add3 = make_adder(3)
add10 = make_adder(10)
print 'adder: {add3(4)}, {add10(4)}'

fib = |n| if n < 2 then n else (fib n - 1) + (fib n - 2)
print 'fib(10): {fib(10)}'

print 'host_mul_add: {host_mul_add(6, 7, 1)}'
apply = |g, a, b|
  g(a, b) + 1
plus = |a, b| a + b
print 'higher-order: {apply(plus, 20, 21)}'

b1 = bump(3)
b2 = bump(4)
print 'bump: {b1}, {b2}'

m1 = mark('be')
m2 = mark('am')
print 'mark: {m1}, {m2}'

'closures done'
"#;

// ---- 脚本 C：for-while 循环 ----
const SCRIPT_C: &str = r#"
total = 0
for i in 1..=15
  if i % 3 == 0
    continue
  total += i
print 'for-sum-skip3: {total}'

words = ['a', 'bb', 'ccc', 'dddd']
longest = ''
for w in words
  if size(w) > size(longest)
    longest = w
print 'longest: {longest}'

n = 2
found = while n < 500
  if n * n * n > 300
    break n
  n += 1
print 'while-break: {found}'

stack = [1, 2, 3]
popped = []
until stack.is_empty()
  popped.push(stack.pop())
print 'until-pop: {popped}'

acc = 1
rounds = loop
  acc *= 2
  if acc > 20
    break acc
print 'loop-break: {rounds}'

pairs = []
for i in 1..=3
  for j in (10, 20)
    pairs.push('{i}:{j}')
print 'nested: {pairs}'

for i in 1..=4
  bump(i)

'loops done'
"#;

fn main() {
    let bump_count: Counter = Rc::new(RefCell::new(0));
    let marks: Log = Rc::new(RefCell::new(Vec::new()));

    for (label, script) in [
        ("script A", SCRIPT_A),
        ("script B", SCRIPT_B),
        ("script C", SCRIPT_C),
    ] {
        match run_script(label, script, &bump_count, &marks) {
            Ok(result) => println!("{label} result = {result}"),
            Err(e) => println!("{label} error = {e}"),
        }
    }

    println!("bump counter = {}", *bump_count.borrow());
    println!("marks = {:?}", *marks.borrow());
}
