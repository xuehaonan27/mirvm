#!/usr/bin/env mirvm
---
[dependencies]
# wasmtime =46.0.1（2026-07 时 crates.io 最新 stable，满足「32.x+ 旗舰」要求；
# max_stable_version=46.0.1，MSRV 1.94 < 本机 nightly-2026-07-02=1.98）。
# default features 全开 + 显式 cranelift（default 已含 cranelift/wat/threads/
# stack-switching/parallel-compilation/component-model/async/cache/gc 全家——
# 旗舰定位即压满 feature 图的 crate 闭包，实测 178 个（cargo tree 去重含根；
# -e no-build 运行期 165），重条目预算 ≤40min）。
wasmtime = { version = "=46.0.1", features = ["cranelift"] }
---
// wasmtime 46.0.1 VM-in-VM 旗舰：guest-crate（wasmtime 民事代码）在解释进程里
// 发机器码再执行。引擎装在 Store<App>（自定义 data 往返）上，共享一个 Engine。
//
// 测试面：
// ① Engine+Module 从 2 个 WAT 实例化（fib/fact）→ 导出版 fib(20)=6765（typed
//    调用面）/ fact(12)=479001600 断言打印；fib_iter(40) 走 untyped 动态调用面。
// ② memory/global/table 三对象 wasm 侧 + host 侧交叉读写：memory store8/load8
//    与 host write/read 互见、grow 到 max 后溢出错误文案；global i32/i64 host
//    与 wasm 双向 set/get；table host 读 funcref 元素（arity 打印）、host 写入
//    sq 后 wasm call_indirect 命中。
// ③ trap 文案：call_indirect off-by-one（table size=3 调 idx=3）
//    → Trap::TableOutOfBounds 断言 + Display 打印；idx=2 未初始化元素
//    → Trap::IndirectCallToNull。
// ④ Store<App> 自定义 data：host 函数（Caller 改 data）经导入 tick3 触发，
//    前后 data 打印往返（ticks 0→1, last 7→43）。
//
// 运行期旋钮（Config 运行层，非 feature 裁剪，头注记录）：
// - signals_based_traps(false)：FRONTIER 绕行——mirvm 已知边界「sync 故障信号
//   （SEGV/BUS/FPE/ILL/TRAP）guest handler 响亮拒绝」（current-status 边界表
//   D8d 类）；wasmtime 在 unix 默认于 Engine 建时注册 SIGSEGV/SIGILL 陷阱
//   handler，A 维必撞。关闭后 trap 语义完全不变（本 driver 的 trap 全是显式
//   边界检查，从不依赖信号），native wasmtime 官方支持该配置。
// - strategy(Cranelift)：显式钉死（default 无 winch，与 Auto 解析结果相同）。
// - cranelift_opt_level(None)：控制 A 维解释器跑 cranelift 自身代码的耗时，
//   编译产物语义不变（fib/fact 等值是全序断言的）。
// 其余全默认（含 rayon 并行编译面、native-archive 的 wasmtime-fiber .S 链入）。
//
// 现状（2026-07-18 三维差分定案：expected-red，两层引擎欠账，driver 本体 B 维
// 28 行全绿）：
// 层①（mandated pin 即撞，A/C 同址 exit 101）：debug-builtins 的 helpers.c
//   蹦床对 wasmtime_resolve_vmctx_memory_ptr_46_0_1 / wasmtime_set_vmctx_memory_
//   46_0_1 调用的 resolve_vmctx_memory_ptr_46_0_1 / set_vmctx_memory_46_0_1 由
//   Rust 侧 #[export_name]（versioned-export-macros）定义进 wasmtime 自身
//   rlib——libwasmtime-helpers.a 引用「本 crate rlib 符号」，native_archive
//   单归档自闭合（cc -shared -z defs --whole-archive）响亮拒绝
//   （src/lower/mod.rs:2019 materialize_static_libraries）。最小复现（无
//   wasmtime）：rlibsym 五文件同款构造（build.rs cc helper.c 调
//   #[export_name] 符号）native 绿 / mirvm 同址 101——批5 bzip2-sys 记档
//   「符号在 rlib」闭包欠账形态的第二真实实例。
// 层②（探针实勘：features=default 全单仅减 debug-builtins，其余逐字节同 driver）：
//   归档关即过，解释维与 JIT=1 维均跑出至 data after 共 26 行且与 native 逐字节
//   一致（cranelift 在解释进程里发出机器码执行：fib/fact/fib_iter、memory/global/
//   table 三对象交叉读写、host 回调+Store data 全通），随后首个 wasm trap 抛掷撞
//   wasmtime-unwinder 的 resume_to_exception_handler = inline asm noreturn——
//   引擎诊断「inline asm noreturn（M5.x；三面孔无）」exit 70：asm 物化仅覆盖
//   call-return stub，noreturn 面孔缺席。
// 转正路径建议：① native-archive 闭包判断纳入「本 crate rlib 导出符号集」
//   （批7 867b3de system_dylibs 同思路的 rlib 版）；② inline asm noreturn
//   面孔（M5.x 欠账类目）。gate 接线建议 red_code=101 +
//   red_pattern「无法安全转换为共享库」。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_wasmtime_wat.rs
//   B: d=$(dirname "$(grep -l 'name = "c_wasmtime_wat"' ~/.cache/mirvm/scripts/*/Cargo.toml)")
//      && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//         "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_wasmtime_wat.rs
use wasmtime::{
    Caller, Config, Engine, Instance, Linker, Module, OptLevel, Ref, Store, Strategy, Trap, Val,
};

/// Store 承载的自定义数据：ticks = host 回调计数，last = 最近一次回调算出的值。
struct App {
    ticks: u64,
    last: i64,
}

/// 未做 i32/i64/funcref 展开匹配时的统一 Val 打印。
fn ival(v: &Val) -> String {
    match v {
        Val::I32(x) => format!("i32 {x}"),
        Val::I64(x) => format!("i64 {x}"),
        other => format!("{other:?}"),
    }
}

/// 模块导出名序（定义序，确定）。
fn export_names(m: &Module) -> String {
    m.exports()
        .map(|e| e.name().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

const FIB_WAT: &str = r#"
(module
  (func $fib (export "fib") (param $n i32) (result i32)
    (if (result i32) (i32.lt_s (local.get $n) (i32.const 2))
      (then (local.get $n))
      (else
        (i32.add
          (call $fib (i32.sub (local.get $n) (i32.const 1)))
          (call $fib (i32.sub (local.get $n) (i32.const 2)))))))
  (func $fib_iter (export "fib_iter") (param $n i32) (result i64)
    (local $a i64) (local $b i64) (local $i i32)
    (local.set $a (i64.const 0))
    (local.set $b (i64.const 1))
    (local.set $i (i32.const 0))
    (block $done
      (loop $loop
        (br_if $done (i32.ge_s (local.get $i) (local.get $n)))
        (local.set $b (i64.add (local.get $a) (local.get $b)))
        (local.set $a (i64.sub (local.get $b) (local.get $a)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $loop)))
    (local.get $a))
)
"#;

const FACT_WAT: &str = r#"
(module
  (func $fact (export "fact") (param $n i32) (result i64)
    (if (result i64) (i32.le_s (local.get $n) (i32.const 1))
      (then (i64.const 1))
      (else
        (i64.mul
          (i64.extend_i32_s (local.get $n))
          (call $fact (i32.sub (local.get $n) (i32.const 1)))))))
)
"#;

const OBJ_WAT: &str = r#"
(module
  (import "host" "tick" (func $tick (param i32) (result i64)))
  (memory (export "mem") 1 2)
  (global $g (export "g") (mut i32) (i32.const 7))
  (global $h (export "h64") (mut i64) (i64.const -9))
  (table $t (export "tbl") 3 funcref)
  (func $sq (export "sq") (param i32) (result i32)
    (i32.mul (local.get 0) (local.get 0)))
  (elem (i32.const 0) $sq)
  (func (export "store8") (param $addr i32) (param $v i32)
    (i32.store8 (local.get $addr) (local.get $v)))
  (func (export "load8") (param $addr i32) (result i32)
    (i32.load8_u (local.get $addr)))
  (func (export "set_g") (param $v i32) (global.set $g (local.get $v)))
  (func (export "get_g") (result i32) (global.get $g))
  (func (export "set_h") (param $v i64) (global.set $h (local.get $v)))
  (func (export "get_h") (result i64) (global.get $h))
  (func (export "tick3") (param $x i32) (result i64)
    (call $tick (local.get $x)))
  (func (export "call_sq_at") (param $arg i32) (param $idx i32) (result i32)
    (call_indirect (param i32) (result i32) (local.get $arg) (local.get $idx)))
)
"#;

fn main() -> wasmtime::Result<()> {
    let mut config = Config::new();
    config
        .signals_based_traps(false)
        .strategy(Strategy::Cranelift)
        .cranelift_opt_level(OptLevel::None);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, App { ticks: 0, last: 7 });

    // ---- ① 两个 WAT 模块：fib / fact ----
    let fib_mod = Module::new(&engine, FIB_WAT)?;
    println!("fib exports = {}", export_names(&fib_mod));
    let fib_inst = Instance::new(&mut store, &fib_mod, &[])?;
    let fib = fib_inst.get_typed_func::<i32, i32>(&mut store, "fib")?;
    let v = fib.call(&mut store, 20)?;
    assert_eq!(v, 6765);
    println!("fib(20) = {v}");

    let fact_mod = Module::new(&engine, FACT_WAT)?;
    println!("fact exports = {}", export_names(&fact_mod));
    let fact_inst = Instance::new(&mut store, &fact_mod, &[])?;
    let fact = fact_inst.get_typed_func::<i32, i64>(&mut store, "fact")?;
    let v = fact.call(&mut store, 12)?;
    assert_eq!(v, 479_001_600);
    println!("fact(12) = {v}");

    // untyped 动态调用面：Val 切片入参 + 动态结果缓冲。
    let fib_iter = fib_inst.get_func(&mut store, "fib_iter").unwrap();
    let mut results = [Val::I64(0)];
    fib_iter.call(&mut store, &[Val::I32(40)], &mut results)?;
    assert!(matches!(&results[0], Val::I64(x) if *x == 102_334_155));
    println!("fib_iter(40) = {}", ival(&results[0]));

    // ---- ② 对象模块（memory/global/table）+ host 导入 + Store data ----
    let mut linker: Linker<App> = Linker::new(&engine);
    linker.func_wrap(
        "host",
        "tick",
        |mut caller: Caller<'_, App>, x: i32| -> i64 {
            let d = caller.data_mut();
            d.ticks += 1;
            d.last = (x as i64) * 2 + 1;
            d.last
        },
    )?;
    println!(
        "data before = ticks {} last {}",
        store.data().ticks,
        store.data().last
    );
    let obj_mod = Module::new(&engine, OBJ_WAT)?;
    println!("obj exports = {}", export_names(&obj_mod));
    let obj = linker.instantiate(&mut store, &obj_mod)?;

    // memory：wasm 侧读写
    let store8 = obj.get_typed_func::<(i32, i32), ()>(&mut store, "store8")?;
    let load8 = obj.get_typed_func::<i32, i32>(&mut store, "load8")?;
    store8.call(&mut store, (24, 0xAB))?;
    let lv = load8.call(&mut store, 24)?;
    assert_eq!(lv, 0xAB);
    println!("mem wasm store8/load8(24) = {lv:#06x}");

    // memory：host 侧读写 + wasm 侧互见
    let mem = obj.get_memory(&mut store, "mem").unwrap();
    println!(
        "mem pages = {} bytes = {}",
        mem.size(&store),
        mem.data_size(&store)
    );
    mem.write(&mut store, 16, b"wasmtime")?;
    let mut buf = [0u8; 8];
    mem.read(&store, 16, &mut buf)?;
    println!("mem host rw = {}", std::str::from_utf8(&buf).unwrap());
    let cross = load8.call(&mut store, 16)?; // wasm 读 host 写的 'w' = 0x77
    println!("mem cross byte = {cross:#06x}");

    // memory grow：到 max 后溢出错误文案
    let prev = mem.grow(&mut store, 1)?;
    println!(
        "mem grow prev = {prev} now = {} pages bytes = {}",
        mem.size(&store),
        mem.data_size(&store)
    );
    match mem.grow(&mut store, 1) {
        Ok(_) => println!("mem grow max unexpected ok"),
        Err(e) => println!("mem grow max err = {e}"),
    }

    // global：host 与 wasm 双向
    let g = obj.get_global(&mut store, "g").unwrap();
    let h = obj.get_global(&mut store, "h64").unwrap();
    println!(
        "g ty = {:?} {:?}",
        g.ty(&store).content(),
        g.ty(&store).mutability()
    );
    println!("g init = {}", ival(&g.get(&mut store)));
    g.set(&mut store, Val::I32(1234))?;
    let get_g = obj.get_typed_func::<(), i32>(&mut store, "get_g")?;
    let set_g = obj.get_typed_func::<i32, ()>(&mut store, "set_g")?;
    let v = get_g.call(&mut store, ())?;
    assert_eq!(v, 1234);
    println!("g after host set (wasm get) = {v}");
    set_g.call(&mut store, 77)?;
    println!("g after wasm set (host get) = {}", ival(&g.get(&mut store)));
    println!("h64 init = {}", ival(&h.get(&mut store)));
    let set_h = obj.get_typed_func::<i64, ()>(&mut store, "set_h")?;
    set_h.call(&mut store, 0x1122_3344_5566_7788)?;
    let hv = h.get(&mut store);
    assert!(matches!(&hv, Val::I64(x) if *x == 0x1122_3344_5566_7788));
    println!("h64 wasm set (host get) = {}", ival(&hv));

    // table：host 读 funcref + host 写入 + call_indirect
    let tbl = obj.get_table(&mut store, "tbl").unwrap();
    println!("tbl size = {}", tbl.size(&store));
    match tbl.get(&mut store, 0) {
        Some(Ref::Func(Some(f))) => {
            let ty = f.ty(&store);
            println!(
                "tbl[0] func = true sig {}->{}",
                ty.params().len(),
                ty.results().len()
            );
        }
        _ => println!("tbl[0] func = false"),
    }
    println!("tbl[1] null = {}", tbl.get(&mut store, 1).is_none());
    let call_at = obj.get_typed_func::<(i32, i32), i32>(&mut store, "call_sq_at")?;
    let v = call_at.call(&mut store, (9, 0))?;
    assert_eq!(v, 81);
    println!("call_sq_at(9,0) = {v}");
    let sq = obj.get_func(&mut store, "sq").unwrap();
    tbl.set(&mut store, 1, Ref::from(sq))?;
    let v = call_at.call(&mut store, (5, 1))?;
    assert_eq!(v, 25);
    println!("call_sq_at(5,1) after host table write = {v}");

    // host 回调 + Store<App> data 往返
    let tick3 = obj.get_typed_func::<i32, i64>(&mut store, "tick3")?;
    let r = tick3.call(&mut store, 21)?;
    assert_eq!(r, 43);
    println!("tick3(21) = {r}");
    println!(
        "data after = ticks {} last {}",
        store.data().ticks,
        store.data().last
    );

    // ---- ③ trap：call_indirect off-by-one（越界）+ 未初始化元素 ----
    let err = call_at.call(&mut store, (9, 3)).unwrap_err();
    let t = err.downcast_ref::<Trap>().unwrap();
    assert_eq!(*t, Trap::TableOutOfBounds);
    println!("trap oob = {t}");
    let err = call_at.call(&mut store, (1, 2)).unwrap_err();
    let t = err.downcast_ref::<Trap>().unwrap();
    assert_eq!(*t, Trap::IndirectCallToNull);
    println!("trap null = {t}");

    Ok(())
}
