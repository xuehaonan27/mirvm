#!/usr/bin/env mirvm
---
[dependencies]
crc32fast = "1"
---
// crc32fast differential; every byte is compared with native. With the std feature
// on, Hasher::new probes cpuid at runtime (guest passthrough of host feature bits)
// and selects specialized/pclmulqdq.rs: a single update of >=128B takes the
// hardware CRC fold path while <128B falls back to baseline update_fast_16, so
// 127/128/129 straddle the threshold and mixed chunking exercises the hand-off.
// Covers the known vector ("123456789" -> cbf43926); one-shot hashes of
// 0/1/127/128/129/8K/1M; cross-threshold chunking (1/64/127/128/129/251/mix, plus
// 4096/65536/7777 for 1M); combine (amount tracking, new_with_initial_len, len2=0,
// four-segment chain); new_with_initial custom seeds, empty-finalize identity and
// chain continuation; reset/clone/Debug/Default; and the core::hash::Hasher adapter.
// Deterministic: an xorshift64 with a fixed seed makes the data; only hex, bools
// and counts are printed.
use crc32fast::Hasher;
use std::hash::Hash;

const KIB8: usize = 8192;
const MIB: usize = 1 << 20;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

struct XorShift(u64);

impl XorShift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn make_data(len: usize, seed: u64) -> Vec<u8> {
    let mut xs = XorShift(seed);
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        v.extend_from_slice(&xs.next_u64().to_le_bytes());
    }
    v.truncate(len);
    v
}

/// Feeds the Hasher in chunks following a repeating block-length pattern.
fn chunked(data: &[u8], pat: &[usize]) -> u32 {
    let mut h = Hasher::new();
    let mut off = 0usize;
    let mut i = 0usize;
    while off < data.len() {
        let n = pat[i % pat.len()].min(data.len() - off);
        h.update(&data[off..off + n]);
        off += n;
        i += 1;
    }
    h.finalize()
}

#[derive(Hash)]
struct Rec {
    a: u32,
    b: String,
    c: Vec<u8>,
    d: Option<(i64, bool)>,
}

fn main() {
    // (1) known vectors (zlib check values)
    println!("known empty       = {:08x}", crc32fast::hash(b""));
    let check = crc32fast::hash(b"123456789");
    println!("known 123456789   = {check:08x} ok={}", check == 0xcbf4_3926);
    let mut h = Hasher::new();
    h.update(b"123456789");
    let one = h.finalize();
    println!("known single-upd  = {one:08x} ok={}", one == check);
    let mut d = Hasher::default();
    d.update(b"123456789");
    println!("known default     = {:08x}", d.finalize());

    // (2) master data (1M, seeded xorshift) + one-shot hashes (127/128/129 edge)
    let master = make_data(MIB, 0x9E37_79B9_7F4A_7C15);
    println!("master len={} fnv={:016x}", master.len(), fnv1a(&master));
    for n in [0usize, 1, 127, 128, 129, KIB8, MIB] {
        println!("one len={n:<8} crc={:08x}", crc32fast::hash(&master[..n]));
    }

    // (3) cross-threshold chunking: small blocks baseline, 128+ pclmulqdq, mix both
    let pats: [(&str, &[usize]); 7] = [
        ("1", &[1]),
        ("64", &[64]),
        ("127", &[127]),
        ("128", &[128]),
        ("129", &[129]),
        ("251", &[251]),
        ("mix", &[1, 127, 128, 129, 64, 192, 7, 4096]),
    ];
    for n in [127usize, 128, 129, KIB8] {
        let want = crc32fast::hash(&master[..n]);
        for (name, pat) in pats {
            let got = chunked(&master[..n], pat);
            println!("chk len={n:<5} pat={name:<4} crc={got:08x} ok={}", got == want);
        }
    }
    let want = crc32fast::hash(&master);
    for (name, pat) in [
        ("128", &[128][..]),
        ("251", &[251][..]),
        ("4096", &[4096][..]),
        ("65536", &[65536][..]),
        ("7777", &[7777][..]),
        ("mix", &[1, 127, 128, 129, 64, 192, 7, 4096][..]),
    ] {
        let got = chunked(&master, pat);
        println!("chk len={MIB} pat={name:<6} crc={got:08x} ok={}", got == want);
    }

    // (4) combine: crc(a||b) assembled from crc(a) + crc(b) + len(b)
    let data8k = &master[..KIB8];
    let want8k = crc32fast::hash(data8k);
    for cut in [0usize, 1, 127, 128, 129, 4096, 8191, 8192] {
        let (a, b) = data8k.split_at(cut);
        let mut h1 = Hasher::new();
        h1.update(a);
        let mut h2 = Hasher::new();
        h2.update(b);
        h1.combine(&h2);
        let got = h1.finalize();
        println!("comb cut={cut:<5} crc={got:08x} ok={}", got == want8k);
    }
    // new_with_initial_len: preset the tail when crc and length are both known
    let (a, b) = (&master[..300], &master[300..1000]);
    let mut h1 = Hasher::new();
    h1.update(a);
    let h2 = Hasher::new_with_initial_len(crc32fast::hash(b), b.len() as u64);
    h1.combine(&h2);
    let got = h1.finalize();
    println!(
        "comb known-len  crc={got:08x} ok={}",
        got == crc32fast::hash(&master[..1000])
    );
    // four-segment chain
    let cuts = [100usize, 300, 301, 2048];
    let mut acc = Hasher::new();
    acc.update(&master[..cuts[0]]);
    for w in cuts.windows(2) {
        let mut t = Hasher::new();
        t.update(&master[w[0]..w[1]]);
        acc.combine(&t);
    }
    let got = acc.finalize();
    println!(
        "comb chain4     crc={got:08x} ok={}",
        got == crc32fast::hash(&master[..2048])
    );

    // (5) new_with_initial with a custom seed
    let d1000 = &master[..1000];
    let base = crc32fast::hash(d1000);
    for init in [0u32, 1, 0xdead_beef, 0xffff_ffff] {
        let mut h = Hasher::new_with_initial(init);
        h.update(d1000);
        let got = h.finalize();
        println!("init init={init:08x} crc={got:08x} eq_hash={}", got == base);
    }
    // empty finalize is the identity: with no update it returns the seed
    let bare = Hasher::new_with_initial(0xdead_beef).finalize();
    println!("init bare       = {bare:08x} ok={}", bare == 0xdead_beef);
    // chain continuation: new_with_initial(crc(a)) + update(b) == crc(a||b)
    let crc_a = crc32fast::hash(&master[..137]);
    let mut h = Hasher::new_with_initial(crc_a);
    h.update(&master[137..1000]);
    let got = h.finalize();
    println!("init chain      = {got:08x} ok={}", got == base);

    // (6) state machine: clone fork / reset reuse / empty update / Debug / adapter
    let mut h = Hasher::new();
    h.update(&master[..200]);
    let h2 = h.clone();
    h.update(&master[200..500]);
    let fin1 = h.finalize();
    let mut h3 = h2;
    h3.update(&master[200..500]);
    println!("clone eq        = {}", fin1 == h3.finalize());

    let mut r = Hasher::new();
    r.update(&master[..999]);
    r.reset();
    r.update(&master[..500]);
    println!(
        "reset ok        = {}",
        r.finalize() == crc32fast::hash(&master[..500])
    );

    let mut e = Hasher::new();
    e.update(&master[..128]);
    e.update(b"");
    e.update(&master[128..256]);
    println!(
        "empty-update ok = {}",
        e.finalize() == crc32fast::hash(&master[..256])
    );

    println!("debug           = {:?}", Hasher::new());

    let rec = Rec {
        a: 0xdead_beef,
        b: String::from("crc 校验 汉字"),
        c: master[..257].to_vec(),
        d: Some((-42, true)),
    };
    let mut ah = Hasher::new();
    rec.hash(&mut ah);
    println!("adapter struct  = {:016x}", std::hash::Hasher::finish(&ah));
    let mut aw = Hasher::new();
    std::hash::Hasher::write(&mut aw, b"direct write");
    let fin = std::hash::Hasher::finish(&aw);
    println!(
        "adapter write   = {fin:016x} ok={}",
        fin == u64::from(crc32fast::hash(b"direct write"))
    );
}
