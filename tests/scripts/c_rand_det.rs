#!/usr/bin/env mirvm
---
[dependencies]
rand = "0.9"
rand_chacha = "0.9"
---
// rand 0.9 + rand_chacha 0.9: seeded ChaCha8/ChaCha20 dual-stream draws; output is
// fully deterministic, so runs compare bit-for-bit. Covers seed_from_u64, u64/f64
// draws (f64 bit-locked via to_bits), shuffle, random_range over several integer
// spans, random_bool/random_ratio fixed-trial counts, random_iter/sample_iter, Uniform, error paths.
use rand::distr::{Alphanumeric, Uniform};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::{ChaCha20Rng, ChaCha8Rng};

fn main() {
    // ① ChaCha8 stream: u64 / f64 draws
    let mut r8 = ChaCha8Rng::seed_from_u64(0x1234_5678_9abc_def0);
    let mut mix: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for i in 0..8usize {
        let v: u64 = r8.random();
        mix ^= v;
        mix = mix.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
        println!("c8 u64[{i}] = {v:016x}");
    }
    for i in 0..4usize {
        let f: f64 = r8.random(); // [0,1)
        println!("c8 f64[{i}] bits = {:016x}", f.to_bits());
    }
    println!("c8 u64 fnv = {mix:016x}");

    // ② ChaCha20 stream: a second engine of the same shape
    let mut r20 = ChaCha20Rng::seed_from_u64(0xfedc_ba98_7654_3210);
    let mut mix2: u64 = 0xcbf2_9ce4_8422_2325;
    for i in 0..8usize {
        let v: u64 = r20.random();
        mix2 ^= v;
        mix2 = mix2.wrapping_mul(0x0000_0100_0000_01b3);
        println!("c20 u64[{i}] = {v:016x}");
    }
    for i in 0..4usize {
        let f: f64 = r20.random();
        println!("c20 f64[{i}] bits = {:016x}", f.to_bits());
    }
    println!("c20 u64 fnv = {mix2:016x}");

    // ③ shuffle a fixed vec (once per engine)
    let base: Vec<i32> = (0..16).collect();
    let mut v8 = base.clone();
    v8.shuffle(&mut r8);
    println!("shuffled8  = {v8:?}");
    let mut v20 = base.clone();
    v20.shuffle(&mut r20);
    println!("shuffled20 = {v20:?}");
    let mut sorted = v8.clone();
    sorted.sort_unstable();
    println!("shuffle is permutation = {}", sorted == base);

    // ④ random_range: several integer spans + one float span
    println!("rr u32  0..100      = {}", r8.random_range(0u32..100));
    println!("rr i64  -500..=500  = {}", r8.random_range(-500i64..=500));
    println!("rr u64  full range  = {:016x}", r8.random_range(u64::MIN..=u64::MAX));
    println!("rr i32  full range  = {}", r8.random_range(i32::MIN..=i32::MAX));
    println!("rr usize 1..=16     = {}", r8.random_range(1usize..=16));
    let fr: f64 = r8.random_range(-1.5f64..2.5f64);
    println!("rr f64  -1.5..2.5 bits = {:016x}", fr.to_bits());

    // ⑤ bool ratio counts (fixed trial count, seeded -> counts are deterministic)
    let mut t_bool = 0u32;
    for _ in 0..10_000 {
        if r20.random_bool(0.3) {
            t_bool += 1;
        }
    }
    println!("bool(0.3) trues / 10000 = {t_bool}");
    let mut t_ratio = 0u32;
    for _ in 0..10_000 {
        if r20.random_ratio(1, 4) {
            t_ratio += 1;
        }
    }
    println!("ratio(1/4) trues / 10000 = {t_ratio}");
    let mut t_std = 0u32;
    for _ in 0..10_000 {
        if r20.random::<bool>() {
            t_std += 1;
        }
    }
    println!("standard bool trues / 10000 = {t_std}");

    // ⑥ random_iter / sample_iter (random_sample does not exist in rand 0.9, only in 0.10)
    let bytes: Vec<u8> = (&mut r8).random_iter::<u8>().take(16).collect();
    let mut bchk: u64 = 0xcbf2_9ce4_8422_2325;
    for b in &bytes {
        bchk ^= *b as u64;
        bchk = bchk.wrapping_mul(0x0000_0100_0000_01b3);
    }
    println!("iter bytes len = {} fnv = {bchk:016x}", bytes.len());

    let alnum: String = (&mut r8)
        .sample_iter(Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    println!("alnum = {alnum}");

    // ⑦ Uniform distribution sampling + error paths
    let uni = Uniform::new(-100i32, 100i32).unwrap();
    let vals: Vec<i32> = (&mut r20).sample_iter(&uni).take(8).collect();
    println!("uniform(-100,100) x8 = {vals:?}");
    let uni_f = Uniform::new_inclusive(0.0f64, 1.0f64).unwrap();
    let fv: f64 = r20.sample(uni_f);
    println!("uniform_incl f64 bits = {:016x}", fv.to_bits());
    match Uniform::new(10u32, 5u32) {
        Ok(_) => println!("uniform inverted: unexpected ok"),
        Err(e) => println!("uniform inverted err = {e:?}"),
    }

    // ⑧ Dual-engine cross-check: rebuilding a stream from the same seed must be byte-identical
    let r8a = ChaCha8Rng::seed_from_u64(42);
    let r8b = ChaCha8Rng::seed_from_u64(42);
    let sa: Vec<u64> = r8a.random_iter::<u64>().take(4).collect();
    let sb: Vec<u64> = r8b.random_iter::<u64>().take(4).collect();
    println!("reseed reproducible = {}", sa == sb);
    println!("reseed stream = {sa:?}");
}
