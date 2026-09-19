#!/usr/bin/env mirvm
---
[dependencies]
nom = "7"
---
// nom 7 combinator arithmetic parser: parentheses / precedence / unary minus / whitespace.
// Exercises bytes::complete::tag, character::complete::{char,digit1,multispace0},
// multi::{many0,many1}, branch::alt, combinator::{map,map_res,opt,recognize},
// sequence::{pair,preceded,delimited,terminated}. The operator table is built from Box<dyn Fn>
// constructors -- dense pressure on closures, higher-order combinators, trait-object dispatch.
// Output: the AST of a fixed expression set (derive Debug, deterministic field order), checked
// evaluation with fixed error text, Debug text of nom Err, trailing-input diagnostics. No IO.
use nom::branch::alt;
use nom::bytes::complete::tag;
use nom::character::complete::{char, digit1, multispace0};
use nom::combinator::{map, map_res, opt, recognize};
use nom::multi::{many0, many1};
use nom::sequence::{delimited, pair, preceded, terminated};
use nom::IResult;

#[derive(Debug)]
enum Expr {
    Num(i64),
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
    Rem(Box<Expr>, Box<Expr>),
    Pow(Box<Expr>, Box<Expr>),
}

// ===== operator table: char -> Box<dyn Fn> binary constructor (trait-object call pressure) =====
type BinCtor = Box<dyn Fn(Expr, Expr) -> Expr>;

fn add_ops() -> Vec<(char, BinCtor)> {
    vec![
        ('+', Box::new(|a, b| Expr::Add(Box::new(a), Box::new(b)))),
        ('-', Box::new(|a, b| Expr::Sub(Box::new(a), Box::new(b)))),
    ]
}

fn mul_ops() -> Vec<(char, BinCtor)> {
    vec![
        ('*', Box::new(|a, b| Expr::Mul(Box::new(a), Box::new(b)))),
        ('/', Box::new(|a, b| Expr::Div(Box::new(a), Box::new(b)))),
        ('%', Box::new(|a, b| Expr::Rem(Box::new(a), Box::new(b)))),
    ]
}

fn apply(ops: &[(char, BinCtor)], c: char, a: Expr, b: Expr) -> Expr {
    for (k, f) in ops {
        if *k == c {
            return f(a, b);
        }
    }
    panic!("unknown op {c}");
}

// ===== grammar (low to high precedence): expr(+-) / term(*//%) / unary(-) / power(**) / primary =====
// Whitespace is tolerated around tokens: ws(f) = multispace0 · f · multispace0.
fn ws<'a, F, O>(mut f: F) -> impl FnMut(&'a str) -> IResult<&'a str, O>
where
    F: FnMut(&'a str) -> IResult<&'a str, O>,
{
    move |i: &'a str| {
        let (i, _) = multispace0(i)?;
        let (i, o) = f(i)?;
        let (i, _) = multispace0(i)?;
        Ok((i, o))
    }
}

// Decimal integer literal (allows '_' separators); map_res strips them and parses i64.
fn number(i: &str) -> IResult<&str, Expr> {
    map(
        map_res(recognize(many1(alt((digit1, tag("_"))))), |s: &str| {
            s.replace('_', "").parse::<i64>()
        }),
        Expr::Num,
    )(i)
}

fn paren(i: &str) -> IResult<&str, Expr> {
    delimited(ws(char('(')), expr, ws(char(')')))(i)
}

fn primary(i: &str) -> IResult<&str, Expr> {
    preceded(multispace0, alt((number, paren)))(i)
}

// Power: right-associative (2 ** 3 ** 2 = 2 ** (3 ** 2)); right operand allows unary minus (2 ** -3).
fn power(i: &str) -> IResult<&str, Expr> {
    map(
        pair(primary, opt(preceded(ws(tag("**")), unary))),
        |(base, exp)| match exp {
            Some(e) => Expr::Pow(Box::new(base), Box::new(e)),
            None => base,
        },
    )(i)
}

// Unary minus: stacks (- - 5) and binds looser than ** (-2 ** 2 = -(2 ** 2)).
fn unary(i: &str) -> IResult<&str, Expr> {
    alt((
        map(preceded(ws(char('-')), unary), |e| {
            Expr::Neg(Box::new(e))
        }),
        power,
    ))(i)
}

fn term(i: &str) -> IResult<&str, Expr> {
    let (i, first) = unary(i)?;
    let (i, rest) = many0(pair(ws(alt((char('*'), char('/'), char('%')))), unary))(i)?;
    let ops = mul_ops();
    let mut acc = first;
    for (c, rhs) in rest {
        acc = apply(&ops, c, acc, rhs);
    }
    Ok((i, acc))
}

fn expr(i: &str) -> IResult<&str, Expr> {
    let (i, first) = term(i)?;
    let (i, rest) = many0(pair(ws(alt((char('+'), char('-')))), term))(i)?;
    let ops = add_ops();
    let mut acc = first;
    for (c, rhs) in rest {
        acc = apply(&ops, c, acc, rhs);
    }
    Ok((i, acc))
}

// Top level: expression + trailing whitespace; the caller checks the remaining input.
fn parse_full(i: &str) -> IResult<&str, Expr> {
    terminated(expr, multispace0)(i)
}

// ===== checked evaluation: error text is a fixed &str (same and deterministic on both sides) =====
fn eval(e: &Expr) -> Result<i64, &'static str> {
    match e {
        Expr::Num(n) => Ok(*n),
        Expr::Neg(a) => eval(a)?.checked_neg().ok_or("overflow"),
        Expr::Add(a, b) => eval(a)?.checked_add(eval(b)?).ok_or("overflow"),
        Expr::Sub(a, b) => eval(a)?.checked_sub(eval(b)?).ok_or("overflow"),
        Expr::Mul(a, b) => eval(a)?.checked_mul(eval(b)?).ok_or("overflow"),
        Expr::Div(a, b) => {
            let (x, y) = (eval(a)?, eval(b)?);
            if y == 0 {
                return Err("div_by_zero");
            }
            x.checked_div(y).ok_or("overflow")
        }
        Expr::Rem(a, b) => {
            let (x, y) = (eval(a)?, eval(b)?);
            if y == 0 {
                return Err("rem_by_zero");
            }
            x.checked_rem(y).ok_or("overflow")
        }
        Expr::Pow(a, b) => {
            let (x, y) = (eval(a)?, eval(b)?);
            let e32: u32 = y.try_into().map_err(|_| "bad_exponent")?;
            x.checked_pow(e32).ok_or("overflow")
        }
    }
}

// ===== recursive AST statistics (used for generated forms, avoids huge Debug lines) =====
fn nodes(e: &Expr) -> u64 {
    1 + match e {
        Expr::Num(_) => 0,
        Expr::Neg(a) => nodes(a),
        Expr::Add(a, b)
        | Expr::Sub(a, b)
        | Expr::Mul(a, b)
        | Expr::Div(a, b)
        | Expr::Rem(a, b)
        | Expr::Pow(a, b) => nodes(a) + nodes(b),
    }
}

fn depth(e: &Expr) -> u64 {
    1 + match e {
        Expr::Num(_) => 0,
        Expr::Neg(a) => depth(a),
        Expr::Add(a, b)
        | Expr::Sub(a, b)
        | Expr::Mul(a, b)
        | Expr::Div(a, b)
        | Expr::Rem(a, b)
        | Expr::Pow(a, b) => depth(a).max(depth(b)),
    }
}

fn report(label: &str, disp: &str, input: &str, full_ast: bool) {
    println!("{label} {disp}");
    match parse_full(input) {
        Ok((rest, ast)) => {
            if !rest.is_empty() {
                println!("  trailing = {rest:?}");
                return;
            }
            if full_ast {
                println!("  ast = {ast:?}");
            } else {
                println!("  ast nodes = {} depth = {}", nodes(&ast), depth(&ast));
            }
            match eval(&ast) {
                Ok(v) => println!("  eval = {v}"),
                Err(e) => println!("  eval_err = {e}"),
            }
        }
        Err(e) => println!("  parse_err = {e:?}"),
    }
}

fn main() {
    println!("nom 7 arithmetic parser differential");

    // ① valid / evaluation-error paths: precedence, parentheses, unary minus, power assoc,
    //    whitespace, '_' separators, i64 bounds, checked overflow, div-by-zero, negative exponents.
    const CASES: &[&str] = &[
        "1+2*3",
        "(1+2)*3",
        "-(3 - -4) / 2",
        "2 ** 3 ** 2",
        "-2 ** 2",
        "(-2) ** 2",
        "10 % 3 + 7 % 4",
        "-7 % 3",
        "7 / -2",
        "  -  -   5 ",
        "1_000_000 + 2_3",
        "42",
        "9223372036854775807",
        "0 - 9223372036854775807 - 1",
        "9223372036854775807 + 1",
        "-(0 - 9223372036854775807 - 1)",
        "1 / (2 - 2)",
        "5 % 0",
        "2 ** -3",
        "2 ** 62",
        "(-2) ** 63",
        "2 ** 63",
        "0 ** 0",
        "((((((((((1))))))))))",
        "3 * 4 - 100 / (7 + 3) % 6",
        "1 +\n  2\t*\n 3",
        "- 9223372036854775807",
    ];
    for (i, c) in CASES.iter().enumerate() {
        report(&format!("case {i:02}"), &format!("{c:?}"), c, true);
    }

    // ② parse-error paths: Debug text of nom Err + trailing-input diagnostics.
    const ERRS: &[&str] = &[
        "abc",
        "2 ** * 3",
        "(1+2",
        "1 +",
        "9223372036854775808",
        "3 + ()",
        "5 $ 2",
        "",
        "__",
        "1 2",
    ];
    for (i, c) in ERRS.iter().enumerate() {
        report(&format!("err {i:02}"), &format!("{c:?}"), c, true);
    }

    // ③ generated forms: deep parens, long left-fold chain, stacked negations (drop-glue pressure).
    let deep = format!("{}7{}", "(".repeat(120), ")".repeat(120));
    report(
        "gen 00",
        &format!("parens x120 (len {})", deep.len()),
        &deep,
        false,
    );
    let chain = std::iter::repeat_n("1", 400).collect::<Vec<_>>().join("+");
    report(
        "gen 01",
        &format!("chain 1+...+1 x400 (len {})", chain.len()),
        &chain,
        false,
    );
    let negs = format!("{}3", "-".repeat(64));
    report(
        "gen 02",
        &format!("negs x64 (len {})", negs.len()),
        &negs,
        false,
    );
}
