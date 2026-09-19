#!/usr/bin/env mirvm
---
[dependencies]
# swc_ecma_parser =41.1.2 (latest at the 2026-07-17 snapshot; swc has moved to a semver
# major-version stream: parser 41.x / ast 25.x / common 23.x). parser 41.1.2 hard-requires
# swc_common ^23.0.2, swc_ecma_ast ^25.0.0 and swc_atoms ^9.0.3, so the trio is pinned as a
# compatible set. default-features=false + typescript: default=["typescript",
# "stacker"] -- typescript is kept (needed for the S2/S4 TS/TSX syntax surface) and stacker is
# cut to avoid the stacker->psm global_asm native-archive path; this driver's inputs are shallow
# with no deep-recursion need, so the cut is purely budgetary. flow/verify/debug/tracing-spans
# all stay off. swc_ecma_ast enables serde-impl (serde JSON serialization of the AST, the basis
# for the census; rkyv/encoding/plugin all cut). swc_common 23.0.2 default=[] (tty-emitter, i.e.
# termcolor, concurrent, parking_lot and sourcemap, all off), so diagnostics go through
# kind().msg() hand-formatting rather than a Handler/emitter. serde_json keeps the corpus's
# existing "1" pin. The dependency closure is 89 crates (num-bigint 0.4.8 included -- the
# parser pulls ^0.4.3 for BigInt literal lexing, though this driver has no BigInt literals and
# never reaches div_wide asm; tracing compiles to no-op; there is no C/FFI artifact and every
# build.rs is a Rust version gate), far below the 200 limit.
swc_ecma_parser = { version = "=41.1.2", default-features = false, features = ["typescript"] }
swc_ecma_ast = { version = "=25.0.0", features = ["serde-impl"] }
swc_common = "=23.0.2"
serde_json = "1"
---
// swc_ecma_parser 41.1.2 (swc's hand-written recursive-descent parser; Box-tree AST + global
// BytePos spans + the take_errors recovery model) differential. A sister pressure case to
// c_oxc_parse: sources S1/S2/S3 are byte-for-byte the same text as that driver's, so one input
// hits two very different lexer/visitor/AST implementations (swc has no arena and uses its own
// Ts*/JSX* node-naming family). The measured anchor spans agree (swc BytePos is a 1-based
// absolute offset: oxc (15,91)/(0,92)/(159,494) <-> swc (16,92)/(1,93)/(160,495)) while the
// census names and counts differ entirely (90/122/116/74 vs oxc 92/99/118/62), which is the point.
// Four sources are parsed:
//   S1 ES script (parse_script): functions/recursion, regex literals, template strings,
//      destructuring+rest, spread, for-of, bitwise operators;
//   S2 TS module: generic interface, enum, class implements+parameter
//      properties, satisfies, type alias, export default, never;
//   S3 JSX module: hooks destructuring, fragments, JSX inside ternaries/arrows, attributes,
//      expression containers;
//   S4 TSX with recoverable diagnostics (below): typed generic call (TSX's `<T>(`
//      disambiguation), self-closing elements, legacy octal `042`, top-level return.
// Test surface:
//   ① per-source parse meta: ok/body statement count/diagnostic count;
//   ② AST node kind census -- after re-parsing the serde-impl JSON, every "type" field is
//      counted into one BTreeMap-ordered line (JSON is only an intermediary; ctxt etc. omitted);
//   ③ typed-AST anchor spans: S1 FnDecl fib, S2 TsInterfaceDecl Shape, S3/S4 the first
//      JSXElement (descending the statement tree covers Stmt/Decl/Expr/ModuleItem discriminants);
//   ④ S4 per-diagnostic span+message. assert_eq! anchors key constants: the four node totals,
//      the anchor spans and the diagnostic count (taken from the first native oracle run).
// Error-model difference (why S4 was chosen): oxc recovers from a mismatched JSX closing
// tag, but swc fails fatally on the same input: span=27..28 "Expected corresponding JSX closing tag
// for <div>", the whole AST lost. So S4 does not copy oxc; it uses swc's recoverable class
// (the emit_err family, parse Ok + non-empty take_errors) for diagnostics: a strict-zone
// legacy octal (one literal triggers two diagnostics: the targets-ES5+ verdict "Legacy octal literals are not
// available when targeting ECMAScript 5 and higher" plus the strict-mode verdict "Legacy
// octal escape is not permitted in strict mode", same span) and a top-level return
// ("Return statement is not allowed here"), with the AST fully preserved (the census still
// reports 74 nodes and the JSX anchor still resolves).
// Determinism: each source gets a fresh SourceMap -> the first file starts at BytePos(1) and
// every span is a data-derived absolute offset (same value in all three dimensions);
// EsVersion::latest() is a parser built-in constant (pinned with the version, no environment
// dependency); diagnostics bypass the emitter; BTreeMap census ordering; no IO/time/random/threads; stderr empty.
// FRONTIER: none (the stacker cut is a budget trim, not a confirmed engine block).
//
// Three dimensions:
//   A: target/release/mirvm run tests/scripts/c_swc_parse.rs
//   B: cd "$(grep -l 'name = "c_swc_parse"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_swc_parse.rs
use std::collections::BTreeMap;
use std::fmt::Write as _;

use swc_common::sync::Lrc;
use swc_common::{FileName, FilePathMapping, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::lexer::Lexer;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};

// ① ES script (parse_script, not a module). Byte-for-byte the same text as c_oxc_parse S1.
const SRC_ES: &str = r#"var total = 0;
function fib(n) {
  if (n < 2) return n;
  return fib(n - 1) + fib(n - 2);
}
const re = /ab+c/gi;
const msg = `fib(10)=${fib(10)} re=${re.test("cABba")}`;
const { a = 1, ...rest } = { a: 2, b: 3 };
let arr = [1, 2, ...[3, 4]];
for (const x of arr) { total += x; }
total = total ^ (total >> 1);
"#;

// ② TS module. Byte-for-byte the same text as c_oxc_parse S2.
const SRC_TS: &str = r#"interface Shape<T extends object = object> {
  kind: string;
  area(): number;
  meta?: T;
}
enum Color { Red = 1, Green = 2, Blue = 4 }
class Circle<T> implements Shape<T> {
  constructor(public r: number, readonly kind = "circle") { super(); }
  area(): number { return Math.PI * this.r ** 2; }
}
const c = new Circle<{ tag: string }>(2);
const k = "area" satisfies keyof Shape;
type Pair<A, B> = readonly [A, B];
export default c;
function assertNever(x: never): never { throw new Error("bad"); }
"#;

// ③ JSX module. Byte-for-byte the same text as c_oxc_parse S3.
const SRC_JSX: &str = r#"import { useState } from "react";
const items = ["a", "b", "c"];
export function List({ title, onPick }) {
  const [sel, setSel] = useState(0);
  return (
    <section className="list" data-count={items.length}>
      <h1>{title ?? "untitled"}</h1>
      <>
        {items.map((it, i) =>
          i === sel ? <b key={it}>{it}</b> : (
          <button key={it} disabled={false} onClick={() => onPick(it)}>
            pick {it} #{i + 1}
          </button>
        ))}
      </>
    </section>
  );
}
"#;

// ④ TSX with recoverable diagnostics (the parser.take_errors family): a strict-zone legacy octal
// (two diagnostics on the same span) plus a top-level return, with the AST fully preserved.
// JSX closing mismatches are fatal in swc (see the header), so oxc's S4 is not reused.
const SRC_TSX_BAD: &str = r#"export const App = (props: { name: string }) => {
  const [count, setCount] = useCount<number>(0);
  return (
    <div className="app">
      <p>hello {props.name}</p>
      <Counter value={count} step={1} />
    </div>
  );
};
export const X = <span>{count}</span>;
const legacy = 042;
return;
"#;

/// Walks the JSON value tree and counts objects with a "type" string field = AST node kinds.
fn walk(v: &serde_json::Value, map: &mut BTreeMap<String, u64>, total: &mut u64) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(t)) = m.get("type") {
                *map.entry(t.clone()).or_insert(0) += 1;
                *total += 1;
            }
            for x in m.values() {
                walk(x, map, total);
            }
        }
        serde_json::Value::Array(a) => {
            for x in a {
                walk(x, map, total);
            }
        }
        _ => {}
    }
}

/// Returns (total node count, one BTree-ordered `kind:count|...` line).
fn census(json: &str) -> (u64, String) {
    let v: serde_json::Value = serde_json::from_str(json).unwrap();
    let mut map: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    walk(&v, &mut map, &mut total);
    let mut line = String::new();
    for (k, n) in &map {
        if !line.is_empty() {
            line.push('|');
        }
        write!(line, "{k}:{n}").unwrap();
    }
    (total, line)
}

/// Parses one source, prints meta + census + all diagnostics; returns (node total, diagnostic count).
fn report(label: &str, file: &str, src: &str, syntax: Syntax, is_module: bool) -> (u64, usize) {
    let cm = Lrc::new(SourceMap::new(FilePathMapping::empty()));
    let fm = cm.new_source_file(Lrc::new(FileName::Custom(file.into())), src.to_string());
    let lexer = Lexer::new(syntax, EsVersion::latest(), StringInput::from(&*fm), None);
    let mut p = Parser::new_from(lexer);
    let outcome = if is_module {
        p.parse_module()
            .map(|m| (serde_json::to_string(&m).unwrap(), m.body.len()))
    } else {
        p.parse_script()
            .map(|s| (serde_json::to_string(&s).unwrap(), s.body.len()))
    };
    let errs = p.take_errors();
    println!("== {label} bytes={}", src.len());
    match outcome {
        Ok((json, body_len)) => {
            println!("ok=true body={} diagnostics={}", body_len, errs.len());
            let (total, kinds) = census(&json);
            println!("census[{total} nodes] {kinds}");
            for (i, e) in errs.iter().enumerate() {
                use swc_common::Spanned;
                println!(
                    "diag[{i}] span={}..{} msg={}",
                    e.span().lo.0,
                    e.span().hi.0,
                    e.kind().msg()
                );
            }
            (total, errs.len())
        }
        Err(e) => {
            use swc_common::Spanned;
            println!(
                "ok=false err span={}..{} msg={}",
                e.span().lo.0,
                e.span().hi.0,
                e.kind().msg()
            );
            (0, errs.len())
        }
    }
}

/// Finds the first JSXElement in an expression tree (covering Paren/Cond/Arrow discriminant forms).
fn jsx_of_expr<'a>(e: &'a Expr) -> Option<&'a JSXElement> {
    match e {
        Expr::JSXElement(el) => Some(el),
        Expr::Paren(p) => jsx_of_expr(&p.expr),
        Expr::Cond(c) => jsx_of_expr(&c.cons).or_else(|| jsx_of_expr(&c.alt)),
        Expr::Arrow(a) => match &*a.body {
            BlockStmtOrExpr::Expr(x) => jsx_of_expr(x),
            BlockStmtOrExpr::BlockStmt(b) => b.stmts.iter().find_map(jsx_of_stmt),
        },
        _ => None,
    }
}

fn jsx_of_decl(d: &Decl) -> Option<&JSXElement> {
    match d {
        Decl::Fn(f) => f
            .function
            .body
            .as_ref()
            .and_then(|b| b.stmts.iter().find_map(jsx_of_stmt)),
        Decl::Var(v) => v
            .decls
            .iter()
            .find_map(|vd| vd.init.as_deref().and_then(jsx_of_expr)),
        _ => None,
    }
}

fn jsx_of_stmt(s: &Stmt) -> Option<&JSXElement> {
    match s {
        Stmt::Expr(e) => jsx_of_expr(&e.expr),
        Stmt::Return(r) => r.arg.as_deref().and_then(jsx_of_expr),
        Stmt::Decl(d) => jsx_of_decl(d),
        _ => None,
    }
}

fn jsx_of_module_item(mi: &ModuleItem) -> Option<&JSXElement> {
    match mi {
        ModuleItem::Stmt(s) => jsx_of_stmt(s),
        ModuleItem::ModuleDecl(d) => match d {
            ModuleDecl::ExportDecl(e) => jsx_of_decl(&e.decl),
            ModuleDecl::ExportDefaultDecl(e) => match &e.decl {
                DefaultDecl::Fn(f) => f
                    .function
                    .body
                    .as_ref()
                    .and_then(|b| b.stmts.iter().find_map(jsx_of_stmt)),
                _ => None,
            },
            ModuleDecl::ExportDefaultExpr(e) => jsx_of_expr(&e.expr),
            _ => None,
        },
    }
}

fn parse_module(file: &str, src: &str, syntax: Syntax) -> Module {
    let cm = Lrc::new(SourceMap::new(FilePathMapping::empty()));
    let fm = cm.new_source_file(Lrc::new(FileName::Custom(file.into())), src.to_string());
    let lexer = Lexer::new(syntax, EsVersion::latest(), StringInput::from(&*fm), None);
    let mut p = Parser::new_from(lexer);
    p.parse_module().unwrap()
}

fn jsx_name(el: &JSXElement) -> &str {
    match &el.opening.name {
        JSXElementName::Ident(id) => id.sym.as_str(),
        _ => "<non-ident>",
    }
}

fn main() {
    println!("swc_ecma_parser 41.1.2 JS/TS/JSX differential");

    // ---- ① ES script ----
    let (n1, d1) = report(
        "S1 es-script",
        "s1.js",
        SRC_ES,
        Syntax::Es(EsSyntax::default()),
        false,
    );
    {
        let cm = Lrc::new(SourceMap::new(FilePathMapping::empty()));
        let fm = cm.new_source_file(
            Lrc::new(FileName::Custom("s1.js".into())),
            SRC_ES.to_string(),
        );
        let lexer = Lexer::new(
            Syntax::Es(EsSyntax::default()),
            EsVersion::latest(),
            StringInput::from(&*fm),
            None,
        );
        let mut p = Parser::new_from(lexer);
        let script = p.parse_script().unwrap();
        let f = script
            .body
            .iter()
            .find_map(|s| match s {
                Stmt::Decl(Decl::Fn(f)) => Some(f),
                _ => None,
            })
            .unwrap();
        let sp = f.function.span;
        println!(
            "anchor S1 FnDecl span={}..{} name={:?} params={}",
            sp.lo.0,
            sp.hi.0,
            f.ident.sym.as_str(),
            f.function.params.len()
        );
        assert_eq!((sp.lo.0, sp.hi.0), (16, 92), "S1 fib span");
        assert_eq!(f.ident.sym.as_str(), "fib");
        assert_eq!(f.function.params.len(), 1);
    }
    assert_eq!(n1, 90, "S1 node total");
    assert_eq!(d1, 0);

    // ---- ② TS module ----
    let (n2, d2) = report(
        "S2 ts-module",
        "s2.ts",
        SRC_TS,
        Syntax::Typescript(TsSyntax::default()),
        true,
    );
    {
        let m = parse_module("s2.ts", SRC_TS, Syntax::Typescript(TsSyntax::default()));
        let i = m
            .body
            .iter()
            .find_map(|mi| match mi {
                ModuleItem::Stmt(Stmt::Decl(Decl::TsInterface(i))) => Some(i),
                _ => None,
            })
            .unwrap();
        let sp = i.span;
        println!(
            "anchor S2 TsInterfaceDecl span={}..{} name={:?} body={}",
            sp.lo.0,
            sp.hi.0,
            i.id.sym.as_str(),
            i.body.body.len()
        );
        assert_eq!((sp.lo.0, sp.hi.0), (1, 93), "S2 interface span");
        assert_eq!(i.id.sym.as_str(), "Shape");
        assert_eq!(i.body.body.len(), 3);
    }
    assert_eq!(n2, 122, "S2 node total");
    assert_eq!(d2, 0);

    // ---- ③ JSX module ----
    let (n3, d3) = report(
        "S3 jsx-module",
        "s3.jsx",
        SRC_JSX,
        Syntax::Es(EsSyntax {
            jsx: true,
            ..Default::default()
        }),
        true,
    );
    {
        let m = parse_module(
            "s3.jsx",
            SRC_JSX,
            Syntax::Es(EsSyntax {
                jsx: true,
                ..Default::default()
            }),
        );
        let el = m.body.iter().find_map(jsx_of_module_item).unwrap();
        let sp = el.span;
        println!(
            "anchor S3 JSXElement span={}..{} name={:?} children={}",
            sp.lo.0,
            sp.hi.0,
            jsx_name(el),
            el.children.len()
        );
        assert_eq!((sp.lo.0, sp.hi.0), (160, 495), "S3 jsx span");
        assert_eq!(jsx_name(el), "section");
        assert_eq!(el.children.len(), 5);
    }
    assert_eq!(n3, 116, "S3 node total");
    assert_eq!(d3, 0);

    // ---- ④ TSX recoverable diagnostics ----
    let (n4, d4) = report(
        "S4 tsx-recoverable",
        "s4.tsx",
        SRC_TSX_BAD,
        Syntax::Typescript(TsSyntax {
            tsx: true,
            ..Default::default()
        }),
        true,
    );
    {
        let m = parse_module(
            "s4.tsx",
            SRC_TSX_BAD,
            Syntax::Typescript(TsSyntax {
                tsx: true,
                ..Default::default()
            }),
        );
        let el = m.body.iter().find_map(jsx_of_module_item).unwrap();
        let sp = el.span;
        println!(
            "anchor S4 JSXElement span={}..{} name={:?} children={}",
            sp.lo.0,
            sp.hi.0,
            jsx_name(el),
            el.children.len()
        );
        assert_eq!((sp.lo.0, sp.hi.0), (115, 220), "S4 jsx span");
        assert_eq!(jsx_name(el), "div");
        assert_eq!(el.children.len(), 5);
        println!("recover ok = true");
    }
    assert_eq!(n4, 74, "S4 node total");
    assert_eq!(d4, 3, "S4 octal double diagnostic + top-level return");

    println!("assert anchors OK");
}
