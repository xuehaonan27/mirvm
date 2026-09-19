#!/usr/bin/env mirvm
---
[dependencies]
lz4_flex = "0.11"
snap = "1"
---
// lz4_flex 0.11 + snap 1 (both zero-dependency pure Rust): two-level block + frame/stream
// roundtrip differential. Compressors are byte-for-byte deterministic for the same input and
// parameters (compressed-stream FNV anchored); both sides' error `Display` text is asserted too.
//
// Known FRONTIER (unavoidable; kept as a reproduction): snap's frame layer (write::FrameEncoder /
// read::FrameDecoder / read::FrameEncoder) computes a masked crc32c for every data chunk via
// its CheckSummer, which on x86_64 dispatches at run time on `is_x86_feature_detected!("sse4.2")`
// to `crc32c_sse` (core::arch::_mm_crc32_u64/_mm_crc32_u8). mirvm does not build that intrinsic,
// so writing the first data chunk TRAPs and the process exits (exit 70):
//   mirvm[m4-engine]: TRAP: foreign `llvm.x86.sse42.crc32.64.64` (LLVM-internal symbol,
//   built in on demand) (fn ...core_arch...sse42...__mm_crc32_u64...)
// Same "runtime detection" class as aesni: mirvm's guest CPUID reports the host's sse4.2 and
// the crate has no force-soft switch; the soft table path (crc32c_slice16) computes the same
// values but cannot be forced from outside. Every snap 1.x version (1.0.0/1.0.1/1.0.5/1.1.0/
// 1.1.1, checked individually) carries this SSE dispatch, so no version pin helps; the frame
// checksum cannot be skipped either (empty input carries no data chunk and triggers none, but
// that is degenerate coverage). lz4_flex (pure-Rust xxhash) and snap::raw are unaffected.
//
// Coverage:
//  * lz4_flex::block -- compress/decompress (raw block, explicit size),
//    compress_prepend_size/decompress_size_prepended (4-byte LE size header),
//    compress_into/decompress_into (caller buffer), get_maximum_output_size;
//    error paths: junk literals / out-of-range offset / truncated stream / bad size header.
//  * lz4_flex::frame -- FrameEncoder/FrameDecoder (Write/Read streaming, 1000-byte write
//    chunks x 997-byte reads crossing block boundaries), with_frame_info custom header
//    (Max64KB / Linked / block + content checksums + explicit content_size; 96KB text -> many
//    blocks); error paths: bad magic WrongMagicNumber / truncation / flipped checksum byte.
//  * snap::raw -- Encoder/Decoder compress_vec/decompress_vec and the caller-buffer
//    compress/decompress paths, plus max_compress_len/decompress_len;
//    error paths: empty input Empty / empty header Header / oversized varint TooBig / header
//    without body / truncated stream / corrupt data byte.
//  * snap::write::FrameEncoder + snap::read::FrameDecoder (snappy framing: 64KB chunks
//    x masked crc32c), snap::read::FrameEncoder (read-side compressor, reversed API);
//    error paths: bad magic StreamHeader / mid-chunk truncation / checksum mismatch.
//
// Input tiers: structured repetitive text 96KB / all-zero 16KB / seeded xorshift64* random 32KB /
// single byte / empty. Output: length + FNV-1a + roundtrip bool + error text.
// No time/address/HashMap ordering; stderr is empty on success paths.
use std::io::{Read, Write};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Seeded xorshift64* PRNG (same sequence under native and mirvm).
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

/// Structured repetitive text: fixed-width record id + small-period fields; repeats span blocks.
fn make_text() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < 96 * 1024 {
        let line = format!(
            "row {i:05} | user={:03} | tag={} | msg={}\n",
            i % 251,
            ["alpha", "beta", "gamma", "delta"][(i % 4) as usize],
            "lz4snap".repeat((i % 9 + 1) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

/// Loop small reads to EOF: full output, or the first error (error text is also compared).
fn read_chunked<R: Read>(mut r: R, chunk: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return Err(e),
        }
    }
}

fn report(tag: &str, orig: &[u8], c: &[u8], back: &[u8]) {
    println!(
        "{tag} clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(c),
        back == orig
    );
}

fn main() {
    let text = make_text();
    let zeros = vec![0u8; 16 * 1024];
    let rand = Rng(0x9E3779B97F4A7C15).bytes(32 * 1024);
    let one = b"Q".to_vec();
    let empty: Vec<u8> = Vec::new();
    let tiers: [(&str, &[u8]); 5] = [
        ("text", &text),
        ("zeros", &zeros),
        ("rand", &rand),
        ("one", &one),
        ("empty", &empty),
    ];
    for &(name, p) in &tiers {
        println!("payload {name} len={} fnv={:016x}", p.len(), fnv1a(p));
    }

    // ---- ① lz4 block level: raw block (explicit size) x all input tiers ----
    for &(name, p) in &tiers {
        let c = lz4_flex::block::compress(p);
        let back = lz4_flex::block::decompress(&c, p.len()).unwrap();
        report(&format!("lz4-block {name}"), p, &c, &back);
    }

    // ---- ② lz4 block level: 4-byte LE size-header variant ----
    for &(name, p) in &tiers {
        let c = lz4_flex::block::compress_prepend_size(p);
        let back = lz4_flex::block::decompress_size_prepended(&c).unwrap();
        report(&format!("lz4-block-sp {name}"), p, &c, &back);
    }

    // ---- ③ lz4 block level: caller-buffer API (compress_into/decompress_into) ----
    {
        let mut cbuf = vec![0u8; lz4_flex::block::get_maximum_output_size(text.len())];
        let n = lz4_flex::block::compress_into(&text, &mut cbuf).unwrap();
        let mut out = vec![0u8; text.len()];
        let m = lz4_flex::block::decompress_into(&cbuf[..n], &mut out).unwrap();
        println!(
            "lz4-block-into text max={} clen={n} dlen={m} roundtrip={}",
            cbuf.len(),
            out == text
        );
    }

    // ---- ④ lz4 block-level error paths ----
    // 0xff start: literal-length extension byte runs past the end -> ExpectedAnotherByte
    match lz4_flex::block::decompress(b"\xff\xff\xff\xff\xff", 64) {
        Ok(v) => println!("lz4-junk-lit ok len={}", v.len()),
        Err(e) => println!("lz4-junk-lit err: {e}"),
    }
    // token 0x00: after 0 literals offset=1 but the output is empty -> OffsetOutOfBounds
    match lz4_flex::block::decompress(&[0x00, 0x01, 0x00, 0x00], 16) {
        Ok(v) => println!("lz4-junk-off ok len={}", v.len()),
        Err(e) => println!("lz4-junk-off err: {e}"),
    }
    let lz4_text_c = lz4_flex::block::compress(&text);
    let cut = &lz4_text_c[..lz4_text_c.len() * 3 / 5];
    match lz4_flex::block::decompress(cut, text.len()) {
        Ok(v) => println!("lz4-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("lz4-trunc err: {e}"),
    }
    // size header claims 5 bytes, body holds only 2
    match lz4_flex::block::decompress_size_prepended(b"\x05\x00\x00\x00zz") {
        Ok(v) => println!("lz4-sp-short ok len={}", v.len()),
        Err(e) => println!("lz4-sp-short err: {e}"),
    }
    // the size header itself is shorter than 4 bytes
    match lz4_flex::block::decompress_size_prepended(b"\x01\x02") {
        Ok(v) => println!("lz4-sp-tiny ok len={}", v.len()),
        Err(e) => println!("lz4-sp-tiny err: {e}"),
    }

    // ---- ⑤ lz4 frame: streaming Write/Read x all input tiers (1000B write x 997B read) ----
    for &(name, p) in &tiers {
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        for chunk in p.chunks(1000) {
            enc.write_all(chunk).unwrap();
        }
        let c = enc.finish().unwrap();
        let back = read_chunked(lz4_flex::frame::FrameDecoder::new(&c[..]), 997).unwrap();
        report(&format!("lz4-frame {name}"), p, &c, &back);
    }

    // ---- ⑥ lz4 frame: custom header (multi-block linked + both checksums + content_size) ----
    let fi = lz4_flex::frame::FrameInfo::new()
        .block_size(lz4_flex::frame::BlockSize::Max64KB)
        .block_mode(lz4_flex::frame::BlockMode::Linked)
        .block_checksums(true)
        .content_checksum(true)
        .content_size(Some(text.len() as u64));
    let mut enc = lz4_flex::frame::FrameEncoder::with_frame_info(fi, Vec::new());
    enc.write_all(&text).unwrap();
    let lz4fi_c = enc.finish().unwrap();
    let back = read_chunked(lz4_flex::frame::FrameDecoder::new(&lz4fi_c[..]), 4096).unwrap();
    report("lz4-frame-fi text", &text, &lz4fi_c, &back);

    // ---- ⑦ lz4 frame error paths ----
    match read_chunked(
        lz4_flex::frame::FrameDecoder::new(&b"\xde\xad\xbe\xef not an lz4 frame"[..]),
        64,
    ) {
        Ok(v) => println!("lz4-frame-badmagic ok len={}", v.len()),
        Err(e) => println!("lz4-frame-badmagic err: {e}"),
    }
    let cut = &lz4fi_c[..lz4fi_c.len() / 3];
    match read_chunked(lz4_flex::frame::FrameDecoder::new(cut), 4096) {
        Ok(v) => println!("lz4-frame-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("lz4-frame-trunc err: {e}"),
    }
    // last 4 bytes are the content checksum: flip the final byte -> ContentChecksumError
    let mut bad = lz4fi_c.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    match read_chunked(lz4_flex::frame::FrameDecoder::new(&bad[..]), 4096) {
        Ok(v) => println!("lz4-frame-corrupt ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("lz4-frame-corrupt err: {e}"),
    }

    // ---- ⑧ snap raw: Encoder/Decoder x all input tiers ----
    for n in [0usize, 1, 1000, 96 * 1024] {
        println!("snap-max_compress_len({n}) = {}", snap::raw::max_compress_len(n));
    }
    for &(name, p) in &tiers {
        let c = snap::raw::Encoder::new().compress_vec(p).unwrap();
        let dlen = snap::raw::decompress_len(&c).unwrap();
        let back = snap::raw::Decoder::new().decompress_vec(&c).unwrap();
        println!(
            "snap-raw {name} clen={} cfnv={:016x} dlen={} roundtrip={}",
            c.len(),
            fnv1a(&c),
            dlen,
            back == p
        );
    }

    // ---- ⑨ snap raw: caller-buffer API (compress/decompress) ----
    {
        let mut cbuf = vec![0u8; snap::raw::max_compress_len(text.len())];
        let n = snap::raw::Encoder::new()
            .compress(&text, &mut cbuf)
            .unwrap();
        let mut out = vec![0u8; text.len()];
        let m = snap::raw::Decoder::new()
            .decompress(&cbuf[..n], &mut out)
            .unwrap();
        println!(
            "snap-raw-into text max={} clen={n} dlen={m} roundtrip={}",
            cbuf.len(),
            out == text
        );
    }

    // ---- ⑩ snap raw error paths ----
    match snap::raw::Decoder::new().decompress_vec(b"") {
        Ok(v) => println!("snap-empty ok len={}", v.len()),
        Err(e) => println!("snap-empty err: {e}"),
    }
    match snap::raw::decompress_len(b"") {
        Ok(n) => println!("snap-hdrlen-empty ok {n}"),
        Err(e) => println!("snap-hdrlen-empty err: {e}"),
    }
    // 5x0xff varint header ~= 3.4e10 > 2^32-1 -> TooBig (checked before any allocation)
    match snap::raw::Decoder::new().decompress_vec(&[0xff; 5]) {
        Ok(v) => println!("snap-toobig ok len={}", v.len()),
        Err(e) => println!("snap-toobig err: {e}"),
    }
    // varint header claims 128 bytes but there is no data body
    match snap::raw::Decoder::new().decompress_vec(&[0x80, 0x01]) {
        Ok(v) => println!("snap-nobody ok len={}", v.len()),
        Err(e) => println!("snap-nobody err: {e}"),
    }
    let snap_text_c = snap::raw::Encoder::new().compress_vec(&text).unwrap();
    let cut = &snap_text_c[..snap_text_c.len() / 2];
    match snap::raw::Decoder::new().decompress_vec(cut) {
        Ok(v) => println!("snap-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-trunc err: {e}"),
    }
    let mut bad = snap_text_c.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0xFF;
    match snap::raw::Decoder::new().decompress_vec(&bad) {
        Ok(v) => println!("snap-corrupt ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-corrupt err: {e}"),
    }

    // ---- ⑪ snap frame: write::FrameEncoder + read::FrameDecoder x all input tiers ----
    for &(name, p) in &tiers {
        let mut enc = snap::write::FrameEncoder::new(Vec::new());
        for chunk in p.chunks(1000) {
            enc.write_all(chunk).unwrap();
        }
        let c = enc.into_inner().unwrap();
        let back = read_chunked(snap::read::FrameDecoder::new(&c[..]), 997).unwrap();
        report(&format!("snap-frame {name}"), p, &c, &back);
    }

    // ---- ⑫ snap frame: read-side compressor (read::FrameEncoder, reversed API) ----
    let mut rside = Vec::new();
    snap::read::FrameEncoder::new(&text[..])
        .read_to_end(&mut rside)
        .unwrap();
    let back = read_chunked(snap::read::FrameDecoder::new(&rside[..]), 2048).unwrap();
    report("snap-frame-readside text", &text, &rside, &back);

    // ---- ⑬ snap frame error paths ----
    match read_chunked(
        snap::read::FrameDecoder::new(&b"definitely not a snappy stream!!"[..]),
        64,
    ) {
        Ok(v) => println!("snap-frame-badmagic ok len={}", v.len()),
        Err(e) => println!("snap-frame-badmagic err: {e}"),
    }
    let mut enc = snap::write::FrameEncoder::new(Vec::new());
    enc.write_all(&text).unwrap();
    let snap_frame_c = enc.into_inner().unwrap();
    // 96KB text -> two 64KB data chunks; a 1/3 truncation lands mid-way through the first chunk.
    let cut = &snap_frame_c[..snap_frame_c.len() / 3];
    match read_chunked(snap::read::FrameDecoder::new(cut), 4096) {
        Ok(v) => println!("snap-frame-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-frame-trunc err: {e}"),
    }
    // flip one mid-chunk data byte: raw decode error or crc32c checksum error
    let mut bad = snap_frame_c.clone();
    let at = bad.len() * 2 / 3;
    bad[at] ^= 0xFF;
    match read_chunked(snap::read::FrameDecoder::new(&bad[..]), 4096) {
        Ok(v) => println!("snap-frame-corrupt ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-frame-corrupt err: {e}"),
    }
}
