#!/usr/bin/env mirvm
---
[dependencies]
chrono = "0.4"
---
// Date arithmetic (pure computation, never calls now(), so output is deterministic).
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

    // Find the next Monday
    let mut next = d;
    while next.weekday() != Weekday::Mon {
        next = next.succ_opt().unwrap();
    }
    println!("next monday: {next}");

    // Leap-year check
    for y in [2024, 2025, 2026, 2100, 2000] {
        let leap = NaiveDate::from_ymd_opt(y, 2, 29).is_some();
        println!("{y} leap = {leap}");
    }
}
