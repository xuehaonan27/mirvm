// M4.0 gate：纯整数函数。#[unsafe(no_mangle)] 使其成为 mono 收集根 + 稳定导出名
// （--vm-call 按符号名查）。main 留空——M4.0 不跑 std 启动链（M4.3 起）。
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
    // 有符号算术/比较/转换的小混合
    let d = if a < b { b - a } else { a - b };
    let e = (d as i32) as i64; // 窄化再扩展
    (e * 2 - 1) as u64
}

fn main() {}
