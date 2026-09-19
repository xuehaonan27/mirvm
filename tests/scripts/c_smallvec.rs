#!/usr/bin/env mirvm
---
[dependencies]
smallvec = "1"
---
// Inline small-vector storage that spills to the heap; stresses unsafe union storage and layout.
use smallvec::{smallvec, SmallVec};

fn main() {
    let mut v: SmallVec<[i32; 4]> = smallvec![1, 2, 3];
    println!("inline: len {} spilled {}", v.len(), v.spilled());

    v.extend(4..=10); // past the inline capacity of 4 -> spill to the heap
    println!("after extend: len {} spilled {}", v.len(), v.spilled());
    println!("sum = {}", v.iter().sum::<i32>());

    v.retain(|x| *x % 2 == 0);
    println!("evens: {:?}", v.as_slice());

    // Shrink back to inline storage
    v.truncate(1);
    v.shrink_to_fit();
    println!("shrunk: len {} spilled {}", v.len(), v.spilled());
}
