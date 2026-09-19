#!/usr/bin/env mirvm
---
[dependencies]
brotli = "8"
---
// brotli 8 (pure Rust): the one-shot BrotliCompress/BrotliDecompress and the
// streaming CompressorWriter/Decompressor/CompressorReader API paths; quality 1/9 x
// several lgwin windows x payload shapes (structured repeats/zeros/pseudorandom/mixed).
// The encoder is a large state machine plus entropy coding (Huffman, context modeling,
// static dictionary, block splitting); output is byte-for-byte deterministic for the same
// parameters and input. Printed: original/compressed length + FNV-1a checksum + roundtrip
// bool; error paths print only is_err and out-len, never the concrete error text.
use std::io::{Read, Write};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// xorshift64 fixed-seed pseudorandom bytes (for the incompressible payload).
fn prng_bytes(n: usize, mut s: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(n);
    out
}

// Structured repeating text: fixed-width record ids + small-period fields; q1 and q9 diverge.
fn make_text() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < 32 * 1024 {
        let line = format!(
            "row {i:05} | user={:03} | tag={} | pad={}\n",
            i % 251,
            ["alpha", "beta", "gamma", "delta"][(i % 4) as usize],
            "xyz".repeat((i % 17) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

// Mixed payload: alternating text/pseudorandom segments to stress block splitting/context switches.
fn make_mixed() -> Vec<u8> {
    let text = make_text();
    let rand = prng_bytes(12 * 1024, 0x9e3779b97f4a7c15);
    let mut d = Vec::new();
    let (mut ti, mut ri) = (0usize, 0usize);
    while d.len() < 24 * 1024 {
        let tl = 700 + (ti % 5) * 130;
        d.extend_from_slice(&text[ti % (text.len() - tl)..ti % (text.len() - tl) + tl]);
        ti += tl;
        let rl = 300 + (ri % 3) * 90;
        d.extend_from_slice(&rand[ri % (rand.len() - rl)..ri % (rand.len() - rl) + rl]);
        ri += rl;
    }
    d
}

// One-shot compression: params from BrotliEncoderInitParams, override quality/lgwin.
fn oneshot_compress(data: &[u8], q: i32, lgwin: i32) -> (Vec<u8>, usize) {
    let mut params = brotli::enc::BrotliEncoderInitParams();
    params.quality = q;
    params.lgwin = lgwin;
    let mut out = Vec::new();
    let written = brotli::BrotliCompress(&mut &data[..], &mut out, &params).unwrap();
    (out, written)
}

fn oneshot_decompress(c: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::BrotliDecompress(&mut &c[..], &mut out).unwrap();
    out
}

// Stream compression: feed CompressorWriter in chunks; into_inner flushes (FINISH).
fn stream_compress(data: &[u8], chunk: usize, q: u32, lgwin: u32) -> Vec<u8> {
    let mut w = brotli::CompressorWriter::new(Vec::new(), 4096, q, lgwin);
    for c in data.chunks(chunk) {
        w.write_all(c).unwrap();
    }
    w.into_inner()
}

// Stream decompression: read from a Decompressor in a small read_chunk-sized buffer.
fn stream_decompress(c: &[u8], read_chunk: usize) -> Vec<u8> {
    let mut r = brotli::Decompressor::new(&c[..], 4096);
    let mut out = Vec::new();
    let mut buf = vec![0u8; read_chunk];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => panic!("stream decompress failed: {e:?}"),
        }
    }
    out
}

fn report(tag: &str, orig: &[u8], c: &[u8], extra: &str, back: &[u8]) {
    println!(
        "{tag} clen={} cfnv={:016x}{} roundtrip={}",
        c.len(),
        fnv1a(c),
        extra,
        back == orig
    );
}

fn main() {
    let text = make_text();
    let zeros = vec![0u8; 32 * 1024];
    let rand = prng_bytes(16 * 1024, 0xdeadbeefcafef00d);
    let mixed = make_mixed();
    let one = b"x".to_vec();
    let empty: Vec<u8> = Vec::new();
    for (name, p) in [
        ("text", &text),
        ("zeros", &zeros),
        ("rand", &rand),
        ("mixed", &mixed),
        ("one", &one),
        ("empty", &empty),
    ] {
        println!("payload {name} len={} fnv={:016x}", p.len(), fnv1a(p));
    }

    // ① One-shot: text/mixed over the full quality{1,9} x lgwin{10,24} matrix
    for (name, p) in [("text", &text), ("mixed", &mixed)] {
        for q in [1, 9] {
            for w in [10, 24] {
                let (c, written) = oneshot_compress(p, q, w);
                let back = oneshot_decompress(&c);
                let extra = format!(" ret={}", written);
                report(&format!("oneshot {name} q={q} lgwin={w}"), p, &c, &extra, &back);
            }
        }
    }
    // ② Remaining payload shapes at two parameter sets (low quality/small window, high/large)
    for (name, p) in [("zeros", &zeros), ("rand", &rand), ("one", &one), ("empty", &empty)] {
        for (q, w) in [(1, 10), (9, 24)] {
            let (c, written) = oneshot_compress(p, q, w);
            let back = oneshot_decompress(&c);
            let extra = format!(" ret={}", written);
            report(&format!("oneshot {name} q={q} lgwin={w}"), p, &c, &extra, &back);
        }
    }

    // ③ Streaming writer (7-byte chunks crossing metablock boundaries) + reader (13-byte buffer)
    for (name, p, q, w) in [
        ("text", &text, 1u32, 12u32),
        ("text", &text, 9, 22),
        ("mixed", &mixed, 9, 22),
    ] {
        let c = stream_compress(p, 7, q, w);
        let back = stream_decompress(&c, 13);
        report(&format!("stream-7B {name} q={q} lgwin={w}"), p, &c, "", &back);
        // the same compressed stream must also decode one-shot (container format interop)
        let back2 = oneshot_decompress(&c);
        println!("stream-7B {name} q={q} lgwin={w} oneshot-decode roundtrip={}", back2 == *p);
    }

    // ④ Read-side compressor (the reverse API): CompressorReader pulls from a reader
    let mut cr = brotli::CompressorReader::new(&text[..], 4096, 9, 22);
    let mut c = Vec::new();
    cr.read_to_end(&mut c).unwrap();
    let back = oneshot_decompress(&c);
    report("readside text q=9 lgwin=22", &text, &c, "", &back);

    // ⑤ Error path a: pseudorandom junk input -> decompression must fail (or emit short output)
    let junk = prng_bytes(256, 0x123456789abcdef0);
    let mut sink = Vec::new();
    let r = brotli::Decompressor::new(&junk[..], 4096).read_to_end(&mut sink);
    println!("junk-decode ok={} out-len={}", r.is_ok(), sink.len());

    // ⑥ Error path b: truncated high-quality stream -> the read ends early
    let (full, _) = oneshot_compress(&text, 9, 22);
    let cut = &full[..full.len() / 3];
    let mut sink = Vec::new();
    let r = brotli::Decompressor::new(cut, 4096).read_to_end(&mut sink);
    println!(
        "truncated-decode ok={} out-len={} (orig {})",
        r.is_ok(),
        sink.len(),
        text.len()
    );
}
