#!/usr/bin/env mirvm
---
[dependencies]
flate2 = "1"
---
// flate2 (default miniz_oxide pure-Rust backend): Deflate at levels 0/6/9 over
// ~100KB of structured repetitive text, roundtripped; exercises bit packing and
// 32KB sliding-window matching heavily.
// The zlib/gzip native container paths are exercised directly: the container
// checksums (simd-adler32's psad.bw and crc32fast's pinned pclmulqdq path) are
// both built in, so the manual container equivalent is not needed.
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// No manual container construction is needed; the native zlib/gzip APIs cover it.

// Structured repetitive text: fixed-width record + periodic fields, so the three levels differ.
fn make_data() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < 100 * 1024 {
        let line = format!(
            "rec {i:05} | user={:03} | tag={} | pad={}\n",
            i % 251,
            ["alpha", "beta", "gamma", "delta"][(i % 4) as usize],
            "xyz".repeat((i % 17) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

fn deflate_compress(level: u32, data: &[u8]) -> Vec<u8> {
    let mut e = DeflateEncoder::new(Vec::new(), Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn deflate_decompress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    DeflateDecoder::new(data).read_to_end(&mut out).unwrap();
    out
}

fn zlib_compress(level: u32, data: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn zlib_decompress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    ZlibDecoder::new(data).read_to_end(&mut out).unwrap();
    out
}

fn gzip_compress(level: u32, data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn gzip_decompress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzDecoder::new(data).read_to_end(&mut out).unwrap();
    out
}

fn main() {
    let data = make_data();
    println!("data len={} fnv={:016x}", data.len(), fnv1a(&data));

    // ① raw deflate × level 0/6/9: compressed length / stream checksum / roundtrip bool
    for level in [0u32, 6, 9] {
        let c = deflate_compress(level, &data);
        let back = deflate_decompress(&c);
        println!(
            "deflate level={level} clen={} cfnv={:016x} roundtrip={}",
            c.len(),
            fnv1a(&c),
            back == data
        );
    }

    // ② native zlib container (ZlibEncoder/ZlibDecoder, simd-adler32 checksum) × level 0/6/9
    for level in [0u32, 6, 9] {
        let wrapped = zlib_compress(level, &data);
        let back = zlib_decompress(&wrapped);
        println!(
            "zlib level={level} clen={} cfnv={:016x} roundtrip={}",
            wrapped.len(),
            fnv1a(&wrapped),
            back == data
        );
    }

    // ③ native gzip container (GzEncoder/GzDecoder, crc32fast checksum) × level 0/6/9
    for level in [0u32, 6, 9] {
        let wrapped = gzip_compress(level, &data);
        let back = gzip_decompress(&wrapped);
        println!(
            "gzip level={level} clen={} cfnv={:016x} roundtrip={}",
            wrapped.len(),
            fnv1a(&wrapped),
            back == data
        );
    }

    // ④ empty-input edge: one roundtrip each for raw / zlib container / gzip container
    let c = deflate_compress(6, b"");
    let back = deflate_decompress(&c);
    println!(
        "empty deflate clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back.is_empty()
    );
    let wrapped = zlib_compress(6, b"");
    let back = zlib_decompress(&wrapped);
    println!(
        "empty zlib clen={} roundtrip={}",
        wrapped.len(),
        back.is_empty()
    );
    let wrapped = gzip_compress(6, b"");
    let back = gzip_decompress(&wrapped);
    println!(
        "empty gzip clen={} roundtrip={}",
        wrapped.len(),
        back.is_empty()
    );

    // ⑤ streaming chunked writes (7-byte chunks straddling deflate block boundaries) + flush
    let mut e = DeflateEncoder::new(Vec::new(), Compression::new(6));
    for chunk in data.chunks(7) {
        e.write_all(chunk).unwrap();
    }
    e.flush().unwrap();
    let streamed = e.finish().unwrap();
    let back = deflate_decompress(&streamed);
    println!(
        "streamed-7B deflate clen={} cfnv={:016x} roundtrip={}",
        streamed.len(),
        fnv1a(&streamed),
        back == data
    );

    // ⑥ read-side encoder (the reverse API surface): read::DeflateEncoder pulls from a reader
    let mut re = flate2::read::DeflateEncoder::new(&data[..], Compression::new(9));
    let mut c = Vec::new();
    re.read_to_end(&mut c).unwrap();
    let back = deflate_decompress(&c);
    println!(
        "read-side deflate level=9 clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == data
    );

    // ⑦ error path a: reserved block type (BTYPE=0b11) -> inflate fails immediately
    let junk = [0xffu8; 4];
    let mut sink = Vec::new();
    let err = DeflateDecoder::new(&junk[..])
        .read_to_end(&mut sink)
        .unwrap_err();
    println!("bad-block kind={:?} msg={}", err.kind(), err);

    // ⑧ error path b: truncated deflate stream -> output shorter than the input (library-defined; differential)
    let full = deflate_compress(6, &data);
    let cut = &full[..full.len() / 2];
    let mut sink = Vec::new();
    let r = DeflateDecoder::new(cut).read_to_end(&mut sink);
    println!(
        "truncated deflate ok={} out-len={} orig-len={}",
        r.is_ok(),
        sink.len(),
        data.len()
    );
}
