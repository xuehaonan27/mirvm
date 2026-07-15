#!/usr/bin/env mirvm
---
[dependencies]
rustfft = { version = "6", default-features = false }
---
// rustfft 6：FftPlanner 规划 forward/inverse FFT。default-features=false 关
// avx/sse 运行期探测，固定走 scalar 路径（默认 avx feature 下 mirvm 撞
// FRONTIER：llvm.x86.avx2.gather.q.pd.256 未内建，native/mirvm 也无法保证
// 同一代码路径，逐字节对拍无意义）。覆盖：2 的幂（8/64/256）与非 2 的幂
// （63=9·7 GoodThomas、61 素数 Rader、100=4·25、120=8·3·5）长度，n=1 边界；
// f64/f32 双精度族；delta/常数解析已知解；forward→inverse（不归一化，手动
// ÷n）roundtrip 位差。输出：谱向量 bits 的 FNV-1a、前 4 项 bits、roundtrip
// max ULP 差。随机序列用固定种子 xorshift64*（无外部随机 crate）。
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
    // (-1, 1) 均匀，53 bit 精度
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

    // delta 序列 → 谱全 1（解析解，按位检查）
    let mut d = vec![Complex { re: 0.0f64, im: 0.0f64 }; n];
    d[0] = Complex { re: 1.0, im: 0.0 };
    fwd.process(&mut d);
    let all_one = d
        .iter()
        .all(|c| c.re.to_bits() == 1.0f64.to_bits() && c.im.to_bits() == 0.0f64.to_bits());
    println!("f64 n={n} delta all-ones bitexact={all_one}");

    // 常数序列 → X[0]=n，其余 ≈ 0
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
    // 2 的幂
    for (n, seed) in [(1usize, 0x1234u64), (8, 0xBEEF), (64, 0xFEED), (256, 0xABCD)] {
        roundtrip_f64(n, seed);
    }
    // 非 2 的幂：63=9·7（GoodThomas）、61（素数 Rader）、100=4·25、120=8·3·5
    for (n, seed) in [(63usize, 0x7777u64), (61, 0x9999), (100, 0x5555), (120, 0x3333)] {
        roundtrip_f64(n, seed);
    }
    // f32 族：2 的幂 + 非 2 的幂 + 素数
    for (n, seed) in [(32usize, 0x4242u64), (50, 0x2424), (17, 0x1717)] {
        roundtrip_f32(n, seed);
    }
}
