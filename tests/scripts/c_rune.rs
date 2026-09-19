#!/usr/bin/env mirvm
---
[dependencies]
# rune 0.13.4 (pin of the latest 0.13 line; 0.13.4 is the last rune release on
# crates.io, and no 0.14 series was ever published). Features pinned to default
# (emit/std/codespan-reporting/alloc/anyhow): all pure Rust, no C bindings, no
# explicit SIMD/x86 intrinsics (rune-macros is a compile-time syn front-end macro).
rune = "=0.13.4"
---
// rune 0.13.4: Rune script VM (VM-in-VM). Three Rune scripts are compiled and
// evaluated through rune::prepare().build() + Vm::call(["main"], ()); the host
// registers Rust callbacks for the scripts with Module::function, and the driver
// prints each script's return value, the host callback call counts and the final
// fold state.
//
// API fact about the pinned version: the rune::run top-level helper exists in the
// 0.12 line and was removed in the 0.13 line (0.13.4's lib.rs has no pub fn run;
// docs.rs fn.run.html is a 404). The 0.13 pipeline -- the one used here -- is
// rune::prepare(&mut sources).with_context(&ctx).with_diagnostics(&mut diag)
//   .build() → Vm::new(Arc::new(context.runtime()), Arc::new(unit))
//   → vm.call(["main"], ()).
//
// Test surface:
//   S1 arithmetic and strings: mixed integers / bit ops / modulo, while loop,
//     float multiply / divide (bit patterns pinned through the host fbits()
//     embedding to_bits as hex back into the text, bypassing Rune's float
//     formatting path), string array for iteration + template interpolation
//     (${}) + += accumulation.
//   S2 structs and iteration: struct Point/Acc + impl dynamic instance functions
//     (field read/write, self method chaining), struct array while-index
//     iteration, inclusive range for (1..=10) + conditional accumulation,
//     m % n zero test, max search reading back the coordinates.
//   S3 pattern matching: custom enum with data variants (tuple variant
//     construction and destructuring), match as an expression, guards
//     (a > 0 / a == b), vector rest patterns ([1, ..] / [_, 2, ..] / []),
//     tuple patterns, string literal patterns, `is i64` type guard, fallback `_`.
//   Host callback: host_mix(x) counts each call +1 and folds state as
//     state*31 + (x + 0x9E3779B9) wrapping, returning the fold state
//     rem_euclid 9973 -- the scripts mix the return value back into their
//     computation (cross-boundary value interaction verified both ways); the
//     three-script total call count and the final fold value are printed anchors.
//
// Determinism: printed values contain only i64 / fixed strings / float bits; the
// scripts return no object/map (so no hash order reaches the output); no
// time/random/environment/TLS ordering; single-threaded sequential execution
// makes the callback count order unique. stderr is empty.
use rune::{Context, Diagnostics, Module, Source, Sources, Vm};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// S1: arithmetic and strings.
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

/// S2: structs and iteration.
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

/// S3: pattern matching.
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
    // ---- host callbacks (Module::function closures) ----
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

    // 0.13 pipeline: prepare → build Unit → Vm::call (rune::run no longer exists).
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

    // ---- anchors ----
    assert_eq!(c3, 14, "host_mix total call count (4 + 8 + 2)");
}
