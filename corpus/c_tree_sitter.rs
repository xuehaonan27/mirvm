#!/usr/bin/env mirvm
---
[dependencies]
# tree-sitter 0.24.7（任务名义线 0.24 的最新 patch）+ tree-sitter-rust 0.23.3
# （配套 0.24 绑定面的 rust 语法包最新 patch；经 tree-sitter-language 0.1 的
# LanguageFn 衔接）+ streaming-iterator 0.1.9（QueryCursor::matches 迭代 trait，
# tree-sitter 的公开依赖类型，直引同名同版）。feature 全默认：tree-sitter 默认
# 只带 cc 构建的自带 C 运行期（无 wasm 可选件）。闭包 ≈20 crate（含 tree-sitter
# 为 #match? 谓词直引的 regex/regex-syntax——本 driver 不用谓词，仅进闭包）。
# 两个 cc 构建面：tree-sitter 编自带 libtree-sitter（parser.c/query.c/...），
# tree-sitter-rust 编 src/parser.c + scanner.c，静态 .a 由 mirvm 的
# native-archive（.a→.so 闭包）通道加载。语法包入口 tree_sitter_rust() 经
# LanguageFn 以存值 extern fn-ptr 间接调用（Language::from 内 builder()）。
tree-sitter = "=0.24.7"
tree-sitter-rust = "=0.23.3"
streaming-iterator = "=0.1.9"
---
// tree-sitter 0.24（C 运行期）+ tree-sitter-rust 0.23（cc 编译 C 语法包）
// 三维差分——mirvm 的 native-archive / FFI 主战场。解析与查询的计算主体全在
// native C 里跑，两侧同源同输入 → 解析树/错误位置/查询命中天然逐字节确定；
// Rust 绑定层（Language/Parser/Tree/Node/Query/QueryCursor 薄封装、Drop 链、
// TSNode/TSPoint 按值结构体返回、extern fn-ptr 存值间接调用）才是被解释/JIT
// 的对象。
//
// 测试面清单：
//   ① Language 元数据：version/node_kind_count/parse_state_count——语法包 C
//      静态表字段，Language 本体经 LanguageFn 存值的 extern "C" fn-ptr 调用
//      tree_sitter_rust() 取得（FFI 间接调用面）。
//   ② 三个合法片段（fn 定义 / struct+impl / macro_rules 定义+宏调用）各解析
//      一遍：has_error 断言 + descendant_count + root.to_sexp() 全量 sexp。
//   ③ 语法错误片段「fn broken( { let x = ; }」：has_error=true；前序 DFS
//      收集 MISSING/ERROR 节点（kind + 起止 row:col）；整树 sexp 含
//      (MISSING ")") 与 (ERROR)。
//   ④ 语法查询：fn-name / impl-type / macro-name / callee 四个 S 表达式模式
//      各编译成 Query，QueryCursor::matches 迭代（StreamingIterator）；按模式
//      固定序打印 matches 计数、capture 名→次数（BTreeMap 字典序）与每个
//      capture 的 名`文本`@row:col（匹配序）；附一例非法模式（不存在的节点
//      名）走 QueryError row/col/kind 打印。
//
// 确定性：源串/模式全为常量；解析/查询由同一 C 库同参算出；无时间/随机/
// 地址/HashMap 序；stderr 真空。
//
// FRONTIER（2026-07-17 实测定因，expected-red）：A 维停在首个 `parser.parse`。
// tree-sitter 0.24 Rust 绑定的所有 parse 路径（parse/parse_with/parse_utf16_with）
// 都汇到 `ffi::ts_parser_parse(parser, old_tree, input: TSInput)`，其中 TSInput
// = {payload: *mut c_void, read: Option<extern "C" fn>, encoding: c_uint} 是
// 24 字节按值聚合参数；mirvm 的 foreign 直通（lower 期 ffi_kind_of，
// src/lower/mod.rs）只接标量/指针，按值聚合调用点在降低期冻结为入口 Trap：
//   TRAP: foreign `ts_parser_parse` 参数 tree_sitter::ffi::TSInput: 非标量
//   （按值聚合）（libffi 直通仅标量/指针）（fn …Parser10parse_with…）
// 且整 crate 的剩余面全是同类墙：ts_node_*/ts_tree_* 一族按值传 TSNode(32B)、
// 按值返回 TSPoint(8B)/TSNode，ts_query_cursor_* 同样——不存在「改用绑定内别的
// 调用」的合法绕行（绑定从不调 ts_parser_parse_string）；TSInput.read 还是
// 嵌在结构体里的 guest 回调、且其签名自带按值 TSPoint 参数（批3 已记的
// 「结构体内嵌回调盲区」与 thunk 仅标量/指针双撞）——即使按值聚合封送补齐
// 也需这两处一并根治。最小复现（纯 std、无 crate）：/tmp/ts_ffi_repro.rs
// （extern "C" fn 取 #[repr(C)] struct TwoU64 按值参数即 Trap 同文 exit 70）。
// 陷前已验绿（stdout 首行后停）：native-archive .a→.so 闭包加载
// libtree-sitter + rust 语法包；LanguageFn 存值 extern fn-ptr 间接调用
// tree_sitter_rust()；标量/指针 FFI 5 连（ts_language_version/symbol_count/
// state_count、ts_parser_new、ts_parser_set_language）——language version=14
// kinds=355 states=3823 与 native 逐字节一致。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_tree_sitter.rs
//   B: cd "$(grep -l 'name = "c_tree_sitter"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_tree_sitter.rs
use std::collections::BTreeMap;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

const SRC_FN: &str = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

const SRC_IMPL: &str = "struct Pair {\n    x: i32,\n    y: i32,\n}\nimpl Pair {\n    fn total(&self) -> i32 {\n        self.x + self.y\n    }\n}\n";

const SRC_MACRO: &str = "macro_rules! double {\n    ($e:expr) => {\n        $e * 2\n    };\n}\nfn main() {\n    let xs = vec![1, 2, 3];\n    println!(\"count={} dbl={}\", xs.len(), double!(7));\n}\n";

const SRC_ERR: &str = "fn broken( {\n    let x = ;\n}\n";

const SRC_QUERY: &str = "fn alpha() {}\nfn beta(x: u32) -> u32 { x + 1 }\nstruct S;\nimpl S {\n    fn gamma(&self) {}\n    fn delta(&self) -> u32 { 2 }\n}\nfn main() {\n    let _y = beta(9);\n    let v = vec![1, 2, 3];\n    dbg!(v);\n    println!(\"{}\", beta(41));\n}\n";

const QUERIES: &[&str] = &[
    "(function_item name: (identifier) @fn-name)",
    "(impl_item type: (type_identifier) @impl-type)",
    "(macro_invocation macro: (identifier) @macro-name)",
    "(call_expression function: (identifier) @callee)",
];

/// 收集 ERROR / MISSING 节点（前序 DFS，序确定）。
fn find_errors(node: Node, out: &mut Vec<String>) {
    if node.is_error() || node.is_missing() {
        let s = node.start_position();
        let e = node.end_position();
        out.push(format!(
            "{}({}:{}-{}:{})",
            node.kind(),
            s.row,
            s.column,
            e.row,
            e.column
        ));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_errors(child, out);
    }
}

fn main() {
    let language = Language::from(tree_sitter_rust::LANGUAGE);
    println!(
        "language version={} kinds={} states={}",
        language.version(),
        language.node_kind_count(),
        language.parse_state_count()
    );

    let mut parser = Parser::new();
    parser.set_language(&language).unwrap();

    // ① 三个合法片段：fn / impl / macro 调用 → S-expression。
    for (label, src) in [("fn", SRC_FN), ("impl", SRC_IMPL), ("macro", SRC_MACRO)] {
        let tree = parser.parse(src, None).unwrap();
        let root = tree.root_node();
        println!(
            "snippet {label}: has_error={} nodes={}",
            root.has_error(),
            root.descendant_count()
        );
        println!("sexp {label}: {}", root.to_sexp());
    }

    // ② 语法错误片段：ERROR/MISSING 节点位置（row/col）+ 整树 sexp。
    let tree = parser.parse(SRC_ERR, None).unwrap();
    let root = tree.root_node();
    let mut errs = Vec::new();
    find_errors(root, &mut errs);
    println!("snippet err: has_error={} errors={errs:?}", root.has_error());
    println!("sexp err: {}", root.to_sexp());

    // ③ 语法查询：四个模式命中计数按固定序打印；capture 名计数走 BTreeMap。
    let tree = parser.parse(SRC_QUERY, None).unwrap();
    let root = tree.root_node();
    println!("query src: has_error={}", root.has_error());
    for (i, pat) in QUERIES.iter().enumerate() {
        let query = Query::new(&language, pat).unwrap();
        let names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, SRC_QUERY.as_bytes());
        let mut n = 0usize;
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut caps: Vec<String> = Vec::new();
        while let Some(m) = matches.next() {
            n += 1;
            for cap in m.captures {
                let name = names[cap.index as usize];
                *counts.entry(name).or_default() += 1;
                let p = cap.node.start_position();
                caps.push(format!(
                    "{name}`{}`@{}:{}",
                    cap.node.utf8_text(SRC_QUERY.as_bytes()).unwrap(),
                    p.row,
                    p.column
                ));
            }
        }
        println!("query#{i} matches={n} counts={counts:?} caps={caps:?}");
    }

    // ③b 查询编译错误路径（不存在的节点名 → QueryError row/col/kind）。
    match Query::new(&language, "(function_item name: (not_a_real_node) @x)") {
        Ok(_) => println!("bad query unexpectedly ok"),
        Err(e) => println!("bad query: row={} col={} kind={:?}", e.row, e.column, e.kind),
    }
}
