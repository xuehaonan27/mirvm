#!/usr/bin/env mirvm
---
[dependencies]
---

fn intentionally_unused() {}

fn main() {
    // The runner fix must filter a structured rustc summary, not guest bytes.
    eprintln!("warning: 1 warning emitted");
    println!("cargo warning then normal return");
}
