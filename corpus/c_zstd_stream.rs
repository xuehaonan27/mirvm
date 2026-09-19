#!/usr/bin/env mirvm
---
[dependencies]
zstd = "0.13"
---
// zstd 0.13 (FFI binding to C zstd 1.5.7: zstd-sys uses cc to compile lib/*.c plus
// huf_decompress_amd64.S into a static .a, which mirvm loads via the "static archive ->
// .so -> RTLD_NOW" channel) three-way differential. Compression/decompression itself runs
// in native C with identical inputs and parameters on both sides, so frame bytes are
// deterministic; the interpreted/JIT object is the Rust wrapper/io-glue/error-string layer.
//
// Coverage:
// ① bulk::{compress,decompress,compress_to_buffer,decompress_to_buffer} ×
//    level -3/1/9/19 × {structured repetitive log, seeded xorshift random} roundtrip.
// ② stream::{Encoder,Decoder} over the same matrix: 333B chunked writes / 777B chunked reads.
// ③ Convenience surface encode_all/decode_all/copy_encode/copy_decode; with two
//    concatenated frames the default Decoder runs to EOF vs single_frame taking only the first.
// ④ Dictionaries: raw-content dict (stream Encoder/Decoder::with_dictionary),
//    prepared EncoderDictionary/DecoderDictionary (bulk with_prepared_dictionary
//    reusing one ctx to compress 6 small records), and decoding a dict frame without the dict.
// ⑤ Error paths: junk bulk/stream decode, truncated bulk frame, truncated stream (read hits
//    EOF with "incomplete frame"), illegal level=99, small capacity, empty input encode/decode.
//
// Determinism: prints only lengths/FNV-1a/booleans and zstd C library error strings
// (ZSTD_getErrorString constants); seeded xorshift64*; no time/address/HashMap order; stderr empty.
use std::io::{Read, Write};

use zstd::bulk;
use zstd::dict::{DecoderDictionary, EncoderDictionary};
use zstd::stream::{self, Decoder, Encoder};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Seeded xorshift64* (same sequence natively and under mirvm).
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

/// Structured repetitive log: highly compressible, fixed-width sequence numbers for determinism.
fn structured() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < 7168 {
        let line = format!(
            "req {i:05} user=u{:03} action={} cost={}ms status={}\n",
            (i * 13) % 97,
            ["login", "query", "logout"][(i % 3) as usize],
            (i * 7) % 41,
            ["ok", "ok", "fail"][(i % 3) as usize]
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

/// Read to EOF in 777-byte chunks; read errors propagate to the caller.
fn read_chunked<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 777];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn main() {
    let sets = [
        ("struct", structured()),
        ("random", Rng(0x9E3779B97F4A7C15).bytes(4096)),
    ];
    for (name, data) in &sets {
        println!("set {name} raw={} rfnv={:016x}", data.len(), fnv1a(data));
    }

    // ① bulk compress/decompress × four levels × two data sets
    for (name, data) in &sets {
        for level in [-3, 1, 9, 19] {
            let comp = bulk::compress(data, level).unwrap();
            let back = bulk::decompress(&comp, data.len()).unwrap();
            println!(
                "bulk {name} L{level} comp={} cfnv={:016x} rt={}",
                comp.len(),
                fnv1a(&comp),
                back == *data
            );
        }
    }

    // ①b to_buffer variants + compress_bound
    let d0 = &sets[0].1;
    let bound = zstd::zstd_safe::compress_bound(d0.len());
    let mut cbuf = vec![0u8; bound];
    let cn = bulk::compress_to_buffer(d0, &mut cbuf, 5).unwrap();
    let mut dbuf = vec![0u8; d0.len()];
    let dn = bulk::decompress_to_buffer(&cbuf[..cn], &mut dbuf).unwrap();
    println!(
        "to_buffer bound={bound} cn={cn} dn={dn} rt={}",
        dbuf == *d0
    );

    // ② stream Encoder/Decoder over the same matrix (chunked writes, chunked reads)
    for (name, data) in &sets {
        for level in [-3, 1, 9, 19] {
            let mut enc = Encoder::new(Vec::new(), level).unwrap();
            for chunk in data.chunks(333) {
                enc.write_all(chunk).unwrap();
            }
            let comp = enc.finish().unwrap();
            let mut dec = Decoder::new(&comp[..]).unwrap();
            let back = read_chunked(&mut dec).unwrap();
            println!(
                "stream {name} L{level} comp={} cfnv={:016x} rt={}",
                comp.len(),
                fnv1a(&comp),
                back == *data
            );
        }
    }

    // ③ convenience surface + multi-frame concatenation / single_frame
    let e1 = stream::encode_all(&d0[..], 3).unwrap();
    let b1 = stream::decode_all(&e1[..]).unwrap();
    println!(
        "encode_all len={} fnv={:016x} rt={}",
        e1.len(),
        fnv1a(&e1),
        b1 == *d0
    );
    let mut e2 = Vec::new();
    stream::copy_encode(&d0[..], &mut e2, 9).unwrap();
    let mut b2 = Vec::new();
    stream::copy_decode(&e2[..], &mut b2).unwrap();
    println!(
        "copy_encode len={} fnv={:016x} rt={}",
        e2.len(),
        fnv1a(&e2),
        b2 == *d0
    );
    let mut two = e1.clone();
    two.extend_from_slice(&e1);
    let all = read_chunked(&mut Decoder::new(&two[..]).unwrap()).unwrap();
    let mut expect2 = d0.clone();
    expect2.extend_from_slice(d0);
    println!("two-frames default n={} rt={}", all.len(), all == expect2);
    let first = read_chunked(&mut Decoder::new(&two[..]).unwrap().single_frame()).unwrap();
    println!("two-frames single n={} rt={}", first.len(), first == *d0);

    // ④ dictionaries: 6 similar small records; prepared dict + bulk ctx reuse; raw dict + stream
    let dict: Vec<u8> = b"user= action=login ip=10.0. status=ok session=abcdef".to_vec();
    let recs: Vec<Vec<u8>> = (0..6u32)
        .map(|i| {
            format!("user=u{i} action=login ip=10.0.0.{i} status=ok session=abcdef{i:03}")
                .into_bytes()
        })
        .collect();
    let edict = EncoderDictionary::copy(&dict, 3);
    let ddict = DecoderDictionary::copy(&dict);
    let mut cplain = bulk::Compressor::new(3).unwrap();
    let mut cdict = bulk::Compressor::with_prepared_dictionary(&edict).unwrap();
    let mut ddictor = bulk::Decompressor::with_prepared_dictionary(&ddict).unwrap();
    let mut first_dict_frame = Vec::new();
    for (i, rec) in recs.iter().enumerate() {
        let p = cplain.compress(rec).unwrap();
        let c = cdict.compress(rec).unwrap();
        if i == 0 {
            first_dict_frame = c.clone();
        }
        let back = ddictor.decompress(&c, rec.len()).unwrap();
        println!(
            "dict rec{i} raw={} plain={} dict={} rt={}",
            rec.len(),
            p.len(),
            c.len(),
            back == *rec
        );
    }
    match bulk::decompress(&first_dict_frame, 1024) {
        Ok(_) => println!("no-dict decode unexpectedly ok"),
        Err(e) => println!("no-dict decode err: {e}"),
    }
    let blob = recs.concat();
    let mut enc = Encoder::with_dictionary(Vec::new(), 3, &dict).unwrap();
    enc.write_all(&blob).unwrap();
    let cd = enc.finish().unwrap();
    let back = read_chunked(&mut Decoder::with_dictionary(&cd[..], &dict).unwrap()).unwrap();
    println!(
        "dict stream raw={} comp={} cfnv={:016x} rt={}",
        blob.len(),
        cd.len(),
        fnv1a(&cd),
        back == blob
    );

    // ⑤ error paths and boundaries
    let junk = Rng(0xDEADBEEFCAFEF00D).bytes(64);
    match bulk::decompress(&junk, 4096) {
        Ok(_) => println!("bulk junk unexpectedly ok"),
        Err(e) => println!("bulk junk err: {e}"),
    }
    let comp = bulk::compress(d0, 1).unwrap();
    let cut = &comp[..comp.len() * 3 / 5];
    match bulk::decompress(cut, d0.len()) {
        Ok(_) => println!("bulk truncated unexpectedly ok"),
        Err(e) => println!("bulk truncated err: {e}"),
    }
    match bulk::decompress(&comp, 16) {
        Ok(_) => println!("small-cap unexpectedly ok"),
        Err(e) => println!("small-cap err: {e}"),
    }
    match bulk::compress(b"lvl", 99) {
        Ok(v) => println!("level99 ok len={}", v.len()),
        Err(e) => println!("level99 err: {e}"),
    }
    match stream::decode_all(&junk[..]) {
        Ok(_) => println!("stream junk unexpectedly ok"),
        Err(e) => println!("stream junk err: {e}"),
    }
    let mut dec = Decoder::new(&cut[..]).unwrap();
    match read_chunked(&mut dec) {
        Ok(v) => println!("stream truncated ok n={}", v.len()),
        Err(e) => println!("stream truncated err kind={:?} msg={e}", e.kind()),
    }
    let e0 = bulk::compress(b"", 3).unwrap();
    let b0 = bulk::decompress(&e0, 0).unwrap();
    println!("empty bulk clen={} dlen={} rt={}", e0.len(), b0.len(), b0.is_empty());
    match stream::decode_all(&[][..]) {
        Ok(v) => println!("empty stream ok n={}", v.len()),
        Err(e) => println!("empty stream err kind={:?} msg={e}", e.kind()),
    }
    println!(
        "levels {}..={} default={}",
        zstd::zstd_safe::min_c_level(),
        zstd::zstd_safe::max_c_level(),
        zstd::DEFAULT_COMPRESSION_LEVEL
    );
}
