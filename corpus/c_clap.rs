#!/usr/bin/env mirvm
---
[dependencies]
clap = { version = "4", features = ["derive"] }
---
// 参数解析 + derive 宏。用 parse_from 显式给参数，保证确定性。
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "greet")]
struct Args {
    /// 名字（位置参数）
    name: String,
    /// 重复次数
    #[arg(short, long, default_value_t = 2)]
    count: u32,
    /// 大写
    #[arg(short, long)]
    upper: bool,
}

fn main() {
    let args = Args::parse_from(["greet", "world", "-c", "3", "--upper"]);
    let name = if args.upper { args.name.to_uppercase() } else { args.name.clone() };
    for _ in 0..args.count {
        println!("hello {name}");
    }
}
