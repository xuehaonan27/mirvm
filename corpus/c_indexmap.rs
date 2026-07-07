#!/usr/bin/env mirvm
---
[dependencies]
indexmap = "2"
---
// 插入序保持的 map/set（依赖 hashbrown + 确定性 hash 行为）。
use indexmap::{IndexMap, IndexSet};

fn main() {
    let mut m: IndexMap<&str, i32> = IndexMap::new();
    for (i, w) in ["delta", "alpha", "gamma", "alpha", "beta"].iter().enumerate() {
        *m.entry(*w).or_insert(0) += i as i32;
    }
    // 插入序应保持：delta, alpha, gamma, beta
    for (k, v) in &m {
        println!("{k} = {v}");
    }
    println!("index of gamma = {:?}", m.get_index_of("gamma"));

    let s: IndexSet<i32> = [3, 1, 4, 1, 5, 9, 2, 6, 5].into_iter().collect();
    println!("set (insertion order): {:?}", s.iter().collect::<Vec<_>>());
    println!("set len = {}", s.len());
}
