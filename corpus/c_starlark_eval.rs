#!/usr/bin/env mirvm
---
[dependencies]
# starlark 0.13.0 (the only and highest published version on the crates.io 0.13 line; the
# task limits 0.12/0.13, and 0.14.x pulls the default-feature blake3 family -- a cpuid/SIMD
# dispatch hazard -- so it is not taken). The crate has no feature table (0.13.0 features={}):
# default is the full set; rustyline/debugserver-types enter the closure but never fire at runtime.
starlark = "=0.13.0"
# The #[starlark_module] macro expansion emits anyhow::Result references in host code, so the
# host must carry anyhow (per starlark docs and the buck2 convention); pinned exactly.
anyhow = "=1.0.100"
# Upstream semver hole: starlark_map 0.13.0 builds on allocative[hashbrown]'s RawTable
# Allocative impl; allocative 0.3.6 silently raises that optional dependency from
# hashbrown 0.14 to 0.16 without a minor bump, forking from the hashbrown 0.14 that
# starlark_map builds itself -> RawTable: Allocative is unsatisfied and compilation fails.
# 0.3.5 is yanked, so this pins =0.3.4 (whose hashbrown optional is still ^0.14.3 + "raw").
allocative = "=0.3.4"
---
// starlark 0.13.0: the Bazel/Buck Starlark dialect evaluator (a bytecode interpreter, VM in VM).
// Three scripts run in order on one Module via AstModule::parse + Evaluator::eval_module.
// The host injects a bump() callback through #[starlark_module] whose call count and fold
// state travel back via Evaluator.extra, plus four values through both GlobalsBuilder::set
// and Module::set, which the scripts read back and compute on. EvalModule return values
// (lists of tuples/mixed) print one repr per element; the host calls back into script
// functions with positional and named arguments; freeze exports the FrozenModule namespace
// (BTreeSet-sorted names with value reprs); two evaluation errors print fixed ErrorKind labels.
//
// Test surface:
//   S1 recursive fib: def/if/return/list comprehension/range, fib(0..15) reprs; the script
//     calls the host bump(fib(10)) to round-trip a value across the boundary.
//   S2 data structures: dict counting (get default, keys), sorted key order, list.index
//     search, and a loop reading back the extremum; arbitrary-precision integers (starlark
//     int sits on num-bigint): % and // on 1<<100 -- num-bigint 0.4.8 divides u64 magnitudes
//     through div_wide, whose inline `div` asm is the asm-lowering regression surface this
//     case exercises.
//   S3 strings and templates: join/str.format/% formatting, bounded slicing, replace, find,
//     len; the globals-injected (host_mul=7 / host_suffix) and module-injected
//     (injected_base=1000 / injected_label) values are all computed on and read back.
//   Error surface: the script's fail("boom-42") prints a full Traceback under ErrorKind::Fail;
//     a wrong-arity host eval_function gives ErrorKind::Function. Each kind maps through
//     match to a fixed label. Final state: the frozen module exports 7 names
//     (build_strings/fib/greet/two injected values/run/search) plus 5 callback calls and the fold.
//
// NOTE: confirmed engine bug in the JIT dimension (expected-red; not fixed in the engine).
// With MIRVM_JIT_THRESHOLD=1 the jit worker thread panics while compiling starlark's
// ChunkChain::drop (unavoidable heap teardown on any starlark evaluation) with an
// address-offset-must-land-in-frame message (full analyze_frame set) from
// src/vm/engine/jit_compile.rs:1276. The worker dies, later functions fall back to the
// interpreter, and stdout/exit code stay byte-identical to A/B; only stderr is polluted
// by the panic text (which embeds the pid), on three of three runs. Mechanism chain:
//   ChunkChain::drop's MIR has a &mut ZST local as a generic argument; that ZST is the
//   frame's last local, and the freeze at src/lower/frame.rs:113-129 gives it size=0 with
//   an offset that can reach off==frame_size. scan_place (jit_compile.rs:3089-3114) clamps
//   it to (0,fsz) plus Escape, producing the degenerate interval (fsz,fsz), which
//   FrameMap::add drops via `if a<b` (3063): the frame set becomes empty -> frame_ss=None,
//   so translating the same Ref blows up in the addr_of_local (1273) expect. The shape is ubiquitous.
// Minimal repro (21 lines, no crate dependencies; negative control: calling the closure
//   directly does not panic, because the addressed ZST is no longer the last local -- no degenerate interval):
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
// Run: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run <file>
//   -> thread 'mirvm-jit' panicked at jit_compile.rs:1276 (done still prints, rc=0).
//   The Drop::drop form (a drop body calling a generic method plus a &mut closure literal,
//   isomorphic to ChunkChain::drop) panics as well.
//
// Determinism: only int/bigint reprs, fixed strings, dict/list in starlark's specified insertion
// order (stable across processes), qualified function reprs, and error strings (fixed file/line/
// source line) print; export names are BTreeSet-sorted; no floats, time, randomness, env or TLS
// order; single-threaded sequential execution gives bump counts one order; stderr stays empty.
//
// Closure ~165 crates (heavy: the starlark itself plus the rustyline/lalrpop-util legacy
// chain and the lsp-types/serde_json family); cold build time is budgeted separately.
//
// Three dimensions (A/B/C), byte-identical on stdout and exit code; C is expected-red only
// because of the JIT bug above, whose panic text lands on stderr:
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

/// Cross-boundary state: the host bump() call count and fold value (single-threaded, one order).
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

    // Host callback into a script function: positional then named argument channels.
    let heap = module.heap();
    let fib = module.get("fib").unwrap();
    let r = eval.eval_function(fib, &[heap.alloc(12)], &[]).unwrap();
    println!("host-call fib(12) = {}", r.to_str());

    // Named-argument channel: define a function with an extra parameter and call it back.
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

    // Evaluation error surface (each ErrorKind prints as a fixed label).
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

    // eval_function arity error (prints its ErrorKind).
    match eval.eval_function(fib, &[], &[]) {
        Ok(v) => println!("arity-case: unexpectedly ok {}", v.to_str()),
        Err(e) => println!("arity-case kind={} msg={}", kind_label(&e), e),
    }

    drop(eval);

    // Module namespace export: sorted names plus each value's representation.
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
