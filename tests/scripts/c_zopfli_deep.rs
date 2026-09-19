#!/usr/bin/env mirvm
---
[dependencies]
zopfli = "0.8"
flate2 = "1"
---
// zopfli 0.8 (pure-Rust high-ratio deflate; recomputation pressure on the JIT): three
// inputs (768B structured text / 384B seeded random / 768B mixed) × three container
// formats (Zlib/Gzip/Deflate) through the full compress() matrix, iterations pinned
// to 2; output is byte-for-byte deterministic for the same input+options, and each
// stream is anchored by length + FNV-1a. roundtrip decodes with the flate2 real
// containers (ZlibDecoder/GzDecoder/DeflateDecoder go through the crc32fast/simd-
// adler32 hardware paths, all built in). Covers the three compress() Formats, the
// three Options field variants (iteration_count / maximum_block_splits /
// iterations_without_improvement), BlockType::Fixed/Uncompressed driving
// DeflateEncoder directly, streaming 7-byte writes (buffered/unbuffered),
// get_ref/get_mut, empty/single-byte boundaries, and the three decode error paths
// (truncated/junk/format cross). Determinism: seeded xorshift64*, no time/address/path
// printed, binary output only len+fnv+bool. Sizes and iterations are pinned to fit
// the MIRVM_JIT_THRESHOLD=1 < 8 minute budget: one zopfli compress under mirvm takes
// ~10-50s (random shape worst), so inputs are smaller than the suggested 50KB/20KB/100KB.
use std::io::{Read, Write};
use std::num::NonZeroU64;

use zopfli::{BlockType, DeflateEncoder, Format, GzipEncoder, Options, ZlibEncoder};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Seeded xorshift64* (same sequence under native and mirvm).
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

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

// Structured repetitive text: fixed-width record numbers + small-cycle fields (highly compressible, so squeeze pays off).
// Size pinned to fit the MIRVM_JIT_THRESHOLD=1 < 8 minute budget (reduce the input first if it times out).
const TEXT_LEN: usize = 768;
// Seeded random (incompressible shape; zopfli still pays the full LZ77/squeeze cost -- the priciest shape under mirvm).
const RAND_LEN: usize = 384;
// Mixed: alternating text and random segments (exercises block splitting and block-type switching).
const MIXED_LEN: usize = 768;
// Random-segment source pool (must be >= the longest rl segment; see make_mixed).
const RAND_POOL: usize = 768;

fn make_text() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < TEXT_LEN {
        let line = format!(
            "row {i:05} | user={:03} | tag={} | msg={}\n",
            i % 251,
            ["alpha", "beta", "gamma", "delta"][(i % 4) as usize],
            "payload".repeat((i % 5 + 1) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

fn make_mixed() -> Vec<u8> {
    let text = make_text();
    let rand = Rng(0x9E3779B97F4A7C15).bytes(RAND_POOL);
    let mut d = Vec::new();
    let (mut ti, mut ri) = (0usize, 0usize);
    while d.len() < MIXED_LEN {
        let tl = 120 + (ti % 5) * 34;
        let s = ti % (text.len() - tl);
        d.extend_from_slice(&text[s..s + tl]);
        ti += tl;
        let rl = 60 + (ri % 3) * 26;
        let s = ri % (rand.len() - rl);
        d.extend_from_slice(&rand[s..s + rl]);
        ri += rl;
    }
    d
}

/// Main options: iterations pinned to 2 (the smallest multi-pass squeeze that fits the
/// time budget; the default 15 exceeds 8 minutes at MIRVM_JIT_THRESHOLD=1), rest default.
fn zopts() -> Options {
    Options {
        iteration_count: NonZeroU64::new(2).unwrap(),
        ..Options::default()
    }
}

/// Decode with the flate2 real containers (the error path returns Err, which the caller prints).
fn inflate(fmt: &Format, c: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    match fmt {
        Format::Deflate => {
            flate2::read::DeflateDecoder::new(c).read_to_end(&mut out)?;
        }
        Format::Zlib => {
            flate2::read::ZlibDecoder::new(c).read_to_end(&mut out)?;
        }
        Format::Gzip => {
            flate2::read::GzDecoder::new(c).read_to_end(&mut out)?;
        }
    }
    Ok(out)
}

/// One top-level compress() plus a flate2 decode roundtrip; prints the anchor line.
fn run_compress(tag: &str, opts: Options, fmt: &Format, data: &[u8]) -> Vec<u8> {
    let mut c = Vec::new();
    zopfli::compress(opts, *fmt, &data[..], &mut c).unwrap();
    let back = inflate(fmt, &c).unwrap();
    println!(
        "{tag} clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == data
    );
    c
}

fn main() {
    let text = make_text();
    let rand = Rng(0xDEADBEEFCAFEF00D).bytes(RAND_LEN);
    let mixed = make_mixed();
    for (name, p) in [("text", &text), ("rand", &rand), ("mixed", &mixed)] {
        println!("payload {name} len={} fnv={:016x}", p.len(), fnv1a(p));
    }

    // ① Main matrix: 3 payloads × Format::{Zlib,Gzip,Deflate}, iterations=2
    for (name, p) in [("text", &text), ("rand", &rand), ("mixed", &mixed)] {
        for fmt in [Format::Zlib, Format::Gzip, Format::Deflate] {
            let fname = match fmt {
                Format::Zlib => "zlib",
                Format::Gzip => "gzip",
                Format::Deflate => "deflate",
            };
            run_compress(&format!("matrix {name}/{fname}"), zopts(), &fmt, p);
        }
    }

    // ② Options field variants (text/deflate): iteration 8 is better but deterministic on the same terms;
    //    maximum_block_splits=0 disables block splitting; iterations_without_improvement=1 stops early.
    let mut o8 = zopts();
    o8.iteration_count = NonZeroU64::new(8).unwrap();
    run_compress("opts iter=8", o8, &Format::Deflate, &text);
    let mut os0 = zopts();
    os0.maximum_block_splits = 0;
    run_compress("opts splits=0", os0, &Format::Deflate, &text);
    let mut oi1 = zopts();
    oi1.iterations_without_improvement = NonZeroU64::new(1).unwrap();
    run_compress("opts no-imp=1", oi1, &Format::Deflate, &text);

    // ③ BlockType driving DeflateEncoder directly (the two non-dynamic paths that bypass zopfli squeeze)
    for (bt, btname) in [(BlockType::Fixed, "fixed"), (BlockType::Uncompressed, "uncompressed")] {
        let mut e = DeflateEncoder::new(zopts(), bt, Vec::new());
        e.write_all(&text).unwrap();
        let c = e.finish().unwrap();
        let back = inflate(&Format::Deflate, &c).unwrap();
        println!(
            "blocktype {btname} clen={} cfnv={:016x} roundtrip={}",
            c.len(),
            fnv1a(&c),
            back == text
        );
    }

    // ④ Streaming small writes: the unbuffered DeflateEncoder writes 7B at a time -- its
    //    Write semantics run a full compress_chunk per chunk (the whole LZ77+squeeze path),
    //    so only the first 280B is used (40 full-path calls): bounded time, distinct shape.
    //    The buffered GzipEncoder also writes 7B at a time (BufWriter aggregation, as compress).
    let stream_in = &text[..280];
    let mut e = DeflateEncoder::new(zopts(), BlockType::Dynamic, Vec::new());
    for chunk in stream_in.chunks(7) {
        e.write_all(chunk).unwrap();
    }
    println!("stream deflate get_ref-len={}", e.get_ref().len());
    let c = e.finish().unwrap();
    let back = inflate(&Format::Deflate, &c).unwrap();
    println!(
        "stream-7B deflate clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == stream_in
    );
    // Buffered path: new_buffered returns BufWriter<GzipEncoder>, so small chunks are aggregated before feeding.
    let mut bw = GzipEncoder::new_buffered(zopts(), BlockType::Dynamic, Vec::new()).unwrap();
    for chunk in text.chunks(7) {
        bw.write_all(chunk).unwrap();
    }
    let enc = match bw.into_inner() {
        Ok(e) => e,
        Err(_) => panic!("gzip buffered flush failed"),
    };
    let gz = enc.finish().unwrap();
    let back = inflate(&Format::Gzip, &gz).unwrap();
    println!(
        "stream-7B gzip clen={} cfnv={:016x} roundtrip={}",
        gz.len(),
        fnv1a(&gz),
        back == text
    );
    // ZlibEncoder::new (unbuffered) single-call path + a get_mut probe.
    let kib = &text[..512];
    let mut ze = ZlibEncoder::new(zopts(), BlockType::Dynamic, Vec::new()).unwrap();
    ze.get_mut().reserve(64);
    ze.write_all(kib).unwrap();
    let c = ze.finish().unwrap();
    let back = inflate(&Format::Zlib, &c).unwrap();
    println!(
        "oneshot zlib-1K clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == kib
    );

    // ⑤ Boundaries: empty × 3 formats + single-byte deflate
    let empty: Vec<u8> = Vec::new();
    for fmt in [Format::Zlib, Format::Gzip, Format::Deflate] {
        let fname = match fmt {
            Format::Zlib => "zlib",
            Format::Gzip => "gzip",
            Format::Deflate => "deflate",
        };
        run_compress(&format!("empty {fname}"), zopts(), &fmt, &empty);
    }
    let one = b"x".to_vec();
    run_compress("one-byte deflate", zopts(), &Format::Deflate, &one);

    // ⑥ Error paths (flate2 decode side; the text is a fixed library string): truncated / junk / format cross
    let full_gz = run_compress("errbase text/gzip", zopts(), &Format::Gzip, &text[..512]);
    let cut = &full_gz[..full_gz.len() / 2];
    let err = inflate(&Format::Gzip, cut).unwrap_err();
    println!("truncated-gzip kind={:?} msg={}", err.kind(), err);
    let junk = Rng(0x123456789ABCDEF0).bytes(512);
    let err = inflate(&Format::Zlib, &junk).unwrap_err();
    println!("junk-zlib kind={:?} msg={}", err.kind(), err);
    let zl = run_compress("errbase text/zlib", zopts(), &Format::Zlib, &text[..512]);
    let err = inflate(&Format::Gzip, &zl).unwrap_err();
    println!("cross gzip<-zlib kind={:?} msg={}", err.kind(), err);
}
