//! JIT ABI generalization probe (mirrors interpreter ABI v2):
//! pair argument / pair return / large aggregate by-value argument (ParamAbi::Indirect) /
//! large aggregate by-value return (RetAbi::Indirect = sret prepended first argument +
//! Return memcpy) / track_caller phantom trailing argument. Must be byte-for-byte identical
//! across native / JIT-off / JIT=1, and MIRVM_JIT_DEBUG=1 must show real compilation.
use std::panic::Location;

// 64-byte aggregate: by-value argument = Indirect (real src address + prologue memcpy),
// by-value return = sret (hidden first argument passed straight through).
#[derive(Clone, Copy)]
struct Big {
    a: [u64; 8],
}

// u128 argument/return = Pair(lo, hi) two-slot channel.
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
    // Option niche discrimination (NicheDiscr) from iter_mut/enumerate: this was an rvalue_ok
    // blind spot, so the loop stays as an in-place regression.
    for (i, x) in a.iter_mut().enumerate() {
        *x = seed + i as u64;
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
