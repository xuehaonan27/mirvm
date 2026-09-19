#!/usr/bin/env mirvm
---
[dependencies]
rustfft = { version = "6", default-features = false }
---
// rustfft 6: FftPlanner plans forward/inverse FFTs. default-features=false turns
// off avx/sse runtime detection so the scalar path is always taken: with the
// default avx feature mirvm traps on `llvm.x86.avx2.gather.q.pd.256`, so
// native/mirvm could not be held to the same code path for a byte-for-byte
// differential. Covers power-of-two (8/64/256) and non-power-of-two lengths
// (63=9*7 GoodThomas, 61 Rader, 100, 120), the n=1 edge, f64/f32, delta/constant
// analytic solutions, and unnormalized forward->inverse roundtrips reported as
// FNV-1a over the spectrum bits plus the max ULP difference.
use rustfft::num_complex::Complex;
use rustfft::{FftDirection, FftPlanner};

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    // uniform in (-1, 1) with 53-bit precision
    fn next_f64(&mut self) -> f64 {
        let u = (self.next_u64() >> 11) as f64 * (1.0 / 9007199254740992.0);
        u * 2.0 - 1.0
    }
    fn next_f32(&mut self) -> f32 {
        let u = (self.next_u64() >> 40) as f32 * (1.0 / 16777216.0);
        u * 2.0 - 1.0
    }
}

fn fnv_mix(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = (*h).wrapping_mul(0x100000001b3);
    }
}

fn spectrum_fnv64(xs: &[Complex<f64>]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for c in xs {
        fnv_mix(&mut h, &c.re.to_bits().to_le_bytes());
        fnv_mix(&mut h, &c.im.to_bits().to_le_bytes());
    }
    h
}

fn spectrum_fnv32(xs: &[Complex<f32>]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for c in xs {
        fnv_mix(&mut h, &c.re.to_bits().to_le_bytes());
        fnv_mix(&mut h, &c.im.to_bits().to_le_bytes());
    }
    h
}

fn dump_first4_64(tag: &str, n: usize, xs: &[Complex<f64>]) {
    for (i, c) in xs.iter().take(4).enumerate() {
        println!("{tag} n={n} x[{i}] re={:016x} im={:016x}", c.re.to_bits(), c.im.to_bits());
    }
}

fn dump_first4_32(tag: &str, n: usize, xs: &[Complex<f32>]) {
    for (i, c) in xs.iter().take(4).enumerate() {
        println!("{tag} n={n} x[{i}] re={:08x} im={:08x}", c.re.to_bits(), c.im.to_bits());
    }
}

fn roundtrip_f64(n: usize, seed: u64) {
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    println!(
        "f64 n={n} fwd len={} dir={:?} inv len={} dir={:?}",
        fwd.len(),
        fwd.fft_direction(),
        inv.len(),
        inv.fft_direction()
    );

    let mut rng = Rng(seed);
    let orig: Vec<Complex<f64>> = (0..n)
        .map(|_| Complex { re: rng.next_f64(), im: rng.next_f64() })
        .collect();

    let mut buf = orig.clone();
    fwd.process(&mut buf);
    println!("f64 n={n} spectrum len={} fnv={:016x}", buf.len(), spectrum_fnv64(&buf));
    dump_first4_64("f64", n, &buf);

    inv.process(&mut buf);
    let scale = 1.0 / n as f64;
    for c in buf.iter_mut() {
        *c = *c * scale;
    }
    let mut max_ulp = 0u64;
    let mut bitexact = true;
    for (a, b) in orig.iter().zip(buf.iter()) {
        for (x, y) in [(a.re, b.re), (a.im, b.im)] {
            max_ulp = max_ulp.max(x.to_bits().abs_diff(y.to_bits()));
            if x.to_bits() != y.to_bits() {
                bitexact = false;
            }
        }
    }
    println!("f64 n={n} roundtrip bitexact={bitexact} max_ulp={max_ulp}");

    // delta sequence -> an all-ones spectrum (analytic solution, checked per bit)
    let mut d = vec![Complex { re: 0.0f64, im: 0.0f64 }; n];
    d[0] = Complex { re: 1.0, im: 0.0 };
    fwd.process(&mut d);
    let all_one = d
        .iter()
        .all(|c| c.re.to_bits() == 1.0f64.to_bits() && c.im.to_bits() == 0.0f64.to_bits());
    println!("f64 n={n} delta all-ones bitexact={all_one}");

    // constant sequence -> X[0]=n and the rest ~0
    let mut o = vec![Complex { re: 1.0f64, im: 0.0f64 }; n];
    fwd.process(&mut o);
    let mut tail_max = 0.0f64;
    for c in o.iter().skip(1) {
        tail_max = tail_max.max(c.norm());
    }
    println!(
        "f64 n={n} ones X0 re={:016x} im={:016x} tail_maxnorm={:016x}",
        o[0].re.to_bits(),
        o[0].im.to_bits(),
        tail_max.to_bits()
    );
}

fn roundtrip_f32(n: usize, seed: u64) {
    let mut planner = FftPlanner::<f32>::new();
    let fwd = planner.plan_fft(n, FftDirection::Forward);
    let inv = planner.plan_fft(n, FftDirection::Inverse);
    println!(
        "f32 n={n} fwd len={} dir={:?} inv len={} dir={:?}",
        fwd.len(),
        fwd.fft_direction(),
        inv.len(),
        inv.fft_direction()
    );

    let mut rng = Rng(seed);
    let orig: Vec<Complex<f32>> = (0..n)
        .map(|_| Complex { re: rng.next_f32(), im: rng.next_f32() })
        .collect();

    let mut buf = orig.clone();
    fwd.process(&mut buf);
    println!("f32 n={n} spectrum len={} fnv={:016x}", buf.len(), spectrum_fnv32(&buf));
    dump_first4_32("f32", n, &buf);

    inv.process(&mut buf);
    let scale = 1.0 / n as f32;
    for c in buf.iter_mut() {
        *c = *c * scale;
    }
    let mut max_ulp = 0u32;
    let mut bitexact = true;
    for (a, b) in orig.iter().zip(buf.iter()) {
        for (x, y) in [(a.re, b.re), (a.im, b.im)] {
            max_ulp = max_ulp.max(x.to_bits().abs_diff(y.to_bits()));
            if x.to_bits() != y.to_bits() {
                bitexact = false;
            }
        }
    }
    println!("f32 n={n} roundtrip bitexact={bitexact} max_ulp={max_ulp}");
}

fn main() {
    // powers of two
    for (n, seed) in [(1usize, 0x1234u64), (8, 0xBEEF), (64, 0xFEED), (256, 0xABCD)] {
        roundtrip_f64(n, seed);
    }
    // non-powers of two: 63=9*7 (GoodThomas), 61 (prime, Rader), 100=4*25, 120=8*3*5
    for (n, seed) in [(63usize, 0x7777u64), (61, 0x9999), (100, 0x5555), (120, 0x3333)] {
        roundtrip_f64(n, seed);
    }
    // f32 family: powers of two + non-powers of two + a prime
    for (n, seed) in [(32usize, 0x4242u64), (50, 0x2424), (17, 0x1717)] {
        roundtrip_f32(n, seed);
    }
}
