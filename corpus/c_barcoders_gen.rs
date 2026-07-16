#!/usr/bin/env mirvm
---
[dependencies]
barcoders = { version = "1", features = ["ascii", "json", "svg"] }
---
// barcoders 1.0.2：一维条码生成差分。crate 无 datamatrix（纯 1D 库），全量覆盖其
// 9 个 symbology 模块：code39（含 with_checksum 的 mod-43 检验位）/ ean13（12/13
// 位双长度怪癖）/ ean8 / codabar / code93（C+K 双检验，CHARS 表有上游 '[' 重复 bug
// → ']' 非法）/ code11（>10 字符追加第二检验位 K）/ code128（À/Ɓ/Ć 字符集前缀 +
// mod-103 检验）/ tf（interleaved 奇长自动补 mod-10）/ ean_supp（EAN2/EAN5）。
// UPCA/JAN/Bookland/USD8 类型别名各走一次。生成器覆盖 ascii/json/svg 三个可选
// feature（ascii 行矩阵尺寸 + 行 FNV + '# '谱原文；json 全文；svg 矩形计数 +
// viewBox + 半透明颜色的 fill-opacity 浮点格式路径）。encode() 01 谱全行打印
// （>160 模块截首尾）。校验位谱系：EAN 用 pub ENCODINGS 表反解检验数字、code39
// 抽取检验字符 12 模块位型、code128/tf/code93/code11 以 fnv 锚定。错误路径：
// Character（非法字符，含 code128 无前缀/C 集奇数位/']' 撞上游表 bug）、Length
// （空/超限/边界 256/257）。确定性：全固定输入；只打印整数/hex/布尔与确定性文本；
// 驱动自身零 cargo warning。
use barcoders::error::Result as BResult;
use barcoders::generators::ascii::ASCII;
use barcoders::generators::json::JSON;
use barcoders::generators::svg::{Color, SVG};
use barcoders::sym::codabar::Codabar;
use barcoders::sym::code11::{Code11, USD8};
use barcoders::sym::code128::Code128;
use barcoders::sym::code39::Code39;
use barcoders::sym::code93::Code93;
use barcoders::sym::ean13::{Bookland, EAN13, JAN, UPCA, ENCODINGS};
use barcoders::sym::ean8::EAN8;
use barcoders::sym::ean_supp::EANSUPP;
use barcoders::sym::tf::TF;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 模块 0/1 谱：短谱全行，长谱截首尾。
fn pr_row(label: &str, enc: &[u8]) {
    let s: String = enc.iter().map(|&d| (b'0' + d) as char).collect();
    if s.len() <= 160 {
        println!("{label} row[{s}]");
    } else {
        println!("{label} row[{}..{}]", &s[..80], &s[s.len() - 16..]);
    }
}

/// 统一指纹：模块数 + FNV + 01 谱。
fn fp(label: &str, enc: &[u8]) {
    println!("{label} len={} fnv={:016x}", enc.len(), fnv1a(enc));
    pr_row(label, enc);
}

/// 错误路径统一打印（Error 变体名由 Debug 给出）。
fn dump_err<T, F: FnOnce(&str) -> BResult<T>>(label: &str, input: &str, f: F) {
    match f(input) {
        Ok(_) => println!("{label} err[{}]=unexpected-ok", show(input)),
        Err(e) => println!("{label} err[{}]={e:?}", show(input)),
    }
}

/// 错误用例输入的确定性显示：控制字符转义为 \u{xx}，超长截断为前 40 字符 + 余量。
fn show(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if c.is_control() {
            o.push_str(&format!("\\u{{{:02x}}}", c as u32));
        } else {
            o.push(c);
        }
    }
    let n = o.chars().count();
    if n > 40 {
        let t: String = o.chars().take(40).collect();
        format!("{t}..(+{})", n - 40)
    } else {
        o
    }
}

/// 反解 EAN13/EAN8 检验数字：末尾 10 模块为「检验 7 模块 + 右 guard 3」，用
/// barcoders 公开的 ENCODINGS 表反查。返回 -1 表示反查失败（不应发生）。
fn ean_cd(enc: &[u8]) -> i32 {
    let seg = &enc[enc.len() - 10..enc.len() - 3];
    for (d, pat) in ENCODINGS[2].iter().enumerate() {
        if *seg == *pat {
            return d as i32;
        }
    }
    -1
}

fn main() {
    // ---- ① EAN-13：常规 + 12/13 双长度怪癖 + 全 0/全 9 边界 ----
    for s in [
        "750103131130",
        "590123412345",
        "9780306406157", // 13 位输入仍接受（valid_len 12..=13），enc 变 102 模块
        "000000000000",
        "999999999999",
    ] {
        let b = EAN13::new(s).unwrap();
        fp(&format!("ean13[{s}]"), &b.encode());
    }
    // 校验位谱系：末位扫 0..10 时 mod-10 检验数字遍历（反解自编码）
    for d in 0..10u32 {
        let s = format!("10000000000{d}");
        let e = EAN13::new(&s).unwrap().encode();
        println!("ean13 cd[{s}]={} fnv={:016x}", ean_cd(&e), fnv1a(&e));
    }
    // 类型别名面：UPC-A（0 开头）/ JAN（49）/ Bookland（978）
    let upc: UPCA = EAN13::new("016600069865").unwrap();
    fp("upca[016600069865]", &upc.encode());
    let jan: JAN = EAN13::new("491234567890").unwrap();
    fp("jan[491234567890]", &jan.encode());
    let bk: Bookland = EAN13::new("978030640615").unwrap();
    fp("bookland[978030640615]", &bk.encode());
    // 错误路径
    dump_err("ean13", "12345678901", |s: &str| EAN13::new(s)); // 11 位 → Length
    dump_err("ean13", "12345678901234", |s: &str| EAN13::new(s)); // 14 位 → Length
    dump_err("ean13", "75010313113A", |s: &str| EAN13::new(s)); // 非数字 → Character
    dump_err("ean13", "", |s: &str| EAN13::new(s)); // 空 → Length

    // ---- ② EAN-8：7/8 双长度 + 边界 ----
    for s in ["55123457", "1234567", "0000000", "9992227"] {
        let b = EAN8::new(s).unwrap();
        fp(&format!("ean8[{s}]"), &b.encode());
    }
    for d in 0..10u32 {
        let s = format!("000000{d}");
        let e = EAN8::new(&s).unwrap().encode();
        println!("ean8 cd[{s}]={} fnv={:016x}", ean_cd(&e), fnv1a(&e));
    }
    dump_err("ean8", "123456", |s: &str| EAN8::new(s)); // 6 位 → Length
    dump_err("ean8", "123456789", |s: &str| EAN8::new(s)); // 9 位 → Length
    dump_err("ean8", "12345A6", |s: &str| EAN8::new(s)); // → Character

    // ---- ③ Codabar：起止符 + 全符号字符 + 长度边界 1/256/257 ----
    for s in ["A98B", "B12354999A", "A5675+++3$$B", "C401.56:92/1D", "A"] {
        let b = Codabar::new(s).unwrap();
        fp(&format!("codabar[{s}]"), &b.encode());
    }
    let max_ok = "A".repeat(256);
    let b = Codabar::new(&max_ok).unwrap();
    println!(
        "codabar[A*256] len={} fnv={:016x}",
        b.encode().len(),
        fnv1a(&b.encode())
    );
    dump_err("codabar", &"A".repeat(257), |s: &str| Codabar::new(s)); // 超 256 → Length
    dump_err("codabar", "A12Q", |s: &str| Codabar::new(s)); // 'Q' → Character
    dump_err("codabar", "a98b", |s: &str| Codabar::new(s)); // 小写起止 → Character
    dump_err("codabar", "", |s: &str| Codabar::new(s)); // → Length

    // ---- ④ Code39：plain 与 with_checksum 对照 + 检验位模块谱 ----
    for s in ["TEST8052", "MIRVM-2026", "CODE 39/$+%."] {
        let p = Code39::new(s).unwrap().encode();
        let c = Code39::with_checksum(s).unwrap().encode();
        fp(&format!("code39[{s}]"), &p);
        println!(
            "code39+ck[{s}] len={} fnv={:016x}",
            c.len(),
            fnv1a(&c)
        );
    }
    // mod-43 检验字符的 12 模块位型抽取：guard(12)+0+13n 之后、末 guard 之前。
    // 输入含单字符边界（'0'=索引 0，'%'=索引 42）与全 43 字符遍历。
    for s in ["0", "%", "MIRVM", "WIKIPEDIA", "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-. $/+%"] {
        let c = Code39::with_checksum(s).unwrap().encode();
        let n = s.chars().count();
        let chk: String = c[13 + 13 * n..25 + 13 * n]
            .iter()
            .map(|&d| (b'0' + d) as char)
            .collect();
        println!(
            "code39 chk[{s}] len={} mods={chk} fnv={:016x}",
            c.len(),
            fnv1a(&c)
        );
    }
    dump_err("code39", "1212s", |s: &str| Code39::new(s)); // 小写 → Character
    dump_err("code39", "", |s: &str| Code39::new(s)); // → Length
    dump_err("code39", &"X".repeat(257), |s: &str| Code39::new(s)); // → Length

    // ---- ⑤ Code93：自动 C+K 双检验；']' 撞上游 CHARS 表重复 bug → Character ----
    for s in ["TEST93", "CIVIC VIDEO", "BAR(93)RANGE"] {
        let b = Code93::new(s).unwrap();
        fp(&format!("code93[{s}]"), &b.encode());
    }
    dump_err("code93", "ABC]", |s: &str| Code93::new(s)); // ']' → Character
    dump_err("code93", "", |s: &str| Code93::new(s)); // → Length

    // ---- ⑥ Code11/USD8：C 检验恒在，>10 字符追加 K ----
    for s in ["12-9", "9923-1111", "1234567890", "12345678901", "122333444455556666"] {
        let b = Code11::new(s).unwrap();
        fp(&format!("code11[{s}]"), &b.encode());
    }
    let usd: USD8 = Code11::new("7-3").unwrap();
    fp("usd8[7-3]", &usd.encode());
    dump_err("code11", "12A", |s: &str| Code11::new(s)); // → Character
    dump_err("code11", "", |s: &str| Code11::new(s)); // → Length

    // ---- ⑦ Code128：三字符集 + 中段切集 + mod-103 ----
    for s in ["ÀHELLO", "ƁHE1234A*1", "ÀHE@$AĆ123456", "Ć123456", "À"] {
        let b = Code128::new(s).unwrap();
        fp(&format!("code128[{s}]"), &b.encode());
    }
    dump_err("code128", "HELLO", |s: &str| Code128::new(s)); // 缺字符集前缀 → Character
    dump_err("code128", "Ć123", |s: &str| Code128::new(s)); // C 集奇数位 → Character
    dump_err("code128", "À\u{007F}", |s: &str| Code128::new(s)); // DEL 不在 A 集 → Character
    dump_err("code128", "", |s: &str| Code128::new(s)); // len<2 → Length

    // ---- ⑧ TF：interleaved 奇长自动补 mod-10 / standard 原样 ----
    for s in ["1234567", "1234", "98766543561"] {
        let i = TF::interleaved(s).unwrap().encode();
        let t = TF::standard(s).unwrap().encode();
        println!("itf[{s}] len={} fnv={:016x}", i.len(), fnv1a(&i));
        println!("stf[{s}] len={} fnv={:016x}", t.len(), fnv1a(&t));
    }
    dump_err("tf", "12A4", |s: &str| TF::interleaved(s)); // → Character
    dump_err("tf", "", |s: &str| TF::standard(s)); // → Length

    // ---- ⑨ EANSUPP：EAN2/EAN5 + 中间长度走 match 兜底 Length ----
    for s in ["34", "50799", "00", "99999"] {
        let b = EANSUPP::new(s).unwrap();
        fp(&format!("eansupp[{s}]"), &b.encode());
    }
    dump_err("eansupp", "123", |s: &str| EANSUPP::new(s)); // 3 位 → match 兜底 Length
    dump_err("eansupp", "1234", |s: &str| EANSUPP::new(s)); // 4 位 → 同上
    dump_err("eansupp", "1", |s: &str| EANSUPP::new(s)); // parse 层 Length
    dump_err("eansupp", "5A", |s: &str| EANSUPP::new(s)); // → Character

    // ---- ⑩ 生成器：ASCII 矩阵 / JSON 全文 / SVG ----
    let e13 = EAN13::new("750103131130").unwrap().encode();
    let g = ASCII::new().generate(&e13).unwrap();
    let lines: Vec<&str> = g.split('\n').collect();
    println!(
        "ascii/ean13 bytes={} lines={} rows_eq={} fnv={:016x}",
        g.len(),
        lines.len(),
        lines.iter().all(|l| *l == lines[0]),
        fnv1a(g.as_bytes())
    );
    for (i, l) in lines.iter().enumerate().take(2) {
        println!("ascii/ean13 line{i} len={} fnv={:016x}", l.len(), fnv1a(l.as_bytes()));
    }
    println!("ascii/ean13 line0 |{}|", lines[0]);
    let g2 = ASCII { height: 3, xdim: 3 }
        .generate(Code39::new("MIRVM").unwrap().encode())
        .unwrap();
    let lines2: Vec<&str> = g2.split('\n').collect();
    println!(
        "ascii/code39 bytes={} lines={} fnv={:016x}",
        g2.len(),
        lines2.len(),
        fnv1a(g2.as_bytes())
    );
    println!("ascii/code39 line0 |{}|", lines2[0]);

    let j1 = JSON::new().generate(Codabar::new("A98B").unwrap().encode()).unwrap();
    println!("json/codabar bytes={} fnv={:016x}", j1.len(), fnv1a(j1.as_bytes()));
    println!("json/codabar {j1}");
    let j2 = JSON { height: 4, xdim: 2 }
        .generate(EAN8::new("1234567").unwrap().encode())
        .unwrap();
    println!("json/ean8 bytes={} fnv={:016x}", j2.len(), fnv1a(j2.as_bytes()));

    let s1 = SVG::new(60).generate(Code93::new("TEST93").unwrap().encode()).unwrap();
    println!(
        "svg/code93 bytes={} rects={} fnv={:016x}",
        s1.len(),
        s1.matches("<rect").count(),
        fnv1a(s1.as_bytes())
    );
    let s2 = SVG {
        height: 40,
        xdim: 2,
        foreground: Color::new([255, 38, 42, 120]),
        background: Color::new([10, 20, 30, 255]),
    }
    .generate(EAN8::new("5512345").unwrap().encode())
    .unwrap();
    let vb_end = s2.find('>').unwrap() + 1;
    let opa = s2.find("fill-opacity").unwrap();
    println!(
        "svg/ean8-semi bytes={} rects={} fnv={:016x}",
        s2.len(),
        s2.matches("<rect").count(),
        fnv1a(s2.as_bytes())
    );
    println!("svg/ean8-semi root |{}|", &s2[..vb_end]);
    println!("svg/ean8-semi opa |{}|", &s2[opa - 16..opa + 26]);
}
