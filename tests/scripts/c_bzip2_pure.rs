#!/usr/bin/env mirvm
---
[dependencies]
bzip2 = "0.6"
---
// bzip2 0.6.1, default pure-Rust backend (libbz2-rs-sys 0.2, a faithful C-to-Rust translation;
// no build.rs, no libc under the rust allocator), exercised as a three-way differential. The
// compression core (block sort, BWT, MTF, Huffman, CRC) is interpreted/JIT'd Rust only.
//
// Version pinning: bzip2 0.6 with its default pure-Rust backend, rather than the C-backed
// bzip2 0.4 route (bzip2-sys 0.1.13, which compiles vendored libbz2-1.0.8 into libbz2.a),
// because the C route hits a mirvm engine limit. The engine refuses to load a static archive
// whose undefined symbols are resolved from a separate Rust rlib instead of from inside the
// archive itself, and reports that cross-archive reference as FRONTIER. In bzip2-sys,
// `bz_internal_error` -- referenced by the vendored C under -DBZ_NO_STDIO -- is defined in
// the Rust rlib instead of in the archive, so every mirvm run aborts while lowering (exit
// 101). Native linking is unaffected because the final link carries the rlib symbol, so only
// mirvm fails. bzip2 0.6 keeps the whole backend in Rust, so no native archive is involved
// and the limit does not apply. Its public surface matches the C-backed release (Compression,
// read/write/bufread/mem, MultiBzDecoder, identical error text), so the same driver body
// works either way.
//
// The differential runs every case natively and under mirvm and compares stdout
// byte-for-byte: lengths, FNV-1a hashes, booleans, Status and ErrorKind Debug, and
// mem::Error Display text must all match. Although the 0.6 backend is byte-compatible with C
// libbz2 output, both differential sides run the same Rust implementation; what the
// comparison checks is that interpreted/JIT'd semantics agree with native semantics.
//
// Coverage:
// ① read::{BzEncoder,BzDecoder} × level 1/5/9 × {structured, seeded random, all-zero,
//    empty} payloads (512B chunks reading the compressed stream / 777B chunks reading back).
// ② write::{BzEncoder,BzDecoder} over the same level matrix × {struct, random}: 333B
//    chunked writes, flush() after the first chunk (the BZ_FLUSH path), try_finish +
//    finish, total_in/out; 777B chunked writes decoding a compressed stream.
// ③ bufread::{BzEncoder,BzDecoder} roundtrip through a small-capacity BufReader + totals.
// ④ mem low level: hand-fed Compress/Decompress Run/Finish loops (compress_vec /
//    decompress_vec), small=true low-memory decode, 5000B chunked feeding, the wf=3
//    degraded-sort fallback and wf=250 work_factor, and >100KB payloads crossing bzip2
//    block boundaries (level 1 block=100k, 220KB -> 3 blocks; level 5 block=500k -> 1 block).
// ⑤ multi-stream concatenation: BzDecoder takes only the first stream vs
//    read::MultiBzDecoder taking all; garbage appended after the last stream exercises the
//    multi error path.
// ⑥ Compression::{new,fast,best,default} level values + try_new rejecting out-of-range.
// ⑦ error paths: junk stream (missing magic), valid magic + corrupt block body (read side
//    in one pass / mem side hand-fed in continuation), truncated stream (UnexpectedEof),
//    junk on the write side, mem-side DataMagic/Data/truncated-empty-feed Result shapes,
//    decoding purely empty input, and out-of-range level=0 panicking in Compression::new
//    (explicit validation; silent hook + catch_unwind).
//
// Determinism: prints only lengths/FNV-1a/booleans/Status and ErrorKind Debug, plus
// mem::Error constant Display text; randomness comes from a seeded xorshift64*; no
// time/address/HashMap order; stderr stays empty (panics install a silent hook first).
use std::io::{BufReader, Read, Write};

use bzip2::{bufread, read, write, Action, Compress, Compression, Decompress, Status};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Seeded xorshift64*; native and mirvm see the same sequence.
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

/// Structured repetitive log; fixed-width sequence numbers key determinism. `target` = min bytes.
fn structured(target: usize) -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < target {
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

/// Read in chunks until EOF.
fn read_chunked<R: Read>(r: &mut R, n: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; n];
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        out.extend_from_slice(&buf[..k]);
    }
    Ok(out)
}

/// One-shot mem-level compression: feed chunks with Run, then Finish until StreamEnd.
fn mem_oneshot(data: &[u8], level: u32, wf: u32) -> Vec<u8> {
    let mut c = Compress::new(Compression::new(level), wf);
    // bzip2's worst-case expansion is far below this reserve; under-reserving panics, so reserve.
    let mut out = Vec::new();
    out.reserve(data.len() + data.len() / 8 + 65536);
    for ch in data.chunks(1000) {
        let st = c.compress_vec(ch, &mut out, Action::Run).unwrap();
        debug_assert_eq!(st, Status::RunOk);
    }
    loop {
        match c.compress_vec(&[], &mut out, Action::Finish).unwrap() {
            Status::StreamEnd => break,
            _ => {}
        }
    }
    out
}

/// Mem-level decode: `chunk=None` feeds all at once, else in blocks; returns (output, last status).
fn mem_decode(comp: &[u8], small: bool, chunk: Option<usize>, expect: usize) -> (Vec<u8>, Status) {
    let mut d = Decompress::new(small);
    let mut out = Vec::with_capacity(expect + 16);
    let mut last = Status::Ok;
    match chunk {
        Some(n) => {
            for ch in comp.chunks(n) {
                last = d.decompress_vec(ch, &mut out).unwrap();
            }
        }
        None => {
            last = d.decompress_vec(comp, &mut out).unwrap();
        }
    }
    (out, last)
}

fn main() {
    let sets: Vec<(String, Vec<u8>)> = vec![
        ("struct".to_string(), structured(7168)),
        ("random".to_string(), Rng(0x9E3779B97F4A7C15).bytes(4096)),
        ("zeros".to_string(), vec![0u8; 4096]),
        ("empty".to_string(), Vec::new()),
    ];
    for (name, data) in &sets {
        println!("set {name} raw={} rfnv={:016x}", data.len(), fnv1a(data));
    }

    // ① read::{BzEncoder,BzDecoder} × level 1/5/9 × four payloads
    for (name, data) in &sets {
        for level in [1u32, 5, 9] {
            let mut enc = read::BzEncoder::new(&data[..], Compression::new(level));
            let comp = read_chunked(&mut enc, 512).unwrap();
            let mut dec = read::BzDecoder::new(&comp[..]);
            let back = read_chunked(&mut dec, 777).unwrap();
            println!(
                "read {name} L{level} clen={} cfnv={:016x} rt={}",
                comp.len(),
                fnv1a(&comp),
                back == *data
            );
        }
    }

    // ② write::{BzEncoder,BzDecoder} × level 1/5/9 × {struct, random} (mid-stream flush)
    for (name, data) in &sets[..2] {
        for level in [1u32, 5, 9] {
            let mut enc = write::BzEncoder::new(Vec::new(), Compression::new(level));
            for (i, ch) in data.chunks(333).enumerate() {
                enc.write_all(ch).unwrap();
                if i == 0 {
                    enc.flush().unwrap();
                }
            }
            let tin = enc.total_in();
            let tout_pre = enc.total_out();
            enc.try_finish().unwrap();
            let comp = enc.finish().unwrap();
            let mut dec = write::BzDecoder::new(Vec::new());
            for ch in comp.chunks(777) {
                dec.write_all(ch).unwrap();
            }
            dec.try_finish().unwrap();
            let din = dec.total_in();
            let dout = dec.total_out();
            let back = dec.finish().unwrap();
            println!(
                "write {name} L{level} tin={tin} tout_pre={tout_pre} clen={} cfnv={:016x} din={din} dout={dout} rt={}",
                comp.len(),
                fnv1a(&comp),
                back == *data
            );
        }
    }

    // ③ bufread path (1KB small buffer)
    let d0 = &sets[0].1;
    let comp_b = {
        let br = BufReader::with_capacity(1024, &d0[..]);
        let mut enc = bufread::BzEncoder::new(br, Compression::new(5));
        let c = read_chunked(&mut enc, 999).unwrap();
        println!(
            "buf-enc tin={} tout={} clen={}",
            enc.total_in(),
            enc.total_out(),
            c.len()
        );
        c
    };
    {
        let br = BufReader::with_capacity(1024, &comp_b[..]);
        let mut dec = bufread::BzDecoder::new(br);
        let back = read_chunked(&mut dec, 999).unwrap();
        println!(
            "buf-dec din={} dout={} cfnv={:016x} rt={}",
            dec.total_in(),
            dec.total_out(),
            fnv1a(&comp_b),
            back == *d0
        );
    }

    // ④ mem low level + work_factor + block-boundary crossing
    let comp_m = mem_oneshot(d0, 5, 30);
    let (back_f, st_f) = mem_decode(&comp_m, false, None, d0.len());
    println!(
        "mem L5 clen={} cfnv={:016x} full-st={st_f:?} full-rt={}",
        comp_m.len(),
        fnv1a(&comp_m),
        back_f == *d0
    );
    let (back_c, st_c) = mem_decode(&comp_m, true, Some(5000), d0.len());
    println!("mem small chunked-st={st_c:?} rt={}", back_c == *d0);
    let zeros = &sets[2].1;
    let comp_z = mem_oneshot(zeros, 1, 3);
    let (back_z, _) = mem_decode(&comp_z, false, None, zeros.len());
    println!(
        "mem zeros L1 wf3 clen={} cfnv={:016x} rt={}",
        comp_z.len(),
        fnv1a(&comp_z),
        back_z == *zeros
    );
    let rnd = &sets[1].1;
    let comp_r = mem_oneshot(rnd, 9, 250);
    let (back_r, _) = mem_decode(&comp_r, false, None, rnd.len());
    println!(
        "mem random L9 wf250 clen={} cfnv={:016x} rt={}",
        comp_r.len(),
        fnv1a(&comp_r),
        back_r == *rnd
    );
    // block crossing: 220KB structured (L1 block=100k -> 3 blocks; L5 block=500k -> 1 block)
    let big = structured(220_000);
    for level in [1u32, 5] {
        let comp_big = mem_oneshot(&big, level, 30);
        let (back_big, _) = mem_decode(&comp_big, false, Some(65536), big.len());
        println!(
            "mem big L{level} raw={} clen={} cfnv={:016x} rt={}",
            big.len(),
            comp_big.len(),
            fnv1a(&comp_big),
            back_big == big
        );
    }

    // ⑤ multi-stream: the single-stream decoder keeps only the first; MultiBzDecoder keeps all
    let c1 = mem_oneshot(d0, 5, 30);
    let c2 = mem_oneshot(&sets[1].1, 5, 30);
    let mut cat = c1.clone();
    cat.extend_from_slice(&c2);
    let first = read_chunked(&mut read::BzDecoder::new(&cat[..]), 777).unwrap();
    println!("multi single n={} first={}", first.len(), first == *d0);
    let mut expect = d0.clone();
    expect.extend_from_slice(&sets[1].1);
    let all = read_chunked(&mut read::MultiBzDecoder::new(&cat[..]), 777).unwrap();
    println!("multi all n={} both={}", all.len(), all == expect);
    let mut tail = c1.clone();
    tail.extend_from_slice(&Rng(0xDEADBEEFCAFEF00D).bytes(64));
    match read_chunked(&mut read::MultiBzDecoder::new(&tail[..]), 777) {
        Ok(v) => println!("multi junk-tail ok n={}", v.len()),
        Err(e) => println!("multi junk-tail err kind={:?} msg={e}", e.kind()),
    }

    // ⑥ Compression constructors: fast/best/default/new + try_new rejecting out-of-range
    println!(
        "levels fast={} best={} default={} new5={}",
        Compression::fast().level(),
        Compression::best().level(),
        Compression::default().level(),
        Compression::new(5).level()
    );
    println!(
        "try_new 0={:?} 10={:?} 7={:?}",
        Compression::try_new(0),
        Compression::try_new(10),
        Compression::try_new(7)
    );

    // ⑦ error paths
    let junk = Rng(0x0123456789ABCDEF).bytes(64);
    match read_chunked(&mut read::BzDecoder::new(&junk[..]), 777) {
        Ok(v) => println!("read junk ok n={}", v.len()),
        Err(e) => println!("read junk err kind={:?} msg={e}", e.kind()),
    }
    let mut corrupt = c1.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xFF;
    match read_chunked(&mut read::BzDecoder::new(&corrupt[..]), 777) {
        Ok(v) => println!("read corrupt ok n={}", v.len()),
        Err(e) => println!("read corrupt err kind={:?} msg={e}", e.kind()),
    }
    let cut = &c1[..c1.len() * 3 / 5];
    match read_chunked(&mut read::BzDecoder::new(cut), 777) {
        Ok(v) => println!("read truncated ok n={}", v.len()),
        Err(e) => println!("read truncated err kind={:?} msg={e}", e.kind()),
    }
    let mut wd = write::BzDecoder::new(Vec::new());
    match wd.write_all(&junk) {
        Ok(()) => println!("write junk ok out={}", wd.total_out()),
        Err(e) => println!("write junk err kind={:?} msg={e}", e.kind()),
    }
    let mut dm = Decompress::new(false);
    let mut scratch = [0u8; 256];
    match dm.decompress(&junk, &mut scratch) {
        Ok(st) => println!("mem junk st={st:?}"),
        Err(e) => println!("mem junk err: {e}"),
    }
    // mem corrupt block: fresh decoder hand-fed the unconsumed tail each round until error/drain.
    {
        let mut dc = Decompress::new(false);
        let mut outb = vec![0u8; d0.len() + 16];
        let mut off = 0usize;
        loop {
            let before = dc.total_in();
            let res = dc.decompress(&corrupt[off..], &mut outb);
            off += (dc.total_in() - before) as usize;
            match res {
                Ok(Status::StreamEnd) => {
                    println!("mem corrupt st=StreamEnd out={}", dc.total_out());
                    break;
                }
                Ok(_st) if off < corrupt.len() => {}
                Ok(st) => {
                    println!("mem corrupt st={st:?} out={}", dc.total_out());
                    break;
                }
                Err(e) => {
                    println!("mem corrupt err: {e}");
                    break;
                }
            }
        }
    }
    // mem truncation: a full feed leaves status != StreamEnd; an empty feed shows the return shape.
    let mut dt = Decompress::new(false);
    let mut big_out = vec![0u8; d0.len() + 16];
    let st_t = dt.decompress(cut, &mut big_out).unwrap();
    let eof = dt.decompress(&[], &mut []);
    println!(
        "mem truncated st={st_t:?} out={} eof={eof:?}",
        dt.total_out()
    );
    match read_chunked(&mut read::BzDecoder::new(&[][..]), 777) {
        Ok(v) => println!("read empty ok n={}", v.len()),
        Err(e) => println!("read empty err kind={:?} msg={e}", e.kind()),
    }
    // invalid level=0: Compression::new panics outside 1..=9 (a #[track_caller] const fn;
    // a silent hook keeps stderr empty and catch_unwind asserts the panic happens).
    std::panic::set_hook(Box::new(|_| {}));
    let r = std::panic::catch_unwind(|| {
        let _ = Compression::new(0);
    });
    println!("invalid-level panic={}", r.is_err());
}
