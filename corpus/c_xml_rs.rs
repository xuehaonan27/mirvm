#!/usr/bin/env mirvm
---
[dependencies]
xml-rs = "0.8"
---
// xml-rs 0.8（解析为 0.8.28；零依赖、无 feature，default 即最小闭包）EventReader
// 事件流差分。xml-rs 是纯 Rust 的 pull parser（手写 lexer，无 SIMD/unsafe 面）。
// 测试面：
//  ① 样本 XML 全事件遍历（ParserConfig.ignore_comments(false)，其余全默认）：
//     XML 声明（version/encoding/standalone）、处理指令 ×2、DOCTYPE（含内部子集；
//     0.8 无 doctype 事件，lexer 消费其内容后静默跳过——本 driver 验证跳过行为）、
//     注释、默认+前缀命名空间（解析后元素/属性的 prefix/local/namespace URI、
//     root 元素的全量 ns 映射表按 BTreeMap 序打印）、文档序属性表（未去重）、
//     CDATA（含结尾 `]] ` 边界 + 未转义 `<` `&` `"`）、实体引用
//     （&amp; &#65; &#x41; 字符引用与内经实体，含 &#x4E 中文码点走 UTF-8 解码）、
//     空元素自闭合（StartElement 后紧跟 EndElement）、Whitespace 事件谱系
//     （coalesce 默认合并相邻 Characters）。
//  ② 事件流锚定：9 类事件计数 + 每事件单字符 shape 序列（D/S/E/C/W/M/T/P/X）。
//  ③ well-formedness 错误定位：不匹配闭合标签（<b> 未闭直接 </root>）——打印
//     错误处 row/column（0 基 TextPosition）+ Display 全文 + 出错前 shape。
// 确定性：全内嵌常量输入、ASCII 源码（中文仅经数字实体引用进入），无时间/随机/
// 环境/地址打印；context 小，输出 <80 行。
// FRONTIER 记录：无（预期直通）。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_xml_rs.rs
//   B: sd=$(grep -rl 'name = "c_xml_rs"' ~/.cache/mirvm/scripts/*/Cargo.toml -m1 | xargs dirname)
//      && cd "$sd" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//         "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_xml_rs.rs
use xml::common::Position;
use xml::name::OwnedName;
use xml::reader::{ParserConfig, XmlEvent};

/// 样本 XML：命名空间/属性/CDATA/注释/DOCTYPE/实体/处理指令全谱系。
const DOC: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<?render-mode full scale="0.8"?>
<!DOCTYPE catalog [
  <!ELEMENT catalog ANY>
  <!ATTLIST catalog version CDATA #FIXED "1.4">
]>
<catalog xmlns="http://example.com/ns/default" xmlns:b="http://example.com/ns/book">
  <!-- shelf comment -->
  <b:book id="bk101" b:lang="zh" price="39.5">Emma &amp; &#65;lice&#x41;</b:book>
  <b:book id="bk102" empty=""><![CDATA[raw <not-a-tag> & "quotes" ]] ]]></b:book>
  <note xmlns:n="http://example.com/ns/note">
    <n:to priority="high" n:rank="2">Tove; Jani &#x4E2D;&#x6587;</n:to>
    <?pi-inside data="kv"?>
  </note>
  <b:empty selfclosed="y &amp; &#x41;"/>
</catalog>
"#;

/// well-formedness 错误样本：<b> 未闭合即遇 </root>。
const BAD: &str = "<root>\n  <a>text</a>\n  <b>open\n</root>\n";

/// 9 类事件的固定序：D S E C W M T P X。
const KINDS: [&str; 9] = ["D", "S", "E", "C", "W", "M", "T", "P", "X"];

fn fmt_name(n: &OwnedName) -> String {
    match &n.prefix {
        Some(p) => format!("{p}:{}", n.local_name),
        None => n.local_name.clone(),
    }
}

fn fmt_ns(n: &OwnedName) -> String {
    n.namespace.clone().unwrap_or_else(|| "-".to_string())
}

fn bump(counts: &mut [u32; 9], i: usize) {
    counts[i] += 1;
}

fn main() {
    // ===== ① 样本文档全事件遍历 =====
    let reader = ParserConfig::new()
        .ignore_comments(false)
        .create_reader(DOC.as_bytes());
    let mut counts = [0u32; 9];
    let mut shape = String::new();
    let mut idx = 0usize;
    for ev in reader {
        let ev = ev.unwrap_or_else(|e| panic!("doc event {idx} failed: {e}"));
        match ev {
            XmlEvent::StartDocument {
                version,
                encoding,
                standalone,
            } => {
                bump(&mut counts, 0);
                shape.push('D');
                println!("doc version={version:?} encoding={encoding} standalone={standalone:?}");
            }
            XmlEvent::StartElement {
                name,
                attributes,
                namespace,
            } => {
                bump(&mut counts, 1);
                shape.push('S');
                let attrs: Vec<String> = attributes
                    .iter()
                    .map(|a| format!("{}={}", fmt_name(&a.name), a.value))
                    .collect();
                println!(
                    "SE[{idx}] {} ns={} attrs={attrs:?}",
                    fmt_name(&name),
                    fmt_ns(&name)
                );
                if name.local_name == "catalog" {
                    // root 元素的 ns 映射表（BTreeMap 序 = 前缀字典序，默认前缀 ""）。
                    let ns: Vec<String> = namespace
                    .iter()
                    .map(|(p, u)| format!("{p}->{u}"))
                    .collect();
                    println!("root ns_map={ns:?}");
                }
            }
            XmlEvent::EndElement { name } => {
                bump(&mut counts, 2);
                shape.push('E');
                println!("EE[{idx}] {}", fmt_name(&name));
            }
            XmlEvent::Characters(s) => {
                bump(&mut counts, 3);
                shape.push('C');
                println!("CH[{idx}] {s:?}");
            }
            XmlEvent::Whitespace(_) => {
                bump(&mut counts, 4);
                shape.push('W');
            }
            XmlEvent::Comment(s) => {
                bump(&mut counts, 5);
                shape.push('M');
                println!("CM[{idx}] {s:?}");
            }
            XmlEvent::CData(s) => {
                bump(&mut counts, 6);
                shape.push('T');
                println!("CD[{idx}] {s:?}");
            }
            XmlEvent::ProcessingInstruction { name, data } => {
                bump(&mut counts, 7);
                shape.push('P');
                println!("PI[{idx}] name={name} data={data:?}");
            }
            XmlEvent::EndDocument => {
                bump(&mut counts, 8);
                shape.push('X');
            }
        }
        idx += 1;
    }
    let mut cl = String::new();
    for (k, &c) in KINDS.iter().zip(counts.iter()) {
        cl.push_str(&format!("{k}={c} "));
    }
    println!("counts {}", cl.trim_end());
    println!("shape {shape}");
    println!("events {idx}");
    assert_eq!(counts[0], 1, "exactly one StartDocument");
    assert_eq!(counts[8], 1, "exactly one EndDocument");
    assert_eq!(counts[1], counts[2], "start/end elements balanced");
    assert_eq!(shape.len(), idx, "shape covers every event");

    // ===== ② well-formedness 错误定位 =====
    let reader = ParserConfig::new()
        .ignore_comments(false)
        .create_reader(BAD.as_bytes());
    let mut shape2 = String::new();
    let mut saw_err = false;
    for ev in reader {
        match ev {
            Ok(e) => shape2.push(match e {
                XmlEvent::StartDocument { .. } => 'D',
                XmlEvent::StartElement { .. } => 'S',
                XmlEvent::EndElement { .. } => 'E',
                XmlEvent::Characters(_) => 'C',
                XmlEvent::Whitespace(_) => 'W',
                XmlEvent::Comment(_) => 'M',
                XmlEvent::CData(_) => 'T',
                XmlEvent::ProcessingInstruction { .. } => 'P',
                XmlEvent::EndDocument => 'X',
            }),
            Err(e) => {
                let p = e.position();
                println!("err row={} col={} msg={e}", p.row, p.column);
                println!("err shape {shape2}");
                saw_err = true;
                break;
            }
        }
    }
    assert!(saw_err, "BAD doc must yield an error");
}
