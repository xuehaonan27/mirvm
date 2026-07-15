#!/usr/bin/env mirvm
---
[dependencies]
wasmi = "0.38"
wat = "1"
---
// wasmi 0.38：纯 Rust wasm 解释器（VM-in-VM）。wat 文本内嵌两个模块：
// ① add/fib/fac/f32/f64/位运算纯函数（typed + untyped 两种调用面）；
// ② memory 读写 + 可变 global + 多值返回 + 宿主函数（Caller 改宿主状态）。
// 逐调用打印返回值（浮点打 to_bits）；trap 路径：除零 / int 溢出 /
// unreachable / 内存越界，打印 TrapCode 类别；另有 grow 边界、缺失导出、
// typed 签名不匹配、坏二进制、坏 wat 五类错误路径。全确定：无时间/随机。
use wasmi::{
    Caller, Engine, ExternType, FuncType, Instance, Linker, Module, Store, Val,
};

struct HostState {
    host_add_calls: u64,
    ticks: u64,
}

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn hex(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 0xf) as usize] as char);
    }
    s
}

fn val_str(v: &Val) -> String {
    match v {
        Val::I32(x) => format!("i32 {x}"),
        Val::I64(x) => format!("i64 {x}"),
        Val::F32(x) => format!("f32 bits={:08x}", x.to_bits()),
        Val::F64(x) => format!("f64 bits={:016x}", x.to_bits()),
        other => format!("{other:?}"),
    }
}

fn func_sig(ft: &FuncType) -> String {
    let p: Vec<String> = ft.params().iter().map(|v| format!("{v:?}")).collect();
    let r: Vec<String> = ft.results().iter().map(|v| format!("{v:?}")).collect();
    format!("({}) -> ({})", p.join(","), r.join(","))
}

fn extern_kind(t: &ExternType) -> String {
    match t {
        ExternType::Func(ft) => format!("func {}", func_sig(ft)),
        ExternType::Global(gt) => format!("global {:?} {:?}", gt.content(), gt.mutability()),
        ExternType::Memory(_) => "memory".to_string(),
        ExternType::Table(_) => "table".to_string(),
    }
}

fn dump_module(tag: &str, module: &Module) {
    for imp in module.imports() {
        println!("{tag} import {}.{} : {}", imp.module(), imp.name(), extern_kind(imp.ty()));
    }
    for exp in module.exports() {
        println!("{tag} export {} : {}", exp.name(), extern_kind(exp.ty()));
    }
}

fn trap_str(e: &wasmi::Error) -> String {
    match e.as_trap_code() {
        Some(tc) => format!("trap {tc:?}"),
        None => format!("err {e}"),
    }
}

/// untyped Func::call：Val 切片入参、动态结果缓冲，覆盖非泛型调用面。
fn call_dynamic(store: &mut Store<HostState>, instance: &Instance, name: &str, args: &[Val]) {
    let f = instance.get_func(&*store, name).unwrap();
    let n_results = f.ty(&*store).results().len();
    let mut results = vec![Val::I32(0); n_results];
    let args_s: Vec<String> = args.iter().map(val_str).collect();
    match f.call(store, args, &mut results) {
        Ok(()) => {
            let r: Vec<String> = results.iter().map(val_str).collect();
            println!("dyn {name}({}) => [{}]", args_s.join(", "), r.join(", "));
        }
        Err(e) => println!("dyn {name}({}) => {}", args_s.join(", "), trap_str(&e)),
    }
}

const WAT_PURE: &str = r#"
(module
  (func (export "add") (param i32 i32) (result i32)
    (i32.add (local.get 0) (local.get 1)))
  (func $fib (export "fib") (param $n i32) (result i32)
    (if (result i32) (i32.lt_s (local.get $n) (i32.const 2))
      (then (local.get $n))
      (else (i32.add
        (call $fib (i32.sub (local.get $n) (i32.const 1)))
        (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
  (func $fac (export "fac") (param $n i64) (result i64)
    (if (result i64) (i64.le_s (local.get $n) (i64.const 1))
      (then (i64.const 1))
      (else (i64.mul (local.get $n) (call $fac (i64.sub (local.get $n) (i64.const 1)))))))
  (func (export "mul_f64") (param f64 f64) (result f64)
    (f64.mul (local.get 0) (local.get 1)))
  (func (export "add_f32") (param f32 f32) (result f32)
    (f32.add (local.get 0) (local.get 1)))
  (func (export "rotl_xor") (param i32 i32) (result i32)
    (i32.xor (i32.rotl (local.get 0) (local.get 1)) (i32.const 0x5bd1e995)))
  (func (export "div_s") (param i32 i32) (result i32)
    (i32.div_s (local.get 0) (local.get 1)))
  (func (export "boom")
    unreachable)
)
"#;

const WAT_HOST: &str = r#"
(module
  (import "env" "host_add" (func $host_add (param i32 i32) (result i32)))
  (import "env" "host_tick" (func $host_tick (result i64)))
  (memory (export "memory") 1 2)
  (global $g (export "g") (mut i32) (i32.const 7))
  (data (i32.const 64) "\01\02\03\04\05\06\07\08hello-wasm!")
  (func (export "write8") (param i32 i32)
    (i32.store8 (local.get 0) (local.get 1)))
  (func (export "read8") (param i32) (result i32)
    (i32.load8_u (local.get 0)))
  (func (export "sum_range") (param $addr i32) (param $len i32) (result i32)
    (local $i i32) (local $acc i32)
    (block $done
      (loop $loop
        (br_if $done (i32.ge_u (local.get $i) (local.get $len)))
        (local.set $acc (i32.add (local.get $acc)
          (i32.load8_u (i32.add (local.get $addr) (local.get $i)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $loop)))
    (local.get $acc))
  (func (export "divmod") (param i32 i32) (result i32 i32)
    (i32.div_s (local.get 0) (local.get 1))
    (i32.rem_s (local.get 0) (local.get 1)))
  (func (export "bump_g") (param i32) (result i32)
    (global.set $g (i32.add (global.get $g) (local.get 0)))
    (global.get $g))
  (func (export "use_host") (param i32 i32) (result i32)
    (i32.mul (call $host_add (local.get 0) (local.get 1)) (i32.const 2)))
  (func (export "tick_sum") (result i64)
    (i64.add (call $host_tick) (call $host_tick)))
)
"#;

fn main() {
    let engine = Engine::default();

    // ---- 宿主函数：Caller 访问/修改宿主状态 ----
    let mut linker: Linker<HostState> = Linker::new(&engine);
    linker
        .func_wrap(
            "env",
            "host_add",
            |mut caller: Caller<'_, HostState>, a: i32, b: i32| -> i32 {
                caller.data_mut().host_add_calls += 1;
                a.wrapping_add(b)
            },
        )
        .unwrap();
    linker
        .func_wrap("env", "host_tick", |mut caller: Caller<'_, HostState>| -> i64 {
            caller.data_mut().ticks += 1;
            caller.data().ticks as i64
        })
        .unwrap();

    let mut store = Store::new(&engine, HostState { host_add_calls: 0, ticks: 0 });

    // ================= 模块①：纯函数 =================
    let wasm1 = wat::parse_str(WAT_PURE).unwrap();
    println!("m1 wasm bytes len={} fnv={:016x}", wasm1.len(), fnv1a(&wasm1));
    let module1 = Module::new(&engine, &wasm1[..]).unwrap();
    dump_module("m1", &module1);
    let inst1 = linker.instantiate(&mut store, &module1).unwrap().start(&mut store).unwrap();

    let add = inst1.get_typed_func::<(i32, i32), i32>(&store, "add").unwrap();
    println!("m1 add(3, 4) = {}", add.call(&mut store, (3, 4)).unwrap());
    println!(
        "m1 add(i32::MAX, 1) = {}",
        add.call(&mut store, (i32::MAX, 1)).unwrap()
    );

    let fib = inst1.get_typed_func::<i32, i32>(&store, "fib").unwrap();
    for n in [0, 1, 2, 5, 10, 12] {
        println!("m1 fib({n}) = {}", fib.call(&mut store, n).unwrap());
    }

    let fac = inst1.get_typed_func::<i64, i64>(&store, "fac").unwrap();
    println!("m1 fac(20) = {}", fac.call(&mut store, 20).unwrap());
    println!("m1 fac(1) = {}", fac.call(&mut store, 1).unwrap());

    let mul_f64 = inst1.get_typed_func::<(f64, f64), f64>(&store, "mul_f64").unwrap();
    for (a, b) in [(1.5f64, -2.25f64), (0.1, 0.2)] {
        let r = mul_f64.call(&mut store, (a, b)).unwrap();
        println!("m1 mul_f64({a}, {b}) = bits {:016x}", r.to_bits());
    }

    let add_f32 = inst1.get_typed_func::<(f32, f32), f32>(&store, "add_f32").unwrap();
    let r = add_f32.call(&mut store, (1.5f32, 2.25f32)).unwrap();
    println!("m1 add_f32(1.5, 2.25) = bits {:08x}", r.to_bits());

    let rotl_xor = inst1.get_typed_func::<(i32, i32), i32>(&store, "rotl_xor").unwrap();
    println!("m1 rotl_xor(0x12345678, 5) = {:#010x}", rotl_xor.call(&mut store, (0x12345678, 5)).unwrap());

    // untyped 调用面：Val 数组
    call_dynamic(&mut store, &inst1, "add", &[Val::I32(40), Val::I32(2)]);
    call_dynamic(&mut store, &inst1, "fib", &[Val::I32(9)]);

    // trap 路径（模块①）：除零 / int 溢出 / unreachable
    call_dynamic(&mut store, &inst1, "div_s", &[Val::I32(1), Val::I32(0)]);
    call_dynamic(&mut store, &inst1, "div_s", &[Val::I32(i32::MIN), Val::I32(-1)]);
    call_dynamic(&mut store, &inst1, "boom", &[]);

    // ================= 模块②：memory + global + 多值 + 宿主函数 =================
    let wasm2 = wat::parse_str(WAT_HOST).unwrap();
    println!("m2 wasm bytes len={} fnv={:016x}", wasm2.len(), fnv1a(&wasm2));
    let module2 = Module::new(&engine, &wasm2[..]).unwrap();
    dump_module("m2", &module2);
    let inst2 = linker.instantiate(&mut store, &module2).unwrap().start(&mut store).unwrap();

    // memory：宿主侧读 data 段初值
    let memory = inst2.get_memory(&store, "memory").unwrap();
    println!("m2 memory pages={} bytes={}", memory.size(&store), memory.data(&store).len());
    let mut buf = [0u8; 19];
    memory.read(&store, 64, &mut buf).unwrap();
    println!("m2 data[64..83] hex={} fnv={:016x}", hex(&buf), fnv1a(&buf));
    println!("m2 data[72..83] utf8={:?}", std::str::from_utf8(&buf[8..]).unwrap());

    // 宿主写 → wasm 读（sum_range）
    memory.write(&mut store, 128, b"mirvm-wasmi").unwrap();
    let sum_range = inst2.get_typed_func::<(i32, i32), i32>(&store, "sum_range").unwrap();
    println!("m2 sum_range(128, 11) = {}", sum_range.call(&mut store, (128, 11)).unwrap());

    // wasm 写 → 宿主读
    let write8 = inst2.get_typed_func::<(i32, i32), ()>(&store, "write8").unwrap();
    write8.call(&mut store, (200, 0xAB)).unwrap();
    write8.call(&mut store, (201, 0xCD)).unwrap();
    let read8 = inst2.get_typed_func::<i32, i32>(&store, "read8").unwrap();
    println!("m2 read8(200) = {:#04x}", read8.call(&mut store, 200).unwrap());
    let mut pair = [0u8; 2];
    memory.read(&store, 200, &mut pair).unwrap();
    println!("m2 host sees [200..202] = {}", hex(&pair));

    // global：wasm 改 ↔ 宿主改
    let g = inst2.get_global(&store, "g").unwrap();
    println!("m2 g initial = {}", val_str(&g.get(&store)));
    let bump_g = inst2.get_typed_func::<i32, i32>(&store, "bump_g").unwrap();
    println!("m2 bump_g(5) = {}", bump_g.call(&mut store, 5).unwrap());
    g.set(&mut store, Val::I32(100)).unwrap();
    println!("m2 bump_g(1) after host set = {}", bump_g.call(&mut store, 1).unwrap());
    println!("m2 g final = {}", val_str(&g.get(&store)));

    // 多值返回
    let divmod = inst2.get_typed_func::<(i32, i32), (i32, i32)>(&store, "divmod").unwrap();
    let (q, r) = divmod.call(&mut store, (17, 5)).unwrap();
    println!("m2 divmod(17, 5) = ({q}, {r})");
    let (q, r) = divmod.call(&mut store, (-17, 5)).unwrap();
    println!("m2 divmod(-17, 5) = ({q}, {r})");

    // 宿主函数经 wasm 调用；Caller 改的宿主状态最后打印
    let use_host = inst2.get_typed_func::<(i32, i32), i32>(&store, "use_host").unwrap();
    println!("m2 use_host(20, 22) = {}", use_host.call(&mut store, (20, 22)).unwrap());
    let tick_sum = inst2.get_typed_func::<(), i64>(&store, "tick_sum").unwrap();
    println!("m2 tick_sum() = {}", tick_sum.call(&mut store, ()).unwrap());
    println!(
        "m2 host state host_add_calls={} ticks={}",
        store.data().host_add_calls,
        store.data().ticks
    );

    // memory.grow：1→2 页成功，再 grow 超 max=2 失败
    let prev = memory.grow(&mut store, 1).unwrap();
    println!("m2 grow(1) prev_pages={prev} now_pages={}", memory.size(&store));
    println!("m2 read8(131071) post-grow = {}", read8.call(&mut store, 131071).unwrap());
    match memory.grow(&mut store, 1) {
        Ok(p) => println!("m2 grow past max: prev={p}"),
        Err(e) => println!("m2 grow past max => err {e}"),
    }
    // 越界读 → MemoryOutOfBounds trap（2 页 = 131072 字节，地址 131072 越界）
    call_dynamic(&mut store, &inst2, "read8", &[Val::I32(131072)]);

    // ================= 错误路径 =================
    println!("m1 missing export is_none = {}", inst1.get_func(&store, "nope").is_none());
    match inst1.get_typed_func::<(i64,), i64>(&store, "add") {
        Ok(_) => println!("m1 wrong-sig typed: unexpectedly ok"),
        Err(e) => println!("m1 wrong-sig typed => err {e}"),
    }
    match Module::new(&engine, &b"\x00asm not-a-real-module"[..]) {
        Ok(_) => println!("bad binary: unexpectedly ok"),
        Err(e) => println!("bad binary => err {e}"),
    }
    match wat::parse_str("(module (func") {
        Ok(_) => println!("bad wat: unexpectedly ok"),
        Err(e) => {
            let first = e.to_string().lines().next().unwrap_or("").to_string();
            println!("bad wat => err {first}");
        }
    }
}
