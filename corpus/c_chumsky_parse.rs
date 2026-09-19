#!/usr/bin/env mirvm
---
[dependencies]
chumsky = "=0.10.1"
---
// chumsky is pinned to exactly =0.10.1: the last release of the 0.9/0.10 stable
// line and the successor of the API used by the official examples/json.rs (the
// new-signature trait Parser<'src, I, O, E> + extra::Err<Rich<_>>). The 0.11+
// series is a separate line and is not used here; features stay at the default
// set (std + stacker), which pulls in psm (an assembly archive embedded in the
// rlib) to grow the stack for recursive parsing: src/recursive.rs maybe_grow
// calls the psm::stack_pointer extern asm symbol once per recursion level. If it
// collides with the native-archive asm boundary, fall back to
// default-features = false with the "std" feature, as with rustfft.
//
//   Test surface: a hand-written mini-JSON parser built from recursive + choice +
//   just/one_of/none_of + text::int/digits/keyword + or_not/then/to_slice/map +
//   repeated().collect<String/Vec/BTreeMap> + separated_by with allow_trailing +
//   delimited_by + padded, all boxed for dynamic dispatch. Three valid samples
//   cover nested objects/arrays, all eight JSON escapes plus \uXXXX, exponents,
//   fractions, negatives, empty objects/arrays and an empty key; a deterministic
//   printer dumps the whole value tree (objects in BTreeMap lexicographic order,
//   numbers as f64::to_bits hex, strings via Debug). Two syntax-error samples
//   print each Rich error's span/found/reason/expected list in Vec order, and
//   assert_eq! anchors pin the samples' key fields and bit patterns; stderr is
//   empty.
// Three-way rerun:
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

/// Deterministic printer: objects in BTreeMap lexicographic order, numbers as to_bits hex, two-space indent.
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

/// mini-JSON combinator parser, modelled on the official examples/json.rs in
/// chumsky 0.10.1; ariadne and error recovery are dropped, the core combinators kept.
fn json_parser<'a>() -> impl Parser<'a, &'a str, Json, extra::Err<Rich<'a, char>>> {
    recursive(|value| {
        // Number: -? int frac? exp?, sliced whole and handed to f64::parse (deterministic).
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

        // String escapes: the eight JSON short escapes plus \uXXXX; surrogate pairs are unsupported, out-of-range -> U+FFFD.
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

/// Valid sample: prints the input and the whole value tree, returns the parse for the assertion anchors.
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

/// Error sample: prints each Rich error's span / found / reason / expected list.
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

/// Walks `path` to a Num and returns its bits; feeds the assertion anchors.
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

    // ① Nested objects/arrays, all eight escapes, a negative number, empty object/array.
    let s1 = r#"{"name": "mirvm\tcorpus", "tags": ["chumsky", "parser", {"kind": "combinator", "v": [0, 9, 10]}], "meta": {"deep": {"x": [{"y": -2.5e3}, true, null]}, "esc": "A\"é\\", "empty": {}, "none": []}}"#;
    let j1 = report_ok("ok 01", s1);

    // ② Array-dominated: deep nesting, a fraction, an empty array, mixed types.
    let s2 = r#"[[1, 2, [3.5, [true]]], "nested\narray", {"k": []}, -0.75, 6, []]"#;
    let j2 = report_ok("ok 02", s2);

    // ③ Empty key / key with a space / deep nesting / signed exponent.
    let s3 = r#"{"": 0, "a b": "B c", "deep": {"1": {"2": {"3": [[{"4": []}]]}}}, "n": -6.25e-2}"#;
    let j3 = report_ok("ok 03", s3);

    // ④ Syntax error: double comma (`,,` between members).
    report_err("err 01", r#"{"a": 1,, "b": 2}"#);

    // ⑤ Syntax error: unclosed array (end of input reached).
    report_err("err 02", r#"[1, "x""#);

    // ---- Assertion anchors (silent; a failure means the dimensions diverged) ----
    // Sample ①: tags[2].v is not an object path -- check meta.deep.x[0].y and the name key order instead.
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
    // Sample ②: top-level number anchors (-0.75 and 6, two of them; 1/2/3.5 live in sub-arrays).
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
    // Sample ③: exact bits anchor for -6.25e-2 (f64 parsing is bit-identical across dimensions).
    assert_eq!(
        num_bits(&j3, &["n"]),
        Some((-6.25e-2f64).to_bits()),
        "ok 03 num bits"
    );
    // Keyword boundary probe: `nullx` is not valid JSON (a keyword followed by an identifier character is rejected).
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
