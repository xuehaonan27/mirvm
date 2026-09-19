#!/usr/bin/env mirvm
---
[dependencies]
fst = "0.4"
---
// fst 0.4 differential: building and querying in-memory finite state transducers (FSTs).
// Covers SetBuilder over sorted keys (empty key / prefix chain / unicode / long unicode / non-UTF-8
// byte keys / bulk keys) with size and FNV-1a fingerprints; contains hits and misses; full Stream
// traversal in lexicographic order; MapBuilder over a u64 value spectrum (0 / 1 / u64::MAX /
// cross-varint) lookup and traversal; union / intersection / difference / symmetric_difference;
// automaton search (Str exact and prefix, Subsequence); error paths (out-of-order insert /
// duplicate key / junk bytes / bad version); byte rebuild roundtrip. All in memory, no IO.
use fst::automaton::{Str, Subsequence};
use fst::{Automaton, IntoStreamer, Map, MapBuilder, Set, SetBuilder, Streamer};

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn show(k: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(k))
}

// The streamer needs an HRTB bound (same shape as fst::set::OpBuilder::add).
fn dump_keys<S>(tag: &str, mut st: S)
where
    S: for<'a> Streamer<'a, Item = &'a [u8]>,
{
    let mut n = 0usize;
    while let Some(k) = st.next() {
        println!("{tag}[{n}] = {}", show(k));
        n += 1;
    }
    println!("{tag} total = {n}");
}

fn main() {
    // ① SetBuilder: sorted keys -> fst bytes, printing size and fingerprint
    let long_key = format!("{}尾", "長".repeat(50));
    let mut keys: Vec<Vec<u8>> = vec![
        b"".to_vec(), // empty key: the root final-state boundary
        b"a".to_vec(),
        b"a\0b".to_vec(), // byte key with an embedded NUL
        b"ab".to_vec(),
        b"abc".to_vec(),
        b"abcd".to_vec(),
        b"abcz".to_vec(),
        b"abd".to_vec(),
        b"b".to_vec(),
        b"ba".to_vec(),
        b"he".to_vec(),
        b"hel".to_vec(),
        b"hell".to_vec(),
        b"hello".to_vec(),
        b"help".to_vec(),
        b"helper".to_vec(),
        b"wor".to_vec(),
        b"world".to_vec(),
        b"worm".to_vec(),
        "汉字".into(),
        "汉学".into(),
        "日本".into(),
        "日本語".into(),
        "漢字".into(), // traditional form; shares a byte prefix with "汉字" (data)
        "🦀".into(),
        "🦀x".into(),
        "🦀🦀".into(),
        long_key.clone().into_bytes(), // 151-byte long unicode key
        vec![0xff, 0x00, 0x7f],        // non-UTF-8 byte key
    ];
    for i in 0..3000u32 {
        keys.push(format!("key{i:03}").into_bytes());
    }
    keys.sort();
    keys.dedup();
    let mut sb = SetBuilder::memory();
    for k in &keys {
        sb.insert(k).unwrap();
    }
    let bytes = sb.into_inner().unwrap();
    println!(
        "1 set keys={} bytes={} fnv={:016x}",
        keys.len(),
        bytes.len(),
        fnv1a(&bytes)
    );
    let set = Set::new(bytes.clone()).unwrap();
    println!(
        "1 len={} fst_size={} as_bytes_eq={}",
        set.len(),
        set.as_fst().size(),
        set.as_fst().as_bytes() == bytes.as_slice()
    );

    // ② contains hits and misses (empty key, prefix-chain midpoints, unicode, non-UTF-8)
    for q in [
        "", "a", "ab", "abc", "abcd", "abc\0", "hell", "hello", "hellp", "help", "汉字", "汉学",
        "漢", "日本語", "key000", "key120", "key2999", "key3000", "zebra", "🦀", "🦀🦀", "🦀y",
    ] {
        println!("2 contains {} = {}", show(q.as_bytes()), set.contains(q));
    }
    println!(
        "2 contains {} = {}",
        show(&[0xff, 0x00, 0x7f]),
        set.contains([0xff, 0x00, 0x7f])
    );
    println!(
        "2 contains {} = {}",
        show(b"a\0b"),
        set.contains(b"a\0b")
    );
    println!(
        "2 contains long_key = {}",
        set.contains(&long_key)
    );

    // ③ Full Stream traversal: must be strictly lexicographic
    dump_keys("3 stream", set.stream());

    // ④ MapBuilder: u64 value spectrum (0 / 1 / cross-varint / u64::MAX)
    let long_map_key = format!("long:{}", "長".repeat(20));
    let mut entries: Vec<(Vec<u8>, u64)> = vec![
        (b"".to_vec(), 42),
        (b"a".to_vec(), 100),
        (b"ab".to_vec(), 101),
        (b"abc".to_vec(), 102),
        (b"big".to_vec(), u64::MAX - 1),
        (b"key007".to_vec(), 7007),
        (long_map_key.clone().into_bytes(), 999_999),
        (b"max".to_vec(), u64::MAX),
        (b"odd".to_vec(), 4_294_967_297),
        (b"one".to_vec(), 1),
        (b"two".to_vec(), 2),
        (b"zero".to_vec(), 0),
        ("汉字值".into(), 88),
        ("🦀".into(), 7),
    ];
    entries.sort_by(|x, y| x.0.cmp(&y.0));
    let mut mb = MapBuilder::memory();
    for (k, v) in &entries {
        mb.insert(k, *v).unwrap();
    }
    let mbytes = mb.into_inner().unwrap();
    println!(
        "4 map entries={} bytes={} fnv={:016x}",
        entries.len(),
        mbytes.len(),
        fnv1a(&mbytes)
    );
    let map = Map::new(mbytes).unwrap();
    println!("4 map len={}", map.len());
    for q in [
        "", "a", "abc", "abcd", "big", "max", "odd", "zero", "nope", "汉字值", "🦀",
    ] {
        println!("4 get {} = {:?}", show(q.as_bytes()), map.get(q));
    }
    println!(
        "4 get {} = {:?}",
        show(long_map_key.as_bytes()),
        map.get(&long_map_key)
    );
    let mut ms = map.stream();
    let mut mn = 0usize;
    while let Some((k, v)) = ms.next() {
        println!("4 stream[{mn}] {} = {v}", show(k));
        mn += 1;
    }
    println!("4 stream total = {mn}");

    // ⑤ Second set, overlapping set1 on hello/help/CJK keys/🦀/key%6==0/long key/non-UTF-8
    let mut keys2: Vec<Vec<u8>> = vec![
        b"ab".to_vec(),
        b"abc".to_vec(),
        b"hello".to_vec(),
        b"help".to_vec(),
        b"wxyz".to_vec(),
        b"yak".to_vec(),
        b"zebra".to_vec(),
        b"zz".to_vec(),
        "汉字".into(),
        "日本語".into(),
        "🦀".into(),
        long_key.clone().into_bytes(),
        vec![0xff, 0x00, 0x7f],
    ];
    for i in 0..500u32 {
        keys2.push(format!("key{:03}", i * 6).into_bytes());
    }
    keys2.sort();
    keys2.dedup();
    let mut sb2 = SetBuilder::memory();
    for k in &keys2 {
        sb2.insert(k).unwrap();
    }
    let bytes2 = sb2.into_inner().unwrap();
    println!(
        "5 set2 keys={} bytes={} fnv={:016x}",
        keys2.len(),
        bytes2.len(),
        fnv1a(&bytes2)
    );
    let set2 = Set::new(bytes2).unwrap();
    dump_keys("5 union", set.op().add(&set2).union());
    dump_keys("5 inter", set.op().add(&set2).intersection());
    dump_keys("5 diff", set.op().add(&set2).difference());
    dump_keys("5 symdiff", set.op().add(&set2).symmetric_difference());

    // ⑥ Automaton search: exact / prefix / subsequence; map prefix search with values
    dump_keys("6 exact-hello", set.search(Str::new("hello")).into_stream());
    dump_keys("6 exact-empty", set.search(Str::new("")).into_stream());
    dump_keys(
        "6 prefix-hel",
        set.search(Str::new("hel").starts_with()).into_stream(),
    );
    dump_keys(
        "6 prefix-key23",
        set.search(Str::new("key23").starts_with()).into_stream(),
    );
    dump_keys(
        "6 prefix-none",
        set.search(Str::new("nomatch").starts_with()).into_stream(),
    );
    dump_keys(
        "6 subseq-hlo",
        set.search(Subsequence::new("hlo")).into_stream(),
    );
    dump_keys(
        "6 subseq-cjk",
        set.search(Subsequence::new("日語")).into_stream(),
    );
    let mut as6 = map.search(Str::new("a").starts_with()).into_stream();
    let mut an = 0usize;
    while let Some((k, v)) = as6.next() {
        println!("6 map-search[{an}] {} = {v}", show(k));
        an += 1;
    }
    println!("6 map-search total = {an}");

    // ⑦ Error paths: out-of-order insert / duplicate key (a set is an idempotent no-op;
    // a map reports DuplicateKey) / junk bytes / bad version (text is deterministic)
    let mut bad = SetBuilder::memory();
    bad.insert(b"b").unwrap();
    match bad.insert(b"a") {
        Ok(()) => println!("7 order: no err"),
        Err(e) => println!("7 order: {e:?}"),
    }
    let mut sdup = SetBuilder::memory();
    sdup.insert(b"x").unwrap();
    sdup.insert(b"x").unwrap(); // duplicate insert into a set = idempotent no-op
    let sdup_set = Set::new(sdup.into_inner().unwrap()).unwrap();
    println!(
        "7 set-dup no-op: len={} contains={}",
        sdup_set.len(),
        sdup_set.contains("x")
    );
    let mut mdup = MapBuilder::memory();
    mdup.insert(b"x", 1).unwrap();
    match mdup.insert(b"x", 2) {
        Ok(()) => println!("7 map-dup: no err"),
        Err(e) => println!("7 map-dup: {e:?}"),
    }
    match Set::new(vec![1u8, 2, 3]) {
        Ok(_) => println!("7 garbage: no err"),
        Err(e) => println!("7 garbage: {e:?}"),
    }
    match Set::new(vec![0u8; 64]) {
        Ok(_) => println!("7 badver: no err"),
        Err(e) => println!("7 badver: {e:?}"),
    }

    // ⑧ Roundtrip: rebuild the set from its bytes; rebuilding the same keys gives identical bytes
    let re = Set::new(set.as_fst().as_bytes().to_vec()).unwrap();
    println!(
        "8 rehydrate len={} contains-hello={} fnv={:016x}",
        re.len(),
        re.contains("hello"),
        fnv1a(re.as_fst().as_bytes())
    );
    let mut sb3 = SetBuilder::memory();
    for k in &keys {
        sb3.insert(k).unwrap();
    }
    let bytes3 = sb3.into_inner().unwrap();
    println!(
        "8 rebuild-identical = {}",
        bytes3 == set.as_fst().as_bytes()
    );
}
