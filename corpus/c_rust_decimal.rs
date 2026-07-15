#!/usr/bin/env mirvm
---
[dependencies]
rust_decimal = "1"
---
// rust_decimal：96 位十进制定点（lo/mid/hi 三个 u32 + scale/符号）——M5.4b
// 128 位族压力：乘除内部走 96/192 位中间运算与归一化。
// 覆盖：字符串解析谱系（尾零/科学计数/负数/极限/错误）/ 四则运算链 /
// round/trunc/floor/ceil/rescale/normalize / from_f64 边界 /
// checked_* 溢出路径（MAX+1、除零）/ Ord / Hash（固定种子 hasher）。
// 输出全为 Display 文本与布尔/hex，确定；无 serde feature。
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

fn show(label: &str, x: Decimal) {
    println!("{label}: {} (scale={}, mantissa={})", x, x.scale(), x.mantissa());
}

fn main() {
    // ① 字符串解析谱系：尾零 / 前导零 / 科学计数 / 负数 / 96 位极限 / 错误
    for s in [
        "0",
        "-0",
        "1",
        "-1.5",
        "1.2300",
        "007.5",
        "1.23e3",
        "4.56E-2",
        "-0.000000001",
        "79228162514264337593543950335",
    ] {
        let d = Decimal::from_str(s).unwrap();
        println!("parse {s:?} => {} scale={}", d, d.scale());
    }
    let bad = "12.3.4".parse::<Decimal>().unwrap_err();
    println!("parse err: {bad}");
    let huge = "79228162514264337593543950336"
        .parse::<Decimal>()
        .unwrap_err();
    println!("overflow err: {huge}");

    // ② 四则运算链：scale 传播 + 除法 28 位舍入
    let a = Decimal::from_str("1.20").unwrap();
    let b = Decimal::from_str("3.4").unwrap();
    println!("{a} + {b} = {}", a + b);
    println!("{a} * {b} = {}", a * b);
    let one = Decimal::ONE;
    let three = Decimal::from(3u32);
    println!("1 / 3 = {}", one / three);
    println!("2 / 3 = {}", Decimal::from(2u32) / three);
    let chain = (Decimal::from(10u32) / Decimal::from(4u32)) * Decimal::from(2u32)
        + Decimal::from_str("1.5").unwrap()
        - Decimal::from_str("0.25").unwrap();
    show("chain", chain);
    let mut acc = Decimal::ZERO;
    for i in 1..=10u32 {
        acc += one / Decimal::from(i * i);
    }
    println!("sum 1/i^2 (i=1..10) = {acc}");

    // ③ round / trunc / floor / ceil / rescale / normalize / fract
    let x = Decimal::from_str("12.34567").unwrap();
    println!("x = {x}");
    println!("round(x)    = {}", x.round());
    println!("round_dp(2) = {}", x.round_dp(2));
    println!("round_dp(4) = {}", x.round_dp(4));
    println!("trunc(x)    = {}", x.trunc());
    println!("floor(x)    = {}", x.floor());
    println!("ceil(x)     = {}", x.ceil());
    println!("fract(x)    = {}", x.fract());
    let mut r2 = x;
    r2.rescale(2);
    println!("rescale(2)  = {r2}");
    let mut r8 = x;
    r8.rescale(8);
    println!("rescale(8)  = {r8} scale={}", r8.scale());
    for s in ["2.5", "3.5", "-2.5", "-3.5", "2.45", "2.55"] {
        let d = Decimal::from_str(s).unwrap();
        println!("round({s}) = {} round_dp(1) = {}", d.round(), d.round_dp(1));
    }
    let neg = Decimal::from_str("-12.34567").unwrap();
    println!(
        "floor(-x) = {} ceil(-x) = {} trunc(-x) = {}",
        neg.floor(),
        neg.ceil(),
        neg.trunc()
    );
    let z = Decimal::from_str("1.230000").unwrap();
    println!("normalize({z}) = {}", z.normalize());
    println!(
        "normalize(1.000) = {}",
        Decimal::from_str("1.000").unwrap().normalize()
    );

    // ④ from_f64 边界：常规 / 超范围 / 非有限
    for f in [0.1f64, 0.2, 1.0 / 3.0, 1.5e300, -273.15] {
        match Decimal::from_f64(f) {
            Some(d) => println!("from_f64({f:?}) = {d}"),
            None => println!("from_f64({f:?}) = None"),
        }
    }
    for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        println!("from_f64({f:?}) is_none = {}", Decimal::from_f64(f).is_none());
    }
    let rt = Decimal::from_f64_retain(0.1).unwrap();
    println!("from_f64_retain(0.1) = {rt} scale={}", rt.scale());

    // ⑤ checked_* 溢出路径：MAX±1 / MAX*2 / 除零 / 模零；对照可行情形
    let max = Decimal::MAX;
    let min = Decimal::MIN;
    println!("MAX = {max}");
    println!("MIN = {min}");
    println!("MAX + 1  is_none = {}", max.checked_add(one).is_none());
    println!("MIN - 1  is_none = {}", min.checked_sub(one).is_none());
    println!(
        "MAX * 2  is_none = {}",
        max.checked_mul(Decimal::from(2u32)).is_none()
    );
    println!("1 / 0    is_none = {}", one.checked_div(Decimal::ZERO).is_none());
    println!("1 % 0    is_none = {}", one.checked_rem(Decimal::ZERO).is_none());
    println!("MAX + -1 = {}", max.checked_add(-one).unwrap());
    println!("MAX / 1  = {}", max.checked_div(one).unwrap());
    let tiny = Decimal::from_str("0.0000000000000000000000000001").unwrap();
    println!("tiny / 3 = {}", tiny / three);
    println!("tiny checked_div 3 = {}", tiny.checked_div(three).unwrap());

    // ⑥ Ord / Hash（DefaultHasher::new() 固定种子，确定）
    let mut vals = [
        Decimal::from_str("2.5").unwrap(),
        Decimal::from_str("-0.5").unwrap(),
        Decimal::ZERO,
        max,
        min,
        Decimal::from_str("0.5").unwrap(),
        Decimal::from_str("-1").unwrap(),
        Decimal::from_str("2.50").unwrap(),
    ];
    vals.sort();
    for v in vals {
        println!("sorted: {v}");
    }
    let d1 = Decimal::from_str("1.0").unwrap();
    let d2 = Decimal::from_str("1.00").unwrap();
    println!("1.0 == 1.00 : {}", d1 == d2);
    println!("1.0 cmp 1.00: {:?}", d1.cmp(&d2));
    let h = |d: Decimal| {
        let mut s = DefaultHasher::new();
        d.hash(&mut s);
        s.finish()
    };
    println!("hash(1.0)  = {:016x}", h(d1));
    println!("hash(1.00) = {:016x}", h(d2));
    println!("hash eq = {}", h(d1) == h(d2));
    println!("hash(MAX) = {:016x}", h(max));
    println!("hash(MIN) = {:016x}", h(min));

    // ⑦ 杂项确定性 API
    let neg0 = Decimal::from_str("-0").unwrap();
    println!("is_sign_negative(-0) = {}", neg0.is_sign_negative());
    println!("abs(-2.5) = {}", Decimal::from_str("-2.5").unwrap().abs());
    println!("MAX scale={} sign_neg={}", max.scale(), max.is_sign_negative());
}
