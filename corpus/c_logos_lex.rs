#!/usr/bin/env mirvm
---
[dependencies]
# logos 0.14（最新 0.14.x），default features（std + derive 内含 logos-derive，
# 生成表驱动 DFA 的宏在编译期展开，运行期是纯字符串区间扫描）。词法器 crate
# 的代表面：derive 大表词法 + span/slice 区间记账。
logos = "0.14"
---
// logos 0.14（表驱动 DFA 词法器，derive 编译期生成）三维差分：mini 四则表达式
// lexer——数字 / 四运算（+ - * /）/ 左右括号 / 空白与 // 行注释（skip 规则），
// token 类型 + slice + span 序列全打印；两处词法错误（'@'、'#'）的 Err(()) 的
// span 定位，且错误后迭代继续（下一 token 照常给出）。另锚定 Lexer 元 API：
// source()/remainder()/next() 交错与最终归空。
//
// 纯字符串区间计算，无 IO/随机/时间/哈希序——输出天然确定；stderr 真空
// （driver 零 warning）。注意调用点 #[derive] 的 Logos 宏把 "/" 单字符 token
// 与 "//" 起头的行注释 skip regex 并置：logos 采用最长匹配胜出，"//..." 整体
// 匹配比一般长命中 skip；孤 "/" 只命中 token——本 driver 两者都踩。
//
// 三维复跑命令（仓库根）：
//   A: target/release/mirvm run corpus/c_logos_lex.rs
//   B: cd $(grep -l 'name = "c_logos_lex"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_logos_lex.rs
//
// FRONTIER：无（期待全绿）。
use logos::Logos;
use std::ops::Range;

#[derive(Logos, Debug, PartialEq, Clone, Copy)]
#[logos(skip r"[ \t\n\r]+", skip r"//[^\n]*")]
enum Token {
    #[regex(r"[0-9]+")]
    Num,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
}

/// 全量打印一个源串的 token 流（类型/slice/span），错误按 ERR 行打印 span。
fn lex_dump(tag: &str, src: &str) -> (Vec<(Token, Range<usize>)>, Vec<Range<usize>>) {
    println!("[{tag}] len={} src=\"{}\"", src.len(), src.escape_debug());
    let mut toks = Vec::new();
    let mut errs = Vec::new();
    let mut lex = Token::lexer(src);
    while let Some(res) = lex.next() {
        let span = lex.span();
        match res {
            Ok(tok) => {
                println!("[{tag}] tok {tok:?} slice={:?} span={span:?}", lex.slice());
                toks.push((tok, span));
            }
            Err(()) => {
                println!("[{tag}] ERR slice={:?} span={span:?}", lex.slice());
                errs.push(span);
            }
        }
    }
    println!("[{tag}] ok={} err={} rem_len={}", toks.len(), errs.len(), lex.remainder().len());
    (toks, errs)
}

fn main() {
    // ① 干净输入：全 7 种 token + 空白 + 中段行注释 + 换行后接括号组。
    let (t1, e1) = lex_dump(
        "clean",
        "12 + 34*( 56- 7 ) / 89 // final result\n( 10 )",
    );
    assert_eq!(
        t1,
        vec![
            (Token::Num, 0..2),
            (Token::Plus, 3..4),
            (Token::Num, 5..7),
            (Token::Star, 7..8),
            (Token::LParen, 8..9),
            (Token::Num, 10..12),
            (Token::Minus, 12..13),
            (Token::Num, 14..15),
            (Token::RParen, 16..17),
            (Token::Slash, 18..19),
            (Token::Num, 20..22),
            (Token::LParen, 39..40),
            (Token::Num, 41..43),
            (Token::RParen, 44..45),
        ]
    );
    assert!(e1.is_empty());

    // ② 词法错误 '@'：span 指向 2..3，错误后迭代正常继续。
    let (t2, e2) = lex_dump("err-at", "7 @ 8");
    assert_eq!(e2, vec![2..3]);
    assert_eq!(t2, vec![(Token::Num, 0..1), (Token::Num, 4..5)]);

    // ③ 词法错误 '#' 混在括号表达式内：span 指向 5..6。
    let (t3, e3) = lex_dump("err-hash", "(1 + #2)");
    assert_eq!(e3, vec![5..6]);
    assert_eq!(
        t3,
        vec![
            (Token::LParen, 0..1),
            (Token::Num, 1..2),
            (Token::Plus, 3..4),
            (Token::Num, 6..7),
            (Token::RParen, 7..8),
        ]
    );

    // ④ Lexer 元 API：source/remainder 归约与重新 lexer 起点。
    let mut lex = Token::lexer("99+ 5*11");
    println!(
        "meta src_len={} rem0={}",
        lex.source().len(),
        lex.remainder().len()
    );
    let first = lex.next().unwrap().unwrap();
    println!(
        "meta first={first:?} span={:?} rem_len={}",
        lex.span(),
        lex.remainder().len()
    );
    assert_eq!(first, Token::Num);
    assert_eq!(lex.span(), 0..2);

    println!("done toks={} errs={}", t1.len() + t2.len() + t3.len(), e1.len() + e2.len() + e3.len());
}
