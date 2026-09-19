#!/usr/bin/env mirvm
---
[dependencies]
regex = "=1.12.2"
---

use regex::bytes::Regex;

fn main() {
    let regex = Regex::new("(?m)^name = ").unwrap();
    println!("{:?}", regex.find(b"[package]\nname = \"mirvm\"\n"));
}
