// HashMap（getrandom 种子）+ BTreeMap（确定性输出）
use std::collections::{BTreeMap, HashMap};

fn main() {
    let mut hm: HashMap<String, i32> = HashMap::new();
    for (i, w) in ["apple", "banana", "cherry", "apple", "banana", "apple"].iter().enumerate() {
        *hm.entry(w.to_string()).or_insert(0) += i as i32 + 1;
    }
    // 输出经 BTreeMap 排序，避免 HashMap 迭代顺序不确定
    let sorted: BTreeMap<_, _> = hm.into_iter().collect();
    for (k, v) in &sorted {
        println!("{k} => {v}");
    }
    println!("total={}", sorted.values().sum::<i32>());
}
