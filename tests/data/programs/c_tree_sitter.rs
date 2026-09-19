#!/usr/bin/env mirvm
---
[dependencies]
# tree-sitter 0.24.7 (newest patch on the 0.24 line) + tree-sitter-rust 0.23.3
# (newest patch of the rust grammar aimed at the 0.24 bindings; it bridges through
# tree-sitter-language 0.1's LanguageFn) + streaming-iterator 0.1.9, pinned to the
# exact version tree-sitter depends on because QueryCursor::matches consumes its
# StreamingIterator. Features are all default: tree-sitter carries only the
# cc-built C runtime (no wasm optional). The closure is about 20 crates, including
# the regex/regex-syntax pulled in for #match? predicates (unused here). Two cc
# build surfaces: tree-sitter compiles its bundled libtree-sitter (parser.c/
# query.c/...), and tree-sitter-rust compiles src/parser.c + scanner.c; the static
# .a files load through mirvm's native-archive (.a -> .so) channel.
tree-sitter = "=0.24.7"
tree-sitter-rust = "=0.23.3"
streaming-iterator = "=0.1.9"
---
// tree-sitter 0.24 (C runtime) + tree-sitter-rust 0.23 (cc-compiled C grammar)
// three-way differential: mirvm's native-archive / FFI battleground. Parsing and
// query evaluation run entirely in native C, and both sides see the same source
// and inputs, so the parse tree, error positions and query hits are byte-identical
// by construction; the interpreted/JIT-compiled object is the Rust binding layer
// (thin Language/Parser/Tree/Node/Query/QueryCursor wrappers, the Drop chain,
// by-value TSNode/TSPoint struct returns, and stored extern fn-ptr indirect calls).
//
// Coverage:
//   ① Language metadata (version/node_kind_count/parse_state_count) read from the
//      grammar's static C tables via a stored extern "C" fn-ptr call to
//      tree_sitter_rust() (the FFI indirect-call surface).
//   ② Three legal snippets (fn / struct+impl / macro_rules) parsed once each into
//      an S-expression: has_error assertion, descendant_count, full root.to_sexp().
//   ③ The broken snippet "fn broken( { let x = ; }": has_error=true, a pre-order
//      DFS of the MISSING/ERROR nodes (kind + row:col), and the whole-tree sexp
//      containing (MISSING ")") and (ERROR).
//   ④ Four S-expression query patterns compiled to Query and iterated with
//      QueryCursor::matches (StreamingIterator): match count, capture counts
//      (BTreeMap), and each capture as name`text`@row:col; plus one invalid
//      pattern exercising the QueryError row/col/kind print.
//
// Determinism: sources and patterns are constants; the same C library computes
// everything from the same arguments; no time/random/address/HashMap ordering;
// stderr is empty.
//
// FRONTIER (measured; expected-red): the first `parser.parse` traps. Every parse
// path in the tree-sitter 0.24 bindings funnels into
// `ffi::ts_parser_parse(parser, old_tree, input: TSInput)`, where TSInput =
// {payload: *mut c_void, read: Option<extern "C" fn>, encoding: c_uint} is a
// 24-byte by-value aggregate. mirvm's foreign passthrough (ffi_kind_of during
// lowering, src/lower/mod.rs) accepts only scalars and pointers, so the call site
// is frozen into an entry trap:
//   TRAP: foreign `ts_parser_parse` parameter tree_sitter::ffi::TSInput: not a scalar
//   (by-value aggregate) (libffi passthrough is scalar/pointer only) (fn ...Parser10parse_with...)
// The rest of the crate hits the same wall: ts_node_*/ts_tree_* and
// ts_query_cursor_* pass TSNode(32B) or TSPoint(8B) by value, and the bindings
// never call ts_parser_parse_string, so there is no legal detour. TSInput.read is
// also a callback embedded in the struct whose own signature takes a by-value
// TSPoint. Minimal repro: /tmp/ts_ffi_repro.rs (a #[repr(C)] struct passed by
// value to an extern "C" fn traps with exit 70). Verified green before the trap:
// the native-archive .a -> .so closure loads libtree-sitter and the grammar, and
// five scalar/pointer FFI calls give version=14 kinds=355 states=3823, matching
// native byte-for-byte.
//
// Three-way rerun:
//   A: target/release/mirvm run tests/data/programs/c_tree_sitter.rs
//   B: cd "$(grep -l 'name = "c_tree_sitter"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_tree_sitter.rs
//   (B runs the materialized script dir under the pinned nightly toolchain.)
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

/// Collect ERROR / MISSING nodes (pre-order DFS, deterministic order).
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

    // ① three legal snippets: fn / impl / macro invocation -> S-expression.
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

    // ② broken snippet: ERROR/MISSING node positions (row/col) + whole-tree sexp.
    let tree = parser.parse(SRC_ERR, None).unwrap();
    let root = tree.root_node();
    let mut errs = Vec::new();
    find_errors(root, &mut errs);
    println!("snippet err: has_error={} errors={errs:?}", root.has_error());
    println!("sexp err: {}", root.to_sexp());

    // ③ syntax queries: four patterns print their hit counts in fixed order; capture counts via BTreeMap.
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

    // ③b query compilation error path (nonexistent node name -> QueryError row/col/kind).
    match Query::new(&language, "(function_item name: (not_a_real_node) @x)") {
        Ok(_) => println!("bad query unexpectedly ok"),
        Err(e) => println!("bad query: row={} col={} kind={:?}", e.row, e.column, e.kind),
    }
}
