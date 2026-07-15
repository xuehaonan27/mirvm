#!/usr/bin/env mirvm
---
[dependencies]
zip = { version = "2", default-features = false, features = ["deflate"] }
---
// zip 2.4（Stored 为主体 + flate2 后端 deflate）：内存 Cursor 建档 → 整档字节
// fnv 锚定 → ZipArchive 读回逐 entry 校验（名/目录位/CRC32/尺寸/mtime/mode/
// 内容 checksum）。覆盖：目录、空文件、UTF-8 文本（CJK+emoji）、定种随机二进制、
// 结构化大文本、unicode 文件名、unix 权限、固定 mtime、archive comment、
// by_index/by_name/file_names/name_for_index/by_index_raw/finish_into_readable，
// 坏档/截断/数据损坏三条错误路径，deflate level 6/9/默认三档 roundtrip。
//
// 已知 FRONTIER 绕行（语义不变）：crc32fast 单次 update ≥128B 会切 pclmulqdq
// 硬件路径（llvm.x86.pclmulqdq 未内建，执行到即 TRAP 进程退出）。全程以 64B
// 块写/读（<128B 阈值 → 可移植表路径）；算出的 CRC32 与产出的 zip 字节与
// 整块写法完全一致，native/mirvm 逐字节对拍不受影响。deflate 用 flate2 raw
// deflate 后端（miniz_oxide 只在 zlib 容器才算 adler32，raw 路径不碰
// simd-adler32 → 无 psad.bw）。
use std::io::{self, Cursor, Read, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

/// crc32fast 硬件路径阈值是单次 update 128B；64B 块保持可移植表路径。
const CHUNK: usize = 64;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 软件 CRC32（IEEE，表驱动）——独立交叉校验 entry 头里的 CRC32 字段。
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

/// 定种 xorshift64* PRNG（native/mirvm 同序列）。
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

/// 分 64B 块写入（见文件头 FRONTIER 注）。
fn chunked_write<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    for c in data.chunks(CHUNK) {
        w.write_all(c)?;
    }
    Ok(())
}

/// 分 64B 块读到 EOF（读空才触发 Crc32Reader 的 CRC 校验），避开
/// read_to_end 的单次大 update（≥128B 会切硬件路径）。
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

/// 结构化重复日志（deflate 下高压缩率；定长记录号保证确定性）。
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

/// 逐 entry 读回并打印全字段校验行；返回是否与期望完全一致。
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
    // ---- ① Stored 主体：建档 ----
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

    // ---- ② 读回：逐 entry 校验 + by_name 命中/未命中 ----
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

    // ---- ③ 错误路径：坏档 / 截断 / 数据字节损坏（CRC 在 EOF 报错）----
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

    // ---- ④ deflate（flate2 raw deflate 后端）× level 6/9/默认 ----
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
    // finish_into_readable：finish + ZipArchive 一步完成（另一 API 面）。
    let mut dar = dw.finish_into_readable().unwrap();
    println!("deflate archive comment = {:?}", std::str::from_utf8(dar.comment()).unwrap());
    dump_archive(&mut dar, &dexpected);
    // by_index_raw：不解压读回压缩流原文（raw 字节亦须逐位一致）。
    for i in 0..dar.len() {
        let mut f = dar.by_index_raw(i).unwrap();
        let raw = chunked_read_all(&mut f).unwrap();
        println!("raw[{i}] {} len={} fnv={:016x}", f.name(), raw.len(), fnv1a(&raw));
    }
}
