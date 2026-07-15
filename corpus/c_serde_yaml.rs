#!/usr/bin/env mirvm
---
[dependencies]
serde_yaml = "0.9"
serde = { version = "1", features = ["derive"] }
---
// serde_yaml 0.9：内部是 libyaml 的纯 Rust 移植（unsafe-libyaml，C 风格
// 状态机 scanner/parser/emitter）。覆盖：多文档 / 嵌套 map-seq /
// 锚点别名+merge key / 数字谱系 / unicode / 块标量与折叠标量 /
// parse→改→serialize roundtrip / 错误路径行列号。
// 键序确定性：Mapping 基于 IndexMap，保持插入序，输出按插入序。
use serde::{Deserialize, Serialize};
use serde_yaml::{Deserializer, Mapping, Number, Value};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Config {
    name: String,
    replicas: u32,
    ratio: f64,
    flags: Vec<bool>,
    ports: BTreeMap<String, u16>,
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Sequence(_) => "seq",
        Value::Mapping(_) => "map",
        Value::Tagged(_) => "tagged",
    }
}

// 统计子树节点数（确定性遍历：seq 按下标，map 按插入序）。
fn count_nodes(v: &Value) -> u64 {
    1 + match v {
        Value::Sequence(s) => s.iter().map(count_nodes).sum(),
        Value::Mapping(m) => m.iter().map(|(k, val)| count_nodes(k) + count_nodes(val)).sum(),
        Value::Tagged(t) => count_nodes(&t.value),
        _ => 0,
    }
}

fn main() {
    // ① 多文档流：--- / ... 分隔，逐文档反序列化
    let multi = "---\nname: alpha\nvalue: 1\n...\n---\n[1, 2, 3]\n---\njust a scalar\n";
    let mut docs = 0;
    for document in Deserializer::from_str(multi) {
        let v = Value::deserialize(document).unwrap();
        docs += 1;
        let one_line = serde_yaml::to_string(&v).unwrap();
        println!("doc{docs}: kind={} text={}", kind(&v), one_line.trim_end());
    }
    println!("docs total = {docs}");

    // ② 嵌套 map-seq：下标链式访问 + 子树计数
    let nested = "root:\n  - name: n0\n    tags: [x, y]\n  - name: n1\n    tags:\n      - z\n      - w\nmeta:\n  count: 2\n  ok: true\n";
    let v: Value = serde_yaml::from_str(nested).unwrap();
    println!("nested nodes = {}", count_nodes(&v));
    println!("root len = {}", v["root"].as_sequence().unwrap().len());
    println!("n1.tag0 = {}", v["root"][1]["tags"][0].as_str().unwrap());
    println!("meta.ok = {}", v["meta"]["ok"].as_bool().unwrap());
    println!("missing is null = {}", v["no"]["such"]["key"].is_null());
    let top_keys: Vec<&str> = v
        .as_mapping()
        .unwrap()
        .keys()
        .map(|k| k.as_str().unwrap())
        .collect();
    println!("top keys = {top_keys:?}");

    // ③ 锚点 / 别名 / merge key
    let anchored = "defaults: &def\n  retries: 3\n  timeout: 30\nprod:\n  <<: *def\n  timeout: 60\ncopy: *def\n";
    let mut v: Value = serde_yaml::from_str(anchored).unwrap();
    println!("alias resolved = {}", v["copy"]["retries"].as_u64().unwrap());
    println!("merge raw kind = {}", kind(&v["prod"]["<<"]));
    v.apply_merge().unwrap();
    println!(
        "merged prod retries={} timeout={}",
        v["prod"]["retries"].as_u64().unwrap(),
        v["prod"]["timeout"].as_u64().unwrap()
    );
    println!("merge key gone = {}", v["prod"].get("<<").is_none());

    // ④ 数字谱系：十进制/hex/octal/边界/浮点/特殊值；浮点按位型打印
    for tok in [
        "0",
        "-0",
        "42",
        "-17",
        "0x1F",
        "-0x2a",
        "0o17",
        "9223372036854775807",
        "-9223372036854775808",
        "18446744073709551615",
        "2.5",
        "-0.0",
        "1e3",
        "1.5e-7",
        ".inf",
        "-.inf",
        ".nan",
    ] {
        let v: Value = serde_yaml::from_str(tok).unwrap();
        let Value::Number(n) = &v else {
            panic!("num {tok} not a number: {}", kind(&v));
        };
        let desc = if n.is_i64() {
            format!("i64 {}", n.as_i64().unwrap())
        } else if n.is_u64() {
            format!("u64 {}", n.as_u64().unwrap())
        } else {
            format!("f64 bits={:016x}", n.as_f64().unwrap().to_bits())
        };
        println!("num {tok:>22} => {desc}");
    }
    // Number 手工构造 + as_f64 对整数的强制转换
    let n = Number::from(7);
    println!("num7 i64={} f64bits={:016x}", n.as_i64().unwrap(), n.as_f64().unwrap().to_bits());

    // ⑤ unicode：CJK / 非 BMP / 转义
    let uni = "greeting: '你好，世界'\nemoji: \"\\U0001F600 smile\"\nesc: \"tab\\there quote\\\"\"\n";
    let v: Value = serde_yaml::from_str(uni).unwrap();
    let g = v["greeting"].as_str().unwrap();
    let e = v["emoji"].as_str().unwrap();
    println!("greeting bytes={} chars={}", g.len(), g.chars().count());
    println!("emoji debug = {e:?}");
    println!("esc debug = {:?}", v["esc"].as_str().unwrap());
    let uni_ser = serde_yaml::to_string(&v).unwrap();
    println!("uni roundtrip eq = {}", serde_yaml::from_str::<Value>(&uni_ser).unwrap() == v);

    // ⑥ 块标量（literal | / |- / |+）与折叠标量（> / >-）
    let blocks = "literal: |\n  line1\n  line2\nliteral_strip: |-\n  a\n  b\nliteral_keep: |+\n  c\n\nfolded: >\n  hello\n  world\nfolded_strip: >-\n  x\n  y\n";
    let v: Value = serde_yaml::from_str(blocks).unwrap();
    for k in ["literal", "literal_strip", "literal_keep", "folded", "folded_strip"] {
        println!("block {k} = {:?}", v[k].as_str().unwrap());
    }

    // ⑦ parse → 改 → serialize → reparse roundtrip
    let src = "name: demo\nitems: [1, 2, 3]\nnested:\n  flag: true\n";
    let mut v: Value = serde_yaml::from_str(src).unwrap();
    v["items"].as_sequence_mut().unwrap().push(Value::from(4u64));
    v["nested"]["flag"] = Value::Bool(false);
    v.as_mapping_mut()
        .unwrap()
        .insert(Value::from("added"), Value::from("汉字"));
    let out = serde_yaml::to_string(&v).unwrap();
    print!("serialized begin\n{out}serialized end\n");
    let v2: Value = serde_yaml::from_str(&out).unwrap();
    println!("roundtrip eq = {}", v == v2);

    // ⑧ 键序：手工 Mapping（插入序）+ flow mapping 解析保序
    let mut m = Mapping::new();
    for (k, i) in [("zeta", 1u64), ("alpha", 2), ("mid", 3)] {
        m.insert(Value::from(k), Value::from(i));
    }
    let m_ser = serde_yaml::to_string(&Value::Mapping(m)).unwrap();
    println!("manual order = {}", m_ser.trim_end().replace('\n', " | "));
    let fm: Value = serde_yaml::from_str("{z: 1, a: 2, m: 3}").unwrap();
    let keys: Vec<&str> = fm
        .as_mapping()
        .unwrap()
        .keys()
        .map(|k| k.as_str().unwrap())
        .collect();
    println!("flow keys = {keys:?}");

    // ⑨ 显式 tag：!tag 解析与再发射
    let tagged: Value = serde_yaml::from_str("!mytag [1, 2]").unwrap();
    match &tagged {
        Value::Tagged(t) => println!(
            "tag = {} inner = {}",
            t.tag,
            serde_yaml::to_string(&t.value).unwrap().trim_end()
        ),
        other => println!("unexpected kind = {}", kind(other)),
    }
    println!("tagged ser = {}", serde_yaml::to_string(&tagged).unwrap().trim_end());

    // ⑩ 派生结构体的 typed 反序列化 / 序列化（含嵌套容器、浮点位型）
    let cfg: Config = serde_yaml::from_str(
        "name: svc\nreplicas: 3\nratio: 0.75\nflags: [true, false, true]\nports:\n  http: 80\n  grpc: 9000\n",
    )
    .unwrap();
    println!(
        "cfg name={} replicas={} ratio_bits={:016x} flags={:?}",
        cfg.name,
        cfg.replicas,
        cfg.ratio.to_bits(),
        cfg.flags
    );
    for (k, p) in &cfg.ports {
        println!("cfg port {k} = {p}");
    }
    let cfg_ser = serde_yaml::to_string(&cfg).unwrap();
    let cfg2: Config = serde_yaml::from_str(&cfg_ser).unwrap();
    println!("cfg roundtrip eq = {}", cfg == cfg2);
    println!("cfg ser = {}", cfg_ser.trim_end().replace('\n', " | "));

    // ⑪ 错误路径：行/列/字节索引（文本确定性）
    for (label, bad) in [
        ("unclosed_flow", "a: [1, 2"),
        ("tab_indent", "\tx: 1"),
        ("unclosed_quote", "a: 'oops"),
        ("bad_alias", "b: *nope"),
        ("bad_anchor", "a: &"),
        ("map_in_seq_indent", "- a: 1\n b: 2"),
    ] {
        match serde_yaml::from_str::<Value>(bad) {
            Ok(v) => println!("err {label}: parsed {v:?}"),
            Err(err) => match err.location() {
                Some(loc) => println!(
                    "err {label}: line {} col {} idx {} :: {}",
                    loc.line(),
                    loc.column(),
                    loc.index(),
                    err
                ),
                None => println!("err {label}: no loc :: {err}"),
            },
        }
    }
    // 类型不匹配错误（typed 路径）
    match serde_yaml::from_str::<u32>("name: not_a_number") {
        Ok(x) => println!("err typed: got {x}"),
        Err(err) => match err.location() {
            Some(loc) => println!("err typed: line {} col {} :: {err}", loc.line(), loc.column()),
            None => println!("err typed: no loc :: {err}"),
        },
    }
}
