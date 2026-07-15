#!/usr/bin/env mirvm
---
[dependencies]
serde_json = "1"
---
// serde_json：Value 树、嵌套解析/stringify roundtrip、数字/unicode 边界。
// Pair 返回密集的迭代器链——M5.4c ABI 泛化（Pair/Indirect ret）的压力形状。
use serde_json::{Map, Value, json};

fn main() {
    // ① json! 宏 + 排序键输出（默认 preserve_order off = BTreeMap 键序确定）
    let v = json!({
        "zeta": [1, 2.5, -3e10, true, null],
        "alpha": {"nested": "unicode 汉字 \u{1f600}", "deep": [[1],[2,[3]]]},
        "num": 18446744073709551615u64,
        "neg": -9223372036854775808i64,
    });
    let s = serde_json::to_string(&v).unwrap();
    println!("compact: {s}");

    // ② parse → 修改 → 再 stringify 的 roundtrip 不变量
    let mut t: Value = serde_json::from_str(&s).unwrap();
    t["alpha"]["deep"][1][1] = json!(99);
    t["extra"] = json!("added");
    let s2 = serde_json::to_string_pretty(&t).unwrap();
    println!("pretty len = {}", s2.len());
    let t2: Value = serde_json::from_str(&s2).unwrap();
    println!("roundtrip eq = {}", t == t2);

    // ③ 数字谱系：u64::MAX / i64::MIN / f64 分数位
    for n in ["0", "-0", "1e2", "2.5", "1e-7", "9007199254740993", "18446744073709551615"] {
        let x: Value = serde_json::from_str(n).unwrap();
        println!("num {n} => {} (is_f64={}, is_i64={}, is_u64={})",
                 x, x.is_f64(), x.is_i64(), x.is_u64());
    }

    // ④ 错误路径：行/列号（文本确定性）
    let bad = serde_json::from_str::<Value>("{\"a\": [1, 2, }").unwrap_err();
    println!("err at line {} col {}", bad.line(), bad.column());

    // ⑤ Map 手工构造 + 迭代（BTree 序）
    let mut m = Map::new();
    for (k, i) in [("pear", 3), ("apple", 1), ("fig", 2)] {
        m.insert(k.to_string(), json!(i));
    }
    for (k, val) in &m {
        println!("map {k} = {val}");
    }

    // ⑥ escape：控制字符 / 斜杠 / 非 BMP
    let esc = json!("tab\tnl\nquote\"backslash\\ctrl\u{0007}emoji\u{1f980}");
    println!("escaped: {}", serde_json::to_string(&esc).unwrap());
}
