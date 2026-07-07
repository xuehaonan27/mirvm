#!/usr/bin/env mirvm
---
[dependencies]
num-bigint = "0.4"
num-traits = "0.2"
---
// 任意精度整数：阶乘、modpow。纯计算，压大 Vec<u64> 运算正确性。
use num_bigint::BigUint;
use num_traits::One;

fn main() {
    let mut f = BigUint::one();
    for i in 1u32..=50 {
        f *= i;
    }
    println!("50! = {f}");

    let base = BigUint::from(2u32);
    let exp = BigUint::from(1000u32);
    let modulus = BigUint::from(1_000_000_007u32);
    println!("2^1000 mod 1e9+7 = {}", base.modpow(&exp, &modulus));

    // 大数相等/比较
    let a = BigUint::parse_bytes(b"123456789012345678901234567890", 10).unwrap();
    let b = &a * &a;
    println!("a^2 = {b}");
}
