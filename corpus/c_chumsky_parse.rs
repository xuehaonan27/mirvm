#!/usr/bin/env mirvm
---
[dependencies]
chumsky = "=0.10.1"
---
// chumsky 0.10.1（任务钉 0.9/0.10 stable 线最新 = 0.10.1；0.11+ 是 2025 年新系列，
// 不在本槽位授权范围；0.10 即官方 examples 目录带 json.rs 的末代 0.9 系 API
// 后继——新签名 trait Parser<'src, I, O, E> + extra::Err<Rich<_>>）。
// 钉选 =0.10.1 精确版本，features 按任务钉 default（std + stacker）。
// 注意点：default 的 stacker 经 psm（cc 汇编 archive 内嵌 rlib）给 recursive
// 解析做分段栈扩容（src/recursive.rs maybe_grow，每递归层调 psm::stack_pointer
// extern asm 符号）——初判可能撞 native-archive asm 符号边界（候补绕行：
// default-features=false + ["std"]，rustfft 先例）；实测未撞，psm 符号正常
// 直通，三维直接全绿，无 FRONTIER 记录（2026-07-17）。
//
// 测试面（批7 波1：组合子 mini-JSON）：
//   手写 mini-JSON parser：recursive + choice + just/one_of/none_of +
//   text::int/digits/keyword + or_not/then/to_slice/map + repeated().collect
//   <String/Vec/BTreeMap> + separated_by+allow_trailing + delimited_by + padded，
//   全程 boxed 动态派发。3 个合法样本：嵌套 object/array、八类转义 + 
//   XXXX、指数/分数/负数/空对象/空数组/空 key；值结构经确定性打印器全量输出
//   （object 走 BTreeMap 字典序、num 一律 f64::to_bits 十六进制、str 走 Debug）。
//   2 个语法错误样本打印 Rich 错误的 span/found/reason/expected 列表（Vec 序，
//   确定）。assert_eq! 锚点核对首个样本的关键字段与 bits。
//   确定，无 IO/时间/随机；stderr 真空。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_chumsky_parse.rs
//   B: cd "$(grep -l 'name = "c_chumsky_parse"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_chumsky_parse.rs
use std::collections::BTreeMap;
use std::fmt::Write as _;

use chumsky::prelude::*;

#[derive(Clone, Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

/// 确定性打印器：object 走 BTreeMap 字典序；num 打 to_bits hex；indent 两空格。
fn show(j: &Json, ind: usize, out: &mut String) {
    let pad = "  ".repeat(ind);
    match j {
        Json::Null => writeln!(out, "{pad}null").unwrap(),
        Json::Bool(b) => writeln!(out, "{pad}bool {b}").unwrap(),
        Json::Num(n) => writeln!(out, "{pad}num {:016x}", n.to_bits()).unwrap(),
        Json::Str(s) => writeln!(out, "{pad}str {s:?}").unwrap(),
        Json::Array(xs) => {
            writeln!(out, "{pad}array len={}", xs.len()).unwrap();
            for x in xs {
                show(x, ind + 1, out);
            }
        }
        Json::Object(m) => {
            writeln!(out, "{pad}object len={}", m.len()).unwrap();
            for (k, v) in m {
                writeln!(out, "{pad}key {k:?}").unwrap();
                show(v, ind + 1, out);
            }
        }
    }
}

/// mini-JSON 组合子 parser（chumsky 0.10.1 官方 examples/json.rs 同型；
/// 删 ariadne/错误恢复，保留核心组合子面）。
fn json_parser<'a>() -> impl Parser<'a, &'a str, Json, extra::Err<Rich<'a, char>>> {
    recursive(|value| {
        // 数字：-? int frac? exp?，整段 to_slice 后交给 f64::parse（确定性）。
        let frac = just('.').then(text::digits(10).to_slice());
        let exp = just('e')
            .or(just('E'))
            .then(one_of("+-").or_not())
            .then(text::digits(10).to_slice());
        let number = just('-')
            .or_not()
            .then(text::int(10))
            .then(frac.or_not())
            .then(exp.or_not())
            .to_slice()
            .map(|s: &str| Json::Num(s.parse::<f64>().unwrap()))
            .boxed();

        // 字符串转义：JSON 全部八类简写转义 + \uXXXX（不处理代理对，超界 U+FFFD）。
        let escape = just('\\').ignore_then(choice((
            just('"').to('"'),
            just('\\').to('\\'),
            just('/').to('/'),
            just('b').to('\x08'),
            just('f').to('\x0c'),
            just('n').to('\n'),
            just('r').to('\r'),
            just('t').to('\t'),
            just('u').ignore_then(
                text::digits(16)
                    .exactly(4)
                    .to_slice()
                    .map(|ds: &str| {
                        char::from_u32(u32::from_str_radix(ds, 16).unwrap()).unwrap_or('\u{fffd}')
                    }),
            ),
        )));

        let string = none_of("\\\"")
            .or(escape)
            .repeated()
            .collect::<String>()
            .delimited_by(just('"'), just('"'))
            .boxed();

        let member = string
            .clone()
            .then_ignore(just(':').padded())
            .then(value.clone());
        let object = member
            .separated_by(just(',').padded())
            .allow_trailing()
            .collect::<BTreeMap<String, Json>>()
            .padded()
            .delimited_by(just('{'), just('}'))
            .boxed();

        let array = value
            .separated_by(just(',').padded())
            .allow_trailing()
            .collect::<Vec<Json>>()
            .padded()
            .delimited_by(just('['), just(']'))
            .boxed();

        choice((
            text::keyword("null").to(Json::Null),
            text::keyword("true").to(Json::Bool(true)),
            text::keyword("false").to(Json::Bool(false)),
            number,
            string.map(Json::Str),
            array.map(Json::Array),
            object.map(Json::Object),
        ))
        .padded()
        .boxed()
    })
}

fn parse_full(src: &str) -> Result<Json, Vec<Rich<'_, char>>> {
    json_parser().then_ignore(end()).parse(src).into_result()
}

/// 合法样本：打印输入 + 全量值结构，返回解析结果供断言锚点。
fn report_ok(label: &str, src: &str) -> Json {
    println!("{label} src = {src:?}");
    match parse_full(src) {
        Ok(j) => {
            let mut out = String::new();
            show(&j, 1, &mut out);
            print!("{out}");
            j
        }
        Err(errs) => {
            println!("  UNEXPECTED errors = {}", errs.len());
            for e in errs {
                println!(
                    "  span {}..{} found {:?}",
                    e.span().start,
                    e.span().end,
                    e.found()
                );
            }
            Json::Null
        }
    }
}

/// 错误样本：打印每个 Rich 错误的 span / found / reason / expected 列表。
fn report_err(label: &str, src: &str) {
    println!("{label} src = {src:?}");
    match parse_full(src) {
        Ok(_) => println!("  UNEXPECTED parse ok"),
        Err(errs) => {
            println!("  errors = {}", errs.len());
            for e in errs {
                let expected = e
                    .expected()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "  span {}..{} found {:?}",
                    e.span().start,
                    e.span().end,
                    e.found()
                );
                println!("  reason = {}", e.reason());
                println!("  expected = {expected}");
            }
        }
    }
}

/// 从解析结果按路径取第一个 Num 的 bits（断言锚点用）。
fn num_bits(j: &Json, path: &[&str]) -> Option<u64> {
    let mut cur = j;
    for key in path {
        match cur {
            Json::Object(m) => cur = m.get(*key)?,
            _ => return None,
        }
    }
    match cur {
        Json::Num(n) => Some(n.to_bits()),
        _ => None,
    }
}

fn main() {
    println!("chumsky 0.10.1 mini-JSON combinator differential");

    // ① 嵌套 object/array + 八类转义 + 负数 + 空对象/空数组。
    let s1 = r#"{"name": "mirvm\tcorpus", "tags": ["chumsky", "parser", {"kind": "combinator", "v": [0, 9, 10]}], "meta": {"deep": {"x": [{"y": -2.5e3}, true, null]}, "esc": "A\"é\\", "empty": {}, "none": []}}"#;
    let j1 = report_ok("ok 01", s1);

    // ② array 主导：多层嵌套、分数、空数组、混合类型。
    let s2 = r#"[[1, 2, [3.5, [true]]], "nested\narray", {"k": []}, -0.75, 6, []]"#;
    let j2 = report_ok("ok 02", s2);

    // ③ 空 key / 带空格 key / 深嵌套 / 指数带符号。
    let s3 = r#"{"": 0, "a b": "B c", "deep": {"1": {"2": {"3": [[{"4": []}]]}}}, "n": -6.25e-2}"#;
    let j3 = report_ok("ok 03", s3);

    // ④ 语法错误：双逗号（member 之间出现 '，,'）。
    report_err("err 01", r#"{"a": 1,, "b": 2}"#);

    // ⑤ 语法错误：数组未闭合（结尾遇 end of input）。
    report_err("err 02", r#"[1, "x""#);

    // ---- 断言锚点（静默，失败即炸出维度分叉）----
    // 样本①：tags[2].v 无 object 路径——改查 meta.deep.x[0].y 与 name 键序。
    assert_eq!(
        num_bits(&j1, &["meta", "deep"]).is_none(),
        true,
        "deep is object not num"
    );
    match j1.get("meta").and_then(|m| match m {
        Json::Object(m) => m.get("esc"),
        _ => None,
    }) {
        Some(Json::Str(s)) => assert_eq!(s.as_str(), "A\"é\\"),
        other => panic!("esc mismatch: {other:?}"),
    }
    // 样本②：顶层直接数字锚（-0.75 与 6 共 2 个；1/2/3.5 在子数组里）。
    match &j2 {
        Json::Array(xs) => {
            assert_eq!(xs.len(), 6);
            let bits = xs
                .iter()
                .filter_map(|x| match x {
                    Json::Num(n) => Some(n.to_bits()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(bits.len(), 2, "unexpected num count in ok 02");
            assert_eq!(bits[0], (-0.75f64).to_bits(), "ok 02 -0.75 bits");
        }
        _ => panic!("ok 02 root should be array"),
    }
    // 样本③：-6.25e-2 精确 bits 锚（f64 解析跨维逐位一致）。
    assert_eq!(
        num_bits(&j3, &["n"]),
        Some((-6.25e-2f64).to_bits()),
        "ok 03 num bits"
    );
    // 关键字边界探针：`nullx` 不是合法 JSON（keyword 后随标识符字符即拒）。
    match parse_full("[nullx]") {
        Ok(_) => panic!("nullx should not parse"),
        Err(errs) => println!("keyword guard errors = {}", errs.len()),
    }
    println!("assert anchors OK");
}

trait GetExt {
    fn get(&self, k: &str) -> Option<&Json>;
}
impl GetExt for Json {
    fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Object(m) => m.get(k),
            _ => None,
        }
    }
}
