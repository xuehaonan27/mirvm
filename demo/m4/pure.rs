// M4.0 gate: pure integer functions. #[unsafe(no_mangle)] makes them mono collection roots + stable exported names
// (looked up by symbol name for --vm-call). main is left empty — M4.0 does not run the std startup chain (from M4.3 onward).
#![allow(dead_code)]

#[unsafe(no_mangle)]
pub fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

#[unsafe(no_mangle)]
pub fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

#[unsafe(no_mangle)]
pub fn sum_to(n: u64) -> u64 {
    let mut s = 0u64;
    let mut i = 1u64;
    while i <= n {
        s += i;
        i += 1;
    }
    s
}

#[unsafe(no_mangle)]
pub fn collatz_steps(mut n: u64) -> u64 {
    let mut c = 0u64;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        c += 1;
    }
    c
}

#[unsafe(no_mangle)]
pub fn popcount_manual(mut x: u64) -> u64 {
    let mut c = 0u64;
    while x != 0 {
        c += x & 1;
        x >>= 1;
    }
    c
}

#[unsafe(no_mangle)]
pub fn mix_signed(a: i64, b: i64) -> u64 {
    // small mix of signed arithmetic/compare/conversion
    let d = if a < b { b - a } else { a - b };
    let e = (d as i32) as i64; // narrow then widen
    (e * 2 - 1) as u64
}

fn main() {}
