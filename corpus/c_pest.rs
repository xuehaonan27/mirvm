#!/usr/bin/env mirvm
---
[dependencies]
# pest 2.8.7 + pest_derive 2.8.7 + pest_generator 2.8.7 + pest_meta 2.8.7
# （同 train 四钉）。上游漂移实锤（2026-07-27）：pest_derive 2.8.7 对
# pest_generator 用 ^2.8.7、pest_generator 对 pest_meta 用 ^2.8.7，2.8.8
# 系列又要求 pest ^2.8.8——pest 钉 2.8.7 即逐级撞（cargo 自家 fresh 解析
# 同撞，非 mirvm 分叉）。四钉对齐 train。
pest = "=2.8.7"
pest_derive = "=2.8.7"
pest_generator = "=2.8.7"
pest_meta = "=2.8.7"
---
// pest 2.8.7 PEG 解析器三维差分（批9：c_pest）。
//
// 测试面：
//   ① 两 grammar 同文件并存（grammar_inline 内联文法，各自独立模块隔离
//      生成 Rule 枚举重名）：
//      (a) JSON 子集：obj/arr/str/num/bool/null 全递归 + 四层容器嵌套 +
//          字符串转义（\" \\）+ 科学计数数字 + 多行输入含 \n；
//      (b) 计算器：优先级（*/ 高于 +-）+ 括号嵌套 + 一元负（含双重否定、
//          负号贴括号）+ 具名算符 rule（add/sub/mul/div/neg，eval 靠
//          as_rule 分派，未命名字面量不出 pair 的经典坑在此规避）；
//      WHITESPACE _ 静默规则两侧都踩（JSON 四字符集、calc 空格+tab）。
//   ② 合法输入解析树扁平节点类型序打印（先序遍历 Pair 树，规则名以空格
//      连接，JSON 两样本 + 计算器一样本）。
//   ③ 计算器 i64 求值结果打印 + assert_eq! 锚（14/30/3/10，手算可复核，
//      除法取整除、输入选整除样本避 0 除）。
//   ④ 3 个非法输入（JSON 双逗号、JSON 多行截断字面量 tru（错误落在
//      line>1）、calc 括号内缺操作数）：打印 line:col + positives 规则名
//      列表 + Error Display 全文（含 --> l:c 锚点与 caret 行），错误规则名
//      一一锚定。
//
// 确定性：全量字符串游标计算，无 IO/随机/时间/哈希序；positives Vec 序
// 由尝试顺序决定、单线程确定；i64 求值无浮点；assert 锚静默（不炸则零
// 输出）；stderr 真空（模块级 allow(non_camel_case_types, dead_code) 预
// 消 pest 派生枚举 snake_case 变体告警）。
//
// 钉版本/绕行：双钉 =2.8.7（最新稳定，见上）。绕行：无。
//
// 三维复跑（仓库根）：
//   A: target/release/mirvm run corpus/c_pest.rs
//   B: cd $(grep -l 'name = "c_pest"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_pest.rs
//
// FRONTIER：无（期待全绿）。
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

/// 先序遍历把 Pair 树压平为规则名序列（跨 grammar 通用，RuleType 抽象）。
fn flatten<R: pest::RuleType>(p: Pair<'_, R>, out: &mut Vec<String>) {
    out.push(format!("{:?}", p.as_rule()));
    for c in p.into_inner() {
        flatten(c, out);
    }
}

/// 合法样本：打印扁平节点类型序，返回序列供断言锚点。
fn tree_line<R: pest::RuleType>(label: &str, root: Pair<'_, R>) -> Vec<String> {
    let mut names = Vec::new();
    flatten(root, &mut names);
    println!("{label}: tree {} nodes", names.len());
    println!("  {}", names.join(" "));
    names
}

/// 错误报告：line:col + expected 规则名 + Display 全文（文本锚定）。
fn report_err<R: pest::RuleType>(label: &str, e: &PError<R>) {
    let (line, col) = match &e.line_col {
        LineColLocation::Pos(lc) => *lc,
        LineColLocation::Span(a, _) => *a,
    };
    let expected = match &e.variant {
        pest::error::ErrorVariant::ParsingError { positives, .. } => {
            // BTreeSet 拷贝仅统计去重数；正文按原 Vec 序打印（确定）。
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

// ---- 计算器求值（i64，除法须整除、输入已选定）----
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

    // ---- ① JSON 合法样本：全规则类型 + 转义 + 指数 + 八层嵌套 ----
    let s1 = r#"{"name": "mirvm", "data": [1, -2.5e3, "esc\"q\\z", true, false, null, {"k": []}]}"#;
    println!("json ok 01 src = {s1}");
    let t1 = match JsonParser::parse(JR::json, s1) {
        Ok(mut ps) => tree_line("json ok 01", ps.next().unwrap()),
        Err(e) => {
            report_err("json ok 01 UNEXPECTED", &e);
            Vec::new()
        }
    };

    // ---- ② JSON 多行合法样本（\n 空白 + line>1 记账）----
    let s2 = "{\n  \"nested\": {\n    \"a\": [ true, null ]\n  },\n  \"n\": 42\n}";
    println!("json ok 02 src = {}", s2.escape_debug());
    let t2 = match JsonParser::parse(JR::json, s2) {
        Ok(mut ps) => tree_line("json ok 02", ps.next().unwrap()),
        Err(e) => {
            report_err("json ok 02 UNEXPECTED", &e);
            Vec::new()
        }
    };

    // ---- ③ 计算器：优先级 / 括号 / 一元负 / 双重否定 ----
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

    // ---- ④ 计算器扁平序一样本（含具名算符 pair）----
    let c1 = CalcParser::parse(CR::calc, "2 + 3 * 4").unwrap().next().unwrap();
    let tc = tree_line("calc 01", c1);

    // ---- ⑤ 3 个非法输入：错误位置 + expected 规则名锚定 ----
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

    // ---- 断言锚点（静默，失败即炸出维度分叉）----
    assert_eq!(t1.first().map(String::as_str), Some("json"), "j1 root");
    assert_eq!(t1.len(), 30, "j1 node count");
    assert_eq!(t1.iter().filter(|n| n.as_str() == "value").count(), 11, "j1 values");
    assert_eq!(t2.first().map(String::as_str), Some("json"), "j2 root");
    assert!(t2.contains(&"boolean".to_string()) && t2.contains(&"null".to_string()));
    // 求值锚（手算）：2+12=14；5*6=30；-2* -(-1) - (-5) = -2+5=3；7+3=10。
    assert_eq!(eval_calc("2 + 3 * 4").unwrap(), 14);
    assert_eq!(eval_calc("(2 + 3) * (10 - 4)").unwrap(), 30);
    assert_eq!(eval_calc("-2 * -(3 + -4) - -10 / 2").unwrap(), 3);
    assert_eq!(eval_calc("--(7) + 3").unwrap(), 10);
    assert_eq!(tc.first().map(String::as_str), Some("calc"), "c1 root");
    assert!(tc.contains(&"add".to_string()) && tc.contains(&"mul".to_string()));
    println!("assert anchors OK");
}
