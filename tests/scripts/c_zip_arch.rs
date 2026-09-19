#!/usr/bin/env mirvm
---
[dependencies]
zip = { version = "2", default-features = false, features = ["deflate"] }
---
// zip 2.4 (Stored main body + flate2 backend deflate): build an in-memory Cursor
// archive, fnv-anchor the whole-archive bytes, read it back through ZipArchive and
// verify each entry (name/dir bit/CRC32/size/mtime/mode/content checksum). Covers
// directories, empty files, UTF-8 text (CJK+emoji), seeded random binary, structured
// large text, unicode file names, unix permissions, fixed mtime, archive comment, all
// reader accessors, the three error paths, and deflate level 6/9/default roundtrips.
//
// Known workaround (semantics unchanged): a crc32fast update of >= 128B takes the
// pclmulqdq hardware path, which is not built in, so reaching it TRAPs the process.
// All reads/writes use 64B chunks (< the 128B threshold, portable table path); the
// CRC32 and zip bytes match the whole-block form exactly, so native/mirvm byte-for-
// byte differential comparison is unaffected. deflate uses the flate2 raw deflate
// backend (miniz_oxide computes adler32 only for zlib; raw never reaches psad.bw).
use std::io::{self, Cursor, Read, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

/// crc32fast's hardware path threshold is 128B per update; 64B chunks stay on the portable table path.
const CHUNK: usize = 64;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Software CRC32 (IEEE, table-driven) -- independent cross-check of the CRC32 field in an entry header.
fn crc32_sw(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, e) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
        }
        *e = c;
    }
    let mut crc: u32 = 0xffff_ffff;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
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

/// Write in 64B chunks (see the header note).
fn chunked_write<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    for c in data.chunks(CHUNK) {
        w.write_all(c)?;
    }
    Ok(())
}

/// Read in 64B chunks to EOF (the CRC check in Crc32Reader only fires once the reader
/// is drained), avoiding read_to_end's single large update (>=128B would switch to the hardware path).
fn chunked_read_all<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; CHUNK];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn stored_opts() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(DateTime::from_date_and_time(2024, 3, 14, 15, 9, 26).unwrap())
}

/// Structured repetitive log (compresses well under deflate; fixed-width record numbers keep it deterministic).
fn make_big_log() -> Vec<u8> {
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

/// Read every entry back and print all decoded fields, the software-CRC cross-check, and the content comparison.
fn dump_archive<R: Read + io::Seek>(ar: &mut ZipArchive<R>, expected: &[(String, Vec<u8>)]) {
    println!("entries = {} is_empty = {}", ar.len(), ar.is_empty());
    let names: Vec<&str> = ar.file_names().collect();
    println!("names = {names:?}");
    println!("name_for_index(1) = {:?}", ar.name_for_index(1));
    for i in 0..ar.len() {
        let mut f = ar.by_index(i).unwrap();
        let content = chunked_read_all(&mut f).unwrap();
        let (en, eb) = &expected[i];
        let mtime = f
            .last_modified()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".to_string());
        let mode = f
            .unix_mode()
            .map(|m| format!("{m:06o}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "[{i}] {} dir={} size={} csize={} crc={:08x} mtime={} mode={} fnv={:016x}",
            f.name(),
            f.is_dir(),
            f.size(),
            f.compressed_size(),
            f.crc32(),
            mtime,
            mode,
            fnv1a(&content)
        );
        println!(
            "[{i}] crc-xcheck={} content-ok={}",
            crc32_sw(&content) == f.crc32(),
            f.name() == en && content == *eb
        );
    }
}

fn main() {
    // ---- ① Stored main body: build the archive ----
    let readme = "# mirvm zip_arch\n\nUTF-8 文本：汉字、假名（かな）、emoji \u{1f4e6}\n\
                  第二行：tab\t分隔 与 \"引号\" 反斜杠\\\n重复行 padding padding padding\n"
        .as_bytes()
        .to_vec();
    let blob = Rng(0x9E3779B97F4A7C15).bytes(3000);
    let big_log = make_big_log();
    let uni_body = "文件名即内容：数据 文件.txt\n".as_bytes().to_vec();
    let expected: Vec<(String, Vec<u8>)> = vec![
        ("docs/".to_string(), Vec::new()),
        ("docs/readme.txt".to_string(), readme.clone()),
        ("bin/blob.bin".to_string(), blob.clone()),
        ("logs/big.log".to_string(), big_log.clone()),
        ("数据 文件.txt".to_string(), uni_body.clone()),
        ("empty.txt".to_string(), Vec::new()),
    ];

    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    w.add_directory("docs/", stored_opts()).unwrap();
    w.start_file("docs/readme.txt", stored_opts().unix_permissions(0o644))
        .unwrap();
    chunked_write(&mut w, &readme).unwrap();
    w.start_file("bin/blob.bin", stored_opts().unix_permissions(0o755))
        .unwrap();
    chunked_write(&mut w, &blob).unwrap();
    w.start_file("logs/big.log", stored_opts().unix_permissions(0o644))
        .unwrap();
    chunked_write(&mut w, &big_log).unwrap();
    w.start_file("数据 文件.txt", stored_opts().unix_permissions(0o600))
        .unwrap();
    chunked_write(&mut w, &uni_body).unwrap();
    w.start_file("empty.txt", stored_opts()).unwrap();
    w.set_comment("mirvm-corpus zip_arch v1");
    let bytes = w.finish().unwrap().into_inner();
    println!("archive len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));

    // ---- ② Read back: per-entry verification + by_name hit/miss ----
    let mut ar = ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
    println!("comment = {}", std::str::from_utf8(ar.comment()).unwrap());
    dump_archive(&mut ar, &expected);
    {
        let mut f = ar.by_name("logs/big.log").unwrap();
        let c = chunked_read_all(&mut f).unwrap();
        println!("by_name logs/big.log size={} fnv={:016x}", f.size(), fnv1a(&c));
    }
    match ar.by_name("nope.txt") {
        Ok(_) => println!("by_name nope.txt unexpectedly ok"),
        Err(e) => println!("by_name nope.txt err = {e}"),
    }

    // ---- ③ Error paths: junk archive / truncation / corrupt data byte (CRC error at EOF) ----
    match ZipArchive::new(Cursor::new(b"definitely not a zip archive".to_vec())) {
        Ok(_) => println!("junk archive unexpectedly ok"),
        Err(e) => println!("junk archive err = {e}"),
    }
    let cut = bytes[..bytes.len() * 3 / 5].to_vec();
    match ZipArchive::new(Cursor::new(cut)) {
        Ok(_) => println!("truncated archive unexpectedly ok"),
        Err(e) => println!("truncated archive err = {e}"),
    }
    let data_off = {
        let mut probe = ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
        probe.by_name("bin/blob.bin").unwrap().data_start() as usize
    };
    let mut corrupted = bytes.clone();
    corrupted[data_off + 7] ^= 0xFF;
    let mut ac = ZipArchive::new(Cursor::new(corrupted)).unwrap();
    let mut f = ac.by_name("bin/blob.bin").unwrap();
    match chunked_read_all(&mut f) {
        Ok(_) => println!("corrupt read unexpectedly ok"),
        Err(e) => println!("corrupt read kind={:?} msg={}", e.kind(), e),
    }

    // ---- ④ deflate (flate2 raw deflate backend) × level 6/9/default ----
    let defl_opts = |level: Option<i64>| {
        SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(level)
            .last_modified_time(DateTime::from_date_and_time(2023, 11, 5, 0, 0, 0).unwrap())
    };
    let rand2 = Rng(0xDEADBEEFCAFEF00D).bytes(2500);
    let tiny = b"hi".to_vec();
    let dexpected: Vec<(String, Vec<u8>)> = vec![
        ("defl/structured.log".to_string(), big_log.clone()),
        ("defl/random.bin".to_string(), rand2.clone()),
        ("defl/tiny.txt".to_string(), tiny.clone()),
    ];
    let mut dw = ZipWriter::new(Cursor::new(Vec::new()));
    dw.start_file("defl/structured.log", defl_opts(Some(6)))
        .unwrap();
    chunked_write(&mut dw, &big_log).unwrap();
    dw.start_file("defl/random.bin", defl_opts(Some(9))).unwrap();
    chunked_write(&mut dw, &rand2).unwrap();
    dw.start_file("defl/tiny.txt", defl_opts(None)).unwrap();
    chunked_write(&mut dw, &tiny).unwrap();
    // finish_into_readable: finish + ZipArchive in one step (another API surface).
    let mut dar = dw.finish_into_readable().unwrap();
    println!("deflate archive comment = {:?}", std::str::from_utf8(dar.comment()).unwrap());
    dump_archive(&mut dar, &dexpected);
    // by_index_raw: read back the raw compressed stream without decompressing (raw bytes must match too).
    for i in 0..dar.len() {
        let mut f = dar.by_index_raw(i).unwrap();
        let raw = chunked_read_all(&mut f).unwrap();
        println!("raw[{i}] {} len={} fnv={:016x}", f.name(), raw.len(), fnv1a(&raw));
    }
}
