#!/usr/bin/env mirvm
---
[dependencies]
kdl = "6"
---
// kdl 6.7: KDL v2 document-format differential (winnow parser + miette diagnostics, pure Rust).
// ① mixed document parsing: nested nodes/props/type annotations/line+block comments/slashdash
//    (node/entry/children forms)/semicolon termination/CRLF/empty children; values: bare identifier
//    strings, each escape form, raw #""#, multi-line, empty, decimal/hex/octal/binary integers with
//    underscores, i128 boundaries, floats, #inf/#-inf/#nan, #true/#false/#null.
// ② deterministic tree printing (document order; floats pinned by to_bits).
// ③ API surface: KdlDocument parse/Display/get/get_mut/get_arg/iter_args/
//    iter_dash_args/nodes/nodes_mut/len/is_empty/clear_format_recursive/
//    autoformat; KdlNode new/name/set_name/ty/set_ty/entries/entry/entry_mut/
//    get/get_mut/children/children_mut/set_children/len/is_empty/push/remove/
//    retain; KdlEntry new/new_prop/name/value/value_mut/set_value/ty/set_ty/
//    len/is_empty/clear_format/parse; KdlIdentifier value/repr/parse;
//    KdlValue is_*/as_*.
// ④ mutation (add/remove node, remove entry, change value, insert at head) -> autoformat ->
//    to_string -> reparse semantic equality (hand-written sem_eq, ignoring format/repr/span);
//    clear_format_recursive canonicalization anchored by FNV-1a.
// ⑤ three error paths (ASCII input avoids span char/byte ambiguity): diagnostic message/label/
//    help/severity/span + hand-computed line/column.
// Known API traps (native behaves the same, not a mirvm difference): node.remove(name) compares by
// full KdlIdentifier equality (including repr); parsed names carry repr, so remove-by-name fails
// -- the demo removes by value() predicate with retain. Also: inf/nan/true/false/null are
// reserved words and cannot be bare node names (use pos-inf/not-a-num); i128::MIN overflows the
// parser (magnitude is parsed as i128 first), so the negative boundary uses -i128::MAX.
use kdl::{KdlDocument, KdlEntry, KdlIdentifier, KdlNode, KdlValue};

/// FNV-1a 64 (anchors the text output).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn fmt_value(v: &KdlValue) -> String {
    match v {
        KdlValue::String(s) => format!("str:{s:?}"),
        KdlValue::Integer(i) => format!("int:{i}"),
        KdlValue::Float(f) => format!("float:0x{:016x}", f.to_bits()),
        KdlValue::Bool(b) => format!("bool:{b}"),
        KdlValue::Null => "null".into(),
    }
}

fn dump_node(n: &KdlNode, depth: usize, out: &mut String) {
    let ind = "  ".repeat(depth);
    let ty = n.ty().map(|t| t.value()).unwrap_or("-");
    out.push_str(&format!(
        "{ind}node {:?} ty={ty:?} entries={}\n",
        n.name().value(),
        n.entries().len()
    ));
    for e in n.entries() {
        let ety = e.ty().map(|t| t.value()).unwrap_or("-");
        match e.name() {
            Some(name) => out.push_str(&format!(
                "{ind}  prop {:?} ty={ety:?} = {}\n",
                name.value(),
                fmt_value(e.value())
            )),
            None => {
                out.push_str(&format!("{ind}  arg ty={ety:?} = {}\n", fmt_value(e.value())))
            }
        }
    }
    if let Some(ch) = n.children() {
        out.push_str(&format!("{ind}  children={}\n", ch.nodes().len()));
        for c in ch.nodes() {
            dump_node(c, depth + 1, out);
        }
    }
}

fn entry_eq(a: &KdlEntry, b: &KdlEntry) -> bool {
    a.name().map(|n| n.value()) == b.name().map(|n| n.value())
        && a.ty().map(|t| t.value()) == b.ty().map(|t| t.value())
        && match (a.value(), b.value()) {
            (KdlValue::String(x), KdlValue::String(y)) => x == y,
            (KdlValue::Integer(x), KdlValue::Integer(y)) => x == y,
            (KdlValue::Float(x), KdlValue::Float(y)) => x.to_bits() == y.to_bits(),
            (KdlValue::Bool(x), KdlValue::Bool(y)) => x == y,
            (KdlValue::Null, KdlValue::Null) => true,
            _ => false,
        }
}

fn node_eq(a: &KdlNode, b: &KdlNode) -> bool {
    a.name().value() == b.name().value()
        && a.ty().map(|t| t.value()) == b.ty().map(|t| t.value())
        && a.entries().len() == b.entries().len()
        && a.entries().iter().zip(b.entries()).all(|(x, y)| entry_eq(x, y))
        && match (a.children(), b.children()) {
            (None, None) => true,
            (Some(x), Some(y)) => sem_eq(x, y),
            _ => false,
        }
}

/// Semantic equality: ignores format/repr/span and compares only name/type/entry order/subtree.
fn sem_eq(a: &KdlDocument, b: &KdlDocument) -> bool {
    a.nodes().len() == b.nodes().len()
        && a.nodes().iter().zip(b.nodes()).all(|(x, y)| node_eq(x, y))
}

/// Line/column of an offset in ASCII input (1-based).
fn line_col(input: &str, offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (i, c) in input.char_indices() {
        if i >= offset {
            break;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn report_err(tag: &str, input: &str) {
    match KdlDocument::parse(input) {
        Ok(_) => println!("{tag}: unexpected ok"),
        Err(e) => {
            println!("{tag}: ndiag={}", e.diagnostics.len());
            for d in &e.diagnostics {
                let off = d.span.offset();
                let (line, col) = line_col(input, off);
                println!(
                    "{tag}  msg={:?} label={:?} help={:?} sev={:?} span={}..{} at {}:{}",
                    d.message,
                    d.label,
                    d.help,
                    d.severity,
                    off,
                    off + d.span.len(),
                    line,
                    col
                );
            }
        }
    }
}

const DOC: &str = "\
// KDL v2 mixed spectrum
(kdl-spec)meta \"2.0\" lang=\"kdl\"

strings {
    plain bare-ident
    quoted \"esc: \\n \\t \\\\ \\\" \\b \\f \\u{1f600} CJK 中文\"
    raw #\"C:\\new\\path \"quoted\" tail\"#
    raw2 ##\"has \"# inside\"##
    multi \"\"\"
        first line
        second \"quoted\" \\u{41}nd more
        \"\"\"
    empty-s \"\"
}
numbers {
    dec 42
    neg -17
    plus +99
    underscored 1_000_000
    hex 0xdead_beef
    oct 0o755
    bin 0b1010_1100
    float 3.5
    float-exp -1.25e-3
    float-plus +2.5e10
    pos-inf #inf
    ninf #-inf
    not-a-num #nan
    imax 170141183460469231731687303715884105727
    imin -170141183460469231731687303715884105727
}
keywords {
    t #true
    f #false
    n #null
}
typed (i8)-5 (u64)0xffff_ffff_ffff_ffff when=(date)\"2026-07-16\" {
    (person)inner \"x\"
}
list {
    - 1
    - 2
    - #false
}
props key1=\"v1\" key2=2 key3=#false {
    child
}
/- commented-out 1 2 3
sd /- 1 keep 2
sc /- { inner 1 }
semi; terminated
empty-node
empty-children {}
crlf 1
";

fn main() {
    // ① parse + format-preserving roundtrip
    let doc = KdlDocument::parse(DOC).unwrap();
    let rendered = doc.to_string();
    println!(
        "preserve={} rendered_len={} top_nodes={} is_empty={}",
        rendered == DOC,
        rendered.len(),
        doc.nodes().len(),
        doc.is_empty()
    );

    // ② deterministic-order tree printing
    let mut tree = String::new();
    for n in doc.nodes() {
        dump_node(n, 0, &mut tree);
    }
    print!("{tree}");

    // ③ API surface probes
    let meta = doc.get("meta").unwrap();
    println!(
        "meta name={:?} ty={:?} entries={} arg0={}",
        meta.name().value(),
        meta.ty().unwrap().value(),
        meta.len(),
        fmt_value(doc.get_arg("meta").unwrap())
    );
    println!(
        "iter_args meta n={} missing n={}",
        doc.iter_args("meta").count(),
        doc.iter_args("missing").count()
    );
    let dash: Vec<String> = doc.iter_dash_args("list").map(fmt_value).collect();
    println!("dash_args list n={} [{}]", dash.len(), dash.join(", "));
    println!("get missing={:?}", doc.get("missing").is_none());
    let strings = doc.get("strings").unwrap();
    let sch = strings.children().unwrap();
    println!(
        "strings children={} first={:?} empty_children_len={}",
        sch.nodes().len(),
        sch.nodes()[0].name().value(),
        doc.get("empty-children").unwrap().children().unwrap().nodes().len()
    );
    let numbers = doc.get("numbers").unwrap();
    let hex = numbers
        .children()
        .unwrap()
        .nodes()
        .iter()
        .find(|n| n.name().value() == "hex")
        .unwrap();
    let e0 = hex.entry(0usize).unwrap();
    println!(
        "hex entry idx name={:?} val={} len={} is_empty={}",
        e0.name().is_some(),
        fmt_value(e0.value()),
        e0.len(),
        e0.is_empty()
    );
    let props = doc.get("props").unwrap();
    println!(
        "props key1={} key3={} nope_is_none={}",
        fmt_value(props.get("key1").unwrap()),
        fmt_value(props.get("key3").unwrap()),
        props.get("nope").is_none()
    );
    let typed = doc.get("typed").unwrap();
    let earg = typed.entry(1usize).unwrap();
    println!(
        "typed arg1 ty={:?} val={} when={}",
        earg.ty().unwrap().value(),
        fmt_value(earg.value()),
        fmt_value(typed.get("when").unwrap())
    );
    let kw = doc.get("keywords").unwrap();
    let kn = kw.children().unwrap();
    println!(
        "kw t is_bool={} as_bool={:?} n is_null={} f as_bool={:?}",
        kn.nodes()[0].get(0).unwrap().is_bool(),
        kn.nodes()[0].get(0).unwrap().as_bool(),
        kn.nodes()[2].get(0).unwrap().is_null(),
        kn.nodes()[1].get(0).unwrap().as_bool()
    );
    let plain = &sch.nodes()[0];
    let pv = plain.get(0).unwrap();
    println!(
        "plain is_string={} as_string={:?} as_integer={:?} as_float={:?}",
        pv.is_string(),
        pv.as_string(),
        pv.as_integer(),
        pv.as_float()
    );
    let nanv = numbers
        .children()
        .unwrap()
        .nodes()
        .iter()
        .find(|n| n.name().value() == "not-a-num")
        .unwrap()
        .get(0)
        .unwrap();
    println!(
        "nan is_float={} bits=0x{:016x}",
        nanv.is_float(),
        nanv.as_float().unwrap().to_bits()
    );

    // ④ mutation -> autoformat -> to_string -> reparse semantic equality
    let mut m = KdlDocument::parse(DOC).unwrap();
    // add: hand-build a node with a type annotation, argument, properties and subtree
    let mut extra = KdlNode::new("added");
    extra.set_ty("marker");
    extra.push(KdlEntry::new(KdlValue::Integer(7)));
    extra.push(KdlEntry::new_prop("flag", true));
    extra.push(KdlEntry::new_prop("ratio", 2.5f64));
    extra.push(KdlEntry::new_prop("opt", KdlValue::from(None::<i128>)));
    let mut sub = KdlNode::new("sub");
    sub.set_name("sub-renamed");
    sub.push(KdlEntry::new(KdlValue::Null));
    let mut subdoc = KdlDocument::new();
    subdoc.nodes_mut().push(sub);
    extra.set_children(subdoc);
    m.nodes_mut().push(extra);
    // remove: numbers/hex child node (by position); props/key2 (first demonstrates
    // the repr-equality trap of remove by name, then removes by value() predicate with retain)
    let numbers = m.get_mut("numbers").unwrap();
    let ch = numbers.children_mut().as_mut().unwrap();
    let pos = ch
        .nodes()
        .iter()
        .position(|n| n.name().value() == "hex")
        .unwrap();
    ch.nodes_mut().remove(pos);
    let props = m.get_mut("props").unwrap();
    let rm_by_name = props.remove("key2").is_some();
    props.retain(|e| e.name().map(|n| n.value()) != Some("key2"));
    println!(
        "remove_by_name={rm_by_name} props_entries_after={}",
        props.entries().len()
    );
    // change value 1: entry_mut + set_value + clear_format (clears the stale value_repr)
    let strings = m.get_mut("strings").unwrap();
    let plain = strings.children_mut().as_mut().unwrap().nodes_mut()
        [0]
        .entry_mut(0usize)
        .unwrap();
    plain.set_value(KdlValue::Integer(99));
    plain.clear_format();
    // change value 2: mutate value_mut directly (no entry access; autoformat canonicalizes it)
    let strings = m.get_mut("strings").unwrap();
    let quoted = strings.children_mut().as_mut().unwrap().nodes_mut()
        [1]
        .get_mut(0)
        .unwrap();
    *quoted = KdlValue::String("replaced".into());
    // insert at the head
    let mut first = KdlNode::new("inserted");
    first.push(KdlEntry::new_prop("at", 0));
    m.nodes_mut().insert(0, first);
    m.autoformat();
    let out = m.to_string();
    println!(
        "modified_len={} fnv=0x{:016x}",
        out.len(),
        fnv1a(out.as_bytes())
    );
    let m2 = KdlDocument::parse(&out).unwrap();
    println!("reparse_sem_eq={}", sem_eq(&m, &m2));
    println!("reparse_preserve={}", m2.to_string() == out);
    // clear_format_recursive canonicalization anchor (drops comment/slashdash text)
    let mut c = KdlDocument::parse(DOC).unwrap();
    c.clear_format_recursive();
    let cs = c.to_string();
    println!(
        "canonical_len={} fnv=0x{:016x}",
        cs.len(),
        fnv1a(cs.as_bytes())
    );

    // ⑤ error paths: bad syntax (ASCII) -> diagnostics + line/column
    report_err("e-float", "bad 1.\n");
    report_err("e-string", "node \"unterminated\n");
    report_err("e-brace", "a {\n  b 1\n");

    // ⑥ single-item parse API: KdlEntry / KdlNode / KdlIdentifier
    let e = KdlEntry::parse("key=(u8)0xff").unwrap();
    println!(
        "entry.parse name={:?} ty={:?} val={}",
        e.name().unwrap().value(),
        e.ty().unwrap().value(),
        fmt_value(e.value())
    );
    let n = KdlNode::parse("solo 1 \"two\" {\n  kid #null\n}\n").unwrap();
    println!(
        "node.parse name={:?} entries={} children={}",
        n.name().value(),
        n.entries().len(),
        n.children().unwrap().nodes().len()
    );
    let id = KdlIdentifier::parse("\"quoted id\"").unwrap();
    println!("ident.parse value={:?} repr={:?}", id.value(), id.repr());
    let id2 = KdlIdentifier::parse("plain").unwrap();
    println!("ident.parse2 value={:?} repr={:?}", id2.value(), id2.repr());
}
