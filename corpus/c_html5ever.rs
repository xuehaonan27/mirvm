#!/usr/bin/env mirvm
---
[dependencies]
html5ever = "0.29"
markup5ever_rcdom = "=0.5.0-unofficial"
---
// html5ever (servo's spec-level HTML5 parser) differential: parse_document ->
// RcDom -> serialize. The pin html5ever = "0.29" resolves to 0.29.1, because
// 0.29.2 was yanked on 2025-03-13 after three days and "0.29" cannot reach it;
// the feature set stays at default. The DOM slot uses
// markup5ever_rcdom =0.5.0-unofficial: official 0.3.0 is stuck on the html5ever
// 0.27 generation (markup5ever 0.12) and cannot work with 0.29.x (markup5ever
// 0.14), while 0.5.0-unofficial is Kornel's rcdom published from the same
// servo/html5ever repository with html5ever ^0.29 + markup5ever ^0.14, exactly
// aligned with 0.29.1. (0.5.1-unofficial requires the yanked html5ever ^0.29.2
// and does not resolve). The run harness pins it as html5ever = "c_html5ever".
// xml5ever comes in as a non-optional rcdom dependency but is unused here. The
// whole closure is pure Rust (string_cache/phf/tendril/parking_lot), no C/asm.
//
// Four embedded fragments are parsed with parse_document (default ParseOpts,
// DOCTYPE preserved), walked through RcDom and written back by serialize
// (default SerializeOpts, ChildrenOnly):
// (1) entities: named/decimal/hex character references (&amp; &lt; &#39; &#x41;
//     &#X42; &notin; &nbsp;), the semicolon-less legacy entity &not (allowed
//     decoding plus a deterministic parse error), and entity decoding in
//     attribute values followed by re-normalized escaping.
// (2) deep48: a programmatically built 48-level div/span alternation with CJK
//     leaf text, stressing the tree builder stack and the recursive serializer.
// (3) malformed: error-tolerant recovery as defined by the HTML5 spec -- implicit
//     p/li closing, a missing tbody created, foster parenting of bare table text,
//     b/i adoption agency, implied end tags, </br> rewritten to <br>.
// (4) cjk: Chinese/Japanese title, p and attribute values, multi-byte UTF-8.
// Per fragment the anchors are quirks mode, parse-error count, tree stats
// (nodes/elems/depth), the serialized text and an FNV-1a; key nodes (tag/attr/
// text) get existence asserts and prints.
//
//
//
//
//
//
//
use std::fmt::Write as _;

use html5ever::driver::ParseOpts;
use html5ever::tendril::TendrilSink;
use html5ever::{parse_document, serialize};
use markup5ever_rcdom::{Handle, NodeData, RcDom, SerializableHandle};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const F1_ENTITIES: &str = "<!DOCTYPE html>\n\
<html><head><title>Entity &amp; escape</title></head><body>\n\
<p class=\"note\" data-mix=\"a&lt;b&amp;c&quot;d\">\
5 &lt; 6 &amp; 7 &gt; 2; &quot;quoted&quot;; &#39;apos&#39;; \
hex &#x41;&#X42;; &notin;; &nbsp;pad; &not semi</p>\n\
<p id=\"raw\">1 &lt; 2 is truthy</p>\n\
</body></html>\n";

const F3_MALFORMED: &str = "<!DOCTYPE html><html><body>\n\
<p>alpha\n<p>beta\n\
<ul><li>one<li>two<li>three</ul>\n\
<table>foster<tr><td>x<td>y<tr><td>z</table>tail\n\
<b>bold<i>both</b>italic</i>\n\
<div><p>stray</div>\n\
</br>\n\
<!--tail comment-->\n\
</body></html>\n";

const F4_CJK: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<title>中文标题と日本語のタイトル</title></head>\n\
<body><p lang=\"zh\">你好，世界。</p><p lang=\"ja\">東京・大阪・京都</p>\
<p data-e=\"繁简\">繁體中文與简体中文</p></body></html>\n";

/// 程序化深嵌套：48 层 div(奇)/span(偶) 交替 + CJK 叶文本。
fn build_deep() -> String {
    let mut s = String::from("<!DOCTYPE html><html><body>");
    for i in 1..=48usize {
        if i % 2 == 1 {
            write!(s, "<div id=\"d{i}\">").unwrap();
        } else {
            write!(s, "<span data-n=\"{i}\">").unwrap();
        }
    }
    s.push_str("葉leaf文本");
    for i in (1..=48usize).rev() {
        if i % 2 == 1 {
            s.push_str("</div>");
        } else {
            s.push_str("</span>");
        }
    }
    s.push_str("</body></html>");
    s
}

/// (总节点数, 元素节点数, 最大深度)；children Vec 序 DFS，确定。
fn stats(h: &Handle, depth: usize, out: &mut (usize, usize, usize)) {
    out.0 += 1;
    if matches!(h.data, NodeData::Element { .. }) {
        out.1 += 1;
    }
    if depth > out.2 {
        out.2 = depth;
    }
    for c in h.children.borrow().iter() {
        stats(c, depth + 1, out);
    }
}

/// 先序 DFS 找任一满足 pred 的节点。
fn any_node(h: &Handle, pred: &mut dyn FnMut(&Handle) -> bool) -> bool {
    if pred(h) {
        return true;
    }
    for c in h.children.borrow().iter() {
        if any_node(c, pred) {
            return true;
        }
    }
    false
}

fn has_tag(root: &Handle, tag: &str) -> bool {
    any_node(root, &mut |h| match &h.data {
        NodeData::Element { name, .. } => &*name.local == tag,
        _ => false,
    })
}

fn count_tag(root: &Handle, tag: &str) -> usize {
    let mut n = 0usize;
    any_node(root, &mut |h| {
        if let NodeData::Element { name, .. } = &h.data {
            if &*name.local == tag {
                n += 1;
            }
        }
        false
    });
    n
}

fn has_attr(root: &Handle, tag: &str, attr: &str, val: &str) -> bool {
    any_node(root, &mut |h| match &h.data {
        NodeData::Element { name, attrs, .. } if &*name.local == tag => attrs
            .borrow()
            .iter()
            .any(|a| &*a.name.local == attr && &*a.value == val),
        _ => false,
    })
}

fn has_text(root: &Handle, needle: &str) -> bool {
    any_node(root, &mut |h| match &h.data {
        NodeData::Text { contents } => contents.borrow().contains(needle),
        _ => false,
    })
}

/// 解析 + 序列化一个片段，打印全量锚点，返回 document 句柄供存在性检查。
fn parse_serialize(label: &str, html: &str) -> Handle {
    let mut input = html.as_bytes();
    let dom = parse_document(RcDom::default(), ParseOpts::default())
        .from_utf8()
        .read_from(&mut input)
        .unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let doc: SerializableHandle = dom.document.clone().into();
    serialize(&mut buf, &doc, Default::default()).unwrap();
    let ser = String::from_utf8(buf).unwrap();
    let mut st = (0usize, 0usize, 0usize);
    stats(&dom.document, 0, &mut st);
    println!("== {label} ==");
    println!("quirks = {:?}", dom.quirks_mode.get());
    println!("errors = {}", dom.errors.borrow().len());
    println!("nodes={} elems={} depth={}", st.0, st.1, st.2);
    println!("ser_fnv = {:016x}", fnv1a(ser.as_bytes()));
    println!("ser = {ser}");
    dom.document
}

fn check(name: &str, ok: bool) {
    assert!(ok, "check failed: {name}");
    println!("check {name} = {ok}");
}

fn main() {
    // ① 实体转义。
    let d1 = parse_serialize("entities", F1_ENTITIES);
    check("p.note", has_attr(&d1, "p", "class", "note"));
    check("attr-entity-mix", has_attr(&d1, "p", "data-mix", "a<b&c\"d"));
    check("notin-char", has_text(&d1, "∉"));
    check("hex-entity", has_text(&d1, "hex AB"));
    check("nbsp-char", has_text(&d1, "\u{a0}pad"));
    check("legacy-not", has_text(&d1, "¬ semi"));
    println!();

    // ② 48 层深嵌套。
    let f2 = build_deep();
    let d2 = parse_serialize("deep48", &f2);
    check("div d1", has_attr(&d2, "div", "id", "d1"));
    check("div d47", has_attr(&d2, "div", "id", "d47"));
    check("span n48", has_attr(&d2, "span", "data-n", "48"));
    check("leaf cjk", has_text(&d2, "葉leaf文本"));
    println!();

    // ③ 畸形容错闭合。
    let d3 = parse_serialize("malformed", F3_MALFORMED);
    check("tbody auto", has_tag(&d3, "tbody"));
    check("tds=3", count_tag(&d3, "td") == 3);
    check("lis=3", count_tag(&d3, "li") == 3);
    check("brs=1", count_tag(&d3, "br") == 1);
    check("foster text", has_text(&d3, "foster"));
    check("italic text", has_text(&d3, "italic"));
    println!();

    // ④ 中日文。
    let d4 = parse_serialize("cjk", F4_CJK);
    check("lang zh", has_attr(&d4, "p", "lang", "zh"));
    check("lang ja", has_attr(&d4, "p", "lang", "ja"));
    check("title text", has_text(&d4, "中文标题と日本語のタイトル"));
    check("tokyo", has_text(&d4, "東京・大阪・京都"));
    check("trad-simp attr", has_attr(&d4, "p", "data-e", "繁简"));
}
