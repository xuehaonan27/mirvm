#!/usr/bin/env mirvm
---
[dependencies]
compact_str = "=0.9.0"
---
// compact_str 0.9.0：niche 内联字符串差分。CompactString 固定 24B（= size_of::<String>()），
// 长度 ≤24B 时整体内联，最后字节同时充当 len 标记与 inline/heap/static 三态判别式，
// Option<CompactString> 借 niche 仍为 24B——对 mirvm 的 ABI、枚举判别式、union 布局是精准压力。
// 覆盖：临界长度谱系 0/1/11/12/13/23/24/25/64/200 字节（ASCII + 多字节恰好 24B）、
// niche 尺寸、const_new(StaticStr)、push_str/push 跨界增长、pop 多字节、truncate/insert_str
// 的 UTF-8 边界 panic（catch_unwind 打印 payload）、大小写扩展（ß→SS、İ→i+组合符）、
// from_utf8/from_utf16 错误路径、format_compact!、ToCompactString(itoa/ryu 特化)、
// FromIterator/concat_compact/join_compact、跨表示 roundtrip 相等性。
// 无随机/时间/地址输出；panic hook 置静默，成功路径 stderr 为空。
use compact_str::{format_compact, CompactString, CompactStringExt, ToCompactString};
use std::panic;

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn payload_msg(e: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string payload>".to_string()
    }
}

fn show(tag: &str, c: &CompactString) {
    println!(
        "{tag}: len={} heap={} cap={} text={:?}",
        c.len(),
        c.is_heap_allocated(),
        c.capacity(),
        c.as_str()
    );
}

fn main() {
    panic::set_hook(Box::new(|_| {}));

    // ① 临界长度谱系：inline 上限 = size_of::<String>() = 24B（64 位）
    let pat: String = (0..200u32).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    for n in [0usize, 1, 11, 12, 13, 23, 24, 25, 64, 200] {
        show(&format!("spectrum {n:>3}"), &CompactString::new(&pat[..n]));
    }
    // 多字节恰好压线：24B 内联 / 25B 上堆
    show("cjk24", &CompactString::new("汉".repeat(8)));
    show("emoji24", &CompactString::new("🦀".repeat(6)));
    show("e-acute24", &CompactString::new("é".repeat(12)));
    show("mix25", &CompactString::new(format!("{}汉", "a".repeat(22))));

    // ② niche 布局：尺寸断言 + Option 匹配
    println!(
        "size: CompactString={} String={} Option<CS>={} Option<Option<CS>>={}",
        std::mem::size_of::<CompactString>(),
        std::mem::size_of::<String>(),
        std::mem::size_of::<Option<CompactString>>(),
        std::mem::size_of::<Option<Option<CompactString>>>()
    );
    let some = Some(CompactString::new("niche value"));
    match &some {
        Some(c) => println!("opt: some len={} heap={}", c.len(), c.is_heap_allocated()),
        None => println!("opt: none??"),
    }
    let none: Option<CompactString> = None;
    println!("opt: none is_none={}", none.is_none());
    let mut v: Vec<CompactString> = [
        "pear",
        "a much longer string that must heap allocate",
        "fig",
        "apple",
        "汉字排序键",
    ]
    .into_iter()
    .map(CompactString::new)
    .collect();
    v.sort();
    for c in &v {
        show("sorted", c);
    }

    // ③ const_new：短的内联，长的走 StaticStr 变体
    const SHORT: CompactString = CompactString::const_new("untitled");
    const LONG: CompactString = CompactString::const_new("That is not dead which can eternal lie.");
    show("const short", &SHORT);
    show("const long", &LONG);
    println!("static: short as_static={:?}", SHORT.as_static_str());
    println!("static: long as_static={:?}", LONG.as_static_str());
    // 跨表示相等性：static 变体 vs heap 变体，内容相同
    let long_heap = CompactString::new("That is not dead which can eternal lie.");
    println!(
        "static==heap: eq={} static.heap={} heap.heap={}",
        LONG == long_heap,
        LONG.is_heap_allocated(),
        long_heap.is_heap_allocated()
    );

    // ④ 拼接增长：反复 push_str 跨越 inline→heap 边界
    let mut g = CompactString::new("abc");
    show("grow0", &g);
    for i in 1..=6 {
        g.push_str("0123456789");
        println!(
            "grow{i}: len={} heap={} cap={}",
            g.len(),
            g.is_heap_allocated(),
            g.capacity()
        );
    }
    println!("grow tail: {:?}", &g.as_str()[g.len() - 7..]);
    // push 多字节字符 + pop 谱系（含空串 pop）
    let mut p = CompactString::new("ab");
    p.push('汉');
    p.push('🦀');
    show("pushed", &p);
    println!("pop: {:?} {:?} {:?}", p.pop(), p.pop(), p.pop());
    show("popped", &p);
    let mut empty = CompactString::new("");
    println!("pop empty: {:?}", empty.pop());

    // ⑤ truncate / insert_str：正常路径 + UTF-8 边界 panic（catch 打印 payload）
    // "Hello, 世界!" 字节布局：7B ASCII + 世(7..10) + 界(10..13) + !(13..14)
    let mut t = CompactString::new("Hello, 世界!");
    t.truncate(10);
    show("trunc10", &t);
    t.truncate(0);
    show("trunc0", &t);
    let mut noop = CompactString::new("unchanged");
    noop.truncate(100); // new_len >= len：无操作
    show("trunc-noop", &noop);
    let r = panic::catch_unwind(|| {
        let mut bad = CompactString::new("Hello, 世界!");
        bad.truncate(8); // 落在「世」中间
    });
    match r {
        Ok(()) => println!("trunc-panic: no panic??"),
        Err(e) => println!("trunc-panic: {}", payload_msg(e)),
    }
    let mut ins = CompactString::new("Hello, 世界!");
    ins.insert_str(7, "INSERT ");
    show("insert-ok", &ins);
    ins.insert(0, '»');
    show("insert-ch", &ins);
    let r = panic::catch_unwind(|| {
        let mut bad = CompactString::new("Hello, 世界!");
        bad.insert_str(8, "x"); // 落在「世」中间
    });
    match r {
        Ok(()) => println!("insert-panic: no panic??"),
        Err(e) => println!("insert-panic: {}", payload_msg(e)),
    }

    // ⑥ 大小写：ß→SS、ſ→S、ﬁ→FI 扩展；İ 小写为 i+U+0307（两标量）
    let u = CompactString::new("Hello, Wörld! ß ſ ﬁ");
    let up = u.to_uppercase();
    show("upper", &up);
    let low = up.to_lowercase();
    show("lower", &low);
    show("ascii-up", &CompactString::from_str_to_uppercase("abcDef012"));
    show("tr-lower", &CompactString::from_str_to_lowercase("Iİ"));
    let big = CompactString::new("lorem ipsum dolor sit amet, consectetur adipiscing elit 汉字");
    let big_up = big.to_uppercase();
    println!(
        "big-up: len={} heap={} eq={}",
        big_up.len(),
        big_up.is_heap_allocated(),
        big_up == "LOREM IPSUM DOLOR SIT AMET, CONSECTETUR ADIPISCING ELIT 汉字"
    );

    // ⑦ UTF-8/UTF-16 构造：错误路径 + lossy 替换
    let e = CompactString::from_utf8(vec![b'a', b'b', 0xFF, b'c']).unwrap_err();
    println!("utf8-err: {e}");
    show("utf8-ok", &CompactString::from_utf8(vec![240, 159, 146, 150]).unwrap());
    show("lossy", &CompactString::from_utf8_lossy(&[b'x', 0xFF, 0xFE, b'y']));
    let e = CompactString::from_utf16([0x0061u16, 0xD800, 0x0062]).unwrap_err();
    println!("utf16-err: {e}");
    let u16: Vec<u16> = "héllo 汉".encode_utf16().collect();
    show("utf16-ok", &CompactString::from_utf16(u16).unwrap());

    // ⑧ format_compact! 宏：内插 / 填充 / 进制 / 定精度浮点 / 超长上堆
    show("fmt1", &format_compact!("{}+{}={}", 2, 3, 5));
    show("fmt2", &format_compact!("{:.3}|{:>8}|{:#x}", 1.0f64 / 3.0, "xy", 48879u32));
    let long = "x".repeat(100);
    let f3 = format_compact!("{long}{long}");
    println!(
        "fmt3: len={} heap={} fnv={:016x}",
        f3.len(),
        f3.is_heap_allocated(),
        fnv1a(f3.as_bytes())
    );

    // ⑨ ToCompactString：castaway 特化走 itoa/ryu
    show("tcs u8", &255u8.to_compact_string());
    show("tcs i64min", &i64::MIN.to_compact_string());
    show("tcs u64max", &u64::MAX.to_compact_string());
    show("tcs f64", &3.5f64.to_compact_string());
    show("tcs f64-neg0", &(-0.0f64).to_compact_string());
    show("tcs f64-big", &1.25e300f64.to_compact_string());
    show("tcs bool", &true.to_compact_string());
    show("tcs char", &'汉'.to_compact_string());

    // ⑩ 收集器：FromIterator<char> / concat_compact / join_compact
    show("from-chars", &"collect me 汉 🦀".chars().collect::<CompactString>());
    let fruits = ["apples", "oranges", "bananas"];
    show("join", &fruits.join_compact(", "));
    show("concat", &["hello", " ", "world", "!"].concat_compact());
    let long_join = (0..20).map(|i| i.to_compact_string()).collect::<Vec<_>>();
    let joined = long_join.join_compact("-");
    println!(
        "join-long: len={} heap={} fnv={:016x}",
        joined.len(),
        joined.is_heap_allocated(),
        fnv1a(joined.as_bytes())
    );

    // ⑪ roundtrip 相等性：CompactString → String → CompactString（eager inline 语义）
    for n in [0usize, 12, 24, 25, 100] {
        let c = CompactString::new(&pat[..n]);
        let s: String = c.clone().into();
        let back = CompactString::from(s);
        println!(
            "rt{n}: eq={} heap {}->{} text={:?}",
            c == back,
            c.is_heap_allocated(),
            back.is_heap_allocated(),
            back.as_str()
        );
    }
    // Box<str> 往返 + 全量大串 checksum roundtrip
    let c = CompactString::new("box me up 汉字");
    let b: Box<str> = c.clone().into();
    let back = CompactString::from(b);
    println!("box-rt: eq={} heap={}", c == back, back.is_heap_allocated());
    let big2 = (0..50).map(|i| format_compact!("{i}")).concat_compact();
    let rt2: String = big2.clone().into();
    let back2 = CompactString::from(rt2);
    println!(
        "big-rt: len={} fnv={:016x} eq={}",
        big2.len(),
        fnv1a(big2.as_bytes()),
        big2 == back2
    );
}
