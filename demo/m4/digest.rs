// Digest functions over values and memory: each returns a u64 checksum and prints nothing.
// #[unsafe(no_mangle)] = mono collection root + stable --vm-call name.
#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::BuildHasherDefault;

/// Vec: push/grow/iterate/sum (heap allocation + aggregate + projection + Drop glue)
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

/// String: literal + push_str + byte sum (&str constant + fat pointer + UTF-8 bytes)
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

/// HashMap (deterministic hasher, no getrandom -- the OS layer owns that)
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

/// Box: single heap value + dereference (exchange_malloc path)
#[unsafe(no_mangle)]
pub fn box_digest(x: u64) -> u64 {
    let b = Box::new(x * 7);
    *b + 1
}

/// Enum: Option/Result construction + match (discriminant read/write + Downcast projection + niche)
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

/// Array/slice: stack array + dynamic index + bounds check + slice iteration
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

/// static: read-only table reference (statics frozen + relocated)
static TABLE: [u64; 8] = [3, 1, 4, 1, 5, 9, 2, 6];
static MSG: &str = "frozen-static";

#[unsafe(no_mangle)]
pub fn static_digest(idx: u64) -> u64 {
    let t = TABLE[(idx % 8) as usize];
    let m = MSG.len() as u64;
    t * 100 + m
}

/// Raw pointer arithmetic (real-address model shape: pure-computation version of the ptr_int demo)
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
    let r = addr as *const u64; // int->ptr (exposed provenance; trivial under real addresses)
    let mut s = 0u64;
    let mut j = 0u64;
    while j < 8 {
        s = s.wrapping_add(unsafe { *r.add(j as usize) });
        j += 1;
    }
    s
}

/// Floating point: f64 arithmetic + to_bits folded into a u64
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
