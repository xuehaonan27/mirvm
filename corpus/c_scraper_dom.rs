#!/usr/bin/env mirvm
---
[dependencies]
scraper = "0.27"
---
// scraper 0.27.0（crates.io 最新稳定，发布于 2026-05-11；html5ever 0.39 /
// selectors 0.38 / ego-tree 0.11 代际，依赖闭包全纯 Rust，无 C/asm）DOM 差分。
// 钉版记录：任务钉 scraper = "0.27"，crates.io 实测 newest=0.27.0（2026-07-17
// 查询 API），无 yank/破洞，直接解析即该版本，无绕行。
//
// 测试面（批9：DOM 选择器 / 属性 / 序列化）：
//   两段内嵌常量串：DOC = 嵌套 div(id/class/data-* 混布) + table(thead/tbody、
//   row 奇偶类、cell key/val 类) + ul/li 列表(含嵌套子 ul) + h1/h2 + CJK/实体
//   (&amp; &lt; &#x41; &#8212;) + 重复属性(<p class="a" class="b"> 规范保首个)；
//   FRAG = 两个 <li> 裸片段走 parse_fragment。
//   ① 多档选择器命中序：tag(table/li/td/h1) / class(.row/.item/.cell/.note) /
//     id(#main/#data/#toc) / 后代(#main div p a、"table#data tbody tr td"、
//     ul.sub li) / 带属性([data-cat]、li[data-cat="veg"]、a[href][target="_blank"]、
//     td.cell[data-v]) / 复合(h1, h2 逗号、div:has(> ul#toc)、
//     ul#toc > li:nth-child(2))。每命中打印 tag#id.cls 速写 + text() 串联
//     （{:?} 转义单行），selector 级打印命中数 + 链 FNV。
//   ② text() 串联：逐命中打印分片数 + '|' join（暴露分片边界）与无边界直接串联。
//   ③ 属性提取：a 的 href/target、tr 的 data-n、td 的 data-v 定点取；全部命中
//     attrs() 按源码序全量 name=value 打印；id()/classes() 走 Element API。
//   ④ outer-html 序列化 FNV 锚定：#main / table#data / 首命中 .row / 文档全长
//     outer 各打 len+fnv；最短的 .row[0] 与 FRAG inner_html 全文原文打印。
//   ⑤ 其他 API 面：ElementRef::select 作用域内选择、child_elements /
//     descendent_elements 计数、Selector::matches、has_class 大小写两档、
//     Selector::parse 错误路径（err Debug 单行）。
// 确定性：全固定输入；选择命中序 = ego-tree 先序树序（确定）；attrs() 与
//   classes() 在 scraper 默认特性下内部 sort_unstable + dedup（属性按 QualName
//   序、类名按字典序，均确定）；无 HashMap 迭代/时间/随机/浮点 to_string；
//   CJK 与新行一律经 {:?} 转义为单行确定文本；stderr 真空；纯真渲染，零 IO。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_scraper_dom.rs
//   B: cd "$(grep -l 'name = "c_scraper_dom"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_scraper_dom.rs
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

/// 命中速写：tag#id.cls1.cls2（id/classes 均来源序，确定）。
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

/// 一档选择器：逐命中打印 速写 + text() 串联（{:?} 单行），末尾打命中数与链 FNV。
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

/// attrs() 源码序全量打印：name=value; ...
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

    // ---- 文档级锚点 ----
    println!("== doc ==");
    let all = Selector::parse("*").unwrap();
    println!("elems = {}", doc.select(&all).collect::<Vec<_>>().len());
    let full = doc.html();
    println!("doc_outer len={} fnv={:016x}", full.len(), fnv1a(full.as_bytes()));
    let title_sel = Selector::parse("title").unwrap();
    let title = doc.select(&title_sel).next().unwrap();
    println!("title text={:?}", title.text().collect::<String>());

    // ---- ① 多档选择器命中序 ----
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

    // ---- ② text() 串联：分片边界 vs 直接串联 ----
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

    // ---- ③ 属性提取 ----
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

    // ---- ④ outer-html 序列化 FNV 锚定 ----
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

    // ---- ⑤ 其他 API 面 ----
    println!("== api ==");
    let main_el = doc.select(&Selector::parse("#main").unwrap()).next().unwrap();
    println!(
        "main child_elems={} desc_elems={}",
        main_el.child_elements().count(),
        main_el.descendent_elements().count()
    );
    // 作用域内选择：先在 tbody 里再选 td.val
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
    // 选择器错误路径（Debug 派生输出，确定文本）
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
