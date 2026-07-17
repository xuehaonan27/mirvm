#!/usr/bin/env mirvm
---
[dependencies]
# oxc_parser 0.140.0（2026-07-17 时点最新；oxc 全线 crate 同号发布，一并钉
# =0.140.0）。default features 仅 ["regular_expression"]（正则文本走
# oxc_regular_expression 验证），oxc_parser 本体无 napi/wasm 可选件可裁。
# 附带 oxc_ast/oxc_estree 的 serialize feature：Program ESTree JSON 序列化，
# 供 kind census 使用（serializer 为结构驱动、字段序固定，Determinism 安全）。
# serde_json 沿用 corpus 既有 "1" 钉法（批1 已绿）。依赖闭包 62 crates
# （num-bigint 0.5 在列但只走 BigInt 词法解析不经 div_wide asm；miette 实收
# 为 oxc-miette fork；无一 C/FFI 件），远低于 150 上限。
oxc_parser = "=0.140.0"
oxc_allocator = "=0.140.0"
oxc_span = "=0.140.0"
oxc_diagnostics = "=0.140.0"
oxc_ast = { version = "=0.140.0", features = ["serialize"] }
oxc_estree = { version = "=0.140.0", features = ["serialize"] }
serde_json = "1"
---
// oxc_parser 0.140（oxc JS/TS 编译工具链，Arena AST + u32 span）三维差分。
// 批7 波2 大物槽：解析 4 个源 ——
//   S1 ES 脚本（ModuleKind::Script）：函数/递归、regex literal
//      （ParseOptions.parse_regular_expression=true，过 oxc_regular_expression）、
//      模板串、destructuring+rest、spread、for-of、位运算；
//   S2 TS（unambiguous→module）：generic interface、enum、class implements+
//      parameter properties、satisfies、type alias、export default、never；
//   S3 JSX：hooks 解构、fragment、三元/箭头内 JSX、attributes、表达式容器；
//   S4 TSX 含语法错误：JSX 闭合标签错配（可恢复，2 label 含打开点）+ 顶层
//      return（TS 1108，带 code scope/number）——panicked=false、AST 完整。
// 测试面：
//   ① 每源解析 meta：panicked/diagnostics 数/body 语句数/module 判定；
//   ② AST 节点 kind census——Program 经 CompactSerializer(include_ts_fields=
//      false, ranges=true) 出 ESTree JSON，serde_json 重解析后按 "type" 字段
//      全量计数进 BTreeMap 定序单行打印；
//   ③ typed AST 锚点 span：S1 FunctionDeclaration fib、S2 TSInterfaceDeclaration
//      Shape、S3 首个 JSXElement（语句树手写下潜，覆盖 Statement/Declaration/
//      Expression 枚举判别样态）；
//   ④ S4 逐条诊断打印 severity/code/message/label span+文本/help。
//   assert_eq! 锚定 4 源节点总数、锚点 span、诊断计数等关键常量。
// 确定，无 IO/时间/随机；census gather 走 BTreeMap；stderr 真空。
// FRONTIER：无。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_oxc_parse.rs
//   B: cd "$(grep -l 'name = "c_oxc_parse"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_oxc_parse.rs
use std::collections::BTreeMap;
use std::fmt::Write as _;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Expression, JSXElement, Program, Statement};
use oxc_diagnostics::Severity;
use oxc_estree::{CompactSerializer, ESTree};
use oxc_parser::{ParseOptions, Parser, ParserReturn};
use oxc_span::{GetSpan, SourceType};
use serde_json::Value;

// ① ES 脚本（非 module）。
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

// ② TS（unambiguous→module）。
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

// ③ JSX（module）。
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

// ④ TSX 含语法错误：闭合标签错配 + 顶层 return，均可恢复 → AST 保留。
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

/// ESTree JSON：include_ts_fields=false（标准 ESTree 字段集）、ranges=true（
/// 节点带 start/end）。仅作 census 中间物，不直接打印。
fn estree_json(program: &Program<'_>) -> String {
    let mut ser = CompactSerializer::new(false, true);
    program.serialize(&mut ser);
    ser.into_string()
}

/// 遍历 JSON 值树，统计带 "type" 字符串字段的对象 = AST 节点 kind 计数。
/// 返回 (总节点数, BTree 定序的 `kind:count|...` 单行)。
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

/// 表达式树里找第一个 JSXElement（覆盖 JSX/括号/三元/箭头形态枚举判别）。
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

/// 语句树里找第一个 JSXElement（typed AST 下潜，覆盖 Statement/Declaration
/// 枚举判别样态）。
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

/// 解析单个源并打印 meta + census + 全量诊断；返回 (总节点数, 诊断数, panicked)。
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

    // ---- ① ES 脚本 ----
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

    // ---- ② TS ----
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

    // ---- ③ JSX ----
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

    // ---- ④ TSX 语法错误（可恢复）----
    let (n4, d4, p4) = report("S4 tsx-bad", SRC_TSX_BAD, SourceType::tsx());
    println!("recover ok = {}", !p4);
    assert_eq!(n4, 62, "S4 node total");
    assert_eq!(d4, 2);
    assert!(!p4);

    println!("assert anchors OK");
}
