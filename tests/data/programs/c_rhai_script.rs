#!/usr/bin/env mirvm
---
[dependencies]
rhai = "1"
---
// rhai 1.x, an embedded scripting language: a meta interpreter inside the guest.
// Covers Engine eval / eval_with_scope / compile + call_fn with a fixed Scope;
// arithmetic, string interpolation, array map/filter/reduce, closures capturing
// shared variables, a custom operator, try/catch error capture and recursive fib;
// Rust-side register_fn host functions (including cross-call state and f64 bits
// locking), EvalAltResult variant classes with line/column printing, the
// set_max_operations / set_max_call_levels limit errors and parse error paths.
// Deterministic: only values, counts, booleans, error classes and positions are
// printed; no hash-ordered collection reaches the output and floats use fbits().
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

    // ---- host functions (register_fn) that later scripts call ----
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

    // (1) arithmetic: mixed integer ops + bit ops + float bit patterns
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

    // (2) string interpolation (Scope var + local var + expression; backtick strings only)
    scope.push("name", "rhai".to_string());
    let v: String = engine
        .eval_with_scope(
            &mut scope,
            r#"let n = 3; `hello ${name}: n=${n}, n*n=${n * n}`"#,
        )
        .unwrap();
    println!("interp = {v}");

    // (3) array map / filter / reduce (a closure pipeline)
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

    // (4) closures: shared-variable capture (the closure writes an outer variable)
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

    // (5) custom operator (register_custom_operator + a same-named register_fn;
    //     `<|` is a reserved tokenizer symbol, not a builtin, so it can be registered)
    engine.register_fn("<|", |a: INT, b: INT| a * 10 + b);
    engine.register_custom_operator("<|", 160).unwrap();
    let v: INT = engine.eval("1 <| 2 <| 3").unwrap();
    println!("custom-op assoc = {v}");
    let v: INT = engine.eval("2 + 3 <| 4").unwrap();
    println!("custom-op prec = {v}");

    // (6) try-catch: an explicit thrown value and a runtime error (try-catch is a
    //     statement whose value is (); the catch block rewrites an outer variable)
    let v: INT = engine
        .eval("let r = 0; try { throw 42 } catch (e) { r = e + 1; } r")
        .unwrap();
    println!("try-throw = {v}");
    let v: String = engine
        .eval(r#"let r = "no"; try { let x = 1 / 0; } catch (e) { r = "caught: " + e.error + "/" + e.message; } r"#)
        .unwrap();
    println!("try-div0 = {v}");

    // (7) recursive fib: compile to AST, then repeated call_fn from Rust. The
    // explicit set_max_call_levels pins a limit that would otherwise vary between
    // debug and release builds (8 vs 64) and thus affect unrelated output.
    let mut deep = Engine::new();
    deep.set_max_call_levels(64);
    let ast = deep
        .compile("fn fib(n) { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }")
        .unwrap();
    for n in [0i64, 1, 10, 20] {
        let r: INT = deep.call_fn(&mut Scope::new(), &ast, "fib", (n,)).unwrap();
        println!("fib({n}) = {r}");
    }
    // stack-depth limit error path: a small limit with deep recursion
    let mut shallow = Engine::new();
    shallow.set_max_call_levels(4);
    let ast2 = shallow
        .compile("fn dive(n) { if n == 0 { 0 } else { dive(n - 1) + 1 } }")
        .unwrap();
    match shallow.call_fn::<INT>(&mut Scope::new(), &ast2, "dive", (10i64,)) {
        Ok(v) => println!("stack: unexpectedly ok {v}"),
        Err(e) => println!("stack err kind={} msg={}", err_kind(&e), e),
    }

    // (8) scripts calling host functions (pure, plus one with cross-call state)
    let v: INT = engine.eval("host_mul_add(6, 7, 1) + bump(3) + bump(4)").unwrap();
    println!("host-fns = {v}");
    println!("host state = {}", *hits.lock().unwrap());

    // (9) EvalAltResult: variant class + line/column (bounds, unknown fn, parse)
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

    // (10) MAX limits: the operation cap errors out; uncapped, the same code runs
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

    // Scope read-back
    engine
        .eval_with_scope::<()>(&mut scope, "let answer = 42;")
        .unwrap();
    println!("scope answer = {:?}", scope.get_value::<INT>("answer"));
}
