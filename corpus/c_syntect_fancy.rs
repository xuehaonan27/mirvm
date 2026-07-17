#!/usr/bin/env mirvm
---
[dependencies]
# syntect 5.2.0（5.x 最新，钉 =patch 防漂移）。按任务钉选
# default-features=false + ["parsing","default-fancy"]：
#   - 砍 default 的 onig C 后端（regex-onig → onig_sys C 构建 + libonig
#     闭包链接，是重 FFI 且非本 driver 测试面）；
#   - default-fancy 自带 parsing/default-syntaxes/default-themes/html/
#     regex-fancy（fancy-regex 纯 Rust）+ plist-load/yaml-load/dump-*
#     （后四者只多拖 serde/解析依赖，不走其运行面，任务指定全收）。
# "parsing" 显式重列：任务钉选原文如此，二者取并集无害。
# 闭包 ≈30 crate（fancy-regex/bitflags/bincode/flate2(miniz_oxide)/
# plist(quick-xml 系)/yaml-rust/serde_json/thiserror/walkdir…），全部纯 Rust。
syntect = { version = "=5.2.0", default-features = false, features = ["parsing", "default-fancy"] }
---
// syntect 5.2.0（fancy-regex 纯 Rust 后端）语法高亮 HTML 三维差分：
// 对 rust / yaml / markdown 三个内嵌代码片段，用 crate 内置静态语法集
// （SyntaxSet::load_defaults_newlines）与内置主题集（ThemeSet::load_defaults，
// 选 InspiredGitHub）生成带内联颜色样式的 HTML。
//
// 任务书原文 toml，但 syntect 5.2.0 内置语法集（75 个）根本不含 TOML
// ——探针全表列出 exts 实证（Markdown=["md",…]、Rust=["rs"]、无 TOML 条目；
// `find_syntax_by_extension("toml")` 返回 None，A 维实测在第 2 片段 unwrap
// panic）。
// 按任务「语法定义用 crate 内置静态集」的约束，以同角色（配置/标记、带
// # 注释）的内置 YAML 语法替代（ext "yaml"），三片段组合「代码/配置/文档」
// 原意保持。
//
// 测试面：
//   ① 内嵌 dump 装载：default_syntaxes/default_themes 是 zlib 压缩的 bincode
//      静态资产 → flate2(miniz_oxide)+simd-adler32 的 adler32 校验（单次
//      update ≥32B 的 SIMD 派发走 _mm*_sad_epu8 = llvm.x86.psad.bw 族，
//      2026-07-15 已内建的七族修复之一；本 driver 顺带当这条修复的回归网）；
//      资产计数（syntax 数/theme 数/name 全列）+ scope 打印锚定 bincode
//      反序列化与 BTreeMap 序。
//   ② 高亮主体：fancy-regex 驱动的 Sublime 语法状态机逐行着色 →
//      highlighted_html_for_string；每段锚定 syntax 名/scope/输出行数/
//      span 计数/颜色 token 十六进制序（按出现序去重的 Vec，非哈希集），
//      HTML 输出行随后全量打印。
//   ③ 混合面：rust 片段含 raw string 与 // 注释、yaml 含 # 行内注释/非标
//      量（bool/int/列表）与非 ASCII、markdown 含标题/粗体/行内码/链接/
//      列表/引用+非 ASCII，HTML 实体转义（&<>"）经 raw string 与注释压到。
//   ④ 错误/空路径：find_syntax_by_extension("nope") → None 锚定。
//
// 确定性：资产全为 crate 编译期内嵌静态数据；输入为内嵌字面量；无 IO/
// 时间/随机/线程/哈希序（颜色去重用保序 Vec；theme 名遍历走 BTreeMap
// 键序）；输出 ≤80 行；无浮点；stderr 真空（driver 零 warning）。
//
// 三维复跑命令（仓库根）：
//   A: target/release/mirvm run corpus/c_syntect_fancy.rs
//   B: cd $(grep -l 'name = "c_syntect_fancy"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_syntect_fancy.rs
//
// FRONTIER：无（期待全绿）。
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
    // ---- ① 静态资产装载（zlib+bincode 反序列化路径）----
    let ps = SyntaxSet::load_defaults_newlines();
    println!("syntaxes = {}", ps.syntaxes().len());
    let ts = ThemeSet::load_defaults();
    let names: Vec<&String> = ts.themes.keys().collect();
    println!("themes = {} {:?}", names.len(), names);
    let theme = &ts.themes["InspiredGitHub"];
    let bg = theme.settings.background.unwrap();
    println!("theme bg = #{:02x}{:02x}{:02x}{:02x}", bg.r, bg.g, bg.b, bg.a);

    // ---- ② 三片段高亮 ----
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

    // ---- ④ 汇总 + 空路径 ----
    println!("missing ext found = {}", ps.find_syntax_by_extension("nope").is_some());
    println!("fnv-all = {:016x}", fnv1a(all.as_bytes()));
}
