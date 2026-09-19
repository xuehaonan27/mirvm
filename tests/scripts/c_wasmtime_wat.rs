#!/usr/bin/env mirvm
---
[dependencies]
# wasmtime =46.0.1 (the latest stable on crates.io when pinned; satisfies the "32.x+ flagship"
# requirement; max_stable_version=46.0.1, MSRV 1.94 < this host's nightly-2026-07-02=1.98).
# All default features on, plus an explicit cranelift (the defaults already include the
# cranelift/wat/threads/stack-switching/parallel-compilation/component-model/async/cache/gc family --
# the flagship position means the whole feature-graph crate closure, measured at 178 crates
# (cargo tree deduped, root included; 165 at runtime with -e no-build); heavy-entry budget ≤40min).
wasmtime = { version = "=46.0.1", features = ["cranelift"] }
---
// wasmtime 46.0.1 VM-in-VM flagship: a guest crate (wasmtime's own code) emits machine
// code inside the interpreter process, then executes it. One Engine backs a Store<App> (custom data round-trip).
//
// Test surface:
// ① Engine+Module instantiate 2 WAT modules (fib/fact) -> exported fib(20)=6765 (typed
//    call surface) / fact(12)=479001600 asserted and printed; fib_iter(40) uses the untyped dynamic call surface.
// ② memory/global/table cross read-write from both wasm and host: memory store8/load8
//    and host write/read see each other, grow past max prints the overflow error; global i32/i64
//    set/get both ways between host and wasm; table host reads a funcref element (prints arity),
//    host writes sq and wasm call_indirect then hits it.
// ③ trap text: call_indirect off-by-one (table size=3, index idx=3)
//    -> Trap::TableOutOfBounds assertion + Display print; idx=2 uninitialized element
//    -> Trap::IndirectCallToNull.
// ④ Store<App> custom data: a host function (Caller mutates data) triggered via the imported tick3,
//    data printed before and after (ticks 0->1, last 7->43).
//
// Runtime knobs (Config layer, not feature pruning; recorded here because each one matters):
// - signals_based_traps(false): bypasses the SIGSEGV/SIGILL trap handlers wasmtime
//   registers by default when the Engine is built on unix. mirvm loudly rejects guest
//   handlers for sync fault signals (SEGV/BUS/FPE/ILL/TRAP), so the default always
//   collides. Trap semantics here do not change when it is off: every trap below is an
//   explicit bounds check and never signal-driven, and native wasmtime supports this.
// - strategy(Cranelift): pinned explicitly (the default has no winch, so it resolves the same as Auto).
// - cranelift_opt_level(None): caps how long the interpreted path spends in cranelift's own code;
//   compiled-output semantics are unchanged (fib/fact values are asserted exactly).
// Everything else stays default (incl. rayon parallel compilation and native-archive's wasmtime-fiber .S link).
//
// Differential oracle: this script is the fixture's positive path, and its output
//   is compared three ways --
//   A: target/release/mirvm run tests/scripts/c_wasmtime_wat.rs
//   B: native `cargo run` of the generated guest crate, with RUSTC/CARGO pinned to
//      nightly-2026-07-02-x86_64-unknown-linux-gnu (the native oracle)
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_wasmtime_wat.rs
// A and C must match B byte-for-byte: fib/fact/fib_iter values, every memory/global/
// table access, the host callback and Store<App> data round-trip, the printed export
// lists, and both trap kinds (Trap::TableOutOfBounds, Trap::IndirectCallToNull),
// asserted on the downcast Trap value after the call fails.
//
// The pin to =46.0.1 is mandatory and is what makes this fixture heavy: the sandbox
// must materialize a native archive that self-closes (`cc -shared -z defs
// --whole-archive`) even though wasmtime's own rlib defines
// resolve_vmctx_memory_ptr_46_0_1 / set_vmctx_memory_46_0_1 via #[export_name]
// (versioned-export-macros) while libwasmtime-helpers.a references them. It must
// also materialize the inline-asm noreturn face that wasmtime's unwinder calls from
// resume_to_exception_handler; without that face the first wasm trap aborts with an
// "inline asm noreturn" engine diagnostic and exit 70.
// The default JIT path and MIRVM_JIT=off both run the two traps to completion, so
// this fixture doubles as a check that both code domains agree on the same output.
//
// JIT lifetime hazard: a native atexit handler stops and joins the JIT worker before
// the allocator is torn down, so the guest must not outlive that ordering.
// Both paths exit 0 after the second trap.
//
// Three-way rerun:
//   A: target/release/mirvm run tests/scripts/c_wasmtime_wat.rs
//   B: d=$(dirname "$(grep -l 'name = "c_wasmtime_wat"' ~/.cache/mirvm/scripts/*/Cargo.toml)")
//      && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//         "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_wasmtime_wat.rs
use wasmtime::{
    Caller, Config, Engine, Instance, Linker, Module, OptLevel, Ref, Store, Strategy, Trap, Val,
};

/// Custom data carried by the Store: ticks counts host callbacks, last is the value the most recent callback computed.
struct App {
    ticks: u64,
    last: i64,
}

/// Uniform Val printing when a result is not matched out as i32/i64/funcref.
fn ival(v: &Val) -> String {
    match v {
        Val::I32(x) => format!("i32 {x}"),
        Val::I64(x) => format!("i64 {x}"),
        other => format!("{other:?}"),
    }
}

/// Module export names in definition order (deterministic).
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

    // ---- ① two WAT modules: fib / fact ----
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

    // untyped dynamic call surface: a Val slice for arguments plus a dynamic result buffer.
    let fib_iter = fib_inst.get_func(&mut store, "fib_iter").unwrap();
    let mut results = [Val::I64(0)];
    fib_iter.call(&mut store, &[Val::I32(40)], &mut results)?;
    assert!(matches!(&results[0], Val::I64(x) if *x == 102_334_155));
    println!("fib_iter(40) = {}", ival(&results[0]));

    // ---- ② object module (memory/global/table) + host import + Store data ----
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

    // memory: reads and writes from wasm
    let store8 = obj.get_typed_func::<(i32, i32), ()>(&mut store, "store8")?;
    let load8 = obj.get_typed_func::<i32, i32>(&mut store, "load8")?;
    store8.call(&mut store, (24, 0xAB))?;
    let lv = load8.call(&mut store, 24)?;
    assert_eq!(lv, 0xAB);
    println!("mem wasm store8/load8(24) = {lv:#06x}");

    // memory: host-side read/write, visible from wasm
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
    let cross = load8.call(&mut store, 16)?; // wasm reads the 'w' the host wrote = 0x77
    println!("mem cross byte = {cross:#06x}");

    // memory grow: overflow error text after reaching max
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

    // global: both directions between host and wasm
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

    // table: host reads a funcref, host writes one, then call_indirect
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

    // host callback + Store<App> data round-trip
    let tick3 = obj.get_typed_func::<i32, i64>(&mut store, "tick3")?;
    let r = tick3.call(&mut store, 21)?;
    assert_eq!(r, 43);
    println!("tick3(21) = {r}");
    println!(
        "data after = ticks {} last {}",
        store.data().ticks,
        store.data().last
    );

    // ---- ③ trap: call_indirect off-by-one (out of bounds) + uninitialized element ----
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
