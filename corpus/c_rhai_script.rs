#!/usr/bin/env mirvm
---
[dependencies]
rhai = "1"
---
// rhai 1.x：嵌入式脚本语言（meta 解释器，VM-in-VM）。
// 覆盖：Engine + 固定 Scope 的 eval / eval_with_scope / compile + call_fn；
// 算术 / 字符串内插 / 数组 map-filter-reduce / 闭包（共享变量捕获）/
// 自定义 operator / try-catch 错误捕获 / 递归 fib；Rust 侧 register_fn
// 宿主函数（含跨调用状态、f64 to_bits 锁位）、EvalAltResult 变体类别 +
// 行/列打印、set_max_operations / set_max_call_levels 上限错误、parse 错误路径。
// 确定性：只打印值 / 计数 / 布尔 / 错误类别与位置——不打印任何哈希序集合
// （rhai 默认 ahash runtime-rng，进程间种子不同，哈希序内容一律不进输出）；
// 浮点一律经宿主 fbits() 打 to_bits 十六进制。
use rhai::{Engine, Scope, INT};
use std::sync::{Arc, Mutex};

fn err_kind(e: &rhai::EvalAltResult) -> &'static str {
    use rhai::EvalAltResult::*;
    match e {
        ErrorSystem(..) => "System",
        ErrorParsing(..) => "Parsing",
        ErrorFunctionNotFound(..) => "FunctionNotFound",
        ErrorVariableNotFound(..) => "VariableNotFound",
        ErrorIndexingType(..) => "IndexingType",
        ErrorArrayBounds(..) => "ArrayBounds",
        ErrorArithmetic(..) => "Arithmetic",
        ErrorTooManyOperations(..) => "TooManyOperations",
        ErrorStackOverflow(..) => "StackOverflow",
        ErrorMismatchDataType(..) => "MismatchDataType",
        ErrorRuntime(..) => "Runtime",
        _ => "Other",
    }
}

fn main() {
    let mut engine = Engine::new();
    let mut scope = Scope::new();

    // ---- 宿主函数（register_fn），后续脚本调用 ----
    engine.register_fn("host_mul_add", |a: INT, b: INT, c: INT| a * b + c);
    engine.register_fn("fbits", |x: f64| format!("{:016x}", x.to_bits()));
    let hits = Arc::new(Mutex::new(0i64));
    {
        let hits = Arc::clone(&hits);
        engine.register_fn("bump", move |x: INT| {
            let mut g = hits.lock().unwrap();
            *g += x;
            *g
        });
    }

    // ① 算术：整数混合运算 + 位运算 + 浮点位型
    let v: INT = engine
        .eval_with_scope(&mut scope, "40 + 2 * 3 - 10 % 4")
        .unwrap();
    println!("arith1 = {v}");
    let v: INT = engine.eval("(1 << 5) ^ 0x0f0f & 0xff | 0x100").unwrap();
    println!("arith2 = {v}");
    let v: String = engine.eval("fbits(0.1 + 0.2)").unwrap();
    println!("float1 bits = {v}");
    let v: String = engine.eval("fbits(-1.5 * 3.0 / 7.0)").unwrap();
    println!("float2 bits = {v}");

    // ② 字符串内插（Scope 变量 + 局部变量 + 表达式；rhai 1.x 内插只在反引号串）
    scope.push("name", "rhai".to_string());
    let v: String = engine
        .eval_with_scope(
            &mut scope,
            r#"let n = 3; `hello ${name}: n=${n}, n*n=${n * n}`"#,
        )
        .unwrap();
    println!("interp = {v}");

    // ③ 数组 map / filter / reduce（闭包管线）
    let v: String = engine
        .eval("let a = [1, 2, 3, 4, 5, 6, 7, 8, 9]; a.map(|x| x * x).to_string()")
        .unwrap();
    println!("map = {v}");
    let v: String = engine
        .eval("[1, 2, 3, 4, 5, 6, 7, 8, 9].filter(|x| x % 2 == 0).to_string()")
        .unwrap();
    println!("filter = {v}");
    let v: INT = engine
        .eval("[1, 2, 3, 4, 5, 6, 7, 8, 9].map(|x| x * x).filter(|x| x % 2 == 0).reduce(|acc, x| acc + x, 0)")
        .unwrap();
    println!("map-filter-reduce = {v}");

    // ④ 闭包：共享变量捕获（闭包内改写外部变量，两侧可见）
    let v: INT = engine
        .eval(
            r#"
            let total = 0;
            let add = |x| total += x;
            add.call(5);
            add.call(7);
            let snapshot = || total * 2;
            total + snapshot.call()
            "#,
        )
        .unwrap();
    println!("closure shared = {v}");

    // ⑤ 自定义 operator（register_custom_operator + 同名 register_fn；
    //    `<|` 是 tokenizer 保留符号、非内建 operator，可注册为自定义）
    engine.register_fn("<|", |a: INT, b: INT| a * 10 + b);
    engine.register_custom_operator("<|", 160).unwrap();
    let v: INT = engine.eval("1 <| 2 <| 3").unwrap();
    println!("custom-op assoc = {v}");
    let v: INT = engine.eval("2 + 3 <| 4").unwrap();
    println!("custom-op prec = {v}");

    // ⑥ try-catch：显式 throw 值 / 运行时错误捕获（try-catch 是语句，
    //    值为 ()——catch 内改写外层变量再读回）
    let v: INT = engine
        .eval("let r = 0; try { throw 42 } catch (e) { r = e + 1; } r")
        .unwrap();
    println!("try-throw = {v}");
    let v: String = engine
        .eval(r#"let r = "no"; try { let x = 1 / 0; } catch (e) { r = "caught: " + e.error + "/" + e.message; } r"#)
        .unwrap();
    println!("try-div0 = {v}");

    // ⑦ 递归 fib：compile → AST，Rust 侧 call_fn 反复调用。
    // 显式 set_max_call_levels：rhai 默认上限随 debug/release 变（8/64），
    // 显式钉死使上限成为输出无关的常量。
    let mut deep = Engine::new();
    deep.set_max_call_levels(64);
    let ast = deep
        .compile("fn fib(n) { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }")
        .unwrap();
    for n in [0i64, 1, 10, 20] {
        let r: INT = deep.call_fn(&mut Scope::new(), &ast, "fib", (n,)).unwrap();
        println!("fib({n}) = {r}");
    }
    // 栈深上限错误路径：小上限 + 深递归 → ErrorStackOverflow
    let mut shallow = Engine::new();
    shallow.set_max_call_levels(4);
    let ast2 = shallow
        .compile("fn dive(n) { if n == 0 { 0 } else { dive(n - 1) + 1 } }")
        .unwrap();
    match shallow.call_fn::<INT>(&mut Scope::new(), &ast2, "dive", (10i64,)) {
        Ok(v) => println!("stack: unexpectedly ok {v}"),
        Err(e) => println!("stack err kind={} msg={}", err_kind(&e), e),
    }

    // ⑧ 脚本调用宿主函数（纯函数 + 带跨调用状态）
    let v: INT = engine.eval("host_mul_add(6, 7, 1) + bump(3) + bump(4)").unwrap();
    println!("host-fns = {v}");
    println!("host state = {}", *hits.lock().unwrap());

    // ⑨ EvalAltResult：变体类别 + 行/列（运行期越界 / 未定义函数 / parse 错）
    let src = "let a = [1, 2];\nlet b = a[5];\nb";
    match engine.eval::<INT>(src) {
        Ok(v) => println!("bounds: unexpectedly ok {v}"),
        Err(e) => println!(
            "bounds err kind={} line={:?} pos={:?} msg={}",
            err_kind(&e),
            e.position().line(),
            e.position().position(),
            e
        ),
    }
    let src = "let x = 1;\nlet y = nosuch(x);\ny";
    match engine.eval::<INT>(src) {
        Ok(v) => println!("fnnf: unexpectedly ok {v}"),
        Err(e) => println!(
            "fnnf err kind={} line={:?} pos={:?} msg={}",
            err_kind(&e),
            e.position().line(),
            e.position().position(),
            e
        ),
    }
    match engine.compile("let x = ;\nx") {
        Ok(_) => println!("parse: unexpectedly ok"),
        Err(e) => println!(
            "parse err line={:?} pos={:?} msg={}",
            e.position().line(),
            e.position().position(),
            e.err_type()
        ),
    }

    // ⑩ MAX 限制：operations 上限触发错误；同引擎不限则跑通
    let mut limited = Engine::new();
    limited.set_max_operations(1_000);
    match limited.eval::<INT>("let s = 0; for i in 0..100000 { s += i; } s") {
        Ok(v) => println!("maxops: unexpectedly ok {v}"),
        Err(e) => println!("maxops err kind={} msg={}", err_kind(&e), e),
    }
    let v: INT = engine
        .eval("let s = 0; for i in 0..1000 { s += i; } s")
        .unwrap();
    println!("loop sum = {v}");

    // Scope 读回
    engine
        .eval_with_scope::<()>(&mut scope, "let answer = 42;")
        .unwrap();
    println!("scope answer = {:?}", scope.get_value::<INT>("answer"));
}
