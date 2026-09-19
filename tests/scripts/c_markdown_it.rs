#!/usr/bin/env mirvm
---
[dependencies]
# markdown-it 0.6.1, the newest 0.x release (published 2024-07), is the Rust port of
# markdown-it. Its default features are ["linkify", "syntect"]; syntect pulls in ~60
# crates of heavy assets (plist/flate2/fancy-regex plus static syntax and theme data)
# and only serves code-block highlighting, which is not under test, so this fixture
# disables default features and keeps linkify (bare URL/www/email autolinking).
markdown-it = { version = "=0.6.1", default-features = false, features = ["linkify"] }
---
// markdown-it 0.6.1 (CommonMark-compatible, all plugins enabled), three-way
// differential: five CommonMark spec fragments plus four extension fragments, each run
// through parse().render(), printing every HTML line with a per-fragment FNV-1a and
// line count, then four hard assertions: total line count, the concatenated FNV (pins
// the bytes of all nine fragments), and the exact HTML of the typographer and linkify
// fragments (UTF-8 written as \u{}).
//
// Plugin surface (every plugin this crate ships; there is no footnote or tasklists
// plugin, as those are separate JS-ecosystem extensions not built into the Rust port):
//   cmark + extra (strikethrough/tables/linkify[feature]/beautify_links/smartquotes/
//   typographer) + extra::heading_anchors (slug ids) + html (raw inline/block
//   passthrough) + sourcepos (data-sourcepos attributes on every block tag, so this is
//   position-sensitive coverage).
// Nine fragments: cm1 ATX heading and inline em/strong/code; cm2 fenced code block; cm3
// ordered list nesting a tight unordered sublist; cm4 blockquote and link reference; cm5
// setext heading, hard break and horizontal rule; ext1 GFM table; ext2 strikethrough and
// smartquotes/typographer (--/.../(c) replacements); ext3 linkify of a bare https URL
// plus beautify_links (a bare www input stays unlinked in the Rust port); ext4
// heading_anchors slug ids and raw html passthrough.
// Deterministic: embedded literal inputs only; output is rendered text plus counts and
// FNV, with no time, randomness, hash order, paths or addresses. stacker::maybe_grow reads
// the psm rust_psm_stack_pointer assembly symbol (host-SP extern thunk) and measures the
// stack bound via pthread_getattr_np on the first TLS access; shallow input keeps
// remaining >= 64KB, so the direct-call branch runs and the stack is never switched.
// Three-way rerun commands (from the repository root):
//   A: target/release/mirvm run tests/scripts/c_markdown_it.rs
//   B: cd $(grep -l 'name = "c_markdown_it"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_markdown_it.rs
//
// Non-ASCII quotes and replacement characters print as UTF-8 literals.
use markdown_it::MarkdownIt;

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Five CommonMark spec fragments plus four extension fragments: (tag, markdown source).
const CASES: [(&str, &str); 9] = [
    (
        "cm1-heading-inline",
        "# Hello *World*\n\nA **strong** word, an *emphasis*, and `inline code`.\n",
    ),
    (
        "cm2-fence-entity",
        "```rust\nfn main() { let x: i32 = 1 + 2; }\n```\n\nTom &amp; Jerry &lt;3.\n",
    ),
    (
        "cm3-nested-list",
        "1. first\n2. second\n   - sub a\n   - sub b\n3. third\n",
    ),
    (
        "cm4-quote-linkref",
        "> quoted line one\n> with a [ref link][r] inside\n\n[r]: https://example.com/ref \"RefTitle\"\n",
    ),
    (
        "cm5-setext-hardbreak-hr",
        "Setext Title\n============\n\nline one  \nline two\n\n---\n",
    ),
    (
        "ext1-table-align",
        "| Name  | Right |\n|:------|------:|\n| alpha |     1 |\n| beta  |    20 |\n",
    ),
    (
        "ext2-strike-typographer",
        "~~gone~~, \"quoted text\" -- dash... and (c) plus (tm).\n",
    ),
    (
        "ext3-linkify-beautify",
        "See https://example.com/docs/guide or www.rust-lang.org today.\n",
    ),
    (
        "ext4-anchor-html",
        "## Heading Two!\n\n<div class=\"note\">\nraw <b>html</b> block\n</div>\n",
    ),
];

fn main() {
    // All plugins enabled: cmark + extra (strikethrough/beautify_links/linkify/tables/
    // typographer/smartquotes) + heading_anchors + html + sourcepos.
    let md = &mut MarkdownIt::new();
    markdown_it::plugins::cmark::add(md);
    markdown_it::plugins::extra::add(md);
    markdown_it::plugins::extra::heading_anchors::add(
        md,
        markdown_it::plugins::extra::heading_anchors::simple_slugify_fn,
    );
    markdown_it::plugins::html::add(md);
    markdown_it::plugins::sourcepos::add(md);

    let mut total_lines = 0usize;
    let mut concat = String::new();
    let mut htmls = Vec::new();
    for (tag, src) in CASES {
        let html = md.parse(src).render();
        total_lines += html.lines().count();
        concat.push_str(&html);
        println!(
            "== {tag} | html_lines={} fnv={:016x} ==",
            html.lines().count(),
            fnv1a(html.as_bytes())
        );
        for line in html.lines() {
            println!("{line}");
        }
        htmls.push(html);
    }
    println!(
        "TOTAL fragments={} lines={} fnv={:016x}",
        CASES.len(),
        total_lines,
        fnv1a(concat.as_bytes())
    );

    // Hard assertions: the numbers pin all nine fragments' bytes; two exact HTML strings cover typographer/linkify.
    assert_eq!(total_lines, 47);
    assert_eq!(fnv1a(concat.as_bytes()), 0xd7d07124f9b0b7de);
    assert_eq!(
        htmls[6],
        "<p data-sourcepos=\"1:1-1:53\"><s data-sourcepos=\"1:1-1:8\">gone</s>, \
         \u{201c}quoted text\u{201d} \u{2013} dash\u{2026} and \u{a9} plus \u{2122}.</p>\n"
    );
    assert_eq!(
        htmls[7],
        "<p data-sourcepos=\"1:1-1:62\">See <a data-sourcepos=\"1:5-1:34\" \
         href=\"https://example.com/docs/guide\">example.com/docs/guide</a> \
         or www.rust-lang.org today.</p>\n"
    );
}
