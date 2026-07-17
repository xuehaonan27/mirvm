#!/usr/bin/env mirvm
---
[dependencies]
# markdown-it 0.6.1（最新 0.x，2024-07 发布，rust 版 markdown-it）。default
# features = ["linkify", "syntect"]；syntect 拖 ~60 crate 重资产（plist/flate2/
# fancy-regex/语法+主题静态资产）且只服务代码块高亮这一非测试面 →
# default-features=false 仅留 linkify（裸 URL/www/email 自动链接的扩展面）。
# 闭包 ≈24 crate（regex/once_cell/stacker+psm/entities/mdurl 等）。
markdown-it = { version = "=0.6.1", default-features = false, features = ["linkify"] }
---
// markdown-it 0.6.1（CommonMark 兼容，插件全开）三维差分：5 个 CommonMark spec
// 选段 + 4 个扩展片段，逐段 parse().render()，全部 HTML 逐行打印（段头带
// FNV-1a 与行数锚点。输出 57 行），结尾 4 条硬断言：总行数 + 全连接 FNV
// （锚定 9 段全部字节）+ typographer/linkify 两段精确 HTML（UTF-8 按 \u{} 写）。
//
// 插件面（本 crate 全部插件；无 footnote/tasklists 插件——那是 markdown-it JS
// 生态的独立扩展，Rust 版未内置，用 in-crate 扩展全集替代）：
//   cmark（CommonMark 全集）+ extra（strikethrough/tables/linkify[feature]/
//   beautify_links/smartquotes/typographer）+ extra::heading_anchors（slug id）+
//   html（原始 html inline/block 透传）+ sourcepos（data-sourcepos 源码映射
//   属性，行：列定位天然嵌入全部块级标签 → 位置敏感覆盖）。
// 测试面 9 段：cm1 ATX 标题+em/strong/code 行内；cm2 带 info 的围栏代码块+
// 实体转义；cm3 有序列表嵌套无序子表（tight）；cm4 引用块+链接引用定义；
// cm5 setext 标题+硬换行（行尾两空格）+水平线；ext1 GFM 表格（左右对齐列）；
// ext2 删除线+smartquotes+typographer（--/.../(c)替换）；ext3 linkify 裸 https
// URL + beautify_links 链接文本美化（www 裸输入在 Rust 版默认不链接化，作为
// 不命中面保留）；ext4 heading_anchors 标题 slug id + html 原始块透传。
//
// 确定性：输入全为内嵌字面量；输出为纯渲染文本+计数+FNV，无时间/随机/哈希序/
// 路径/地址。引擎触点备注：markdown-it 每次 parse/render 经 stacker::maybe_grow
// 读 psm 汇编的 rust_psm_stack_pointer（宿主 SP extern thunk）+ 首访 TLS 时
// pthread_getattr_np 测栈界——浅输入下 remaining≥64KB 恒成立、走直调分支不切换
// 栈，三维同分支同输出；非 ASCII 引号/替换字符按 UTF-8 字面量打印。
//
// 三维复跑命令（仓库根）：
//   A: target/release/mirvm run corpus/c_markdown_it.rs
//   B: cd $(grep -l 'name = "c_markdown_it"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_markdown_it.rs
//
// FRONTIER：无（三维实测全绿，57 行输出 md5 三相一致）。
use markdown_it::MarkdownIt;

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 5 个 CommonMark spec 选段 + 4 个扩展片段：(标签, markdown 源)。
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
    // 插件全开：cmark + extra（strikethrough/beautify_links/linkify/tables/
    // typographer/smartquotes）+ heading_anchors + html + sourcepos。
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

    // 硬断言（数值锚定全部 9 段字节；两条精确 HTML 覆盖 typographer/linkify）。
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
