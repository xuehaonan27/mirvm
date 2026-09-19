#!/usr/bin/env mirvm
---
[dependencies]
roaring = "0.10"
---
// roaring 0.10 differential: RoaringBitmap (u32 sharded compressed bitmaps with array,
// bitmap and run containers) and RoaringTreemap (u64).
// Covers an insertion spectrum (empty / sparse across containers / dense across the 65536
// boundary / duplicate inserts); the ordered push and append fast paths plus the
// non-monotonic error path; insert_range/remove_range/contains_range/range_cardinality;
// set algebra (and/or/xor/sub plus the *_len family and subset relations) compared against
// a BTreeSet reference; serialization roundtrips in the native format (size + FNV-1a
// checksum + truncated / bad-cookie error paths); iterator sampling (take/rev/step_by/range/
// advance_to/rank/select); container statistics. Data is fixed-seed (xorshift32); no time/HashMap order.
use roaring::{RoaringBitmap, RoaringTreemap};
use std::collections::BTreeSet;

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Fixed-seed xorshift32: a deterministic pseudorandom source.
struct XorShift32(u32);

impl XorShift32 {
    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform draw from [0, n) (rejection sampling removes modulo bias, deterministically).
    fn below(&mut self, n: u32) -> u32 {
        let zone = u32::MAX - (u32::MAX % n);
        loop {
            let v = self.next();
            if v < zone {
                return v % n;
            }
        }
    }
}

fn stats_line(tag: &str, rb: &RoaringBitmap) {
    let s = rb.statistics();
    println!(
        "{tag} stats containers={} array={} run={} bitset={} bytes={}",
        s.n_containers,
        s.n_array_containers,
        s.n_run_containers,
        s.n_bitset_containers,
        s.n_bytes_array_containers + s.n_bytes_run_containers + s.n_bytes_bitset_containers
    );
}

/// Serialize -> size/checksum -> deserialize -> equality boolean.
fn ser_roundtrip(tag: &str, rb: &RoaringBitmap) {
    let mut bytes = Vec::new();
    rb.serialize_into(&mut bytes).unwrap();
    println!(
        "{tag} ser size={} size_field={} len_match={} fnv={:016x}",
        bytes.len(),
        rb.serialized_size(),
        bytes.len() == rb.serialized_size(),
        fnv1a(&bytes)
    );
    let back = RoaringBitmap::deserialize_from(&bytes[..]).unwrap();
    println!("{tag} ser roundtrip={} back_len={}", *rb == back, back.len());
}

fn main() {
    // ① Insertion spectrum: empty -> sparse across containers -> dense across the 65536 boundary
    let empty = RoaringBitmap::new();
    println!(
        "1 empty len={} is_empty={} min={:?} max={:?}",
        empty.len(),
        empty.is_empty(),
        empty.min(),
        empty.max()
    );

    let mut sparse = RoaringBitmap::new();
    for v in [5u32, 70_000, 131_073, 4_000_000_000] {
        println!("1 sparse insert {v} => {}", sparse.insert(v));
    }
    println!("1 sparse re-insert 70000 => {}", sparse.insert(70_000));
    println!(
        "1 sparse len={} min={:?} max={:?} contains[69999]={} contains[70000]={} contains[65536]={}",
        sparse.len(),
        sparse.min(),
        sparse.max(),
        sparse.contains(69_999),
        sparse.contains(70_000),
        sparse.contains(65_536)
    );
    println!("1 sparse debug = {sparse:?}");
    stats_line("1 sparse", &sparse);

    // Dense 0..=70000: container 0 fills all 65536 slots and the range spills into container 1;
    // an array container converts to a bitmap container past 4096 elements.
    let mut dense = RoaringBitmap::new();
    for v in 0..=70_000u32 {
        dense.insert(v);
    }
    println!(
        "1 dense len={} min={:?} max={:?} boundary[65535]={} boundary[65536]={} boundary[70001]={}",
        dense.len(),
        dense.min(),
        dense.max(),
        dense.contains(65_535),
        dense.contains(65_536),
        dense.contains(70_001)
    );
    stats_line("1 dense", &dense);

    // ② push / append / from_sorted_iter: ordered fast paths and error paths
    let mut pushed = RoaringBitmap::new();
    println!(
        "2 push 1={} 3={} dup3={} 5={} back2={}",
        pushed.push(1),
        pushed.push(3),
        pushed.push(3),
        pushed.push(5),
        pushed.push(2)
    );
    println!("2 pushed debug = {pushed:?}");

    let mut app = RoaringBitmap::new();
    println!("2 append 0..10 => {:?}", app.append(0..10).map_err(|e| e.valid_until()));
    // The start is not greater than the current max (7 <= 9): an immediate error, valid_until=0
    let err0 = app.append([7, 65536]).map_err(|e| e.valid_until());
    println!("2 append not-greater-than-max => {err0:?}");
    // Non-monotonic in the middle: after 10/65536/65537 are added, the repeated 65537 errors with valid_until=3
    let err1 = app.append([10, 65536, 65537, 65537, 70000]).map_err(|e| e.valid_until());
    println!("2 append mid-unsorted => {err1:?}");
    println!("2 app len={} max={:?}", app.len(), app.max());
    let fs_err = RoaringBitmap::from_sorted_iter((0..10u32).rev()).map(|_| ()).map_err(|e| e.valid_until());
    println!("2 from_sorted_iter rev => {fs_err:?}");

    // ③ Range insertion: insert_range across boundaries, overlap dedup, contains_range, remove_range
    let mut ranged = RoaringBitmap::new();
    println!("3 insert_range 100..200 => {}", ranged.insert_range(100..200));
    println!("3 insert_range 150..300 (overlap) => {}", ranged.insert_range(150..300));
    println!("3 insert_range 65530..=65540 (crosses 65536) => {}", ranged.insert_range(65_530..=65_540));
    println!(
        "3 contains_range 65530..=65540={} 65530..=65541={} 100..300={}",
        ranged.contains_range(65_530..=65_540),
        ranged.contains_range(65_530..=65_541),
        ranged.contains_range(100..300)
    );
    println!("3 len={} range_cardinality[0..100000]={}", ranged.len(), ranged.range_cardinality(0..100_000));
    // A million-element contiguous range: triggers bulk run-container construction (one call per container)
    let mut big = RoaringBitmap::new();
    let added = big.insert_range(1_000_000..2_000_000);
    println!("3 big insert_range 1M..2M => {added} len={} full_container={}", big.len(), big.contains_range(1_000_000..2_000_000));
    stats_line("3 big", &big);
    let removed = big.remove_range(1_500_000..1_600_000);
    println!("3 big remove_range 1500000..1600000 => {removed} len={}", big.len());
    println!("3 big contains 1499999={} 1500000={}", big.contains(1_499_999), big.contains(1_500_000));

    // ④ Set algebra vs a BTreeSet reference: and/or/xor/sub + the len family + subset relations
    let mut rng_a = XorShift32(0x1234_5678);
    let mut rng_b = XorShift32(0x9abc_def0);
    let vals_a: Vec<u32> = (0..20_000).map(|_| rng_a.below(1_000_000)).collect();
    let vals_b: Vec<u32> = (0..20_000).map(|_| rng_b.below(1_000_000)).collect();
    let a: RoaringBitmap = vals_a.iter().copied().collect();
    let b: RoaringBitmap = vals_b.iter().copied().collect();
    let ref_a: BTreeSet<u32> = vals_a.iter().copied().collect();
    let ref_b: BTreeSet<u32> = vals_b.iter().copied().collect();
    println!("4 build a_len={} b_len={} ref_a={} ref_b={}", a.len(), b.len(), ref_a.len(), ref_b.len());

    let and_rb = &a & &b;
    let and_ref: BTreeSet<u32> = ref_a.intersection(&ref_b).copied().collect();
    println!(
        "4 and len={} ref={} match={}",
        and_rb.len(),
        and_ref.len(),
        and_rb.iter().eq(and_ref.iter().copied())
    );
    let or_rb = &a | &b;
    let or_ref: BTreeSet<u32> = ref_a.union(&ref_b).copied().collect();
    println!(
        "4 or len={} ref={} match={}",
        or_rb.len(),
        or_ref.len(),
        or_rb.iter().eq(or_ref.iter().copied())
    );
    let xor_rb = &a ^ &b;
    let xor_ref: BTreeSet<u32> = ref_a.symmetric_difference(&ref_b).copied().collect();
    println!(
        "4 xor len={} ref={} match={}",
        xor_rb.len(),
        xor_ref.len(),
        xor_rb.iter().eq(xor_ref.iter().copied())
    );
    let sub_rb = &a - &b;
    let sub_ref: BTreeSet<u32> = ref_a.difference(&ref_b).copied().collect();
    println!(
        "4 sub len={} ref={} match={}",
        sub_rb.len(),
        sub_ref.len(),
        sub_rb.iter().eq(sub_ref.iter().copied())
    );
    println!(
        "4 lens and={} or={} diff={} symdiff={} len_fields_match={}",
        a.intersection_len(&b),
        a.union_len(&b),
        a.difference_len(&b),
        a.symmetric_difference_len(&b),
        a.intersection_len(&b) == and_ref.len() as u64
            && a.union_len(&b) == or_ref.len() as u64
            && a.difference_len(&b) == sub_ref.len() as u64
            && a.symmetric_difference_len(&b) == xor_ref.len() as u64
    );
    // Subset/superset/disjoint: build a proper subset of a (every third element)
    let every3: RoaringBitmap = a.iter().step_by(3).collect();
    let ref_every3: BTreeSet<u32> = ref_a.iter().step_by(3).copied().collect();
    println!(
        "4 relations subset={} ref={} superset={} disjoint_ab={} disjoint_self_xor={}",
        every3.is_subset(&a),
        ref_every3.is_subset(&ref_a),
        a.is_superset(&every3),
        a.is_disjoint(&b),
        a.is_disjoint(&(&a ^ &a))
    );

    // ⑤ Serialization roundtrips (native portable format) + error paths
    ser_roundtrip("5 sparse", &sparse);
    ser_roundtrip("5 dense", &dense);
    ser_roundtrip("5 a", &a);
    ser_roundtrip("5 big", &big);
    let mut dense_bytes = Vec::new();
    dense.serialize_into(&mut dense_bytes).unwrap();
    let trunc = RoaringBitmap::deserialize_from(&dense_bytes[..dense_bytes.len() / 2]).unwrap_err();
    println!("5 deser truncated err kind = {:?}", trunc.kind());
    let bogus = [0xDEu8, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0, 0, 0, 0, 0];
    let bad_cookie = RoaringBitmap::deserialize_from(&bogus[..]).unwrap_err();
    println!("5 deser bogus-cookie err kind = {:?}", bad_cookie.kind());

    // ⑥ Iterator sampling: take / rev / step_by / range / advance_to / rank / select
    let first5: Vec<u32> = a.iter().take(5).collect();
    let last5: Vec<u32> = a.iter().rev().take(5).collect();
    println!("6 a first5={first5:?} last5={last5:?}");
    let stride = (a.len() / 7).max(1) as usize;
    let sampled: Vec<u32> = a.iter().step_by(stride).collect();
    println!("6 a step_by({stride}) n={} sample={sampled:?}", sampled.len());
    let mid_rb = a.range(400_000..500_000).count();
    let mid_ref = ref_a.range(400_000..500_000).count();
    println!("6 a range[400000,500000) rb={mid_rb} ref={mid_ref} match={}", mid_rb == mid_ref);
    let mut it = a.iter();
    it.advance_to(500_000);
    println!("6 a advance_to 500000 next={:?}", it.next());
    let mut rit = a.iter();
    rit.advance_back_to(500_000);
    println!("6 a advance_back_to 500000 next_back={:?}", rit.next_back());
    for v in [0u32, 123_456, 500_000, 999_999] {
        println!("6 a rank({v}) = {}", a.rank(v));
    }
    for n in [0u32, 1, 1000, 10_000, 19_999, 20_000] {
        println!("6 a select({n}) = {:?}", a.select(n));
    }

    // ⑦ RoaringTreemap: the u64 surface, across the u32 boundary
    let mut tm = RoaringTreemap::new();
    let tm_vals = [
        0u64,
        65_535,
        65_536,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        u32::MAX as u64 + 65_536,
        u64::MAX,
    ];
    for &v in &tm_vals {
        tm.insert(v);
    }
    println!(
        "7 treemap len={} min={:?} max={:?} contains_u32max={} contains_u32max+1={} contains_gap={}",
        tm.len(),
        tm.min(),
        tm.max(),
        tm.contains(u32::MAX as u64),
        tm.contains(u32::MAX as u64 + 1),
        tm.contains(u32::MAX as u64 + 2)
    );
    println!("7 treemap iter = {:?}", tm.iter().collect::<Vec<u64>>());
    println!("7 treemap remove u64::MAX => {} len={}", tm.remove(u64::MAX), tm.len());
    let mut tm_bytes = Vec::new();
    tm.serialize_into(&mut tm_bytes).unwrap();
    let tm_back = RoaringTreemap::deserialize_from(&tm_bytes[..]).unwrap();
    println!(
        "7 treemap ser len={} fnv={:016x} roundtrip={}",
        tm_bytes.len(),
        fnv1a(&tm_bytes),
        tm == tm_back
    );
}
