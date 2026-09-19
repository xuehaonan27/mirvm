#!/usr/bin/env mirvm
---
[dependencies]
# logos 0.14 (latest 0.14.x), default features (std + derive, which includes
# logos-derive: its table-driven DFA macro expands at compile time, leaving pure
# string-interval scanning at runtime). Lexer-crate surface: derive tables + span/slice bookkeeping.
logos = "0.14"
---
// logos 0.14 (table-driven DFA lexer, derive-generated at compile time) three-way
// differential over a mini arithmetic lexer: numbers, + - * /, parens, whitespace
// and // line comments (skip rules). Prints every token type + slice + span, plus
// Err(()) spans for two lexical errors ('@', '#'); iteration continues after each.
// Also anchors the Lexer meta API (source/remainder/next, final exhaustion to empty).
//
// Pure string-interval computation, no IO/random/time/hash order -- output is
// naturally deterministic, stderr empty (zero driver warnings). Note the #[derive]
// Logos macro places the "/" token next to the "//" line-comment skip regex:
// longest match wins, so "//..." takes skip and a lone "/" takes only the token; both occur.
//
// Three-way rerun commands (from the repo root):
//   A: target/release/mirvm run corpus/c_logos_lex.rs
//   B: cd $(grep -l 'name = "c_logos_lex"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_logos_lex.rs
//
// FRONTIER: none (expect all green).
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

/// Print one source string's full token stream (type/slice/span); errors print their span on an ERR line.
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
    // ① Clean input: 7 token kinds + whitespace + a mid-stream comment + a paren group after a newline.
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

    // ② Lexical error '@': span points at 2..3; iteration continues normally after the error.
    let (t2, e2) = lex_dump("err-at", "7 @ 8");
    assert_eq!(e2, vec![2..3]);
    assert_eq!(t2, vec![(Token::Num, 0..1), (Token::Num, 4..5)]);

    // ③ Lexical error '#' inside a parenthesized expression: span points at 5..6.
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

    // ④ Lexer meta API: source/remainder reduction and a fresh lexer's start.
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
