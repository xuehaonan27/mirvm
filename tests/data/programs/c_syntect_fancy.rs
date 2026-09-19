#!/usr/bin/env mirvm
---
[dependencies]
# syntect 5.2.0 (latest 5.x; pinned with =patch to prevent drift). The pin selects
# default-features=false + ["parsing","default-fancy"]:
#   - drops the default onig C backend (regex-onig -> onig_sys C build + libonig
#     linkage, heavy FFI and outside this driver's test surface);
#   - default-fancy brings parsing/default-syntaxes/default-themes/html/
#     regex-fancy (fancy-regex, pure Rust) + plist-load/yaml-load/dump-*
#     (the last four only add serde/parsing deps and are not exercised; enabled anyway).
# "parsing" is listed explicitly as well; the union is harmless.
# Closure is ~30 crates (fancy-regex/bitflags/bincode/flate2(miniz_oxide)/
# plist(quick-xml family)/yaml-rust/serde_json/thiserror/walkdir...), all pure Rust.
syntect = { version = "=5.2.0", default-features = false, features = ["parsing", "default-fancy"] }
---
// syntect 5.2.0 (fancy-regex pure-Rust backend): three-way differential over
// syntax-highlighted HTML. For three inline snippets (rust / yaml / markdown) it uses the
// crate's built-in static syntax set (SyntaxSet::load_defaults_newlines) and built-in
// theme set (ThemeSet::load_defaults, choosing InspiredGitHub) to emit inline-styled HTML.
//
// TOML is absent from the syntax set built into syntect 5.2.0 (75 entries): the full
// extension table shows it (Markdown=["md",...], Rust=["rs"], no TOML entry;
// `find_syntax_by_extension("toml")` returns None, so a toml snippet would unwrap-
// panic).
// With syntaxes constrained to the crate's built-in static set, the toml slot uses the
// built-in YAML syntax (ext "yaml"), which fills the same role (configuration/markup with
// # comments); the code/config/document trio of the three snippets is preserved.
//
// Test surface:
//   ① Embedded dump loading: default_syntaxes/default_themes are zlib-compressed bincode
//      static assets -> the flate2(miniz_oxide)+simd-adler32 adler32 check (a single
//      update >=32B dispatches SIMD through the _mm*_sad_epu8 = llvm.x86.psad.bw family,
//      one of the runtime's implemented SIMD paths; this driver also regresses it);
//      asset counts (syntax count / theme count / all names) + scope output anchor the
//      bincode deserialization and BTreeMap order.
//   ② Highlighting core: the fancy-regex-driven Sublime syntax state machine colors the
//      text line by line -> highlighted_html_for_string; each snippet anchors syntax
//      name/scope/output line count/span count/hex color-token order (a Vec deduped in
//      first-seen order, not a hash set), then the full HTML output is printed.
//   ③ Mixed surface: the rust snippet has a raw string and // comments; yaml has # line
//      comments, non-scalar values (bool/int/list) and non-ASCII; markdown has heading/
//      bold/inline code/link/list/quote plus non-ASCII and HTML entity escapes (&<>").
//   ④ Error/empty path: find_syntax_by_extension("nope") -> None is anchored.
//
// Determinism: assets are static data embedded at crate compile time; inputs are embedded
// literals; no IO/time/random/threads/hash order (colors dedupe with an order-preserving
// Vec, theme names follow BTreeMap key order); output <=80 lines, no floats, stderr empty.
//
// Three-way rerun commands (repo root):
//   A: target/release/mirvm run tests/data/programs/c_syntect_fancy.rs
//   B: cd $(grep -l 'name = "c_syntect_fancy"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_syntect_fancy.rs
//
// No known limitation; expect all green.
use syntect::highlighting::ThemeSet;
use syntect::html::highlighted_html_for_string;
use syntect::parsing::SyntaxSet;

const RUST: &str = r##"// fib 注释 <&>
fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}
fn main() { let s = r#"raw"; println!("{}", fib(s.len() as u64)); }
"##;

const YAML: &str = r#"# 配置 comment
name: mirvm
version: 0.4.0
features:
  - parsing
  - "default-fancy"
enabled: true
retries: 3
"#;

const MARKDOWN: &str = r#"# 标题 Heading
正文 plain **加粗**、*斜体* 与 `code` 行。

- [链接](https://example.com)
- 尾项 tail

> 引注 quote
"#;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    // ---- ① static asset loading (zlib+bincode deserialization path) ----
    let ps = SyntaxSet::load_defaults_newlines();
    println!("syntaxes = {}", ps.syntaxes().len());
    let ts = ThemeSet::load_defaults();
    let names: Vec<&String> = ts.themes.keys().collect();
    println!("themes = {} {:?}", names.len(), names);
    let theme = &ts.themes["InspiredGitHub"];
    let bg = theme.settings.background.unwrap();
    println!("theme bg = #{:02x}{:02x}{:02x}{:02x}", bg.r, bg.g, bg.b, bg.a);

    // ---- ② highlight the three snippets ----
    let mut all = String::new();
    for (ext, code) in [("rs", RUST), ("yaml", YAML), ("md", MARKDOWN)] {
        let syntax = ps.find_syntax_by_extension(ext).unwrap();
        let html = highlighted_html_for_string(code, &ps, syntax, theme).unwrap();
        let spans = html.matches("<span").count();
        let mut colors: Vec<&str> = Vec::new();
        for seg in html.split("color:#").skip(1) {
            let hex = &seg[..6];
            if !colors.contains(&hex) {
                colors.push(hex);
            }
        }
        println!(
            "[{ext}] syntax={} scope={} lines={} spans={spans} colors={:?}",
            syntax.name,
            syntax.scope,
            html.lines().count(),
            colors
        );
        for line in html.lines() {
            println!("{line}");
        }
        all.push_str(&html);
    }

    // ---- ④ summary + empty path ----
    println!("missing ext found = {}", ps.find_syntax_by_extension("nope").is_some());
    println!("fnv-all = {:016x}", fnv1a(all.as_bytes()));
}
