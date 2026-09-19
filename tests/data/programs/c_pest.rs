#!/usr/bin/env mirvm
---
[dependencies]
# pest 2.8.7 + pest_derive 2.8.7 + pest_generator 2.8.7 + pest_meta 2.8.7.
# Upstream drift: pest_derive 2.8.7 asks pest_generator for ^2.8.7 and
# pest_generator asks pest_meta for ^2.8.7, while the 2.8.8 series demands
# pest ^2.8.8. Pinning pest to 2.8.7 therefore breaks the chain at every level,
# so all four crates are pinned to the same exact version.
pest = "=2.8.7"
pest_derive = "=2.8.7"
pest_generator = "=2.8.7"
pest_meta = "=2.8.7"
---
// pest PEG parser differential, compared byte-for-byte with native.
// Two surfaces:
// (1) Two grammars coexist in one file (grammar_inline literals in separate
//     modules, so their generated Rule enums do not collide):
//     (a) a JSON subset: obj/arr/str/num/bool/null fully recursive, four levels
//         of container nesting, string escapes (\" \\), scientific-notation
//         numbers and multi-line input containing \n;
//     (b) a calculator: precedence (*/ above +-), nested parentheses, unary minus
//         (double negation, minus glued to a parenthesis) and named operator
//         rules (add/sub/mul/div/neg dispatched through as_rule, avoiding the
//         trap of unnamed literals producing no pair);
//     WHITESPACE _ silent rules are hit on both sides (a four-character JSON set
//     and spaces plus tabs for calc).
// (2) Valid input prints the flattened node-type sequence of the parse tree (a
//     preorder walk of the Pair tree, rule names joined by spaces): two JSON
//     samples and one calculator sample.
// (3) The calculator prints i64 evaluation results anchored by assert_eq!
//     (14/30/3/10, checkable by hand: integer division only, inputs chosen to
//     divide evenly and avoid division by zero).
// (4) Three invalid inputs (a doubled comma in JSON, a multi-line JSON literal
//     truncated to tru with the error past line 1, and a missing operand inside
//     calc parentheses): each prints line:col, the positives rule-name list and
//     the full Error Display (including the --> l:c anchor and caret line), with
//     the failing rule names all pinned.
// Deterministic: pure string-cursor work, no IO/randomness/time/hash order; the
// positives Vec order follows attempt order and is single-threaded; i64
// evaluation has no floating point; assertions are silent. stderr stays empty
// (module-level allow(non_camel_case_types, dead_code) suppresses the snake_case
// variant warnings from the derived enums).
//
// No frontier issues: the "c_pest" fixture is a "mirvm" frontmatter script; all cases pass, byte-for-byte.
//
//
//
//
//
use std::collections::BTreeSet;

use pest::error::{Error as PError, LineColLocation};
use pest::iterators::Pair;
use pest::Parser;

#[allow(non_camel_case_types, dead_code)]
mod json_grammar {
    use pest_derive::Parser;
    #[derive(Parser)]
    #[grammar_inline = r#"
WHITESPACE = _{ " " | "\t" | "\r" | "\n" }
json     = { SOI ~ value ~ EOI }
value    = { object | array | string | number | boolean | null }
object   = { "{" ~ (member ~ ("," ~ member)*)? ~ "}" }
member   = { string ~ ":" ~ value }
array    = { "[" ~ (value ~ ("," ~ value)*)? ~ "]" }
string   = @{ "\"" ~ ("\\" ~ ANY | (!("\"" | "\\") ~ ANY))* ~ "\"" }
number   = @{ "-"? ~ ("0" | '1'..'9' ~ '0'..'9'*) ~ ("." ~ '0'..'9'+)? ~ (("e" | "E") ~ ("+" | "-")? ~ '0'..'9'+)? }
boolean  = { "true" | "false" }
null     = { "null" }
"#]
    pub struct JsonParser;
}

#[allow(non_camel_case_types, dead_code)]
mod calc_grammar {
    use pest_derive::Parser;
    #[derive(Parser)]
    #[grammar_inline = r#"
WHITESPACE = _{ " " | "\t" }
calc   = { SOI ~ expr ~ EOI }
expr   = { term ~ ((add | sub) ~ term)* }
term   = { factor ~ ((mul | div) ~ factor)* }
factor = { neg* ~ (number | "(" ~ expr ~ ")") }
neg    = { "-" }
number = @{ '1'..'9' ~ '0'..'9'* }
add    = { "+" }
sub    = { "-" }
mul    = { "*" }
div    = { "/" }
"#]
    pub struct CalcParser;
}

use calc_grammar::{CalcParser, Rule as CR};
use json_grammar::{JsonParser, Rule as JR};

/// Preorder walk flattening the Pair tree into a rule-name sequence (generic over RuleType).
fn flatten<R: pest::RuleType>(p: Pair<'_, R>, out: &mut Vec<String>) {
    out.push(format!("{:?}", p.as_rule()));
    for c in p.into_inner() {
        flatten(c, out);
    }
}

/// Valid sample: prints the flattened node-type sequence and returns it for assertion anchors.
fn tree_line<R: pest::RuleType>(label: &str, root: Pair<'_, R>) -> Vec<String> {
    let mut names = Vec::new();
    flatten(root, &mut names);
    println!("{label}: tree {} nodes", names.len());
    println!("  {}", names.join(" "));
    names
}

/// Error report: line:col + expected rule names + the full Display (text anchors).
fn report_err<R: pest::RuleType>(label: &str, e: &PError<R>) {
    let (line, col) = match &e.line_col {
        LineColLocation::Pos(lc) => *lc,
        LineColLocation::Span(a, _) => *a,
    };
    let expected = match &e.variant {
        pest::error::ErrorVariant::ParsingError { positives, .. } => {
            // The BTreeSet copy only counts distinct names; the text keeps the original Vec order.
            let dedup: BTreeSet<_> = positives.iter().collect();
            let seq = positives
                .iter()
                .map(|r| format!("{r:?}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("{seq} (dedup {})", dedup.len())
        }
        pest::error::ErrorVariant::CustomError { .. } => "custom".to_string(),
    };
    println!("{label}: err at {line}:{col} expected [{expected}]");
    println!("{label} render ---");
    print!("{e}");
    println!();
    println!("{label} render end ---");
}

// ---- calculator evaluation (i64; division must be exact, inputs chosen so) ----
fn eval_expr(p: Pair<'_, CR>) -> i64 {
    let mut it = p.into_inner();
    let mut acc = eval_term(it.next().unwrap());
    while let Some(op) = it.next() {
        let rhs = eval_term(it.next().unwrap());
        match op.as_rule() {
            CR::add => acc += rhs,
            CR::sub => acc -= rhs,
            other => panic!("expr op {other:?}"),
        }
    }
    acc
}

fn eval_term(p: Pair<'_, CR>) -> i64 {
    let mut it = p.into_inner();
    let mut acc = eval_factor(it.next().unwrap());
    while let Some(op) = it.next() {
        let rhs = eval_factor(it.next().unwrap());
        match op.as_rule() {
            CR::mul => acc *= rhs,
            CR::div => acc /= rhs,
            other => panic!("term op {other:?}"),
        }
    }
    acc
}

fn eval_factor(p: Pair<'_, CR>) -> i64 {
    let mut sign = 1i64;
    let mut val = 0i64;
    for inner in p.into_inner() {
        match inner.as_rule() {
            CR::neg => sign = -sign,
            CR::number => val = inner.as_str().parse::<i64>().unwrap(),
            CR::expr => val = eval_expr(inner),
            other => panic!("factor {other:?}"),
        }
    }
    sign * val
}

fn eval_calc(src: &str) -> Result<i64, PError<CR>> {
    let calc = CalcParser::parse(CR::calc, src)?.next().unwrap();
    Ok(eval_expr(calc.into_inner().next().unwrap()))
}

fn main() {
    println!("pest 2.8.7 PEG differential: JSON subset + calculator");

    // ---- (1) valid JSON sample: every rule type + escapes + exponents + nesting ----
    let s1 = r#"{"name": "mirvm", "data": [1, -2.5e3, "esc\"q\\z", true, false, null, {"k": []}]}"#;
    println!("json ok 01 src = {s1}");
    let t1 = match JsonParser::parse(JR::json, s1) {
        Ok(mut ps) => tree_line("json ok 01", ps.next().unwrap()),
        Err(e) => {
            report_err("json ok 01 UNEXPECTED", &e);
            Vec::new()
        }
    };

    // ---- (2) valid multi-line JSON sample (\n whitespace, line>1 accounting) ----
    let s2 = "{\n  \"nested\": {\n    \"a\": [ true, null ]\n  },\n  \"n\": 42\n}";
    println!("json ok 02 src = {}", s2.escape_debug());
    let t2 = match JsonParser::parse(JR::json, s2) {
        Ok(mut ps) => tree_line("json ok 02", ps.next().unwrap()),
        Err(e) => {
            report_err("json ok 02 UNEXPECTED", &e);
            Vec::new()
        }
    };

    // ---- (3) calculator: precedence / parentheses / unary minus / double negation ----
    for (i, src) in [
        "2 + 3 * 4",
        "(2 + 3) * (10 - 4)",
        "-2 * -(3 + -4) - -10 / 2",
        "--(7) + 3",
    ]
    .iter()
    .enumerate()
    {
        let label = format!("calc {:02}", i + 1);
        match eval_calc(src) {
            Ok(v) => println!("{label} eval {src:?} = {v}"),
            Err(e) => report_err(&format!("{label} UNEXPECTED"), &e),
        }
    }

    // ---- (4) one flattened-order calculator sample (with named operator pairs) ----
    let c1 = CalcParser::parse(CR::calc, "2 + 3 * 4").unwrap().next().unwrap();
    let tc = tree_line("calc 01", c1);

    // ---- (5) three invalid inputs: error position + expected rule-name anchors ----
    match JsonParser::parse(JR::json, r#"{"a": 1,, "b": 2}"#) {
        Ok(_) => println!("json err 01 UNEXPECTED ok"),
        Err(e) => report_err("json err 01", &e),
    }
    match JsonParser::parse(JR::json, "{\n  \"a\": 1,\n  \"b\": tru\n}") {
        Ok(_) => println!("json err 02 UNEXPECTED ok"),
        Err(e) => report_err("json err 02", &e),
    }
    match CalcParser::parse(CR::calc, "1 + (2 * )") {
        Ok(_) => println!("calc err 01 UNEXPECTED ok"),
        Err(e) => report_err("calc err 01", &e),
    }

    // ---- assertion anchors (silent unless a divergence appears) ----
    assert_eq!(t1.first().map(String::as_str), Some("json"), "j1 root");
    assert_eq!(t1.len(), 30, "j1 node count");
    assert_eq!(t1.iter().filter(|n| n.as_str() == "value").count(), 11, "j1 values");
    assert_eq!(t2.first().map(String::as_str), Some("json"), "j2 root");
    assert!(t2.contains(&"boolean".to_string()) && t2.contains(&"null".to_string()));
    // evaluation anchors (by hand): 2+12=14; 5*6=30; -2*-(-1)-(-5)=3; 7+3=10.
    assert_eq!(eval_calc("2 + 3 * 4").unwrap(), 14);
    assert_eq!(eval_calc("(2 + 3) * (10 - 4)").unwrap(), 30);
    assert_eq!(eval_calc("-2 * -(3 + -4) - -10 / 2").unwrap(), 3);
    assert_eq!(eval_calc("--(7) + 3").unwrap(), 10);
    assert_eq!(tc.first().map(String::as_str), Some("calc"), "c1 root");
    assert!(tc.contains(&"add".to_string()) && tc.contains(&"mul".to_string()));
    println!("assert anchors OK");
}
