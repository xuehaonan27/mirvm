#!/usr/bin/env mirvm
---
[dependencies]
indexmap = "2"
---
// Insertion-order-preserving map/set (relies on hashbrown plus deterministic hashing).
use indexmap::{IndexMap, IndexSet};

fn main() {
    let mut m: IndexMap<&str, i32> = IndexMap::new();
    for (i, w) in ["delta", "alpha", "gamma", "alpha", "beta"].iter().enumerate() {
        *m.entry(*w).or_insert(0) += i as i32;
    }
    // Insertion order must hold: delta, alpha, gamma, beta
    for (k, v) in &m {
        println!("{k} = {v}");
    }
    println!("index of gamma = {:?}", m.get_index_of("gamma"));

    let s: IndexSet<i32> = [3, 1, 4, 1, 5, 9, 2, 6, 5].into_iter().collect();
    println!("set (insertion order): {:?}", s.iter().collect::<Vec<_>>());
    println!("set len = {}", s.len());
}
