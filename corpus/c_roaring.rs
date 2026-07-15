#!/usr/bin/env mirvm
---
[dependencies]
roaring = "0.10"
---
// roaring 0.10：RoaringBitmap（u32 分段压缩位图，array/bitmap/run 三种 container）
// 与 RoaringTreemap（u64）差分。
// 覆盖：插入谱系（空/稀疏跨 container/稠密跨 65536 边界/重复插入）、push 与
// append 有序快路径 + 非单调错误路径、insert_range/remove_range/contains_range/
// range_cardinality、集合代数（and/or/xor/sub + *_len 族 + 子集关系）对拍
// BTreeSet 参考实现、native 序列化 roundtrip（尺寸 + FNV-1a checksum + 截断/
// 坏 cookie 错误路径）、迭代器抽样（take/rev/step_by/range/advance_to/rank/
// select）、container 统计。数据全部固定种子（xorshift32），无时间/线程/地址/
// HashMap 序。
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

/// 固定种子 xorshift32：确定性伪随机源。
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

    /// [0, n) 均匀取数（拒绝采样去模偏置，确定性）。
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

/// 序列化 → 尺寸/checksum → 解回 → 相等布尔。
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
    // ① 插入谱系：空 → 稀疏跨 container → 稠密跨 65536 边界
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

    // 稠密 0..=70000：container 0 填满 65536，跨边界进 container 1；
    // array container 超 4096 元素转 bitmap container。
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

    // ② push / append / from_sorted_iter：有序快路径与错误路径
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
    // 起点不大于现有 max（7 <= 9）：立即报错，valid_until=0
    let err0 = app.append([7, 65536]).map_err(|e| e.valid_until());
    println!("2 append not-greater-than-max => {err0:?}");
    // 中途非单调：10/65536/65537 入集合后，重复的 65537 报错，valid_until=3
    let err1 = app.append([10, 65536, 65537, 65537, 70000]).map_err(|e| e.valid_until());
    println!("2 append mid-unsorted => {err1:?}");
    println!("2 app len={} max={:?}", app.len(), app.max());
    let fs_err = RoaringBitmap::from_sorted_iter((0..10u32).rev()).map(|_| ()).map_err(|e| e.valid_until());
    println!("2 from_sorted_iter rev => {fs_err:?}");

    // ③ 区间添加：insert_range 跨边界、重叠去重、contains_range、remove_range
    let mut ranged = RoaringBitmap::new();
    println!("3 insert_range 100..200 => {}", ranged.insert_range(100..200));
    println!("3 insert_range 150..300 (overlap) => {}", ranged.insert_range(150..300));
    println!("3 insert_range 65530..=65540 (跨 65536) => {}", ranged.insert_range(65_530..=65_540));
    println!(
        "3 contains_range 65530..=65540={} 65530..=65541={} 100..300={}",
        ranged.contains_range(65_530..=65_540),
        ranged.contains_range(65_530..=65_541),
        ranged.contains_range(100..300)
    );
    println!("3 len={} range_cardinality[0..100000]={}", ranged.len(), ranged.range_cardinality(0..100_000));
    // 百万级连续区间：触发 run container 的批量构建（每 container 一次调用）
    let mut big = RoaringBitmap::new();
    let added = big.insert_range(1_000_000..2_000_000);
    println!("3 big insert_range 1M..2M => {added} len={} full_container={}", big.len(), big.contains_range(1_000_000..2_000_000));
    stats_line("3 big", &big);
    let removed = big.remove_range(1_500_000..1_600_000);
    println!("3 big remove_range 1500000..1600000 => {removed} len={}", big.len());
    println!("3 big contains 1499999={} 1500000={}", big.contains(1_499_999), big.contains(1_500_000));

    // ④ 集合代数 vs BTreeSet 参考：and/or/xor/sub + len 族 + 子集关系
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
    // 子集/超集/不相交：构造 a 的真子集（每第 3 个元素）
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

    // ⑤ 序列化 roundtrip（native portable 格式）+ 错误路径
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

    // ⑥ 迭代器抽样：take / rev / step_by / range / advance_to / rank / select
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

    // ⑦ RoaringTreemap：u64 多类型面，跨 u32 边界
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
