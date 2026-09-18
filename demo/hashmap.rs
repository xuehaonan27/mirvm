// HashMap (getrandom seed) + BTreeMap (deterministic output)
use std::collections::{BTreeMap, HashMap};

fn main() {
    let mut hm: HashMap<String, i32> = HashMap::new();
    for (i, w) in ["apple", "banana", "cherry", "apple", "banana", "apple"].iter().enumerate() {
        *hm.entry(w.to_string()).or_insert(0) += i as i32 + 1;
    }
    // output sorted by BTreeMap to avoid nondeterministic HashMap iteration order
    let sorted: BTreeMap<_, _> = hm.into_iter().collect();
    for (k, v) in &sorted {
        println!("{k} => {v}");
    }
    println!("total={}", sorted.values().sum::<i32>());
}
