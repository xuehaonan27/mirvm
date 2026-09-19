#!/usr/bin/env mirvm
---
[dependencies]
# Pin organic =0.1.16: the only actively maintained parser in the org-mode
# family, and pure Rust with no SIMD dependency.
# Its hot path is nom+memchr+minimal-lexical with a hand-written state machine,
# so no runtime CPU dispatch and no x86 intrinsic surface beyond std.
# It uses nightly feature gates (exit_status_error / trait_alias / test /
# iter_intersperse, ...) and compiles with zero warnings on the pinned
# nightly-2026-07-02 toolchain.
# All features off (the crate defaults to default=[], so the compare, tracing and
# wasm features stay opt-in and disabled).
# The mandatory dependency closure is ~22 crates: the nom parsing trio plus the
# gloo-utils family. wasm-bindgen, js-sys and web-sys only compile on Linux
# x86-64 and never execute, and this driver never touches them.
# It is pinned rather than floated, so the AST shape, the variant set and the
# dependency closure stay fixed across runs of this fixture and of the jit path.
# No optional feature is enabled, so the compiled surface is the parser alone.
organic = "=0.1.16"
---
// organic 0.1.16: org-mode text differential.
// One embedded fixed org document is parsed once by organic and then walked
// manually, because the iter module is private in v0.1.16. The document covers:
//   * #+TITLE and #+OPTIONS keywords;
//   * three heading levels carrying TODO/DONE, tags, SCHEDULED and CLOCK;
//   * inline bold / italic / code / verbatim markup;
//   * regular [[proto://path][desc]] links and plain links;
//   * a footnote reference plus its definition;
//   * mixed unordered/ordered lists with a checkbox item;
//   * a rust source block with switches and parameters;
//   * a table with a rule row;
//   * an active recurring timestamp.
// Every Element/Object variant plus Document, Heading, Section, PlainListItem,
// TableRow, TableCell and Timestamp is tallied into a BTreeMap in key order.
// Anchors are printed in document order: #+ keywords; the outline with level,
// todo keyword, tags and raw title; the first source block's language, switches,
// parameters, line count and first line; the table's cell text per row, with the
// rule row recognized; the first list's type, bullet and checkbox; the first
// regular and plain link; the first bold, italic, code and verbatim contents;
// the first CLOCK's status, duration and timestamp type; the footnote label.
// Determinism: BTreeMap key order plus document order; no floating point, hash
// iteration, time or address; stderr is empty, and two native runs are
// byte-for-byte identical.
use std::collections::BTreeMap;

use organic::parser::parse;
use organic::types::{Document, DocumentElement, Element, Heading, Object, Section};

const ORG_DOC: &str = "#+TITLE: mirvm organic differential sample\n\
#+OPTIONS: toc:nil num:nil\n\
\n\
* TODO 撰写引擎报告 :work:jit:\n\
SCHEDULED: <2026-07-20 Mon>\n\
CLOCK: [2026-07-17 Fri 09:00]--[2026-07-17 Fri 10:30] =>  1:30\n\
正文段落含 *bold 强调文本* /italic 斜体/ ~code 字面量~ =verbatim 原文= 与\n\
regular 链接 [[https://example.com/mirvm][项目主页]]，plain 链接\n\
https://mirvm.example.org/docs 以及脚注引用[fn:1]。\n\
\n\
** DONE 完成 intrinsic 内建\n\
   - 无序列表首项\n\
   - [X] 已勾选复选框项\n\
   1. 序号项一\n\
   2. 序号项二\n\
\n\
*** 源码块章节\n\
#+begin_src rust -n :tangle none\n\
fn fib(n: u64) -> u64 {\n\
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }\n\
}\n\
#+end_src\n\
\n\
*** 表格章节\n\
| 名称    | 通过 | 总数 |\n\
|---------+------+------|\n\
| gate5   |  103 |  103 |\n\
| corpus7 |   12 |   12 |\n\
\n\
* 第二层根标题\n\
另一段落引用 <2026-07-18 Sat 18:00 +1d> 定期时间戳。\n\
\n\
[fn:1] 脚注定义正文一行。\n";

#[derive(Default)]
struct Stats<'r> {
    counts: BTreeMap<&'static str, u32>,
    keywords: Vec<String>,
    outline: Vec<String>,
    first_src: Option<(Option<&'r str>, Option<&'r str>, Option<&'r str>, &'r str)>,
    first_table: Option<Vec<Vec<String>>>,
    first_regular_link: Option<String>,
    first_plain_link: Option<&'r str>,
    first_bold: Option<&'r str>,
    first_italic: Option<&'r str>,
    first_code: Option<&'r str>,
    first_verbatim: Option<&'r str>,
    first_clock: Option<String>,
    first_timestamp: Option<String>,
    first_list: Option<String>,
    first_footnote: Option<&'r str>,
}

fn bump(s: &mut Stats, name: &'static str) {
    *s.counts.entry(name).or_insert(0) += 1;
}

fn walk_objects<'a>(objs: &[Object<'a>], s: &mut Stats<'a>) {
    for o in objs {
        match o {
            Object::Bold(x) => {
                bump(s, "Bold");
                if s.first_bold.is_none() {
                    s.first_bold = Some(x.contents);
                }
                walk_objects(&x.children, s);
            }
            Object::Italic(x) => {
                bump(s, "Italic");
                if s.first_italic.is_none() {
                    s.first_italic = Some(x.contents);
                }
                walk_objects(&x.children, s);
            }
            Object::Underline(x) => {
                bump(s, "Underline");
                walk_objects(&x.children, s);
            }
            Object::StrikeThrough(x) => {
                bump(s, "StrikeThrough");
                walk_objects(&x.children, s);
            }
            Object::Code(x) => {
                bump(s, "Code");
                if s.first_code.is_none() {
                    s.first_code = Some(x.contents);
                }
            }
            Object::Verbatim(x) => {
                bump(s, "Verbatim");
                if s.first_verbatim.is_none() {
                    s.first_verbatim = Some(x.contents);
                }
            }
            Object::PlainText(_) => bump(s, "PlainText"),
            Object::RegularLink(x) => {
                bump(s, "RegularLink");
                if s.first_regular_link.is_none() {
                    s.first_regular_link = Some(format!("{:?} {}", x.link_type, x.get_path()));
                }
            }
            Object::RadioLink(_) => bump(s, "RadioLink"),
            Object::RadioTarget(_) => bump(s, "RadioTarget"),
            Object::PlainLink(x) => {
                bump(s, "PlainLink");
                if s.first_plain_link.is_none() {
                    s.first_plain_link = Some(x.raw_link);
                }
            }
            Object::AngleLink(_) => bump(s, "AngleLink"),
            Object::OrgMacro(_) => bump(s, "OrgMacro"),
            Object::Entity(_) => bump(s, "Entity"),
            Object::LatexFragment(_) => bump(s, "LatexFragment"),
            Object::ExportSnippet(_) => bump(s, "ExportSnippet"),
            Object::FootnoteReference(_) => bump(s, "FootnoteReference"),
            Object::Citation(_) => bump(s, "Citation"),
            Object::CitationReference(_) => bump(s, "CitationReference"),
            Object::InlineBabelCall(_) => bump(s, "InlineBabelCall"),
            Object::InlineSourceBlock(_) => bump(s, "InlineSourceBlock"),
            Object::LineBreak(_) => bump(s, "LineBreak"),
            Object::Target(_) => bump(s, "Target"),
            Object::StatisticsCookie(_) => bump(s, "StatisticsCookie"),
            Object::Subscript(x) => {
                bump(s, "Subscript");
                walk_objects(&x.children, s);
            }
            Object::Superscript(x) => {
                bump(s, "Superscript");
                walk_objects(&x.children, s);
            }
            Object::Timestamp(x) => {
                bump(s, "Timestamp");
                if s.first_timestamp.is_none() {
                    s.first_timestamp = Some(format!("{:?} {}", x.timestamp_type, x.source));
                }
            }
        }
    }
}

fn walk_elements<'a>(els: &[Element<'a>], s: &mut Stats<'a>) {
    for e in els {
        match e {
            Element::Paragraph(x) => {
                bump(s, "Paragraph");
                walk_objects(&x.children, s);
            }
            Element::PlainList(x) => {
                bump(s, "PlainList");
                if s.first_list.is_none() {
                    let items: Vec<String> = x
                        .children
                        .iter()
                        .map(|it| {
                            let cb = it
                                .checkbox
                                .as_ref()
                                .map(|(ty, _)| format!("{:?}", ty))
                                .unwrap_or_else(|| "none".to_string());
                            format!("{}@{}", it.bullet, cb)
                        })
                        .collect();
                    s.first_list = Some(format!("{:?} {}", x.list_type, items.join(",")));
                }
                for it in &x.children {
                    bump(s, "PlainListItem");
                    walk_elements(&it.children, s);
                }
            }
            Element::CenterBlock(x) => {
                bump(s, "CenterBlock");
                walk_elements(&x.children, s);
            }
            Element::QuoteBlock(x) => {
                bump(s, "QuoteBlock");
                walk_elements(&x.children, s);
            }
            Element::SpecialBlock(x) => {
                bump(s, "SpecialBlock");
                walk_elements(&x.children, s);
            }
            Element::DynamicBlock(x) => {
                bump(s, "DynamicBlock");
                walk_elements(&x.children, s);
            }
            Element::FootnoteDefinition(x) => {
                bump(s, "FootnoteDefinition");
                if s.first_footnote.is_none() {
                    s.first_footnote = Some(x.label);
                }
                walk_elements(&x.children, s);
            }
            Element::Comment(_) => bump(s, "Comment"),
            Element::Drawer(x) => {
                bump(s, "Drawer");
                walk_elements(&x.children, s);
            }
            Element::PropertyDrawer(x) => {
                bump(s, "PropertyDrawer");
                for _ in &x.children {
                    bump(s, "NodeProperty");
                }
            }
            Element::Table(x) => {
                bump(s, "Table");
                if s.first_table.is_none() {
                    s.first_table = Some(
                        x.children
                            .iter()
                            .map(|row| {
                                if row.children.is_empty() {
                                    vec!["<rule>".to_string()]
                                } else {
                                    row.children
                                        .iter()
                                        .map(|c| c.contents.trim().to_string())
                                        .collect()
                                }
                            })
                            .collect(),
                    );
                }
                for row in &x.children {
                    bump(s, "TableRow");
                    for cell in &row.children {
                        bump(s, "TableCell");
                        walk_objects(&cell.children, s);
                    }
                }
            }
            Element::VerseBlock(x) => {
                bump(s, "VerseBlock");
                walk_objects(&x.children, s);
            }
            Element::CommentBlock(_) => bump(s, "CommentBlock"),
            Element::ExampleBlock(_) => bump(s, "ExampleBlock"),
            Element::ExportBlock(_) => bump(s, "ExportBlock"),
            Element::SrcBlock(x) => {
                bump(s, "SrcBlock");
                if s.first_src.is_none() {
                    s.first_src = Some((x.language, x.switches, x.parameters, x.value));
                }
            }
            Element::Clock(x) => {
                bump(s, "Clock");
                bump(s, "Timestamp");
                if s.first_clock.is_none() {
                    s.first_clock = Some(format!(
                        "{:?} {:?} {}",
                        x.status,
                        x.timestamp.timestamp_type,
                        x.duration.unwrap_or("-")
                    ));
                }
            }
            Element::DiarySexp(_) => bump(s, "DiarySexp"),
            Element::Planning(_) => bump(s, "Planning"),
            Element::FixedWidthArea(_) => bump(s, "FixedWidthArea"),
            Element::HorizontalRule(_) => bump(s, "HorizontalRule"),
            Element::Keyword(x) => {
                bump(s, "Keyword");
                s.keywords.push(format!("{}={}", x.key, x.value));
            }
            Element::BabelCall(_) => bump(s, "BabelCall"),
            Element::LatexEnvironment(_) => bump(s, "LatexEnvironment"),
        }
    }
}

fn walk_section<'a>(sec: &Section<'a>, s: &mut Stats<'a>) {
    bump(s, "Section");
    walk_elements(&sec.children, s);
}

fn walk_heading<'a>(h: &Heading<'a>, s: &mut Stats<'a>) {
    bump(s, "Heading");
    let todo = h
        .todo_keyword
        .as_ref()
        .map(|(kind, kw)| match kind {
            organic::types::TodoKeywordType::Todo => format!("TODO:{}", kw),
            organic::types::TodoKeywordType::Done => format!("DONE:{}", kw),
        })
        .unwrap_or_else(|| "-".to_string());
    let indent = "  ".repeat(usize::from(h.level.min(8)));
    s.outline.push(format!(
        "{}L{} {} [{}] {}",
        indent,
        h.level,
        todo,
        h.tags.join(","),
        h.get_raw_value()
    ));
    for ts in [&h.scheduled, &h.deadline, &h.closed] {
        if ts.is_some() {
            bump(s, "Timestamp");
        }
    }
    walk_objects(&h.title, s);
    for c in &h.children {
        match c {
            DocumentElement::Heading(sub) => walk_heading(sub, s),
            DocumentElement::Section(sec) => walk_section(sec, s),
        }
    }
}

fn walk_document<'a>(doc: &Document<'a>, s: &mut Stats<'a>) {
    bump(s, "Document");
    if let Some(z) = &doc.zeroth_section {
        walk_section(z, s);
    }
    for h in &doc.children {
        walk_heading(h, s);
    }
}

fn main() {
    let doc = parse(ORG_DOC).expect("organic::parse failed on fixed document");
    let mut s = Stats::default();
    walk_document(&doc, &mut s);

    println!("== counts ==");
    for (name, n) in &s.counts {
        println!("{name}: {n}");
    }
    println!("== keywords ==");
    for kw in &s.keywords {
        println!("{kw}");
    }
    println!("== outline ==");
    for line in &s.outline {
        println!("{line}");
    }
    println!("== first-src ==");
    let (lang, switches, params, value) = s.first_src.expect("missing src block");
    println!("lang={}", lang.unwrap_or("-"));
    println!("switches={}", switches.unwrap_or("-"));
    println!("params={}", params.unwrap_or("-"));
    println!("value-lines={}", value.lines().count());
    println!("first-line={}", value.lines().next().unwrap_or("-").trim());
    println!("== first-table ==");
    let rows = s.first_table.expect("missing table");
    println!("rows={}", rows.len());
    for row in &rows {
        println!("|{}", row.join("|"));
    }
    println!("== first-list ==");
    println!("{}", s.first_list.expect("missing list"));
    println!("== links ==");
    println!("regular={}", s.first_regular_link.expect("missing regular link"));
    println!("plain={}", s.first_plain_link.expect("missing plain link"));
    println!("== markup ==");
    println!("bold={}", s.first_bold.expect("missing bold").trim());
    println!("italic={}", s.first_italic.expect("missing italic").trim());
    println!("code={}", s.first_code.expect("missing code").trim());
    println!("verbatim={}", s.first_verbatim.expect("missing verbatim").trim());
    println!("== time ==");
    println!("clock={}", s.first_clock.expect("missing clock"));
    println!("timestamp={}", s.first_timestamp.expect("missing timestamp"));
    println!("footnote={}", s.first_footnote.expect("missing footnote"));
}
