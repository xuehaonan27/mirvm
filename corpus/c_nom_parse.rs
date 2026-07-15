#!/usr/bin/env mirvm
---
[dependencies]
nom = "7"
---
// nom 7 组合子算术表达式解析器：括号 / 优先级 / 一元负号 / 空格容忍。
// 覆盖 bytes::complete::tag、character::complete::{char,digit1,multispace0}、
// multi::{many0,many1}、branch::alt、combinator::{map,map_res,opt,recognize}、
// sequence::{pair,preceded,delimited,terminated}。运算符表用 Box<dyn Fn>
// 构造器——闭包 / 高阶组合子 / trait 对象动态派发密集压力。
// 输出：固定表达式集的 AST（derive Debug，字段序确定）、checked 求值
// （错误文案固定）、nom Err 的 Debug 文本、trailing 输入诊断。全确定，无 IO。
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

// ===== 运算符表：char → Box<dyn Fn> 二元构造器（trait 对象调用压力）=====
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

// ===== 文法（优先级低→高）：expr(+-) / term(*//%) / unary(-) / power(**) / primary =====
// 记号两侧容忍空白：ws(f) = multispace0 · f · multispace0。
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

// 十进制整数字面量（允许 '_' 分隔），map_res 做 strip + i64 解析。
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

// 幂：右结合（2 ** 3 ** 2 = 2 ** (3 ** 2)），右操作数允许一元负号（2 ** -3）。
fn power(i: &str) -> IResult<&str, Expr> {
    map(
        pair(primary, opt(preceded(ws(tag("**")), unary))),
        |(base, exp)| match exp {
            Some(e) => Expr::Pow(Box::new(base), Box::new(e)),
            None => base,
        },
    )(i)
}

// 一元负号：可叠层（- - 5），绑定比 ** 松（-2 ** 2 = -(2 ** 2)）。
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

// 顶层：表达式 + 尾部空白；剩余输入由调用方判 trailing。
fn parse_full(i: &str) -> IResult<&str, Expr> {
    terminated(expr, multispace0)(i)
}

// ===== checked 求值：错误文案为固定 &str（两边确定性一致）=====
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

// ===== AST 递归统计（生成形态用，避免巨型 Debug 行）=====
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

    // ① 合法 / 求值错误路径：优先级、括号、一元负号、幂结合性、空白容忍、
    //    '_' 分隔、i64 边界、checked 溢出、除零、负指数。
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

    // ② 解析错误路径：nom Err 的 Debug 文本 + trailing 输入诊断。
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

    // ③ 内存生成形态：深括号递归、长左折叠链、叠层负号（递归 + drop glue 压力）。
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
