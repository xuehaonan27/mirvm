#!/usr/bin/env mirvm
---
[dependencies]
# zstd pinned to exact 0.13.3 (binding C zstd 1.5.7 / zstd-sys 2.0.16 / zstd-safe 7.2.4,
# all present in the local registry cache). Default features (legacy/arrays/zdict_builder)
# stay on; the non-default zstdmt is enabled for one reason: zstd-sys only compiles the C
# library with ZSTD_MULTITHREAD + -pthread under zstdmt, so without it NbWorkers(2)
# returns only the deterministic "unsupported parameter" error and the driver's threading
# surface is unreachable (set_parameter itself is ungated; the gate is the C library's threading code).
zstd = { version = "=0.13.3", features = ["zstdmt"] }
---
// zstd 0.13.3 long-input differential over three dimensions: 16 MiB of program-generated,
// deterministic three-layer mixed data -> full compression at level 1/9, level 19 over only
// the first 4 MiB slice to bound the cost, plus one zstd::safe::CCtx multi-threaded
// parameter path (CompressionLevel(3) + NbWorkers(2), a single compress2). The C layer
// splits into ZSTDMT jobs, and the frame bytes are reproducible run to run for the same
// parameters -- thread scheduling never reaches the output. The compression and
// decompression work happens in native C with identical inputs and parameters on both
// dimensions, so the frame bytes match byte for byte (c_zstd_stream already proves that
// channel).
//
// Data generation (seeded LCG only; no time, rand or env):
//   text  8 MiB = 2 MiB of seeded-LCG log-like unique text repeated 4x
//          (extend_from_within doubling to create long-range repeats for the large-window
//          / LDM matcher at high levels);
//   rep   4 MiB = four 1 MiB purely periodic byte segments with periods
//          [6,127,4096,250000] (strictly p-periodic inside a segment: doubling keeps a
//          whole multiple of p, and the tail is padded for the same reason);
//   rnd   4 MiB = a seeded-LCG little-endian byte stream (the near-incompressible layer).
// Structural statistics: each layer's length and generation period, plus entropy counts
// over the first 256 KiB sampling window (distinct byte values, equal-adjacent runs, zero
// bytes and the longest equal run) -- all deterministic integers.
// Fingerprint: fnv64 is an FNV-1a variant that rolls over 8-byte little-endian blocks
// (zero-padding the tail into a block), keeping the iteration count bearable for the
// interpreter on a large byte-wise input; same seed and prime as FNV-1a.
//
// Coverage:
//   ① full 16 MiB bulk::compress at level 1/9 -> size + fnv64 + a permille integer ratio
//      + a byte-for-byte bulk::decompress roundtrip assertion;
//   ② level 19 over only the first 4 MiB slice (bounds the btopt cost), same accounting;
//   ③ the zstd::safe::CCtx parameter path: CompressionLevel(3) + NbWorkers(2), one
//      compress2 (the real ZSTDMT parallel surface), same roundtrip accounting;
//   ④ constant anchors: min/max/default level + runtime version_number().
//
// Deterministic: prints only integers, booleans and hex fingerprints; no absolute paths,
// addresses or HashMap order; no warnings; empty stderr. A roundtrip comparison is
// Vec<u8> ==, i.e. a byte-for-byte memcmp.
//
// Cost: level 19 over 4 MiB of highly redundant text takes seconds to tens of seconds
// (btopt), while level 9 over the full input is sub-second to seconds; the compression
// itself stays in native C on every dimension, and the interpreter/JIT only drives data
// generation and fingerprinting (a few million small iterations).
use zstd::zstd_safe::{CCtx, CParameter};

const MIB: usize = 1024 * 1024;
const TEXT_UNIQUE: usize = 2 * MIB;
const TEXT_LAYER: usize = 8 * MIB;
const REPEAT_SEG: usize = MIB;
const REPEAT_PERIODS: [usize; 4] = [6, 127, 4096, 250_000];
const RANDOM_LAYER: usize = 4 * MIB;
const STAT_WINDOW: usize = 256 * 1024;

/// Seeded LCG (MMIX parameters; wrapping u64, same sequence on every platform).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
}

/// FNV-1a variant fingerprint rolling over 8-byte blocks (1/8 the iterations, same determinism).
fn fnv64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut it = data.chunks_exact(8);
    for c in &mut it {
        h ^= u64::from_le_bytes(c.try_into().unwrap());
        h = h.wrapping_mul(0x100000001b3);
    }
    let rem = it.remainder();
    let mut tail = [0u8; 8];
    tail[..rem.len()].copy_from_slice(rem);
    h ^= u64::from_le_bytes(tail);
    h = h.wrapping_mul(0x100000001b3);
    h
}

fn push_dec(v: &mut Vec<u8>, mut x: u64) {
    let mut buf = [0u8; 20];
    let mut n = 0;
    loop {
        buf[n] = b'0' + (x % 10) as u8;
        x /= 10;
        n += 1;
        if x == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        v.push(buf[n]);
    }
}

const TEXT_TOKENS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
    "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
    "sierra", "tango", "uniform", "victor", "whiskey", "xray",
];

/// The text layer's 2 MiB of unique text: `>L<line> <3-5 words> crc=<0..999>\n` until full.
fn gen_unique_text(target: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(target + 128);
    let mut t = Lcg(0x9E3779B97F4A7C15);
    let mut n: u64 = 0;
    while v.len() < target {
        v.extend_from_slice(b">L");
        push_dec(&mut v, n);
        let words = 3 + ((t.next() >> 44) % 3) as usize;
        for _ in 0..words {
            v.push(b' ');
            v.extend_from_slice(TEXT_TOKENS[((t.next() >> 33) % 24) as usize].as_bytes());
        }
        v.extend_from_slice(b" crc=");
        push_dec(&mut v, (t.next() >> 11) % 1000);
        v.push(b'\n');
        n += 1;
    }
    v.truncate(target);
    v
}

/// rep layer: each period segment is a p-byte seeded lowercase pattern doubled up to ≤ seg,
/// with the tail padded once (the length stays a multiple of p -> strictly p-periodic content).
fn gen_repeat_layer(seg: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(REPEAT_PERIODS.len() * seg + 8);
    let mut t = Lcg(0x1B2C3D4E5F607182);
    for &p in &REPEAT_PERIODS {
        let mut pat = Vec::with_capacity(p);
        for _ in 0..p {
            pat.push(b'a' + ((t.next() >> 29) % 26) as u8);
        }
        let mut block = pat.clone();
        while block.len() * 2 <= seg {
            block.extend_from_within(0..);
        }
        if block.len() < seg {
            let need = seg - block.len();
            block.extend_from_within(0..need);
        }
        out.extend_from_slice(&block);
    }
    out
}

/// rnd layer: a seeded-LCG little-endian byte stream.
fn gen_random_layer(target: usize) -> Vec<u8> {
    let mut t = Lcg(0xDEADF00D12345678);
    let mut out = Vec::with_capacity(target + 8);
    while out.len() < target {
        out.extend_from_slice(&t.next().to_le_bytes());
    }
    out.truncate(target);
    out
}

/// Entropy counts over the first 256 KiB sampling window (fully deterministic).
fn print_stats(label: &str, d: &[u8]) {
    let n = d.len().min(STAT_WINDOW);
    let s = &d[..n];
    let mut seen = [false; 256];
    let mut runs = 0usize;
    let mut zeros = 0usize;
    let mut maxrun = 0usize;
    let mut cur = 0usize;
    let mut prev = 0u8;
    for i in 0..n {
        let b = s[i];
        seen[b as usize] = true;
        if b == 0 {
            zeros += 1;
        }
        if i > 0 && b == prev {
            runs += 1;
            cur += 1;
        } else {
            cur = 1;
        }
        if cur > maxrun {
            maxrun = cur;
        }
        prev = b;
    }
    let distinct = seen.iter().filter(|&&f| f).count();
    println!(
        "stats {label} win={n} distinct={distinct} runs={runs} zeros={zeros} maxrun={maxrun} fnv64={:016x}",
        fnv64(d)
    );
}

/// One level's compression + byte-for-byte roundtrip + permille ratio + fingerprint, one line.
fn roundtrip_line(label: &str, level: i32, input: &[u8]) {
    let comp = zstd::bulk::compress(input, level).unwrap();
    let back = zstd::bulk::decompress(&comp, input.len()).unwrap();
    let rt = back == input;
    assert!(rt, "{label} roundtrip mismatch");
    println!(
        "one {label} level={level} raw={} comp={} permille={} cfnv64={:016x} rt={rt}",
        input.len(),
        comp.len(),
        comp.len() as u64 * 1000 / input.len() as u64,
        fnv64(&comp)
    );
}

fn main() {
    // ---- Data: text (8 MiB), rep (4 MiB) and rnd (4 MiB) concatenated, 16 MiB total ----
    let text_u = gen_unique_text(TEXT_UNIQUE);
    let mut text = text_u.clone();
    while text.len() < TEXT_LAYER {
        text.extend_from_within(0..);
    }
    let rep = gen_repeat_layer(REPEAT_SEG);
    let rnd = gen_random_layer(RANDOM_LAYER);

    println!(
        "layout text={} rep={} rnd={} total={}",
        text.len(),
        rep.len(),
        rnd.len(),
        text.len() + rep.len() + rnd.len()
    );
    println!(
        "periods text-unique={} rep={:?} rnd=none",
        TEXT_UNIQUE, REPEAT_PERIODS
    );
    print_stats("text", &text);
    print_stats("rep ", &rep);
    print_stats("rnd ", &rnd);

    let mut data = text;
    data.extend_from_slice(&rep);
    data.extend_from_slice(&rnd);
    println!("raw len={} fnv64={:016x}", data.len(), fnv64(&data));

    // ---- ① levels 1 and 9 over the whole input; ② level 19 over only the first 4 MiB ----
    roundtrip_line("l1 ", 1, &data);
    roundtrip_line("l9 ", 9, &data);
    roundtrip_line("l19", 19, &data[..4 * MIB]);

    // ---- ③ zstd::safe::CCtx multi-threaded parameter path (level 3 + NbWorkers=2) ----
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(3)).unwrap();
    cctx.set_parameter(CParameter::NbWorkers(2)).unwrap();
    let mut mt: Vec<u8> = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
    let n = cctx.compress2(&mut mt, &data).unwrap();
    assert_eq!(n, mt.len(), "mt written len mismatch");
    let back = zstd::bulk::decompress(&mt, data.len()).unwrap();
    let rt = back == data;
    assert!(rt, "mt roundtrip mismatch");
    println!(
        "mt level=3 nbw=2 raw={} comp={} permille={} cfnv64={:016x} rt={rt}",
        data.len(),
        mt.len(),
        mt.len() as u64 * 1000 / data.len() as u64,
        fnv64(&mt)
    );

    // ---- ④ Constant anchors ----
    println!(
        "levels min={} max={} default={} ver={}",
        zstd::zstd_safe::min_c_level(),
        zstd::zstd_safe::max_c_level(),
        zstd::DEFAULT_COMPRESSION_LEVEL,
        zstd::zstd_safe::version_number()
    );
}
