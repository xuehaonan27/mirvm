#!/usr/bin/env mirvm
---
[dependencies]
smallvec = "1"
---
// 小向量优化：内联存储 → 溢出到堆。压 unsafe union 式存储 + 布局。
use smallvec::{smallvec, SmallVec};

fn main() {
    let mut v: SmallVec<[i32; 4]> = smallvec![1, 2, 3];
    println!("inline: len {} spilled {}", v.len(), v.spilled());

    v.extend(4..=10); // 越过 4 → 溢出到堆
    println!("after extend: len {} spilled {}", v.len(), v.spilled());
    println!("sum = {}", v.iter().sum::<i32>());

    v.retain(|x| *x % 2 == 0);
    println!("evens: {:?}", v.as_slice());

    // 收缩回内联
    v.truncate(1);
    v.shrink_to_fit();
    println!("shrunk: len {} spilled {}", v.len(), v.spilled());
}
