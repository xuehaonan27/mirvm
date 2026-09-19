#!/usr/bin/env mirvm
---
[dependencies]
im = "15"
rpds = "1"
---
// im 15 + rpds 1 persistent data structure differential: native cargo output must
// match interpreter/JIT output byte for byte. Both crates are pure Rust, no FFI;
// stress points: Rc/Arc structural sharing + copy-on-write (make_mut path), HAMT
// bitwise walk/collision reconciliation, RRB split/append, rb-tree balance, drop glue.
// Coverage:
//   im::Vector<i64>: collect / push_back / pop_back / set / persistent update() /
//     split_off-append round trip / take-skip bounds / versions coexist untouched
//   im::HashMap<u32,i64,BuildHasherDefault<DefaultHasher>> (deterministic hasher:
//     DefaultHasher::new has fixed internal keys) -- insert / get / contains_key /
//     persistent update()-without() / in-place remove after clone / raw HAMT
//     iteration-order fingerprint + sorted deterministic order line by line
//   im::OrdMap<i32,u64> -- insert / get_min / get_max / get_prev / get_next /
//     range / persistent without / split_lookup into three parts rejoined by hand
//   rpds::List -- push_front / first / last / drop_first lineage / reverse / iter
//   rpds::Stack -- push / peek / pop lineage / size / empty pop, peek
//   rpds::Queue -- enqueue / peek / dequeue lineage / FIFO iteration order / empty dequeue
//   rpds::RedBlackTreeMap -- insert / remove (mut version returns bool) / first /
//     last / range(Excluded,Included) / contains_key
//   deep clone: im::Vector and rpds::List keep 10k versions each, printing only counts
//     and sampled fingerprints, never pointers -- if structural sharing fails (full
//     materialization = tens of millions of element copies) it must OOM; that is failure.
// determinism: fixed-seed xorshift64*; HashMap prints a raw iteration-order FNV
// fingerprint (same hasher + same insertion sequence => same order) then key-sorted
// lines; no floats, time, thread, address or path; all arithmetic is wrapping.
use std::collections::hash_map::DefaultHasher;
use std::hash::BuildHasherDefault;

use im::{OrdMap, Vector};
use rpds::{List, Queue, RedBlackTreeMap, Stack};

/// im::HashMap alias with a deterministic hasher (replaces the default RandomState).
type DetMap = im::HashMap<u32, i64, BuildHasherDefault<DefaultHasher>>;

/// Fixed-seed xorshift64* (identical sequence in native and mirvm).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Inline FNV-1a: fingerprint of a u64 element sequence.
fn fnv_vals<I: IntoIterator<Item = u64>>(vals: I) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for v in vals {
        for b in v.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
    }
    h
}

/// Draw unique keys from rng (redraw on collision; fixed seed => both sides agree).
fn draw_keys(rng: &mut Rng, n: usize, space: u64) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::with_capacity(n);
    while out.len() < n {
        let k = rng.below(space);
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

fn im_vector() {
    // build; then clone + one mutation exercises the persistent workflow
    let v0: Vector<i64> = (0..512).collect();
    let mut v1 = v0.clone();
    for x in 512..520 {
        v1.push_back(x);
    }
    // persistent update: v2 is a new version of v1; v0/v1 must stay as they were
    let v2 = v1.update(100, -100);
    println!(
        "vec build len={} sum={} front={:?} back={:?}",
        v0.len(),
        v0.iter().copied().fold(0i64, i64::wrapping_add),
        v0.front(),
        v0.back()
    );
    println!(
        "vec persist v0[100]={} v1[100]={} v2[100]={} lens={}/{}/{}",
        v0[100],
        v1[100],
        v2[100],
        v0.len(),
        v1.len(),
        v2.len()
    );

    // split_off (mutates in place, yields the right half) -> append restores the whole
    let mut left = v1.clone();
    let right = left.split_off(200);
    println!(
        "vec split left.len={} left.last={:?} right.len={} right.first={:?}",
        left.len(),
        left.back(),
        right.len(),
        right.front()
    );
    let mut rejoined = left.clone();
    rejoined.append(right.clone());
    println!(
        "vec rejoin len={} eq_v1={} right.len_after={}",
        rejoined.len(),
        rejoined == v1,
        right.len()
    );

    // take / skip persistent slices (including the 0 and len boundaries)
    println!(
        "vec take/skip take100.len={} take0.len={} skip420.len={} skip_all.len={}",
        v1.take(100).len(),
        v1.take(0).len(),
        v1.skip(420).len(),
        v1.skip(v1.len()).len()
    );

    // pop_back chain + set (returns the old value)
    let mut v3 = v1.clone();
    let mut pops = Vec::new();
    for _ in 0..113 {
        pops.push(v3.pop_back());
    }
    let old7 = v3.set(7, 777);
    println!(
        "vec pops n={} first={:?} last={:?} remain={} set_old={} v3[7]={}",
        pops.len(),
        pops[0],
        pops[112],
        v3.len(),
        old7,
        v3[7]
    );

    // boundary/empty paths (evaluate first to avoid &/&mut conflicts in one statement)
    let mut empty: Vector<i64> = Vector::new();
    let e_front = empty.front().copied();
    let e_pop = empty.pop_back();
    println!(
        "vec edge get_oor={:?} get_at_len={:?} empty.front={:?} empty.pop={:?} unit={:?}",
        v3.get(999_999),
        v3.get(v3.len()),
        e_front,
        e_pop,
        Vector::unit(9).get(0)
    );
}

fn im_hashmap() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let keys = draw_keys(&mut rng, 40, 100_000);
    let mut h0 = DetMap::with_hasher(BuildHasherDefault::default());
    for &k in &keys {
        h0.insert(k as u32, k as i64 * 3);
    }
    // persistent update / without: h0 stays bitwise unchanged once h1 and h2 exist
    let k5 = keys[5] as u32;
    let k6 = keys[6] as u32;
    let h1 = h0.update(k5, -1);
    let h2 = h1.without(&k6);
    println!(
        "hmap build len={} get_k5={:?} get_k6={:?} miss={:?}",
        h0.len(),
        h0.get(&k5),
        h0.get(&k6),
        h0.get(&777_777)
    );
    println!(
        "hmap persist lens={}/{}/{} h0.k5={:?} h1.k5={:?} h2.k5={:?} h2.k6={:?}",
        h0.len(),
        h1.len(),
        h2.len(),
        h0.get(&k5),
        h1.get(&k5),
        h2.get(&k5),
        h2.get(&k6)
    );
    // in-place remove after clone (including a miss); the original version is untouched
    let mut h3 = h2.clone();
    let old0 = h3.remove(&(keys[0] as u32));
    let old_miss = h3.remove(&777_777);
    println!(
        "hmap remove old0={:?} miss={:?} h3.len={} h2.len={}",
        old0,
        old_miss,
        h3.len(),
        h2.len()
    );
    // raw HAMT iteration-order fingerprint (deterministic hasher => same order both sides)
    let fp = fnv_vals(h0.iter().map(|(k, v)| (*k as u64) ^ ((*v as u64) << 32)));
    println!("hmap rawiter fp={:016x}", fp);
    // deterministic order after sort: first 6 lines + rollup fingerprint
    let mut flat: Vec<(u32, i64)> = h0.iter().map(|(k, v)| (*k, *v)).collect();
    flat.sort_unstable();
    for (k, v) in flat.iter().take(6) {
        println!("hmap sorted {}={}", k, v);
    }
    println!(
        "hmap sorted n={} fp={:016x}",
        flat.len(),
        fnv_vals(flat.iter().map(|(k, v)| (*k as u64) ^ ((*v as u64) << 32)))
    );
}

fn im_ordmap() {
    let mut rng = Rng(0xc2b2_ae3d_27d4_eb4f);
    let keys = draw_keys(&mut rng, 64, 10_000);
    let mut m: OrdMap<i32, u64> = OrdMap::new();
    for (i, &k) in keys.iter().enumerate() {
        m.insert(k as i32, (k as u64) << 7 | i as u64);
    }
    // ordered endpoints + adjacency probes
    let med = keys[33] as i32;
    println!(
        "omap len={} min={:?} max={:?} prev(med)={:?} next(med)={:?}",
        m.len(),
        m.get_min(),
        m.get_max(),
        m.get_prev(&med).map(|(k, _)| *k),
        m.get_next(&med).map(|(k, _)| *k)
    );
    // range: deterministic ascending window
    let win: Vec<(i32, u64)> = m.range(200..2000).map(|(k, v)| (*k, *v)).collect();
    println!(
        "omap range n={} first={:?} last={:?} fp={:016x}",
        win.len(),
        win.first().map(|(k, _)| *k),
        win.last().map(|(k, _)| *k),
        fnv_vals(win.iter().map(|(k, v)| (*k as u32 as u64) ^ (*v << 32)))
    );
    // persistent without
    let m2 = m.without(&med);
    println!(
        "omap without m.len={} m2.len={} m.has={} m2.has={}",
        m.len(),
        m2.len(),
        m.contains_key(&med),
        m2.contains_key(&med)
    );
    // split_lookup into three parts -> reinsert by hand into an equal copy
    let (left, hit, right) = m.split_lookup(&med);
    let mut back = left.clone();
    if let Some(v) = hit {
        back.insert(med, v);
    }
    for (k, v) in &right {
        back.insert(*k, *v);
    }
    println!(
        "omap split l={} hit={} r={} rejoin_eq={}",
        left.len(),
        hit.is_some(),
        right.len(),
        back == m
    );
}

fn rpds_basics() {
    // List: push_front lineage + drop_first lineage + reverse
    let l0: List<i64> = (0..40).map(|i| i as i64).collect();
    let l1 = l0.push_front(-1);
    let l2 = l1.push_front(-2);
    println!(
        "list len l0/l1/l2={}/{}/{} first={:?} last={:?}",
        l0.len(),
        l1.len(),
        l2.len(),
        l2.first(),
        l2.last()
    );
    let d1 = l2.drop_first();
    let d2 = d1.as_ref().and_then(|l| l.drop_first());
    println!(
        "list drop d1.len={} d1.first={:?} d2.len={} l2.len={}",
        d1.as_ref().map_or(usize::MAX, |l| l.len()),
        d1.as_ref().and_then(|l| l.first()),
        d2.as_ref().map_or(usize::MAX, |l| l.len()),
        l2.len()
    );
    let rev = l0.reverse();
    println!(
        "list rev first={:?} last={:?} sum={}",
        rev.first(),
        rev.last(),
        l0.iter().copied().fold(0i64, i64::wrapping_add)
    );
    let le: List<i64> = List::new();
    println!(
        "list edge first={:?} drop_first={} len=0?{}",
        le.first(),
        le.drop_first().is_none(),
        le.is_empty()
    );

    // Stack: push/pop lineage + empty-stack paths
    let mut st: Stack<i64> = Stack::new();
    for i in 0..30 {
        st = st.push(i);
    }
    println!("stack size={} peek={:?}", st.size(), st.peek());
    let mut pops = Vec::new();
    let mut cur = st.clone();
    while let Some(rest) = cur.pop() {
        pops.push(*cur.peek().unwrap_or(&i64::MIN));
        cur = rest;
    }
    println!(
        "stack pops n={} first={} last={} empty_pop={} empty_peek={:?}",
        pops.len(),
        pops[0],
        pops[pops.len() - 1],
        Stack::<i64>::new().pop().is_none(),
        Stack::<i64>::new().peek()
    );

    // Queue: enqueue/dequeue lineage + FIFO iteration order
    let mut q: Queue<i64> = Queue::new();
    for i in 0..40 {
        q = q.enqueue(i as i64 * 10);
    }
    println!(
        "queue len={} peek={:?} deq1.peek={:?} fifo0..3={:?}",
        q.len(),
        q.peek(),
        q.dequeue().as_ref().and_then(Queue::peek),
        q.iter().take(3).copied().collect::<Vec<_>>()
    );
    let mut qd = q.clone();
    let mut n = 0;
    while let Some(rest) = qd.dequeue() {
        qd = rest;
        n += 1;
    }
    println!(
        "queue drained={} empty_deq={} empty_peek={:?}",
        n,
        Queue::<i64>::new().dequeue().is_none(),
        Queue::<i64>::new().peek()
    );

    // RedBlackTreeMap: reverse-order insertion -> self-balanced ascending; range / remove(flip)
    let mut rbt: RedBlackTreeMap<i32, i64> = RedBlackTreeMap::new();
    for i in (0..256).rev() {
        rbt = rbt.insert(i as i32, (i as i64) * 7);
    }
    let rbt2 = rbt.remove(&100);
    println!(
        "rbmap size={} first={:?} last={:?} get100(before)={:?} has100(after)={}",
        rbt.size(),
        rbt.first(),
        rbt.last(),
        rbt.get(&100),
        rbt2.contains_key(&100)
    );
    let win: Vec<(i32, i64)> = rbt
        .range((
            std::ops::Bound::Excluded(64),
            std::ops::Bound::Included(128),
        ))
        .map(|(k, v)| (*k, *v))
        .collect();
    println!(
        "rbmap range n={} first={:?} last={:?} sum={}",
        win.len(),
        win.first(),
        win.last(),
        win.iter().fold(0i64, |a, (_, v)| a.wrapping_add(*v))
    );
    // first/last 3 lines in ascending order (sorted rb version printed deterministically)
    let sorted: Vec<(i32, i64)> = rbt.iter().map(|(k, v)| (*k, *v)).collect();
    let tail3: Vec<(i32, i64)> = sorted[sorted.len() - 3..].to_vec();
    for (k, v) in sorted.iter().take(3).copied().chain(tail3) {
        println!("rbmap sorted {}={}", k, v);
    }
}

fn deep_clone_10k() {
    // im::Vector: 10k versions coexist, each with one push_back -- only structural sharing fits
    let base: Vector<i64> = (0..2048).collect();
    let mut keep_v: Vec<Vector<i64>> = Vec::with_capacity(10_000);
    keep_v.push(base);
    for i in 0..9_999 {
        let mut nv = keep_v.last().unwrap().clone();
        nv.push_back(i as i64);
        keep_v.push(nv);
    }
    // sampled check: keep[k].len = 2048+k and the tail push values (index 2048 = 0 when k>0)
    let mut ok_lens = 0usize;
    let mut ok_tail = 0usize;
    for (k, v) in keep_v.iter().enumerate() {
        if v.len() == 2048 + k {
            ok_lens += 1;
        }
        if k > 0 && v.get(2048) == Some(&0) && v.get(v.len() - 1) == Some(&(k as i64 - 1)) {
            ok_tail += 1;
        }
    }
    println!(
        "deep10k imvec kept={} ok_lens={} ok_tail={} first.len={} last.len={}",
        keep_v.len(),
        ok_lens,
        ok_tail,
        keep_v[0].len(),
        keep_v[9_999].len()
    );
    let fp = fnv_vals(
        keep_v
            .iter()
            .step_by(997)
            .map(|v| (v.len() as u64) ^ ((*v.back().unwrap_or(&-1)) as u64)),
    );
    println!("deep10k imvec fp={:016x}", fp);
    drop(keep_v);
    println!("deep10k imvec dropped=true");

    // rpds::List: 10k push_front versions -- the entire head chain is shared
    let mut keep_l: Vec<List<i64>> = Vec::with_capacity(10_000);
    let mut cur: List<i64> = List::new();
    for i in 0..10_000 {
        cur = cur.push_front(i as i64);
        keep_l.push(cur.clone());
    }
    let mut ok = 0usize;
    for (k, l) in keep_l.iter().enumerate() {
        if l.len() == k + 1 && l.first() == Some(&(k as i64)) && l.last() == Some(&0) {
            ok += 1;
        }
    }
    println!(
        "deep10k rpdslist kept={} ok={} deep.len={}",
        keep_l.len(),
        ok,
        keep_l[9_999].len()
    );
    drop(keep_l);
    println!("deep10k rpdslist dropped=true");
}

fn main() {
    println!("== im persistent + rpds lineage ==");
    im_vector();
    im_hashmap();
    im_ordmap();
    rpds_basics();
    deep_clone_10k();
    println!("== done ==");
}
