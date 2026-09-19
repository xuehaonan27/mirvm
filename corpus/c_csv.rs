#!/usr/bin/env mirvm
---
[dependencies]
csv = "1"
serde = { version = "1", features = ["derive"] }
---
// CSV parsing + serde deserialization over an in-memory string; no real file.
use serde::Deserialize;

#[derive(Deserialize, Debug)]
struct Row {
    city: String,
    pop: u64,
    area: f64,
}

fn main() {
    let data = "city,pop,area\nTokyo,37400000,2194.0\nDelhi,32900000,1484.0\nShanghai,29200000,6340.5\n";
    let mut rdr = csv::Reader::from_reader(data.as_bytes());
    let mut total = 0u64;
    for result in rdr.deserialize() {
        let row: Row = result.unwrap();
        let density = row.pop as f64 / row.area;
        println!("{}: {} people, {:.1}/km²", row.city, row.pop, density);
        total += row.pop;
    }
    println!("total pop = {total}");
}
