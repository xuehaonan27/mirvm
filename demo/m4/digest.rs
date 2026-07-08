// M4.1 gate/调研：值与内存的 digest 函数集（返回 u64 校验和，无 println——全量差分 M4.3 起）。
// #[unsafe(no_mangle)] = mono 收集根 + --vm-call 稳定名。
#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::BuildHasherDefault;

/// Vec：push/增长/迭代/求和（堆分配 + 聚合 + 投影 + Drop glue）
#[unsafe(no_mangle)]
pub fn vec_digest(n: u64) -> u64 {
    let mut v: Vec<u64> = Vec::new();
    let mut i = 0u64;
    while i < n {
        v.push(i * 3 + 1);
        i += 1;
    }
    let mut s = 0u64;
    for x in &v {
        s = s.wrapping_add(*x);
    }
    s.wrapping_add(v.len() as u64)
}

/// String：字面量 + push_str + 字节求和（&str 常量 + 胖指针 + UTF-8 字节）
#[unsafe(no_mangle)]
pub fn string_digest(reps: u64) -> u64 {
    let mut s = String::from("mirvm-");
    let mut i = 0u64;
    while i < reps {
        s.push_str("m4!");
        i += 1;
    }
    let mut acc = 0u64;
    for b in s.as_bytes() {
        acc = acc.wrapping_mul(31).wrapping_add(*b as u64);
    }
    acc
}

/// HashMap（确定性 hasher，无 getrandom——OS 是 M4.3 的事）
#[unsafe(no_mangle)]
pub fn map_digest(n: u64) -> u64 {
    let mut m: HashMap<u64, u64, BuildHasherDefault<DefaultHasher>> = HashMap::default();
    let mut i = 0u64;
    while i < n {
        m.insert(i, i * i);
        i += 1;
    }
    let mut s = 0u64;
    let mut k = 0u64;
    while k < n {
        if let Some(v) = m.get(&k) {
            s = s.wrapping_add(*v);
        }
        k += 1;
    }
    s.wrapping_add(m.len() as u64)
}

/// Box：堆上单值 + 解引用（exchange_malloc 路径）
#[unsafe(no_mangle)]
pub fn box_digest(x: u64) -> u64 {
    let b = Box::new(x * 7);
    *b + 1
}

/// 枚举：Option/Result 构造 + match（判别式读写 + Downcast 投影 + niche）
#[unsafe(no_mangle)]
pub fn enum_digest(x: u64) -> u64 {
    let o: Option<u64> = if x % 2 == 0 { Some(x) } else { None };
    let r: Result<u64, u64> = if x % 3 == 0 { Ok(x * 2) } else { Err(x + 1) };
    let a = match o {
        Some(v) => v * 10,
        None => 5,
    };
    let b = match r {
        Ok(v) => v,
        Err(e) => e * 100,
    };
    a.wrapping_add(b)
}

/// 数组/切片：栈上数组 + 动态下标 + 越界检查 + 切片迭代
#[unsafe(no_mangle)]
pub fn slice_digest(n: u64) -> u64 {
    let mut arr = [0u64; 16];
    let mut i = 0usize;
    while i < 16 {
        arr[i] = (i as u64) * 5 + n;
        i += 1;
    }
    let sl = &arr[2..10];
    let mut s = 0u64;
    for x in sl {
        s = s.wrapping_add(*x);
    }
    s + sl.len() as u64
}

/// static：只读表引用（statics 冻结 + 重定位）
static TABLE: [u64; 8] = [3, 1, 4, 1, 5, 9, 2, 6];
static MSG: &str = "frozen-static";

#[unsafe(no_mangle)]
pub fn static_digest(idx: u64) -> u64 {
    let t = TABLE[(idx % 8) as usize];
    let m = MSG.len() as u64;
    t * 100 + m
}

/// 裸指针算术（真实地址模型的 §2.5 形状：ptr_int demo 的纯计算版）
#[unsafe(no_mangle)]
pub fn rawptr_digest(n: u64) -> u64 {
    let mut buf = [0u64; 8];
    let p = buf.as_mut_ptr();
    let mut i = 0u64;
    while i < 8 {
        unsafe { *p.add(i as usize) = i * n };
        i += 1;
    }
    let q = buf.as_ptr();
    let addr = q as usize; // ptr→int
    let r = addr as *const u64; // int→ptr（exposed provenance，真实地址下平凡）
    let mut s = 0u64;
    let mut j = 0u64;
    while j < 8 {
        s = s.wrapping_add(unsafe { *r.add(j as usize) });
        j += 1;
    }
    s
}

/// 浮点（M4.1 可选块）：f64 运算 + to_bits 折 u64
#[unsafe(no_mangle)]
pub fn float_digest(n: u64) -> u64 {
    let mut x = 1.5f64;
    let mut i = 0u64;
    while i < n {
        x = x * 1.25 + 0.5;
        i += 1;
    }
    x.to_bits()
}

fn main() {}
