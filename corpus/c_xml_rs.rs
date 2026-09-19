#!/usr/bin/env mirvm
---
[dependencies]
xml-rs = "0.8"
---
// xml-rs 0.8 (resolves to 0.8.28; no dependencies, no features, default = minimal closure).
// EventReader event-stream differential. Pure-Rust pull parser, hand lexer, no SIMD/unsafe.
// What it exercises:
//  ① Full event walk of the sample XML (ParserConfig.ignore_comments(false), else default):
//     XML declaration (version/encoding/standalone), processing instructions ×2, DOCTYPE with
//     internal subset (0.8 emits no doctype event: the lexer consumes and silently skips its
//     contents, which this driver pins), comments, default and prefixed namespaces (resolved
//     prefix/local/namespace URI of elements and attributes, the root element's full ns map
//     printed in BTreeMap order), document-order attributes (not deduplicated), CDATA with the
//     trailing `]] ` boundary plus unescaped `<` `&` `"`, entity references (&amp; &#65; &#x41;
//     plus internal entities, including &#x4E Chinese codepoints decoded as UTF-8), self-closing
//     empty elements, and Whitespace lineage (coalesce merges adjacent Characters by default).
//  ② Event-stream anchors: 9 kind counts + a one-char shape (D/S/E/C/W/M/T/P/X) per event.
//  ③ well-formedness error location: a mismatched closing tag (<b> left open when </root>
//     arrives) -- prints the error row/column (0-based TextPosition), Display, and pre-error shape.
// Determinism: embedded constant inputs, ASCII source (Chinese enters only as numeric
// entity references), no time/random/environment/address printing; small context, output <80 lines.
// FRONTIER: none (expected to pass straight through).
//
// Differential oracle: the fixture runs natively under real rustc, through the cached
// script's cargo project, and under mirvm with MIRVM_JIT_THRESHOLD=1; every run must
// print byte-identical stdout. Interpreted and JIT output are compared against the
// native run, which makes this fixture the probe for the xml-rs event stream,
// namespace resolution, and well-formedness error position.
//
use xml::common::Position;
use xml::name::OwnedName;
use xml::reader::{ParserConfig, XmlEvent};

/// Sample XML: namespaces/attributes/CDATA/comments/DOCTYPE/entities/processing instructions.
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

/// well-formedness error sample: <b> left unclosed when </root> arrives.
const BAD: &str = "<root>\n  <a>text</a>\n  <b>open\n</root>\n";

/// Fixed order of the 9 event kinds: D S E C W M T P X.
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
    // ===== ① sample document, full event walk =====
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
                    // Root element's ns map (BTreeMap order = prefix lexicographic, default "").
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

    // ===== ② well-formedness error location =====
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
