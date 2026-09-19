#!/usr/bin/env mirvm
---
[dependencies]
# Pin oxc_parser =0.140.0 (newest as of 2026-07-17; the whole oxc crate line
# is released under one version, so all oxc crates are pinned together).
# default features are only ["regular_expression"]; no napi/wasm part exists.
# serialize on oxc_ast/oxc_estree yields Program ESTree JSON, structure-driven
# with a fixed field order, so it is determinism-safe.
# serde_json keeps the corpus-wide "1" pin. The closure is 62 crates with no
# C/FFI member, and num-bigint 0.5 only lexes BigInt, never div_wide asm.
# Pinning keeps the AST shape and node counts this fixture asserts on.
oxc_parser = "=0.140.0"
oxc_allocator = "=0.140.0"
oxc_span = "=0.140.0"
oxc_diagnostics = "=0.140.0"
oxc_ast = { version = "=0.140.0", features = ["serialize"] }
oxc_estree = { version = "=0.140.0", features = ["serialize"] }
serde_json = "1"
---
// oxc_parser 0.140 (the oxc JS/TS toolchain, arena AST with u32 spans)
// differential over four sources:
//   S1 ES script (ModuleKind::Script): functions and recursion, a regex literal
//      (parse_regular_expression=true, so oxc_regular_expression handles it),
//      template string, destructuring with rest, spread, for-of, bitwise ops;
//   S2 TS (unambiguous, resolves to a module): a generic interface, an enum, a
//      class with implements and parameter properties, satisfies, type alias,
//      export default, never;
//   S3 JSX: hooks destructuring, a fragment, JSX in ternaries and arrows,
//      attributes, expression containers;
//   S4 TSX with syntax errors: a mismatched JSX closing tag (recoverable, 2
//      labels including the open point) and a top-level return (TS 1108, with a
//      code scope and number); panicked=false and the AST stays complete.
// The oracle checks:
//   1. per-source meta: panicked, diagnostic count, body statement count, module;
//   2. an AST node-kind census: the Program is serialized to ESTree JSON with
//      CompactSerializer(include_ts_fields=false, ranges=true), reparsed by
//      serde_json, and every object with a "type" field is counted into a
//      BTreeMap printed as one line in key order;
//   3. typed-AST anchor spans: S1's FunctionDeclaration fib, S2's interface
//      declaration Shape, S3's first JSXElement, found by walking the statement
//      tree by hand across the Statement/Declaration/Expression variants;
//   4. S4's diagnostics, each printed with severity, code, message, label span
//      and text, and help.
//   assert_eq! pins the four node totals, the anchor spans and the diagnostics.
// Deterministic, with no IO, time or randomness; the census uses a BTreeMap
// and stderr stays empty.
// The three-way re-run:
//   A: target/release/mirvm run tests/scripts/c_oxc_parse.rs
use std::collections::BTreeMap;
use std::fmt::Write as _;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Expression, JSXElement, Program, Statement};
use oxc_diagnostics::Severity;
use oxc_estree::{CompactSerializer, ESTree};
use oxc_parser::{ParseOptions, Parser, ParserReturn};
use oxc_span::{GetSpan, SourceType};
use serde_json::Value;

// (1) ES script (not a module).
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

// (2) TS (unambiguous, resolves to a module).
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

// (3) JSX (module).
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

// (4) TSX with syntax errors: closing-tag mismatch plus a top-level return, both recoverable.
const SRC_TSX_BAD: &str = r#"export const App = (props: { name: string }) => {
  const [count, setCount] = useCount<number>(0);
  return (
    <div className="app">
      <p>hello {props.name}</p>
      <Counter value={count} step={1} />
    </ul>
  );
};
export const X = <span>{count}</span>;
return;
"#;

/// ESTree JSON: include_ts_fields=false (the standard ESTree field set) and
/// ranges=true (nodes carry start/end). A census intermediate, never printed.
fn estree_json(program: &Program<'_>) -> String {
    let mut ser = CompactSerializer::new(false, true);
    program.serialize(&mut ser);
    ser.into_string()
}

/// Walk the JSON value tree and count every object with a "type" string field,
/// i.e. the AST node kinds. Returns (total, BTree-ordered `kind:count|...`).
fn census(json: &str) -> (u64, String) {
    let v: Value = serde_json::from_str(json).unwrap();
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

fn walk(v: &Value, map: &mut BTreeMap<String, u64>, total: &mut u64) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(t)) = m.get("type") {
                *map.entry(t.clone()).or_insert(0) += 1;
                *total += 1;
            }
            for x in m.values() {
                walk(x, map, total);
            }
        }
        Value::Array(a) => {
            for x in a {
                walk(x, map, total);
            }
        }
        _ => {}
    }
}

/// Find the first JSXElement in an expression tree, covering the JSX,
fn jsx_of_expr<'b, 'a>(e: &'b Expression<'a>) -> Option<&'b JSXElement<'a>> {
    match e {
        Expression::JSXElement(el) => Some(el),
        Expression::ParenthesizedExpression(p) => jsx_of_expr(&p.expression),
        Expression::ConditionalExpression(c) => {
            jsx_of_expr(&c.consequent).or_else(|| jsx_of_expr(&c.alternate))
        }
        Expression::ArrowFunctionExpression(a) => {
            if a.expression {
                a.body.statements.first().and_then(|s| match s {
                    Statement::ExpressionStatement(es) => jsx_of_expr(&es.expression),
                    _ => None,
                })
            } else {
                a.body.statements.iter().find_map(first_jsx)
            }
        }
        _ => None,
    }
}

/// Find the first JSXElement in a statement tree by descending the typed AST,
/// covering the Statement and Declaration enum variants.
fn first_jsx<'b, 'a>(s: &'b Statement<'a>) -> Option<&'b JSXElement<'a>> {
    match s {
        Statement::ExpressionStatement(e) => jsx_of_expr(&e.expression),
        Statement::ReturnStatement(r) => r.argument.as_ref().and_then(jsx_of_expr),
        Statement::FunctionDeclaration(f) => {
            f.body.as_ref().and_then(|b| b.statements.iter().find_map(first_jsx))
        }
        Statement::VariableDeclaration(v) => v
            .declarations
            .iter()
            .find_map(|vd| vd.init.as_ref().and_then(jsx_of_expr)),
        Statement::ExportNamedDeclaration(d) => d.declaration.as_ref().and_then(|decl| match decl {
            Declaration::FunctionDeclaration(f) => {
                f.body.as_ref().and_then(|b| b.statements.iter().find_map(first_jsx))
            }
            Declaration::VariableDeclaration(v) => v
                .declarations
                .iter()
                .find_map(|vd| vd.init.as_ref().and_then(jsx_of_expr)),
            _ => None,
        }),
        Statement::ExportDefaultDeclaration(d) => match &d.declaration {
            oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                f.body.as_ref().and_then(|b| b.statements.iter().find_map(first_jsx))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Parse one source, print meta + census + all diagnostics; return
fn report(label: &str, src: &str, st: SourceType) -> (u64, usize, bool) {
    let allocator = Allocator::default();
    let options = ParseOptions {
        parse_regular_expression: true,
        ..ParseOptions::default()
    };
    let ret: ParserReturn = Parser::new(&allocator, src, st)
        .with_options(options)
        .parse();
    println!("== {label} bytes={}", src.len());
    println!(
        "panicked={} diagnostics={} body={} module={}",
        ret.panicked,
        ret.diagnostics.len(),
        ret.program.body.len(),
        ret.program.source_type.is_module()
    );

    let json = estree_json(&ret.program);
    let (total, kinds) = census(&json);
    println!("census[{total} nodes] {kinds}");

    for (i, e) in ret.diagnostics.iter().enumerate() {
        println!(
            "err[{i}] severity={} code={}",
            match e.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Advice => "advice",
            },
            e.code
        );
        println!("  msg={}", e.message);
        for l in e.labels.as_slice() {
            println!(
                "  label {}..{} text={:?}",
                l.offset(),
                l.offset() + l.len(),
                l.label().unwrap_or("")
            );
        }
        if let Some(help) = &e.help {
            println!("  help={help}");
        }
    }
    (total, ret.diagnostics.len(), ret.panicked)
}

fn main() {
    println!("oxc_parser 0.140.0 JS/TS/JSX differential");

    // ---- (1) ES script ----
    let (n1, d1, p1) = report("S1 es-script", SRC_ES, SourceType::script());
    {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, SRC_ES, SourceType::script()).parse();
        let f = ret
            .program
            .body
            .iter()
            .find_map(|s| match s {
                Statement::FunctionDeclaration(f) => Some(f),
                _ => None,
            })
            .unwrap();
        let span = f.span();
        println!(
            "anchor S1 FunctionDeclaration span={}..{} name={:?} params={}",
            span.start,
            span.end,
            f.id.as_ref().map(|i| i.name.as_str()),
            f.params.items.len()
        );
        assert_eq!((span.start, span.end), (15, 91), "S1 fib span");
        assert_eq!(f.id.as_ref().map(|i| i.name.as_str()), Some("fib"));
        assert_eq!(f.params.items.len(), 1);
    }
    assert_eq!(n1, 92, "S1 node total");
    assert_eq!(d1, 0);
    assert!(!p1);

    // ---- (2) TS ----
    let (n2, d2, _p2) = report("S2 ts-module", SRC_TS, SourceType::ts());
    {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, SRC_TS, SourceType::ts()).parse();
        let i = ret
            .program
            .body
            .iter()
            .find_map(|s| match s {
                Statement::TSInterfaceDeclaration(i) => Some(i),
                _ => None,
            })
            .unwrap();
        let span = i.span();
        println!(
            "anchor S2 TSInterfaceDeclaration span={}..{} name={:?} body={}",
            span.start,
            span.end,
            i.id.name.as_str(),
            i.body.body.len()
        );
        assert_eq!((span.start, span.end), (0, 92), "S2 interface span");
        assert_eq!(i.id.name.as_str(), "Shape");
        assert_eq!(i.body.body.len(), 3);
    }
    assert_eq!(n2, 99, "S2 node total");
    assert_eq!(d2, 0);

    // ---- (3) JSX ----
    let (n3, d3, _p3) = report("S3 jsx-module", SRC_JSX, SourceType::jsx());
    {
        let allocator = Allocator::default();
        let ret = Parser::new(&allocator, SRC_JSX, SourceType::jsx()).parse();
        let el = ret.program.body.iter().find_map(first_jsx).unwrap();
        let span = el.span();
        let name = match &el.opening_element.name {
            oxc_ast::ast::JSXElementName::Identifier(id) => id.name.as_str(),
            _ => "<non-ident>",
        };
        println!(
            "anchor S3 JSXElement span={}..{} name={:?} children={}",
            span.start,
            span.end,
            name,
            el.children.len()
        );
        assert_eq!((span.start, span.end), (159, 494), "S3 jsx span");
        assert_eq!(name, "section");
        assert_eq!(el.children.len(), 5);
    }
    assert_eq!(n3, 118, "S3 node total");
    assert_eq!(d3, 0);

    // ---- (4) TSX syntax errors (recoverable) ----
    let (n4, d4, p4) = report("S4 tsx-bad", SRC_TSX_BAD, SourceType::tsx());
    println!("recover ok = {}", !p4);
    assert_eq!(n4, 62, "S4 node total");
    assert_eq!(d4, 2);
    assert!(!p4);

    println!("assert anchors OK");
}
