#!/usr/bin/env mirvm
---
[dependencies]
fast-float2 = "0.2"
ryu = "1"
---
// fast-float2 0.2 + ryu 1.0：浮点解析/打印王者对。
// fast-float2（C++ fast_float 的 Rust 移植，Eisel-Lemire 快路径 + big-number
// 慢路径回退）对固定字符串谱系 full parse → to_bits 锁位型；parse_partial
// 前缀消耗；全错/溢出/特殊记号错误路径。ryu（Grisu 系最短往返打印）对固定
// f64/f32 谱系 format 出文本，分别经 std 与 fast-float2 两条独立解析链
// reparse 验 bits roundtrip——双解析器互证。二进制锚定输出全部 to_bits()
// 十六进制；无时间/地址/HashMap 序。
use fast_float2::FastFloat;

fn main() {
    // ① fast-float2 f64 full parse 谱系（u11585/Clinger-Paxson 边界舍入经典集）
    let f64_strings = [
        // 整数形
        "0", "-0", "1", "-1", "42", "9007199254740993", "18446744073709551617",
        // 小数形
        "0.1", "2.5", "-3.75", ".25", "3.14159265358979323846264338327950288",
        "2.718281828459045", "1.0000000000000002",
        // 科学记号形
        "1e2", "1.23e-02", "6.02214076e23", "-1.602176634e-19", "1E10", "+1.5e+300",
        // 大指数：MAX / MAX 之上一位 / 溢出 → ±inf / 深度亚正规 / 下溢 → 0
        "1e308", "1.7976931348623157e308", "1.7976931348623159e308", "1e309",
        "-1e999", "1e-320", "1e-400",
        // 亚正规与最小正规数
        "5e-324", "4.9406564584124654e-324", "2.4703282292062327e-324",
        "2.2250738585072014e-308",
        // 边界舍入经典集（需正确最近舍入；PHP/Java 挂起值及其 Clinger 扩展形）
        "2.2250738585072011e-308",
        "2.22507385850720113605740979670913197593481954635164564e-308",
        "0.500000000000000166533453693773481063544750213623046875",
        "3.518437208883201171875e13", "62.360000000000004",
        "1.00000000000000011102230246251565404236316680908203126",
        // 长位数慢路径（big-number 回退）
        "9.9999999999999999999999999999999999999999999999999999e099",
        "1.2345678901234567890123456789012345678901234567890123e-100",
        // 特殊记号（inf/nan 大小写不敏感，带符号变体）
        "inf", "-inf", "Infinity", "+INFINITY", "nan", "-NaN", "-0.0",
    ];
    for s in f64_strings {
        match fast_float2::parse::<f64, _>(s) {
            Ok(v) => println!("ff64 {s} => {:016x}", v.to_bits()),
            Err(_) => println!("ff64 {s} => ERR"),
        }
    }
    // FastFloat trait 直接调用路径
    let v = f64::parse_float("6.62607015e-34").unwrap();
    println!("ff64 trait parse_float => {:016x}", v.to_bits());

    // ② fast-float2 f32 谱系（f32 边界：2^24+1 / MAX / 亚正规 / 溢出）
    let f32_strings = [
        "0", "-0", "0.1", "2.5", "-1.5e3", "16777217",
        "3.4028234663852886e38", "3.4028235677973366e38", "1e39",
        "1.1754943508222875e-38", "1e-39", "1.401298464324817e-45", "1e-50",
    ];
    for s in f32_strings {
        match fast_float2::parse::<f32, _>(s) {
            Ok(v) => println!("ff32 {s} => {:08x}", v.to_bits()),
            Err(_) => println!("ff32 {s} => ERR"),
        }
    }
    let v = f32::parse_float("2.718281828").unwrap();
    println!("ff32 trait parse_float => {:08x}", v.to_bits());

    // ③ parse_partial：合法前缀 + 尾随垃圾（报告 consumed）；纯垃圾 → ERR
    let partial = [
        "1.5e3xyz", "2.718tail", "-1.0 rest", "0.5,next", ".25junk",
        "77.0\ttab", "1e2\nnl", "123next456", "infX", "nan!",
        "xyz", "", " 1.0", "e5", ".", ".e3", "-", "+", "--1", "1..0",
    ];
    for s in partial {
        match fast_float2::parse_partial::<f64, _>(s) {
            Ok((v, n)) => println!("pp {s:?} => {:016x} n={n}", v.to_bits()),
            Err(_) => println!("pp {s:?} => ERR"),
        }
    }

    // ④ full parse 硬错误路径（parse_partial 能成、full 必败的形状）
    for s in ["", " ", "abc", "1.2.3", "e10", "1e", "+", "-", ".", "0x10", "1_000", "12 34"] {
        match fast_float2::parse::<f64, _>(s) {
            Ok(v) => println!("full {s:?} => {:016x}", v.to_bits()),
            Err(_) => println!("full {s:?} => ERR"),
        }
    }

    // ⑤ ryu f64 最短往返锚：text 锁位 + std/fast-float2 双链 reparse 验 bits
    let mut buf = ryu::Buffer::new();
    let f64_vals: &[(&str, f64)] = &[
        ("0.0", 0.0),
        ("-0.0", -0.0),
        ("0.1", 0.1),
        ("0.2", 0.2),
        ("0.3", 0.3),
        ("1/3", 1.0 / 3.0),
        ("1e23", 1e23),
        ("8.98846567431158e307", 8.98846567431158e307),
        ("MAX", f64::MAX),
        ("MIN", f64::MIN),
        ("MIN_POSITIVE", f64::MIN_POSITIVE),
        ("PI", core::f64::consts::PI),
        ("E", core::f64::consts::E),
        ("sqrt2", 2.0f64.sqrt()),
        ("1e-7", 1e-7),
        ("1e7", 1e7),
        ("min-sub", f64::from_bits(1)),
        ("max-sub", f64::from_bits(0x000f_ffff_ffff_ffff)),
        ("nextafter1", f64::from_bits(0x3ff0_0000_0000_0001)),
        ("neg-min-sub", f64::from_bits(0x8000_0000_0000_0001)),
        ("INF", f64::INFINITY),
        ("NEG_INF", f64::NEG_INFINITY),
        ("NAN", f64::NAN),
    ];
    for &(label, v) in f64_vals {
        let text = buf.format(v);
        let rt_std = text.parse::<f64>().map(f64::to_bits) == Ok(v.to_bits());
        let rt_ff = fast_float2::parse::<f64, _>(text).map(f64::to_bits) == Ok(v.to_bits());
        println!(
            "ryu64 {label} => text={text} bits={:016x} rt_std={rt_std} rt_ff={rt_ff}",
            v.to_bits()
        );
    }
    // format_finite（非 format 的第二条 API 路径）
    println!("fmt_finite 0.125 => {}", buf.format_finite(0.125));
    println!("fmt_finite -0.0 => {}", buf.format_finite(-0.0));

    // ⑥ ryu f32 谱系 + roundtrip
    let f32_vals: &[(&str, f32)] = &[
        ("0.0", 0.0),
        ("-0.0", -0.0),
        ("0.1", 0.1),
        ("1/3", 1.0 / 3.0),
        ("MAX", f32::MAX),
        ("MIN_POSITIVE", f32::MIN_POSITIVE),
        ("PI", core::f32::consts::PI),
        ("1e10", 1e10),
        ("min-sub", f32::from_bits(1)),
        ("max-sub", f32::from_bits(0x007f_ffff)),
        ("INF", f32::INFINITY),
        ("NAN", f32::NAN),
    ];
    for &(label, v) in f32_vals {
        let text = buf.format(v);
        let rt_std = text.parse::<f32>().map(f32::to_bits) == Ok(v.to_bits());
        let rt_ff = fast_float2::parse::<f32, _>(text).map(f32::to_bits) == Ok(v.to_bits());
        println!(
            "ryu32 {label} => text={text} bits={:08x} rt_std={rt_std} rt_ff={rt_ff}",
            v.to_bits()
        );
    }

    // ⑦ ryu::raw pretty 打印（非最短路径，对齐小数形输出；24 字节裸-unsafe API）
    for &(label, v) in &[("0.1", 0.1), ("PI", core::f64::consts::PI), ("MAX", f64::MAX)] {
        let mut raw = [0u8; 24];
        // SAFETY: raw 24 字节，满足 ryu::raw::format64 至多写 24 字节的契约
        let n = unsafe { ryu::raw::format64(v, raw.as_mut_ptr()) };
        let s = core::str::from_utf8(&raw[..n]).unwrap();
        println!("raw64 {label} => {s} len={n}");
    }
    for &(label, v) in &[("0.1", 0.1f32), ("MAX", f32::MAX)] {
        let mut raw = [0u8; 16];
        // SAFETY: raw 16 字节，满足 ryu::raw::format32 至多写 16 字节的契约
        let n = unsafe { ryu::raw::format32(v, raw.as_mut_ptr()) };
        let s = core::str::from_utf8(&raw[..n]).unwrap();
        println!("raw32 {label} => {s} len={n}");
    }
}
