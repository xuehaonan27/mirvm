#!/usr/bin/env mirvm
---
[dependencies]
itertools = "0.13"
---
// A pure-compute crate: iterator combinators. No OS boundary, so all paths run.
use itertools::Itertools;

fn main() {
    let v = vec![1, 2, 3, 4, 5, 6, 7];
    let chunks: Vec<Vec<i32>> = v.iter().copied().chunks(3).into_iter().map(|c| c.collect()).collect();
    println!("chunks: {chunks:?}");

    let groups: Vec<(bool, Vec<i32>)> =
        v.iter().copied().chunk_by(|x| x % 2 == 0).into_iter().map(|(k, g)| (k, g.collect())).collect();
    println!("groups: {groups:?}");

    let joined = v.iter().map(|x| x.to_string()).join("-");
    println!("join: {joined}");

    let (small, big): (Vec<i32>, Vec<i32>) = v.iter().copied().partition(|&x| x < 4);
    println!("partition: {small:?} | {big:?}");

    let cart: Vec<(i32, char)> = (1..3).cartesian_product(['a', 'b']).collect();
    println!("cartesian: {cart:?}");

    let dedup: Vec<i32> = vec![1, 1, 2, 2, 2, 3, 1].into_iter().dedup().collect();
    println!("dedup: {dedup:?}");
}
