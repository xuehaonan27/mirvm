#!/usr/bin/env mirvm
---
[dependencies]
rayon = "1"
---
// 数据并行：work-stealing 线程池。压测我们的真实多线程实现。
use rayon::prelude::*;

fn main() {
    let sum: u64 = (0..10_000u64).into_par_iter().map(|x| x * x).sum();
    println!("par sum of squares = {sum}");

    let mut v: Vec<u64> = (0..64).rev().collect();
    v.par_sort();
    println!("par_sort ok = {}", v == (0..64).collect::<Vec<_>>());

    let count = (0..100_000u64).into_par_iter().filter(|x| x % 7 == 0).count();
    println!("multiples of 7 = {count}");

    // par_iter + reduce
    let product: u64 = (1..=10u64).into_par_iter().reduce(|| 1, |a, b| a * b);
    println!("10! = {product}");
}
