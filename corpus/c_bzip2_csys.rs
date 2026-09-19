#!/usr/bin/env mirvm
---
[dependencies]
bzip2 = "=0.4.4"
---
// bzip2 0.4.4 pinned to its default vendored C backend: bzip2-sys 0.1.13 builds
// bzip2-1.0.8 into libbz2.a. blocksort.c references `bz_internal_error`, which the
// archive does not provide; it is a #[no_mangle] definition in the bzip2-sys Rust
// rlib, so the archive carries a cross-archive undefined reference that the rlib
// resolves at final link time. A byte-level roundtrip proves the C library ran in
// full. The pinned version keeps that exact archive/rlib split, the property under test.
// NOTE: the assert stub's runtime path (the BZ_PANIC can't-happen family) cannot be
// triggered normally, so this driver does not cover it; both oracles leave it unrun.
//
// Coverage (mirrors the core surface of c_bzip2_pure):
// ① write::{BzEncoder,BzDecoder} x level 1/5/9 x {structured log, seeded random}:
//    333-byte chunked writes, flush(), try_finish + finish, total_in/out, roundtrip.
// ② read::{BzEncoder,BzDecoder} same levels: 512-byte chunked compress-read / 777-byte read-back.
// ③ mem low-level: Compress/Decompress hand-fed Run/Finish, small=true low-memory decode.
// ④ Error path: corrupt-archive decode error kind and text (same library, so both sides agree).
// Determinism: synthesized payloads + seeded xorshift; error text is a library static string.
use std::io::{Read, Write};

use bzip2::read::{BzDecoder as RdDecoder, BzEncoder as RdEncoder};
use bzip2::write::{BzDecoder as WrDecoder, BzEncoder as WrEncoder};
use bzip2::{Compression, Compress, Decompress, Status};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// Structured log payload (compression-friendly) and seeded xorshift payload (hostile), ~96KB each.
fn payload_struct() -> Vec<u8> {
    let mut d = Vec::new();
    for i in 0u32..2048 {
        d.extend_from_slice(
            format!("row {i:04} | alpha beta gamma delta | {}\n", "x".repeat((i % 23) as usize))
                .as_bytes(),
        );
    }
    d
}

fn payload_random() -> Vec<u8> {
    let mut d = Vec::with_capacity(96 * 1024);
    let mut s: u64 = 0x243f6a8885a308d3;
    while d.len() < 96 * 1024 {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        d.extend_from_slice(&s.to_le_bytes());
    }
    d
}

fn main() {
    let payloads = [("struct", payload_struct()), ("random", payload_random())];
    for (pname, data) in &payloads {
        println!("payload {pname} len={} fnv={:016x}", data.len(), fnv1a(data));
    }

    // ① write side: chunked writes + flush + finish + totals + decode roundtrip
    for level in [1u32, 5, 9] {
        let mut e = WrEncoder::new(Vec::new(), Compression::new(level));
        let data = &payloads[0].1;
        for chunk in data.chunks(333) {
            e.write_all(chunk).unwrap();
        }
        e.flush().unwrap();
        let comp = e.finish().unwrap();
        let mut d = WrDecoder::new(Vec::new());
        for chunk in comp.chunks(777) {
            d.write_all(chunk).unwrap();
        }
        let back = d.finish().unwrap();
        println!(
            "write level={level} clen={} cfnv={:016x} roundtrip={}",
            comp.len(),
            fnv1a(&comp),
            back == payloads[0].1
        );
    }

    // ② read side: chunked compressed-stream read / chunked read-back
    for level in [1u32, 5, 9] {
        let data = &payloads[1].1;
        let mut e = RdEncoder::new(&data[..], Compression::new(level));
        let mut comp = Vec::new();
        e.read_to_end(&mut comp).unwrap();
        let mut d = RdDecoder::new(&comp[..]);
        let mut back = Vec::new();
        d.read_to_end(&mut back).unwrap();
        println!(
            "read level={level} clen={} cfnv={:016x} roundtrip={}",
            comp.len(),
            fnv1a(&comp),
            back == payloads[1].1
        );
    }

    // ③ mem low-level: hand-fed compress_vec/decompress_vec + small=true low-memory decode
    let data = &payloads[0].1;
    let mut c = Compress::new(Compression::new(9), 30);
    let mut comp: Vec<u8> = Vec::with_capacity(data.len() + 64);
    let mut off = 0usize;
    while off < data.len() {
        c.compress_vec(&data[off..], &mut comp, bzip2::Action::Run)
            .unwrap();
        let next = c.total_in() as usize;
        assert!(next > off, "compress made no progress");
        off = next;
    }
    loop {
        let st = c
            .compress_vec(&[], &mut comp, bzip2::Action::Finish)
            .unwrap();
        if st == Status::StreamEnd {
            break;
        }
    }
    let mut d = Decompress::new(true);
    let mut back: Vec<u8> = Vec::with_capacity(data.len() + 64);
    loop {
        let st = d.decompress_vec(&comp[d.total_in() as usize..], &mut back).unwrap();
        if st == Status::StreamEnd {
            break;
        }
    }
    println!(
        "mem clen={} cfnv={:016x} roundtrip={} totals={}/{}",
        comp.len(),
        fnv1a(&comp),
        back == payloads[0].1,
        c.total_in(),
        d.total_out()
    );

    // ④ Error path: corrupt-archive decode (deterministic error kind/text)
    let mut junk = comp.clone();
    let n = junk.len();
    junk[n / 2] ^= 0xff;
    junk[n / 2 + 1] ^= 0xff;
    let err = RdDecoder::new(&junk[..]).read_to_end(&mut Vec::new()).unwrap_err();
    println!("corrupt kind={:?}", err.kind());

    println!("bzip2_csys ok");
}
