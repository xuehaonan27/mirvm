#!/usr/bin/env mirvm
---
[dependencies]
# swc_ecma_parser =41.1.2（2026-07-17 时点最新；swc 已转 semver 大版本流：
# parser 41.x / ast 25.x / common 23.x）。parser 41.1.2 硬性要求
# swc_common ^23.0.2、swc_ecma_ast ^25.0.0、swc_atoms ^9.0.3，故三件套按
# 兼容组钉死。default-features=false + typescript：default=["typescript",
# "stacker"]——留 typescript（S2/S4 TS/TSX 语法面所需），裁 stacker（免
# stacker→psm global_asm 的 native-archive 路径；本 driver 输入浅、无深
# 递归需求，纯预算性裁剪）；flow/verify/debug/tracing-spans 全系不开。
# swc_ecma_ast 开 serde-impl（AST 的 serde JSON 序列化，census 依据；
# rkyv/encoding/plugin 系全裁）。swc_common 23.0.2 default=[]（tty-emitter
# 即 termcolor、concurrent、parking_lot、sourcemap 均不开）——诊断走
# kind().msg() 手格式化，不经 Handler/emitter。serde_json 沿用 corpus
# 既有 "1" 钉法（批1 已绿）。依赖闭包 89 crates（num-bigint 0.4.8 在列
# ——parser 拉 ^0.4.3 供 BigInt 字面量词法解析；本 driver 无任何 BigInt
# 字面量，不经 div_wide asm；tracing 为空转 no-op；无一 C/FFI 件，全部
# build.rs 都是 Rust 版本闸），远低于 200 上限。
swc_ecma_parser = { version = "=41.1.2", default-features = false, features = ["typescript"] }
swc_ecma_ast = { version = "=25.0.0", features = ["serde-impl"] }
swc_common = "=23.0.2"
serde_json = "1"
---
// swc_ecma_parser 41.1.2（swc 手写递归下降解析器；Box 树 AST + BytePos 全局
// span + take_errors 恢复模型）三维差分。批8 波2 VM/语言机槽，与 c_oxc_parse
// 构成姊妹压强：S1/S2/S3 三个源与 c_oxc_parse 逐字节同文（同输入撞两套实现
// 迥异的 lexer/visitor/AST ——swc 无 arena、节点另套 Ts*/JSX* 命名族），实测
// 锚点 span 契合（swc BytePos 为 1 基绝对偏移：oxc(15,91)/(0,92)/(159,494)
// ↔ swc(16,92)/(1,93)/(160,495)）而 census 命名/计数全异（90/122/116/74 vs
// oxc 92/99/118/62）——正是压强意图。
// 解析 4 个源：
//   S1 ES 脚本（parse_script）：函数/递归、regex literal、模板串、
//      destructuring+rest、spread、for-of、位运算；
//   S2 TS module：generic interface、enum、class implements+parameter
//      properties、satisfies、type alias、export default、never；
//   S3 JSX module：hooks 解构、fragment、三元/箭头内 JSX、attributes、
//      表达式容器；
//   S4 TSX 含可恢复诊断（见下）：typed generic call（TSX 的 `<T>(` 消歧）、
//      自闭合元素、legacy octal `042`、顶层 return。
// 测试面：
//   ① 每源解析 meta：ok/body 语句数/diagnostics 数；
//   ② AST 节点 kind census——serde-impl JSON 重解析后按 "type" 字段全量计数
//      进 BTreeMap 定序单行（JSON 仅中间物，ctxt 等字段不打印）；
//   ③ typed AST 锚点 span：S1 FnDecl fib、S2 TsInterfaceDecl Shape、S3/S4
//      首个 JSXElement（语句树下潜覆盖 Stmt/Decl/Expr/ModuleItem 枚举判别）；
//   ④ S4 逐条诊断 span+消息。assert_eq! 锚定 4 源节点总数、锚点 span、
//      诊断计数等关键常量（常量取自 native oracle 首跑，三维同源）。
// 错误模型差异实锤（S4 选型依据）：oxc 对 JSX 闭合标签错配可恢复（保留 AST）；
// swc 同一输入致命 Err（span=27..28 "Expected corresponding JSX closing tag
// for <div>"，AST 全丢）——故 S4 不改抄 oxc，改用 swc 的可恢复类（emit_err
// 系，parse Ok + take_errors 非空）诊断：严格 zone 的 legacy octal（一条
// 字面量触发双诊断：targets-ES5+ 判定"Legacy octal literals are not
// available when targeting ECMAScript 5 and higher" + 严格模式判定"Legacy
// octal escape is not permitted in strict mode"，同 span）+ 顶层 return
// ("Return statement is not allowed here")，AST 完整保留（census 74 节点
// 照常出、JSX 锚点照常锚）。
// 确定性：每源独立 fresh SourceMap → 首文件自 BytePos(1) 起，Span 全为数据
// 派生绝对偏移（三维同源同值）；EsVersion::latest() 为 parser 内建常数（与
// 版本同钉，无环境依赖）；诊断不经
// emitter；BTreeMap census 定序；无 IO/时间/随机/线程；stderr 真空。
// FRONTIER：无（stacker 裁除为预算性裁剪，非引擎阻塞实锤）。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_swc_parse.rs
//   B: cd "$(grep -l 'name = "c_swc_parse"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_swc_parse.rs
use std::collections::BTreeMap;
use std::fmt::Write as _;

use swc_common::sync::Lrc;
use swc_common::{FileName, FilePathMapping, SourceMap};
use swc_ecma_ast::*;
use swc_ecma_parser::lexer::Lexer;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};

// ① ES 脚本（parse_script，非 module）。与 c_oxc_parse S1 逐字节同文。
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

// ② TS module。与 c_oxc_parse S2 逐字节同文。
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

// ③ JSX module。与 c_oxc_parse S3 逐字节同文。
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

// ④ TSX 含可恢复诊断（parser.take_errors 系）：严格 zone 的 legacy octal
// （双诊断同 span）+ 顶层 return，AST 完整保留。JSX 闭合错配在 swc 为致命
// 型（见头注），故不沿用 oxc S4。
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

/// 遍历 JSON 值树，统计带 "type" 字符串字段的对象 = AST 节点 kind 计数。
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

/// 返回 (总节点数, BTree 定序的 `kind:count|...` 单行)。
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

/// 解析单个源并打印 meta + census + 全量诊断；返回 (总节点数, 诊断数)。
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

/// 表达式树里找第一个 JSXElement（覆盖 Paren/Cond/Arrow 形态枚举判别）。
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

    // ---- ① ES 脚本 ----
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

    // ---- ④ TSX 可恢复诊断 ----
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
    assert_eq!(d4, 3, "S4 octal 双诊断 + 顶层 return");

    println!("assert anchors OK");
}
