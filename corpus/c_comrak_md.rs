#!/usr/bin/env mirvm
---
[dependencies]
comrak = { version = "0.29", default-features = false, features = ["shortcodes"] }
---
// comrak 0.29（default-features off，避开 syntect/cli）：CommonMark/GFM 大解析器差分。
// 内嵌混合 markdown（front matter/标题/嵌套列表/任务列表/rust 标注围栏代码块/
// 表格/链接引用/脚注/删除线/下划线/剧透/智能标点/硬换行/自动链接/数学块/
// wikilink/描述列表/多行引用/greentext/tagfilter），ComrakOptions 逐项全开
// extension → parse_document → format_html 全文打印 + AST 遍历 39 种
// NodeValue 计数 + 节点细节 + sourcepos + commonmark/xml 再序列化 +
// Anchorizer 去重 + 默认配置边界探针。全固定输入，无随机/时间。
use comrak::arena_tree::NodeEdge;
use comrak::nodes::{AstNode, NodeValue};
use comrak::{
    format_commonmark, format_html, format_xml, markdown_to_commonmark, markdown_to_html,
    parse_document, Anchorizer, Arena, ComrakOptions,
};

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const KINDS: [&str; 39] = [
    "Document",
    "FrontMatter",
    "BlockQuote",
    "List",
    "Item",
    "DescriptionList",
    "DescriptionItem",
    "DescriptionTerm",
    "DescriptionDetails",
    "CodeBlock",
    "HtmlBlock",
    "Paragraph",
    "Heading",
    "ThematicBreak",
    "FootnoteDefinition",
    "Table",
    "TableRow",
    "TableCell",
    "Text",
    "TaskItem",
    "SoftBreak",
    "LineBreak",
    "Code",
    "HtmlInline",
    "Emph",
    "Strong",
    "Strikethrough",
    "Superscript",
    "Link",
    "Image",
    "FootnoteReference",
    "ShortCode",
    "Math",
    "MultilineBlockQuote",
    "Escaped",
    "WikiLink",
    "Underline",
    "SpoileredText",
    "EscapedTag",
];

fn kind_index(v: &NodeValue) -> usize {
    match v {
        NodeValue::Document => 0,
        NodeValue::FrontMatter(_) => 1,
        NodeValue::BlockQuote => 2,
        NodeValue::List(_) => 3,
        NodeValue::Item(_) => 4,
        NodeValue::DescriptionList => 5,
        NodeValue::DescriptionItem(_) => 6,
        NodeValue::DescriptionTerm => 7,
        NodeValue::DescriptionDetails => 8,
        NodeValue::CodeBlock(_) => 9,
        NodeValue::HtmlBlock(_) => 10,
        NodeValue::Paragraph => 11,
        NodeValue::Heading(_) => 12,
        NodeValue::ThematicBreak => 13,
        NodeValue::FootnoteDefinition(_) => 14,
        NodeValue::Table(_) => 15,
        NodeValue::TableRow(_) => 16,
        NodeValue::TableCell => 17,
        NodeValue::Text(_) => 18,
        NodeValue::TaskItem(_) => 19,
        NodeValue::SoftBreak => 20,
        NodeValue::LineBreak => 21,
        NodeValue::Code(_) => 22,
        NodeValue::HtmlInline(_) => 23,
        NodeValue::Emph => 24,
        NodeValue::Strong => 25,
        NodeValue::Strikethrough => 26,
        NodeValue::Superscript => 27,
        NodeValue::Link(_) => 28,
        NodeValue::Image(_) => 29,
        NodeValue::FootnoteReference(_) => 30,
        NodeValue::ShortCode(_) => 31,
        NodeValue::Math(_) => 32,
        NodeValue::MultilineBlockQuote(_) => 33,
        NodeValue::Escaped => 34,
        NodeValue::WikiLink(_) => 35,
        NodeValue::Underline => 36,
        NodeValue::SpoileredText => 37,
        NodeValue::EscapedTag(_) => 38,
    }
}

/// 收集子树内全部 Text 内容（文档序），用于 heading 摘要。
fn collect_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut out = String::new();
    for n in node.descendants() {
        if let NodeValue::Text(t) = &n.data.borrow().value {
            out.push_str(t);
        }
    }
    out
}

fn make_options(escaped_spans: bool) -> ComrakOptions<'static> {
    let mut o = ComrakOptions::default();
    // extension 逐项全开
    o.extension.strikethrough = true;
    o.extension.tagfilter = true;
    o.extension.table = true;
    o.extension.autolink = true;
    o.extension.tasklist = true;
    o.extension.superscript = true;
    o.extension.header_ids = Some("h-".to_string());
    o.extension.footnotes = true;
    o.extension.description_lists = true;
    o.extension.front_matter_delimiter = Some("---".to_string());
    o.extension.multiline_block_quotes = true;
    o.extension.math_dollars = true;
    o.extension.math_code = true;
    o.extension.wikilinks_title_after_pipe = true;
    o.extension.shortcodes = true;
    o.extension.underline = true;
    o.extension.spoiler = true;
    o.extension.greentext = true;
    // parse
    o.parse.smart = true;
    // render
    o.render.hardbreaks = true;
    o.render.unsafe_ = true;
    o.render.github_pre_lang = true;
    // 注意：cm.rs 不认识 Escaped 节点（format_commonmark 会 panic），
    // commonmark 再序列化段用 escaped_spans=false 的配置实例。
    o.render.escaped_char_spans = escaped_spans;
    o
}

fn echo_options(o: &ComrakOptions) {
    println!("opts ext.strikethrough={}", o.extension.strikethrough);
    println!("opts ext.tagfilter={}", o.extension.tagfilter);
    println!("opts ext.table={}", o.extension.table);
    println!("opts ext.autolink={}", o.extension.autolink);
    println!("opts ext.tasklist={}", o.extension.tasklist);
    println!("opts ext.superscript={}", o.extension.superscript);
    println!("opts ext.header_ids={:?}", o.extension.header_ids);
    println!("opts ext.footnotes={}", o.extension.footnotes);
    println!("opts ext.description_lists={}", o.extension.description_lists);
    println!(
        "opts ext.front_matter_delimiter={:?}",
        o.extension.front_matter_delimiter
    );
    println!(
        "opts ext.multiline_block_quotes={}",
        o.extension.multiline_block_quotes
    );
    println!("opts ext.math_dollars={}", o.extension.math_dollars);
    println!("opts ext.math_code={}", o.extension.math_code);
    println!(
        "opts ext.wikilinks_title_after_pipe={}",
        o.extension.wikilinks_title_after_pipe
    );
    println!("opts ext.shortcodes={}", o.extension.shortcodes);
    println!("opts ext.underline={}", o.extension.underline);
    println!("opts ext.spoiler={}", o.extension.spoiler);
    println!("opts ext.greentext={}", o.extension.greentext);
    println!("opts parse.smart={}", o.parse.smart);
    println!("opts render.hardbreaks={}", o.render.hardbreaks);
    println!("opts render.unsafe_={}", o.render.unsafe_);
    println!("opts render.github_pre_lang={}", o.render.github_pre_lang);
    println!(
        "opts render.escaped_char_spans={}",
        o.render.escaped_char_spans
    );
}

const DOC: &str = r#"---
title: comrak differential probe
tags: [mirvm, markdown]
---
# Heading One "Quoted"

Setext Heading Two
------------------

A paragraph with "smart quotes", an em---dash, an en--dash, and ellipsis...
Line ends with two spaces for a hard break.  
Visual line after the break.
Soft break
right here.

> A normal blockquote with *emphasis* and **strong**.
> Second quoted line.

>implying a greentext line

>>>
multiline block quote content
spanning two lines
>>>

- outer item a
  1. nested ordered one
  2. nested ordered two
     - deep bullet x
     - deep bullet y
- outer item b

- [x] finished task
- [ ] pending task
- [X] upper-case done

```rust
fn main() {
    let xs: Vec<u8> = vec![1, 2, 3];
    println!("{}", xs.len());
}
```

| name  | qty | price |
|:------|----:|------:|
| apple |   3 |  1.50 |
| fig   |  12 |  0.25 |
| a \| b |  7 |  2.00 |

![probe image](https://example.com/img.png "Probe Image")

Text with ~~strikethrough~~, superscript x^2^, underline __underlined__,
spoiler ||hidden text||, inline math $e^{i\pi}+1=0$, and code math `$x+y$`.
Emoji shortcodes :rocket: and :crab: here.

$$
\int_0^1 x^2 dx = 1/3
$$

A [reference link][r1], a [broken ref][nope], an autolink www.example.com,
a scheme link https://rust-lang.org, and mail nobody@example.com.

[r1]: https://example.com/docs "Example Docs"

Footnote here[^note1], another[^long], and an undefined one[^missing].

[^note1]: the first footnote
[^long]: a longer footnote with *formatting* and a [link](https://example.com)

A wikilink [[Some Page|Shown Title]] in a sentence.

Term One

: Details of term one

Term Two

: Details alpha
: Details beta

<div class="raw">raw html block passes through</div>

Inline <xmp>tagfiltered</xmp> and <b>bold inline</b> html.

Escaped chars: \*not emphasis\* and \# not a heading.

***
"#;

fn main() {
    let options = make_options(true);
    echo_options(&options);
    println!("doc bytes = {} fnv = {:016x}", DOC.len(), fnv1a(DOC.as_bytes()));

    // ① 一键 API：markdown → html 全文
    let html = markdown_to_html(DOC, &options);
    println!("html len = {} fnv = {:016x}", html.len(), fnv1a(html.as_bytes()));
    print!("html begin\n{html}html end\n");

    // ② parse_document → AST 遍历：39 种 NodeValue 计数 + 深度 + 文本字节
    let arena = Arena::new();
    let root = parse_document(&arena, DOC, &options);
    let mut counts = [0u64; 39];
    let mut total = 0u64;
    let mut text_bytes = 0u64;
    let mut depth = 0u64;
    let mut max_depth = 0u64;
    for edge in root.traverse() {
        match edge {
            NodeEdge::Start(n) => {
                total += 1;
                counts[kind_index(&n.data.borrow().value)] += 1;
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
                if let NodeValue::Text(t) = &n.data.borrow().value {
                    text_bytes += t.len() as u64;
                }
            }
            NodeEdge::End(_) => depth -= 1,
        }
    }
    println!("ast total nodes = {total} max depth = {max_depth} text bytes = {text_bytes}");
    for (i, k) in KINDS.iter().enumerate() {
        println!("ast {k} = {}", counts[i]);
    }

    // ③ 节点细节（文档序）：代码块 / 链接 / 标题 / 脚注定义 / 表格 / 任务项 / front matter
    let (mut task_done, mut task_todo) = (0u64, 0u64);
    for n in root.descendants() {
        match &n.data.borrow().value {
            NodeValue::FrontMatter(fm) => {
                println!("detail frontmatter len = {} fnv = {:016x}", fm.len(), fnv1a(fm.as_bytes()));
            }
            NodeValue::CodeBlock(c) => {
                println!(
                    "detail codeblock info={:?} fenced={} fence_char={} literal_len={} literal_fnv={:016x}",
                    c.info,
                    c.fenced,
                    c.fence_char as char,
                    c.literal.len(),
                    fnv1a(c.literal.as_bytes())
                );
            }
            NodeValue::Link(l) => {
                println!("detail link url={:?} title={:?}", l.url, l.title);
            }
            NodeValue::Heading(h) => {
                println!(
                    "detail heading level={} setext={} text={:?}",
                    h.level,
                    h.setext,
                    collect_text(n)
                );
            }
            NodeValue::FootnoteDefinition(f) => {
                println!("detail footdef name={:?} refs={}", f.name, f.total_references);
            }
            NodeValue::Table(t) => {
                println!(
                    "detail table cols={} rows={} nonempty={} align={:?}",
                    t.num_columns, t.num_rows, t.num_nonempty_cells, t.alignments
                );
            }
            NodeValue::TaskItem(mark) => match mark {
                Some(c) => {
                    task_done += 1;
                    println!("detail taskitem done mark={c}");
                }
                None => task_todo += 1,
            },
            _ => {}
        }
    }
    println!("detail tasks done={task_done} todo={task_todo}");

    // ④ sourcepos 抽样：前 8 个先序节点
    for (i, n) in root.descendants().take(8).enumerate() {
        let ast = n.data.borrow();
        println!("pos[{i}] {} at {}", KINDS[kind_index(&ast.value)], ast.sourcepos);
    }

    // ⑤ format_html（AST 路径）与 markdown_to_html（一键路径）逐字节一致性
    let mut buf: Vec<u8> = Vec::new();
    format_html(root, &options, &mut buf).unwrap();
    println!("format_html bytes = {} eq_markdown_to_html = {}", buf.len(), buf == html.as_bytes());

    // ⑥ commonmark 再序列化：全文 + len + fnv；再解析渲染做 fixpoint 对比。
    // escaped_char_spans 产生的 Escaped 节点会让 cm.rs panic，故本段用关闭项的配置。
    let cm_options = make_options(false);
    let cm = markdown_to_commonmark(DOC, &cm_options);
    println!("cm len = {} fnv = {:016x}", cm.len(), fnv1a(cm.as_bytes()));
    print!("cm begin\n{cm}cm end\n");
    let cm_root = parse_document(&arena, DOC, &cm_options);
    let mut cm_buf: Vec<u8> = Vec::new();
    format_commonmark(cm_root, &cm_options, &mut cm_buf).unwrap();
    println!("format_commonmark eq = {}", cm_buf == cm.as_bytes());
    let html2 = markdown_to_html(&cm, &cm_options);
    let html_cm_opts = markdown_to_html(DOC, &cm_options);
    println!("cm->html len = {} fnv = {:016x} eq_orig = {}", html2.len(), fnv1a(html2.as_bytes()), html2 == html_cm_opts);

    // ⑦ Anchorizer：GFM anchor 算法 + 去重后缀（覆盖 slug/regex 路径）
    let mut az = Anchorizer::new();
    for h in [
        "Hello World!",
        "Hello World!",
        "Unicode 汉字 Heading",
        "symbols &amp; stuff",
        "Hello World!",
    ] {
        println!("anchor {h:?} => {:?}", az.anchorize(h.to_string()));
    }

    // ⑧ 默认配置边界探针（extension 全关的对照行为）
    let def = ComrakOptions::default();
    for (label, src) in [
        ("empty", ""),
        ("whitespace_only", "\n \n\t\n"),
        ("unclosed_fence", "```rust\nlet x = 1;\n"),
        ("broken_link_ref", "see [the docs][missing] today\n"),
        ("table_without_ext", "| a | b |\n|---|---|\n| 1 | 2 |\n"),
        ("nested_emphasis", "***bold italic*** and ___x___\n"),
        ("crlf_input", "line one\r\nline two\r\n"),
        ("nul_char", "a\u{0}b\n"),
        ("setext_vs_thematic", "text\n---\n"),
        ("autolink_std", "<https://example.com/a?b=1&c=2> and <me@example.com>\n"),
    ] {
        let out = markdown_to_html(src, &def);
        println!(
            "probe {label}: in_len={} out_len={} out_fnv={:016x}",
            src.len(),
            out.len(),
            fnv1a(out.as_bytes())
        );
        print!("probe {label} begin\n{out}probe {label} end\n");
    }

    // ⑨ xml 格式化器：小文档全文（第三个输出后端）
    let xml_doc = "# Hi *there*\n\npara with `code` and [l](https://x.y).\n";
    let xml_root = parse_document(&arena, xml_doc, &def);
    let mut xml: Vec<u8> = Vec::new();
    format_xml(xml_root, &def, &mut xml).unwrap();
    let xml_s = String::from_utf8(xml).unwrap();
    println!("xml len = {} fnv = {:016x}", xml_s.len(), fnv1a(xml_s.as_bytes()));
    print!("xml begin\n{xml_s}xml end\n");

    // ⑩ smart 标点单点对照（parse.smart 开/关）
    let smart_src = "\"quotes\" 'single' -- --- ... (c) (tm)\n";
    let mut smart_opts = ComrakOptions::default();
    smart_opts.parse.smart = true;
    println!("smart on: {:?}", markdown_to_html(smart_src, &smart_opts));
    println!("smart off: {:?}", markdown_to_html(smart_src, &def));
}
