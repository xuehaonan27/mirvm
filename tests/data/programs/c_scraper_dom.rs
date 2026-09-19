#!/usr/bin/env mirvm
---
[dependencies]
scraper = "0.27"
---
// scraper 0.27.0 differential (latest stable on crates.io, released 2026-05-11; the
// html5ever 0.39 / selectors 0.38 / ego-tree 0.11 generation, all-pure-Rust dependency
// closure with no C or asm). Pin: scraper = "0.27"; crates.io reports newest = 0.27.0, so
// the requirement resolves straight to it with no yank, gap or workaround.
//
// Test surface (DOM selectors / attributes / serialization):
//   Two embedded constant strings. DOC = nested divs (mixed id/class/data-*) + a table
//   (thead/tbody, odd/even row classes, cell key/val classes) + a ul/li list with a nested
//   ul + h1/h2 + CJK and entities (&amp; &lt; &#x41; &#8212;) + a duplicate attribute
//   (<p class="a" class="b">, normalized to keep the first). FRAG = two bare <li> elements
//   parsed through parse_fragment.
//   ① Multi-tier selector hit order: tag (table/li/td/h1); class (.row/.item/.cell/.note);
//     id (#main/#data/#toc); descendant (#main div p a, "table#data tbody tr td", ul.sub li);
//     attribute ([data-cat], li[data-cat="veg"], a[href][target="_blank"], td.cell[data-v]);
//     compound (h1, h2 comma list; div:has(> ul#toc); ul#toc > li:nth-child(2)). Each hit
//     prints a tag#id.cls brief plus its text() concatenation ({:?}-escaped onto one line);
//     the selector level prints the hit count and a chain FNV.
//   ② text() concatenation: per hit, the piece count plus a '|' join (which exposes the
//     piece boundaries) and the boundary-free concatenation.
//   ③ Attribute extraction: fixed picks for a's href/target, tr's data-n and td's data-v;
//     for every hit, attrs() printed in full source order as name=value; id()/classes()
//     through the Element API.
//   ④ outer-html serialization anchored by FNV: #main, table#data, the first .row and the
//     whole document each print len+fnv; the shortest one (.row[0]) and the FRAG inner_html
//     are printed verbatim.
//   ⑤ Other API surface: scoped ElementRef::select, child_elements / descendent_elements
//     counts, Selector::matches, has_class in both case modes, and the Selector::parse
//     error path (err Debug on one line).
// Deterministic: fixed inputs throughout; hit order is ego-tree pre-order (deterministic);
//   attrs() and classes() sort_unstable + dedup internally under scraper's default features
//   (attributes by QualName, class names lexicographically -- both deterministic); no
//   HashMap iteration, time, randomness or float to_string; CJK and newlines always go
//   through {:?} into deterministic one-line text; empty stderr; pure rendering, zero IO.
//
// Run: target/release/mirvm run tests/data/programs/c_scraper_dom.rs
use scraper::{CaseSensitivity, ElementRef, Html, Selector};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const DOC: &str = r##"<!DOCTYPE html>
<html lang="zh">
<head><title>scraper 差分·示例 &amp; 样本</title><meta charset="utf-8" data-k="m&#x41;in"></head>
<body>
<div id="main" class="wrap wide" data-cat="root" data-n="1">
  <h1 class="title big">总目 &lt;一&gt;</h1>
  <h2 class="title">副目 A</h2>
  <h2 class="title alt">副目 B &#8212; 二号</h2>
  <div class="content" data-cat="body" data-n="2">
    <p class="note" data-x="a&lt;b">图灵 &amp; 冯·诺依曼 &#8212; 1936</p>
    <p id="dup" class="first" class="second" data-x="dup">重复属性：规范保留首个 class</p>
    <p class="note tail" data-x="c">链接见 <a href="/doc/a" target="_blank" data-cat="nav">甲页</a> 与 <a href="https://example.test/b?x=1&amp;y=2" target="_self">乙页</a>。</p>
    <ul class="list" id="toc">
      <li class="item" data-cat="veg">青菜</li>
      <li class="item star" data-cat="fruit">苹果 <b>红</b></li>
      <li class="item" data-cat="veg">萝卜
        <ul class="sub">
          <li class="item sub" data-cat="veg">心里美</li>
          <li class="item sub" data-cat="grain">小米</li>
        </ul>
      </li>
    </ul>
  </div>
  <table id="data" class="grid" data-cat="table">
    <thead><tr class="row head" data-n="h"><th class="cell key" data-k="name">名称</th><th class="cell val" data-k="num">数量</th></tr></thead>
    <tbody>
      <tr class="row odd" data-n="1"><td class="cell key">alpha</td><td class="cell val" data-v="3">3</td></tr>
      <tr class="row even" data-n="2"><td class="cell key">beta</td><td class="cell val" data-v="41">41</td></tr>
      <tr class="row odd hi" data-n="3"><td class="cell key" data-cat="hot">gamma 伽马</td><td class="cell val" data-v="7">7</td></tr>
    </tbody>
  </table>
  <footer id="foot" data-empty="" class="note small">页脚 &middot; 终</footer>
</div>
</body>
</html>
"##;

const FRAG: &str = r##"<li class="fitem" data-cat="x">frag-α &lt;i&gt;</li><li class="fitem">frag-β &#x3B2;</li>tail"##;

/// Hit brief: tag#id.cls1.cls2 (id and classes both come out in source order, deterministically).
fn brief(e: &ElementRef) -> String {
    let v = e.value();
    let mut s = String::from(v.name());
    if let Some(id) = v.id() {
        s.push('#');
        s.push_str(id);
    }
    for c in v.classes() {
        s.push('.');
        s.push_str(c);
    }
    s
}

/// Per selector: each hit's brief + text() join ({:?}, one line); then hit count and chain FNV.
fn dump_select(doc: &Html, sel_str: &str) {
    let sel = Selector::parse(sel_str).unwrap();
    let hits: Vec<ElementRef> = doc.select(&sel).collect();
    let mut chain = String::new();
    for (i, e) in hits.iter().enumerate() {
        let t: String = e.text().collect();
        println!("  hit[{i}] {} text={t:?}", brief(e));
        chain.push_str(&t);
        chain.push('\u{1}');
    }
    println!(
        "sel {sel_str:?} hits={} chain_fnv={:016x}",
        hits.len(),
        fnv1a(chain.as_bytes())
    );
}

/// Full attrs() dump in source order: name=value; ...
fn dump_all_attrs(e: &ElementRef) {
    let parts: Vec<String> = e
        .value()
        .attrs()
        .map(|(k, v)| format!("{k}={v:?}"))
        .collect();
    println!("  attrs {} n={} [{}]", brief(e), parts.len(), parts.join("; "));
}

fn main() {
    let doc = Html::parse_document(DOC);

    // ---- Document-level anchors ----
    println!("== doc ==");
    let all = Selector::parse("*").unwrap();
    println!("elems = {}", doc.select(&all).collect::<Vec<_>>().len());
    let full = doc.html();
    println!("doc_outer len={} fnv={:016x}", full.len(), fnv1a(full.as_bytes()));
    let title_sel = Selector::parse("title").unwrap();
    let title = doc.select(&title_sel).next().unwrap();
    println!("title text={:?}", title.text().collect::<String>());

    // ---- ① Multi-tier selector hit order ----
    println!("== tier:tag ==");
    for s in ["table", "li", "td", "h1"] {
        dump_select(&doc, s);
    }
    println!("== tier:class ==");
    for s in [".row", ".item", ".cell", ".note"] {
        dump_select(&doc, s);
    }
    println!("== tier:id ==");
    for s in ["#main", "#data", "#toc"] {
        dump_select(&doc, s);
    }
    println!("== tier:descendant ==");
    for s in ["#main div p a", "table#data tbody tr td", "ul.sub li"] {
        dump_select(&doc, s);
    }
    println!("== tier:attr ==");
    for s in [
        "[data-cat]",
        "li[data-cat=\"veg\"]",
        "a[href][target=\"_blank\"]",
        "td.cell[data-v]",
        "[data-empty]",
    ] {
        dump_select(&doc, s);
    }
    println!("== tier:compound ==");
    for s in ["h1, h2", "div:has(> ul#toc) > h1", "ul#toc > li:nth-child(2)"] {
        dump_select(&doc, s);
    }

    // ---- ② text() concatenation: piece boundaries vs direct concatenation ----
    println!("== text pieces ==");
    let li_sel = Selector::parse("li.item").unwrap();
    let mut cat_chain = String::new();
    for (i, e) in doc.select(&li_sel).enumerate() {
        let pieces: Vec<&str> = e.text().collect();
        let joined = pieces.join("|");
        let flat: String = pieces.concat();
        println!(
            "  li[{i}] pieces={} joined={joined:?} flat={flat:?}",
            pieces.len()
        );
        cat_chain.push_str(&flat);
    }
    println!("li flat_fnv={:016x}", fnv1a(cat_chain.as_bytes()));

    // ---- ③ Attribute extraction ----
    println!("== attrs ==");
    let a_sel = Selector::parse("a").unwrap();
    for (i, e) in doc.select(&a_sel).enumerate() {
        println!(
            "  a[{i}] href={:?} target={} data-cat={}",
            e.value().attr("href"),
            e.value().attr("target").unwrap_or("-"),
            e.value().attr("data-cat").unwrap_or("-")
        );
        dump_all_attrs(&e);
    }
    let tr_sel = Selector::parse("tr.row").unwrap();
    for (i, e) in doc.select(&tr_sel).enumerate() {
        let v = e.value();
        println!(
            "  tr[{i}] id={} data-n={} classes={:?}",
            v.id().unwrap_or("-"),
            v.attr("data-n").unwrap_or("-"),
            v.classes().collect::<Vec<_>>()
        );
        dump_all_attrs(&e);
    }
    let meta = doc
        .select(&Selector::parse("meta[data-k]").unwrap())
        .next()
        .unwrap();
    println!("meta data-k={:?}", meta.value().attr("data-k"));
    let dup = doc.select(&Selector::parse("#dup").unwrap()).next().unwrap();
    println!(
        "dup class={:?} classes_n={}",
        dup.value().attr("class"),
        dup.value().classes().count()
    );
    let th = doc
        .select(&Selector::parse("th.cell").unwrap())
        .next()
        .unwrap();
    println!(
        "th has_class key ci={} KEY cs={} KEY ci={}",
        th.value().has_class("key", CaseSensitivity::AsciiCaseInsensitive),
        th.value().has_class("KEY", CaseSensitivity::CaseSensitive),
        th.value().has_class("KEY", CaseSensitivity::AsciiCaseInsensitive)
    );

    // ---- ④ outer-html serialization anchored by FNV ----
    println!("== outer html ==");
    for s in ["#main", "table#data", "#toc"] {
        let sel = Selector::parse(s).unwrap();
        let e = doc.select(&sel).next().unwrap();
        let h = e.html();
        println!("outer {s} len={} fnv={:016x}", h.len(), fnv1a(h.as_bytes()));
    }
    let row_sel = Selector::parse(".row").unwrap();
    let row0 = doc.select(&row_sel).next().unwrap();
    println!("row0 outer |{}|", row0.html());
    let inner_toc = doc.select(&Selector::parse("#toc").unwrap()).next().unwrap();
    let ih = inner_toc.inner_html();
    println!("toc inner len={} fnv={:016x}", ih.len(), fnv1a(ih.as_bytes()));

    // ---- ⑤ Other API surface ----
    println!("== api ==");
    let main_el = doc.select(&Selector::parse("#main").unwrap()).next().unwrap();
    println!(
        "main child_elems={} desc_elems={}",
        main_el.child_elements().count(),
        main_el.descendent_elements().count()
    );
    // Scoped selection: pick tbody first, then td.val inside it
    let tbody = doc
        .select(&Selector::parse("tbody").unwrap())
        .next()
        .unwrap();
    let vals: Vec<String> = tbody
        .select(&Selector::parse("td.val").unwrap())
        .map(|e| e.text().collect())
        .collect();
    println!("scoped td.val = {:?}", vals);
    // Selector::matches
    let odd = Selector::parse("tr.odd").unwrap();
    let star = Selector::parse(".star").unwrap();
    for (i, e) in doc.select(&Selector::parse("li").unwrap()).enumerate() {
        println!("li[{i}] matches tr.odd={} .star={}", odd.matches(&e), star.matches(&e));
    }
    // Selector error paths (derived Debug output, deterministic text)
    for bad in ["li > > a", "[[", "p:nth-child()"] {
        match Selector::parse(bad) {
            Ok(_) => println!("badsel {bad:?} = unexpected-ok"),
            Err(e) => println!("badsel {bad:?} = {e:?}"),
        }
    }

    // ---- parse_fragment ----
    println!("== fragment ==");
    let frag = Html::parse_fragment(FRAG);
    let fsel = Selector::parse("li.fitem").unwrap();
    for (i, e) in frag.select(&fsel).enumerate() {
        println!(
            "frag li[{i}] {} text={:?} cat={}",
            brief(&e),
            e.text().collect::<String>(),
            e.value().attr("data-cat").unwrap_or("-")
        );
    }
    let fih = frag.root_element().inner_html();
    println!("frag root_inner len={} fnv={:016x}", fih.len(), fnv1a(fih.as_bytes()));
    println!("frag root_inner |{fih}|");
}
