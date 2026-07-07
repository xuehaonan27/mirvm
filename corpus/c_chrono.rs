#!/usr/bin/env mirvm
---
[dependencies]
chrono = "0.4"
---
// 日期算术（纯计算，不调 now()，保证确定性）。
use chrono::{Datelike, Duration, NaiveDate, Weekday};

fn main() {
    let d = NaiveDate::from_ymd_opt(2026, 7, 3).unwrap();
    println!("date: {d}");
    println!("weekday: {:?}", d.weekday());
    println!("ordinal (day of year): {}", d.ordinal());

    let d2 = d + Duration::days(100);
    println!("+100 days: {d2}");

    let diff = d2.signed_duration_since(d);
    println!("diff back: {} days", diff.num_days());

    // 找下一个周一
    let mut next = d;
    while next.weekday() != Weekday::Mon {
        next = next.succ_opt().unwrap();
    }
    println!("next monday: {next}");

    // 闰年判断
    for y in [2024, 2025, 2026, 2100, 2000] {
        let leap = NaiveDate::from_ymd_opt(y, 2, 29).is_some();
        println!("{y} leap = {leap}");
    }
}
