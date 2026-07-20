//! T1-a JIT ABI 泛化探针（m5.4-design §3.3，镜像 interp ABI v2）：
//! Pair 参数 / Pair 返回 / 大聚合按值传参（ParamAbi::Indirect）/
//! 大聚合按值返回（RetAbi::Indirect = sret 前插首参 + Return memcpy）/
//! track_caller 幻影尾参。三维（native / JIT-off / JIT=1）逐字节一致 +
//! MIRVM_JIT_DEBUG=1 实证发布（真被编译，非解释兜底）。
use std::panic::Location;

// 64 字节聚合：按值传参 = Indirect（src 真地址 + prologue memcpy），
// 按值返回 = sret（隐藏首参直传）。
#[derive(Clone, Copy)]
struct Big {
    a: [u64; 8],
}

// u128 参数/返回 = Pair(lo, hi) 双槽通道。
#[inline(never)]
fn pair_add(a: u128, b: u128) -> u128 {
    a + b
}

#[inline(never)]
fn big_sum(b: Big) -> u64 {
    b.a.iter().sum()
}

#[inline(never)]
fn make_big(seed: u64) -> Big {
    let mut a = [0u64; 8];
    // 下标循环（iter_mut/enumerate 的 Option niche 判别是 admit 的既有
    // rvalue_ok 盲区，T1-d 准入放开的活，与本探针无关）
    for i in 0..8 {
        a[i] = seed + i as u64;
    }
    Big { a }
}

#[track_caller]
#[inline(never)]
fn who() -> &'static Location<'static> {
    Location::caller()
}

fn main() {
    let mut r = 0u128;
    for _ in 0..200000 {
        r = pair_add(1u128 << 100, 39);
    }
    println!("pair hi={:#x} lo={}", (r >> 64) as u64, r as u64);
    let b = make_big(40);
    let mut s1 = 0u64;
    for _ in 0..30000 {
        s1 = big_sum(make_big(40));
    }
    println!("big sum={}", s1);
    let b2 = make_big(100);
    let mut s2 = 0u64;
    for _ in 0..30000 {
        s2 = big_sum(make_big(100));
    }
    println!("big2 sum={}", s2);
    let mut loc = who();
    for _ in 0..30000 {
        loc = who();
    }
    println!("loc {}:{}:{}", loc.file(), loc.line(), loc.column());
}
