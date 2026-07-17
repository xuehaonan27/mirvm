#!/usr/bin/env mirvm
---
[dependencies]
# wasm-tools 253 车同钉精确补丁：wat 错误文案/fancy 渲染与 wasmprinter 的
# 打印排版（决定往返再编码字节）都是版本敏感面，钉死三者才保证跨机逐字节。
wat = "=1.253.0"           # 被测 crate；特征按任务钉 default（component-model）
wasmprinter = "=0.253.0"   # binary→text 打印（wat 本身不带 printer；同车配套）
wast = "=253.0.0"          # wat::Error 包住 wast::Error 不暴露 span，直行引入
                           # 拿语法错误的 offset（同车，本就在传递闭包内）
---

// wat 1.253.0（WebAssembly 文本格式 WAT ↔ 二进制）三维差分。依赖闭包约 10 个
// crate（wat→wast{leb128fmt,unicode-width,memchr}，wasmprinter→wasmparser/
// anyhow/termcolor），纯解析/编码/打印，无 IO/时间/随机，stderr 真空。
//
// 测试面：
//   ① 三个代表性模块 文本→binary（wat::parse_str）：
//      m1 = memory(min/max+data 段含 \00\ff 转义)/global(immut+mut，i32/i64
//           /f64 十六进制浮点)/table+elem/start 段；
//      m2 = type 复用 + import + 三个 @custom 自定义段（before first /
//           before code / after last 三种放置位）；
//      m3 = f32/f64 极值十六进制字面量（max/-0/subnormal）+ select +
//           reinterpret 位转换，多导出。
//      每模块同时测 wat::parse_bytes 的 Cow 双路径（文本→Owned、已是二进
//      制→Borrowed 原样返回）与 wat::Detect 识别（WasmText/WasmBinary/
//      Unknown 垃圾输入）。
//   ② binary→重新打印文本的往返一致性（wasmprinter::print_bytes）：对每模块
//      printed = print(bin)，再 rebin = parse_str(printed)、
//      reprinted = print(rebin)，断言 rebin == bin（逐字节）且
//      reprinted == printed（native 实测成立；name 段由标识符名重生成，
//      本组输入下字节级还原）。打印文本只报告 len/行数/FNV，不全量铺出。
//   ③ 语法错误诊断固定打印：同一错误输入（i32.const 缺操作数）经
//      wat::parse_str 打印 wat::Error Display 全文（fancy gutter 五行：
//      消息/--> <anon>:行:列/源码行/caret），再经 wast::parser 取
//      span.offset()，自行折算行/列打印（line=4 col=14 offset=61）。
//   输出锚点（bin_len/FNV/诊断行）native 实测校准并 assert_eq! 锁定。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_wat_parse.rs
//   B: cd "$(grep -l 'name = "c_wat_parse"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_wat_parse.rs
//
// FRONTIER：无（纯 Rust 解析/打印，无 SIMD/FFI/IO 边界）。
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

/// 自算 FNV-1a64（定值，无外部随机性）。
fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 单模块：文本→binary→打印文本→再编码，全链往返。
fn roundtrip(name: &str, src: &str, expect_bin_len: usize, expect_bin_fnv: u64) {
    let bin = wat::parse_str(src).unwrap();
    println!("{name} text_len={}", src.len());
    println!("{name} bin_len={} bin_fnv={:016x}", bin.len(), fnv1a(&bin));
    assert_eq!(bin.len(), expect_bin_len);
    assert_eq!(fnv1a(&bin), expect_bin_fnv);

    // parse_bytes 双路径：文本输入 → Owned；已是二进制 → Borrowed 原样返回。
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
    // ① + ②：三模块文本→binary→打印往返（锚点为 native 实测值，版本已钉死）。
    roundtrip("m1", M1, 223, 0x225e68cec1885875);
    roundtrip("m2", M2, 147, 0xa34e004ab147b432);
    roundtrip("m3", M3, 181, 0xe94aaa52680f6313);

    // ③ 语法错误诊断：wat 层 Display 全文（含 fancy gutter 行/列渲染）。
    match wat::parse_str(BAD) {
        Ok(_) => println!("bad-wat unexpectedly ok"),
        Err(e) => {
            println!("wat-error-display begin");
            print!("{e}");
            println!("wat-error-display end");
        }
    }
    // wast 层结构化诊断：span.offset → 自算行/列（固定算术，无环境依赖）。
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
