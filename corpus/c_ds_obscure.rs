#!/usr/bin/env mirvm
---
[dependencies]
hyperloglog = "1"
succinct = "0.5"
---
// hyperloglog 1.0 + succinct 0.5: differential pair for two offbeat probabilistic and
// compressed-data structures. hyperloglog (SipHasher13 keyed hash + register array):
// a seeded xorshift sequence (with repeats) is inserted and its cardinality estimate is
// compared against an exact BTreeSet count; new_deterministic fixes the seed so the
// estimate's bit pattern is deterministic, pinned by len().to_bits(). Covers insert(Hash),
// insert_by_hash_value / is_empty / merge / new_from_template / clear, three error rates
// (p=4 / p=7 / p=10) and the empty, single-element, merge-invariant and clear-reset
// boundaries.
// succinct (interface level: root re-exports plus the rank/select module traits; there is
// no RSIndex, the 0.5 counterpart is Rank9): BitVector over both usize and u64 blocks,
// exercising get_bit / get_bits across blocks / set_bit / push / pop / align_block /
// with_fill / block_with_fill / iter; JacobsonRank and Rank9 cross-check every probe,
// BinSearchSelect covers select1 / select0 / generic select hits and out-of-range None
// misses; SpaceUsage and into_inner roundtrip. No time/address/HashMap order; f64s by to_bits().
use hyperloglog::HyperLogLog;
use std::collections::BTreeSet;
use succinct::rank::RankSupport;
use succinct::select::{Select0Support, SelectSupport};
use succinct::{
    BinSearchSelect, BitRankSupport, BitVec, BitVecMut, BitVecPush, BitVector, JacobsonRank, Rank9,
    Select1Support, SpaceUsage,
};

/// Seeded xorshift64*, the inline sequence source shared by both crates.
struct Xor(u64);

impl Xor {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn opt_u64(o: Option<u64>) -> String {
    match o {
        Some(v) => format!("{v}"),
        None => "None".to_string(),
    }
}

fn opt_bool(o: Option<bool>) -> String {
    match o {
        Some(true) => "Some(1)".to_string(),
        Some(false) => "Some(0)".to_string(),
        None => "None".to_string(),
    }
}

/// Prints the estimate bits, rounded estimate, exact count and relative-error bits side by side.
fn show_len(tag: &str, hll: &HyperLogLog, exact: usize) {
    let est = hll.len();
    let rel = (est - exact as f64).abs() / exact as f64;
    println!(
        "{tag}: est_bits={:016x} est_round={} exact={} rel_bits={:016x}",
        est.to_bits(),
        est.round() as u64,
        exact,
        rel.to_bits()
    );
}

fn hll_part() {
    // ① Empty / single-element boundary (the linear-counting V>0 branch)
    let mut h0 = HyperLogLog::new_deterministic(0.05, 0x2f6e_2b1a_9c4d_8e7f);
    println!(
        "hll empty: is_empty={} len_bits={:016x}",
        h0.is_empty(),
        h0.len().to_bits()
    );
    h0.insert(&"lone");
    println!(
        "hll single: is_empty={} len_bits={:016x}",
        h0.is_empty(),
        h0.len().to_bits()
    );

    // ② String sequence with repeats: 20000 inserts over 1500 keys, alongside the exact cardinality
    let mut rng = Xor(0x9e37_79b9_7f4a_7c15);
    let mut h_str = HyperLogLog::new_deterministic(0.05, 0x0123_4567_89ab_cdef);
    let mut exact_str = BTreeSet::new();
    for _ in 0..20_000 {
        let key = format!("user-{}", rng.next() % 1500);
        exact_str.insert(key.clone());
        h_str.insert(&key);
    }
    show_len("hll str", &h_str, exact_str.len());

    // ③ Both insert paths, u64 value hashing and raw hash values (p=10)
    let mut h_num = HyperLogLog::new_deterministic(0.01, 0xfedc_ba98_7654_3210);
    let mut exact_num = BTreeSet::new();
    for _ in 0..8_000 {
        let v = rng.next() % 700;
        exact_num.insert(v);
        h_num.insert(&v);
    }
    show_len("hll u64", &h_num, exact_num.len());

    let mut h_raw = HyperLogLog::new_deterministic(0.01, 0xfedc_ba98_7654_3210);
    let mut exact_raw = BTreeSet::new();
    for _ in 0..8_000 {
        let v = rng.next() % 900;
        exact_raw.insert(v);
        h_raw.insert_by_hash_value(v);
    }
    show_len("hll raw", &h_raw, exact_raw.len());

    // ④ p=4 small register array: ~60 distinct, the small-range estimate band (bias correction)
    let mut h_bias = HyperLogLog::new_deterministic(0.2, 0x1111_2222_3333_4444);
    let mut exact_bias = BTreeSet::new();
    for _ in 0..500 {
        let v = rng.next() % 60;
        exact_bias.insert(v);
        h_bias.insert(&v);
    }
    show_len("hll bias", &h_bias, exact_bias.len());

    // ⑤ Same configuration, large cardinality: 5000 full-range u64s, the pure ep() path beyond 5m
    let mut h_big = HyperLogLog::new_deterministic(0.2, 0x5555_6666_7777_8888);
    let mut exact_big = BTreeSet::new();
    for _ in 0..5_000 {
        let v = rng.next();
        exact_big.insert(v);
        h_big.insert(&v);
    }
    show_len("hll big", &h_big, exact_big.len());

    // ⑥ merge: split inserts on two template copies then merge == one-shot insert (per-register max invariant)
    let mut h_a = HyperLogLog::new_from_template(&h_str);
    let mut h_b = HyperLogLog::new_from_template(&h_str);
    let mut rng2 = Xor(0x9e37_79b9_7f4a_7c15);
    for i in 0..20_000u32 {
        let key = format!("user-{}", rng2.next() % 1500);
        if i % 2 == 0 {
            h_a.insert(&key);
        } else {
            h_b.insert(&key);
        }
    }
    h_a.merge(&h_b);
    println!(
        "hll merge: merged_bits={:016x} single_bits={:016x} eq={}",
        h_a.len().to_bits(),
        h_str.len().to_bits(),
        h_a.len().to_bits() == h_str.len().to_bits()
    );

    // ⑦ clear reset
    h_str.clear();
    println!(
        "hll cleared: is_empty={} len_bits={:016x}",
        h_str.is_empty(),
        h_str.len().to_bits()
    );
}

fn succinct_part() {
    // ① 257 seeded bits (crossing five usize blocks, tail block holding 1 bit)
    let mut rng = Xor(0xdead_beef_cafe_f00d);
    let mut bv: BitVector = BitVector::new();
    for _ in 0..257 {
        bv.push_bit(rng.next() % 4 != 0); // ~75% set
    }
    let ones = (0..bv.bit_len()).filter(|&i| bv.get_bit(i)).count() as u64;
    println!(
        "bv: bit_len={} block_len={} ones={} zeros={}",
        bv.bit_len(),
        bv.block_len(),
        ones,
        bv.bit_len() - ones
    );
    let blocks: Vec<String> = (0..bv.block_len())
        .map(|i| format!("{i}={:016x}", bv.get_block(i)))
        .collect();
    println!("bv blocks: {}", blocks.join(" "));
    let probe = [0u64, 1, 2, 63, 64, 65, 127, 128, 191, 255, 256];
    let bits: Vec<String> = probe
        .iter()
        .map(|&p| format!("{p}={}", bv.get_bit(p) as u8))
        .collect();
    println!("bv bits: {}", bits.join(" "));
    println!(
        "bv spans: get_bits(3,17)={:05x} get_bits(60,9)={:03x}",
        bv.get_bits(3, 17),
        bv.get_bits(60, 9)
    );

    // ② JacobsonRank: rank1/rank0 probes, generic rank(pos, value), and limit
    let jac = JacobsonRank::new(bv.clone());
    let r1: Vec<String> = probe
        .iter()
        .map(|&p| format!("{p}={}", jac.rank1(p)))
        .collect();
    println!("jac rank1: {}", r1.join(" "));
    let r0: Vec<String> = probe
        .iter()
        .map(|&p| format!("{p}={}", jac.rank0(p)))
        .collect();
    println!("jac rank0: {}", r0.join(" "));
    println!(
        "jac: limit={} rank(64,true)={} rank(64,false)={} total_ones={} scan_eq={}",
        jac.limit(),
        jac.rank(64, true),
        jac.rank(64, false),
        jac.rank1(256),
        jac.rank1(256) == ones
    );

    // ③ BinSearchSelect: select1/select0 hits, out-of-range None misses, generic select
    let sel = BinSearchSelect::new(jac);
    let total1 = sel.rank1(256);
    let total0 = sel.rank0(256);
    let picks1 = [0u64, 1, 2, 7, 31, 63, total1 / 2, total1 - 1];
    let s1: Vec<String> = picks1
        .iter()
        .map(|&k| format!("{k}={}", opt_u64(sel.select1(k))))
        .collect();
    println!("sel select1: {}", s1.join(" "));
    println!(
        "sel select1 oob: {}={} {}={}",
        total1,
        opt_u64(sel.select1(total1)),
        total1 + 100,
        opt_u64(sel.select1(total1 + 100))
    );
    let picks0 = [0u64, 1, 5, total0 / 2, total0 - 1];
    let s0: Vec<String> = picks0
        .iter()
        .map(|&k| format!("{k}={}", opt_u64(sel.select0(k))))
        .collect();
    println!("sel select0: {}", s0.join(" "));
    println!(
        "sel select0 oob: {}={} {}={}",
        total0,
        opt_u64(sel.select0(total0)),
        total0 + 100,
        opt_u64(sel.select0(total0 + 100))
    );
    println!(
        "sel generic: select(3,true)={} select(3,false)={}",
        opt_u64(sel.select(3, true)),
        opt_u64(sel.select(3, false))
    );

    // ④ Rank9 (u64 blocks only) rebuilt from the same pattern, cross-checked against JacobsonRank
    let mut rng9 = Xor(0xdead_beef_cafe_f00d);
    let mut bv9: BitVector<u64> = BitVector::new();
    for _ in 0..257 {
        bv9.push_bit(rng9.next() % 4 != 0);
    }
    let r9 = Rank9::new(bv9);
    let r9s: Vec<String> = probe
        .iter()
        .map(|&p| format!("{p}={}", r9.rank1(p)))
        .collect();
    println!("rank9 rank1: {}", r9s.join(" "));
    let agree = probe
        .iter()
        .all(|&p| r9.rank1(p) == sel.rank1(p) && r9.rank0(p) == sel.rank0(p));
    println!("rank9: limit={} agrees_with_jacobson={}", r9.limit(), agree);

    // ⑤ SpaceUsage fingerprints (stack + heap bytes)
    println!(
        "space: jacobson_total={} rank9_total={}",
        sel.total_bytes(),
        r9.total_bytes()
    );

    // ⑥ Mutation surface: set_bit / pop_bit / align_block / iter
    let mut mv = bv.clone();
    mv.set_bit(0, !mv.get_bit(0));
    mv.set_bit(256, false);
    let pop0 = mv.pop_bit();
    let pop1 = mv.pop_bit();
    let before_align = mv.bit_len();
    mv.align_block(true);
    let iter_ones = mv.iter().filter(|&b| b).count();
    println!(
        "mut: pop0={} pop1={} len_before_align={} len_after_align={} flipped0={} iter_ones={}",
        opt_bool(pop0),
        opt_bool(pop1),
        before_align,
        mv.bit_len(),
        mv.get_bit(0) as u8,
        iter_ones
    );

    // ⑦ Constructor surface: with_fill / block_with_fill / get_block
    let fill: BitVector = BitVector::with_fill(100, true);
    let fill_ones = (0..fill.bit_len()).filter(|&i| fill.get_bit(i)).count();
    println!("fill: bit_len={} ones={}", fill.bit_len(), fill_ones);
    let blk: BitVector<u64> = BitVector::block_with_fill(2, 0x8000_0000_0000_0001);
    println!(
        "blockfill: block0={:016x} block1={:016x} bit0={} bit63={} bit_len={}",
        blk.get_block(0),
        blk.get_block(1),
        blk.get_bit(0) as u8,
        blk.get_bit(63) as u8,
        blk.bit_len()
    );

    // ⑧ into_inner unwrapped layer by layer, compared bit-for-bit with the original vector
    let back = sel.into_inner().into_inner();
    println!(
        "roundtrip: bit_len={} eq_original={}",
        back.bit_len(),
        back == bv
    );
}

fn main() {
    hll_part();
    succinct_part();
}
