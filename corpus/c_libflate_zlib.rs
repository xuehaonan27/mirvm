#!/usr/bin/env mirvm
---
[dependencies]
libflate = "2"
---
// libflate 2.3 differential (pure-Rust DEFLATE plus zlib/gzip containers -- the same
// decompressor jieba's dictionary uses): roundtrips for all three containers, streaming
// in chunks, header field introspection and error paths. Covers zlib::{Encoder, Decoder,
// EncodeOptions, Header, Lz77WindowSize, FlushMode} (default / no_compression /
// fixed_huffman_codes / block_size / flush_mode(Sync)), gzip::{Encoder, Decoder,
// MultiDecoder, EncodeOptions, HeaderBuilder, Header, Os, ExtraField, ExtraSubField}
// (mtime/filename/comment/extra/verify pinned, read back field by field on decode),
// and deflate::{Encoder, Decoder}. Three input shapes: structured repetitive text /
// seeded xorshift random / empty. Streaming: 7-byte writes, 13-byte reads. MultiDecoder
// over concatenated members vs a single-member Decoder. Error paths: zlib bad FLG check
// bits / truncation / tampered adler; gzip bad magic / truncation / tampered crc;
// deflate reserved block type.
//
// Deterministic: HeaderBuilder defaults mtime to the wall clock (UNIX_EPOCH.elapsed()),
// so every gzip encode uses EncodeOptions::new().header(a header with a pinned mtime).
// Only lengths, FNV-1a, booleans and fixed header fields are printed -- no time,
// addresses, thread ids or HashMap order.
//
// Upstream behavior (native reproduces it; semantics unchanged, cases just split): a
// gzip header with both F_TEXT and F_HCRC set cannot be decoded -- Header::read_from
// does not copy the FLG F_TEXT bit into is_text, so the flags this.crc16() recomputes
// during the FHCRC check lack it and the crc16 never matches ("CRC16 of GZIP header
// mismatched", InvalidData). verify() and text() therefore use two separate headers:
// ④⑤ take the full-field header without text, ⑤b checks text() encoding and is_text
// readback (also not copied back upstream, so it decodes as false -- both sides agree).
// NOTE: mirvm has no dyn Error+Send+Sync -> dyn Debug vtable upcast, so Debug-formatting
// an io::Error TRAPs; these error paths only match and print kind and the Display msg.
use std::ffi::CString;
use std::io::{self, Read, Write};

use libflate::gzip::{
    EncodeOptions as GzipEncodeOptions, ExtraField, ExtraSubField, HeaderBuilder, MultiDecoder,
    Os,
};
use libflate::zlib::{EncodeOptions as ZlibEncodeOptions, FlushMode, Lz77WindowSize};
use libflate::{deflate, gzip, zlib};

/// Pinned gzip mtime (HeaderBuilder defaults to UNIX_EPOCH.elapsed(), so it must be overridden).
const MTIME: u32 = 0x0DDC_0FFE;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Seeded xorshift64* PRNG with the same sequence on native and mirvm.
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

/// Structured repetitive log (highly compressible; fixed-width record numbers keep it deterministic).
fn make_log() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < 9216 {
        let line = format!(
            "line {i:05} pid={:03} level={} msg={}\n",
            (i * 7) % 251,
            ["INFO", "WARN", "ERROR"][(i % 3) as usize],
            "payload".repeat((i % 5 + 1) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

fn zlib_compress(data: &[u8]) -> Vec<u8> {
    let mut e = zlib::Encoder::new(Vec::new()).unwrap();
    e.write_all(data).unwrap();
    e.finish().into_result().unwrap()
}

fn zlib_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut d = zlib::Decoder::new(data)?;
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

fn deflate_compress(data: &[u8]) -> Vec<u8> {
    let mut e = deflate::Encoder::new(Vec::new());
    e.write_all(data).unwrap();
    e.finish().into_result().unwrap()
}

fn deflate_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut d = deflate::Decoder::new(data);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

/// Gzip header with every field pinned (mtime/os/verify/filename/comment/extra;
/// no text -- see the file header note).
fn pinned_gzip_header() -> gzip::Header {
    HeaderBuilder::new()
        .modification_time(MTIME)
        .os(Os::Unix)
        .verify()
        .filename(CString::new("mirvm-corpus.bin").unwrap())
        .comment(CString::new("libflate corpus pinned header").unwrap())
        .extra_field(ExtraField {
            subfields: vec![ExtraSubField {
                id: *b"MV",
                data: vec![0xde, 0xad, 0xbe, 0xef],
            }],
        })
        .finish()
}

fn gzip_compress(data: &[u8]) -> Vec<u8> {
    let opts = GzipEncodeOptions::new().header(pinned_gzip_header());
    let mut e = gzip::Encoder::with_options(Vec::new(), opts).unwrap();
    e.write_all(data).unwrap();
    e.finish().into_result().unwrap()
}

fn gzip_decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut d = gzip::Decoder::new(data)?;
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

/// Read chunk-sized byte blocks until EOF (the streaming read path).
fn chunked_read_all<R: Read>(mut r: R, chunk: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Uniform error-path report: prints only kind and the Display msg (never {:?} on
/// io::Error itself; see the file header note).
fn report_err(label: &str, r: io::Result<Vec<u8>>) {
    match r {
        Ok(v) => println!("{label} unexpectedly ok out-len={}", v.len()),
        Err(e) => println!("{label} kind={:?} msg={}", e.kind(), e),
    }
}

fn main() {
    let log = make_log();
    let rand = Rng(0x9E3779B97F4A7C15).bytes(3000);
    let empty: Vec<u8> = Vec::new();
    let inputs: [(&str, &[u8]); 3] = [("log", &log), ("rand", &rand), ("empty", &empty)];
    for (label, data) in inputs {
        println!("input {label} len={} fnv={:016x}", data.len(), fnv1a(data));
    }

    // ① zlib container × three inputs: compressed-stream checksum + roundtrip boolean
    for (label, data) in inputs {
        let c = zlib_compress(data);
        let back = zlib_decompress(&c).unwrap();
        println!(
            "zlib/{label} clen={} cfnv={:016x} roundtrip={}",
            c.len(),
            fnv1a(&c),
            back == data
        );
    }

    // ② zlib header introspection: window_size / compression_level / Lz77WindowSize ladder
    let c = zlib_compress(&log);
    let d = zlib::Decoder::new(&c[..]).unwrap();
    println!(
        "zlib hdr win={:?}({}B) level={:?}",
        d.header().window_size(),
        d.header().window_size().to_u16(),
        d.header().compression_level()
    );
    for size in [255u16, 256, 15000, 16384, 16385, 40000] {
        println!("win from_u16({size}) = {:?}", Lz77WindowSize::from_u16(size));
    }

    // ③ raw deflate container × three inputs
    for (label, data) in inputs {
        let c = deflate_compress(data);
        let back = deflate_decompress(&c).unwrap();
        println!(
            "deflate/{label} clen={} cfnv={:016x} roundtrip={}",
            c.len(),
            fnv1a(&c),
            back == data
        );
    }

    // ④ gzip container × three inputs (full-field pinned header, no text -- see the note above)
    for (label, data) in inputs {
        let c = gzip_compress(data);
        let back = gzip_decompress(&c).unwrap();
        println!(
            "gzip/{label} clen={} cfnv={:016x} roundtrip={}",
            c.len(),
            fnv1a(&c),
            back == data
        );
    }

    // ⑤ Decode-side field-by-field header readback (including extra subfields and a passing FHCRC)
    let c = gzip_compress(&log);
    let d = gzip::Decoder::new(&c[..]).unwrap();
    let h = d.header();
    println!(
        "gzip hdr mtime={:#010x} os={:?} text={} verified={} fname={:?} comment={:?}",
        h.modification_time(),
        h.os(),
        h.is_text(),
        h.is_verified(),
        h.filename().map(|s| s.to_string_lossy().into_owned()),
        h.comment().map(|s| s.to_string_lossy().into_owned())
    );
    let ef = h.extra_field().unwrap();
    println!(
        "gzip hdr extra nsub={} id={:?} data={:?}",
        ef.subfields.len(),
        ef.subfields[0].id,
        ef.subfields[0].data
    );

    // ⑤b F_TEXT on its own (mutually exclusive with verify; see the note above): the encoder
    // sets text=true, the upstream decoder does not copy is_text back -> false (both sides agree).
    let text_header = HeaderBuilder::new()
        .modification_time(MTIME)
        .os(Os::Ntfs)
        .text()
        .finish();
    let opts = GzipEncodeOptions::new().header(text_header);
    let mut e = gzip::Encoder::with_options(Vec::new(), opts).unwrap();
    e.write_all(&log).unwrap();
    let c = e.finish().into_result().unwrap();
    let back = gzip_decompress(&c).unwrap();
    let d = gzip::Decoder::new(&c[..]).unwrap();
    println!(
        "gzip-text clen={} cfnv={:016x} roundtrip={} enc-text=true dec-text={} dec-os={:?}",
        c.len(),
        fnv1a(&c),
        back == log,
        d.header().is_text(),
        d.header().os()
    );

    // ⑥ zlib EncodeOptions variants × log roundtrip (flush after each 1 KiB block write)
    let variants: [(&str, ZlibEncodeOptions<libflate::lz77::DefaultLz77Encoder>); 4] = [
        ("no-comp", ZlibEncodeOptions::new().no_compression()),
        ("fixed-huffman", ZlibEncodeOptions::new().fixed_huffman_codes()),
        ("block-97", ZlibEncodeOptions::new().block_size(97)),
        ("flush-sync", ZlibEncodeOptions::new().flush_mode(FlushMode::Sync)),
    ];
    for (label, opts) in variants {
        let mut e = zlib::Encoder::with_options(Vec::new(), opts).unwrap();
        for chunk in log.chunks(1024) {
            e.write_all(chunk).unwrap();
            e.flush().unwrap();
        }
        let c = e.finish().into_result().unwrap();
        let back = zlib_decompress(&c).unwrap();
        println!(
            "zlib-opt {label} clen={} cfnv={:016x} roundtrip={}",
            c.len(),
            fnv1a(&c),
            back == log
        );
    }

    // ⑦ Streaming: 7-byte writes + 13-byte reads across deflate block boundaries, for zlib and gzip
    let mut e = zlib::Encoder::new(Vec::new()).unwrap();
    for chunk in log.chunks(7) {
        e.write_all(chunk).unwrap();
    }
    let c = e.finish().into_result().unwrap();
    let back = chunked_read_all(zlib::Decoder::new(&c[..]).unwrap(), 13).unwrap();
    println!(
        "stream zlib w7/r13 clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == log
    );
    let opts = GzipEncodeOptions::new().header(pinned_gzip_header());
    let mut e = gzip::Encoder::with_options(Vec::new(), opts).unwrap();
    for chunk in rand.chunks(7) {
        e.write_all(chunk).unwrap();
    }
    let c = e.finish().into_result().unwrap();
    let back = chunked_read_all(gzip::Decoder::new(&c[..]).unwrap(), 13).unwrap();
    println!(
        "stream gzip w7/r13 clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == rand
    );

    // ⑧ MultiDecoder: both concatenated members in one read; a single-member Decoder reads only the first
    let ma = gzip_compress(b"Hello, ");
    let mb = gzip_compress(b"multi-member world!");
    let mut cat = ma.clone();
    cat.extend_from_slice(&mb);
    let mut d = MultiDecoder::new(&cat[..]).unwrap();
    let mut all = Vec::new();
    d.read_to_end(&mut all).unwrap();
    println!(
        "gzip multi members=2 out={:?} roundtrip={}",
        String::from_utf8_lossy(&all),
        all == b"Hello, multi-member world!"
    );
    let mut d1 = gzip::Decoder::new(&cat[..]).unwrap();
    let mut first = Vec::new();
    d1.read_to_end(&mut first).unwrap();
    println!("gzip single-decoder out={:?}", String::from_utf8_lossy(&first));

    // ⑨ Error path a: zlib bad FLG check bits (CMF*256+FLG not a multiple of 31)
    match zlib::Decoder::new(&b"jk"[..]) {
        Ok(_) => println!("zlib-bad-hdr unexpectedly ok"),
        Err(e) => println!("zlib-bad-hdr kind={:?} msg={}", e.kind(), e),
    }
    // Error path b: zlib truncation (trailer read_exact -> UnexpectedEof)
    let c = zlib_compress(&log);
    report_err("zlib-truncated", zlib_decompress(&c[..c.len() * 3 / 5]));
    // Error path c: tampered zlib adler32 trailer -> EOF checksum error
    let mut bad = c.clone();
    let n = bad.len();
    bad[n - 1] ^= 0xFF;
    report_err("zlib-bad-adler", zlib_decompress(&bad));

    // Error path d: gzip bad magic
    match gzip::Decoder::new(&b"not a gzip stream"[..]) {
        Ok(_) => println!("gzip-bad-magic unexpectedly ok"),
        Err(e) => println!("gzip-bad-magic kind={:?} msg={}", e.kind(), e),
    }
    // Error path e: gzip truncation
    let g = gzip_compress(&log);
    report_err("gzip-truncated", gzip_decompress(&g[..g.len() * 3 / 5]));
    // Error path f: tampered gzip crc32 trailer -> EOF CRC check error
    let mut badg = g.clone();
    let n = badg.len();
    badg[n - 6] ^= 0xFF; // trailer = crc32(4B LE) + isize(4B LE); this lands inside the crc field
    report_err("gzip-bad-crc", gzip_decompress(&badg));

    // Error path g: deflate reserved block type (BTYPE=0b11) -> inflate errors immediately
    report_err("deflate-bad-block", deflate_decompress(&[0xff; 4]));
}
