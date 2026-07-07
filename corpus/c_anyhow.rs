#!/usr/bin/env mirvm
---
[dependencies]
anyhow = "1"
---
// 错误处理机制：Context / bail / ? 链 / trait object 错误。
use anyhow::{bail, Context, Result};

fn parse(s: &str) -> Result<i32> {
    s.parse::<i32>().with_context(|| format!("failed to parse '{s}'"))
}

fn chain() -> Result<i32> {
    let a = parse("42").context("step a")?;
    let b = parse("58").context("step b")?;
    Ok(a + b)
}

fn run() -> Result<()> {
    println!("chain = {}", chain()?);
    match parse("notanum") {
        Ok(_) => bail!("unexpected ok"),
        Err(e) => {
            println!("caught: {e}");
            // 错误链遍历
            for (i, cause) in e.chain().enumerate() {
                println!("  cause[{i}]: {cause}");
            }
        }
    }
    Ok(())
}

fn main() {
    run().unwrap();
}
