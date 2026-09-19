#!/usr/bin/env mirvm
---
[dependencies]
# wasm-tools 253 is pinned as one exact set: wat error text/fancy rendering and wasmprinter's
# print layout (which determines the round-trip re-encode bytes) are version-sensitive, so all three pin together.
wat = "=1.253.0"           # crate under test; features pinned to default (component-model)
wasmprinter = "=0.253.0"   # binary -> text printing (wat has no printer of its own; same set)
wast = "=253.0.0"          # wat::Error wraps wast::Error without exposing a span, so it is pulled
                           # to get the syntax-error offset (same set, already in the transitive closure)
---

// wat 1.253.0 (WebAssembly text format WAT <-> binary) three-way differential. Dependency
// closure is ~10 crates (wat->wast{leb128fmt,unicode-width,memchr}, wasmprinter->wasmparser/
// anyhow/termcolor); pure parsing/encoding/printing, no IO/time/randomness, stderr empty.
//
// Test surface:
//   ① Three representative modules, text -> binary (wat::parse_str):
//      m1 = memory (min/max + data segment with \00\ff escapes)/global (immut+mut, i32/i64
//           /f64 hex float)/table+elem/start section;
//      m2 = type reuse + import + three @custom sections (placed before first /
//           before code / after last);
//      m3 = f32/f64 extreme hex literals (max/-0/subnormal) + select +
//           reinterpret bit casts, multiple exports.
//      Each module also tests both Cow paths of wat::parse_bytes (text -> Owned, already
//      binary -> Borrowed returned as-is) and wat::Detect recognition (WasmText/WasmBinary/
//      Unknown garbage input).
//   ② Round-trip consistency for binary -> reprinted text (wasmprinter::print_bytes): per
//      module printed = print(bin), then rebin = parse_str(printed), reprinted = print(rebin);
//      asserts rebin == bin (byte-for-byte) and reprinted == printed (holds natively; name
//      sections are regenerated from identifier names, byte-restored for this input set).
//      Only len/line count/FNV of the printed text are reported, not the full text.
//   ③ Fixed syntax-error diagnostics: the same bad input (i32.const missing operand) via
//      wat::parse_str prints the full wat::Error Display (fancy gutter, five lines:
//      message/--> <anon>:line:col/source line/caret), then wast::parser is used to get
//      span.offset() and compute line/col independently (line=4 col=14 offset=61).
// Output anchors (bin_len/FNV/diagnostic lines) are calibrated against native and locked by assert_eq!.
//
// Three-way rerun:
//   A: target/release/mirvm run corpus/c_wat_parse.rs
//   B: cd "$(grep -l 'name = "c_wat_parse"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_wat_parse.rs
//
// No known limitation: pure Rust parsing/printing, no SIMD/FFI/IO boundary.
use std::borrow::Cow;

const M1: &str = r#"
(module $mem_glob_start
  (memory $mem (export "memory") 1 2)
  (data (i32.const 16) "hello mirvm\00\ff")
  (global $g_i32 i32 (i32.const 42))
  (global $g_mut (mut i64) (i64.const -7))
  (global $g_f64 (mut f64) (f64.const 0x1.921fb54442d18p+2))
  (table $t 2 4 funcref)
  (elem (i32.const 0) $inc $dec)
  (func $inc (param i32) (result i32)
    local.get 0
    i32.const 1
    i32.add)
  (func $dec (export "dec") (param i32) (result i32)
    local.get 0
    i32.const 1
    i32.sub)
  (func $init
    (global.set $g_mut (i64.const 100)))
  (start $init)
)
"#;

const M2: &str = r#"
(module
  (@custom "mirvm-meta" (before first) "\00\01\02tail")
  (@custom "mid-note" (before code) "mid\7f")
  (type $binop (func (param i32 i32) (result i32)))
  (import "env" "log" (func $log (param i32)))
  (func (type $binop) local.get 0 local.get 1 i32.add)
  (func (type $binop) local.get 0 local.get 1 i32.sub)
  (export "add" (func 1))
  (@custom "tail.json" (after last) "{\"k\":1}\00")
)
"#;

const M3: &str = r#"
(module $floats
  (func $f32s (export "consts32") (result f32 f32 f32)
    (f32.const -0x1.fffffep127)
    (f32.const 0x1p-149)
    (f32.const 3.5))
  (func $f64s (export "consts64") (result f64 f64)
    (f64.const 0x1.fffffffffffffp1023)
    (f64.const -0x0.0000000000001p-1022))
  (func (export "sel") (param i32 i32 i32) (result i32)
    (select (local.get 0) (local.get 1) (local.get 2)))
  (func (export "bits") (result i32 i64)
    (i32.reinterpret_f32 (f32.const 1.5))
    (i64.reinterpret_f64 (f64.const -2.5)))
)
"#;

const BAD: &str = "(module\n  (func $f (result i32)\n    i32.const 1\n    i32.const)\n)\n";

/// Self-computed FNV-1a64 (fixed value, no external randomness).
fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// One module: text -> binary -> printed text -> re-encoded, full roundtrip.
fn roundtrip(name: &str, src: &str, expect_bin_len: usize, expect_bin_fnv: u64) {
    let bin = wat::parse_str(src).unwrap();
    println!("{name} text_len={}", src.len());
    println!("{name} bin_len={} bin_fnv={:016x}", bin.len(), fnv1a(&bin));
    assert_eq!(bin.len(), expect_bin_len);
    assert_eq!(fnv1a(&bin), expect_bin_fnv);

    // Both parse_bytes paths: text input -> Owned; already-binary -> Borrowed returned as-is.
    let via_text = wat::parse_bytes(src.as_bytes()).unwrap();
    let via_bin = wat::parse_bytes(&bin).unwrap();
    assert_eq!(via_text[..], bin[..]);
    assert_eq!(via_bin[..], bin[..]);
    println!(
        "{name} parse_bytes text_owned={} bin_borrowed={}",
        matches!(via_text, Cow::Owned(_)),
        matches!(via_bin, Cow::Borrowed(_))
    );
    println!(
        "{name} detect text={:?} bin={:?} garbage={:?}",
        wat::Detect::from_bytes(src.as_bytes()),
        wat::Detect::from_bytes(&bin),
        wat::Detect::from_bytes(b"definitely not wasm")
    );

    let printed = wasmprinter::print_bytes(&bin).unwrap();
    println!(
        "{name} printed_len={} printed_lines={} printed_fnv={:016x}",
        printed.len(),
        printed.lines().count(),
        fnv1a(printed.as_bytes())
    );
    let rebin = wat::parse_str(&printed).unwrap();
    let reprinted = wasmprinter::print_bytes(&rebin).unwrap();
    println!(
        "{name} rebin_len={} rebin_fnv={:016x} rebin_eq_bin={} reprint_eq_print={}",
        rebin.len(),
        fnv1a(&rebin),
        rebin == bin,
        reprinted == printed
    );
    assert_eq!(rebin, bin);
    assert_eq!(reprinted, printed);
}

fn main() {
    // ① + ②: three modules text -> binary -> print roundtrip (anchors from native, versions pinned).
    roundtrip("m1", M1, 223, 0x225e68cec1885875);
    roundtrip("m2", M2, 147, 0xa34e004ab147b432);
    roundtrip("m3", M3, 181, 0xe94aaa52680f6313);

    // ③ syntax-error diagnostics: full wat-level Display (with fancy gutter line/col rendering).
    match wat::parse_str(BAD) {
        Ok(_) => println!("bad-wat unexpectedly ok"),
        Err(e) => {
            println!("wat-error-display begin");
            print!("{e}");
            println!("wat-error-display end");
        }
    }
    // Structured wast-level diagnostic: span.offset -> independent line/col (fixed arithmetic, no environment).
    let buf = wast::parser::ParseBuffer::new(BAD).unwrap();
    match wast::parser::parse::<wast::Wat>(&buf) {
        Ok(_) => println!("bad-wat wast unexpectedly ok"),
        Err(e) => {
            let off = e.span().offset();
            let (mut line, mut col) = (1usize, 1usize);
            for &b in &BAD.as_bytes()[..off] {
                if b == b'\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
            }
            println!(
                "wast-err msg={:?} line={line} col={col} offset={off}",
                e.message()
            );
            assert_eq!((line, col, off), (4, 14, 61));
        }
    }
}
