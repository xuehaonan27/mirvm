#!/usr/bin/env mirvm
---
[dependencies]
clap = { version = "4", features = ["derive"] }
---
// Argument parsing via the derive macro; parse_from feeds argv explicitly.
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "greet")]
struct Args {
    /// Positional name.
    name: String,
    /// Repetition count.
    #[arg(short, long, default_value_t = 2)]
    count: u32,
    /// Upper-case the name.
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
