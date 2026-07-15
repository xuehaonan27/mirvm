#!/usr/bin/env mirvm
---
[dependencies]
flate2 = "1"
---
// flate2（默认 miniz_oxide 纯 Rust 后端）：Deflate × level 0/6/9 压缩 ~100KB
// 结构化重复文本并 roundtrip；位打包与 32KB 滑动窗口匹配密集。
// 注：ZlibEncoder/GzEncoder 的容器校验和路径钉死 x86 硬件 intrinsic——
// flate2 无条件开 miniz_oxide/simd（simd-adler32 → llvm.x86.sse2.psad.bw）、
// gz 模块走 crc32fast（x86_64 编译期固定 pclmulqdq 路径 → llvm.x86.pclmulqdq），
// mirvm 均未内建、guest CPUID 又见 host feature，故这两类原生 API 必 trap。
// 因此 zlib/gzip 容器语义改用手工容器覆盖：DeflateEncoder 产载荷 +
// 内联软件 adler32/crc32 组 trailer、拆 trailer 校验，三格式语义全 roundtrip。
use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// 软件 adler32（zlib trailer 语义，mod 65521）；避开 simd-adler32 硬件路径。
fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

// 软件 CRC32（IEEE，表驱动，gzip trailer 语义）；避开 crc32fast 硬件路径。
fn crc32(data: &[u8]) -> u32 {
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

// 结构化重复文本：定长记录号 + 小周期字段，保证三 level 压缩行为差异明显。
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

// 手工 zlib 容器：CMF/FLG 头（FLEVEL 按 level）+ raw deflate + adler32（大端）。
fn zlib_wrap(level: u32, data: &[u8]) -> Vec<u8> {
    let body = deflate_compress(level, data);
    let flg: u8 = match level {
        0 | 1 => 0x01,
        2..=5 => 0x5e,
        6 => 0x9c,
        _ => 0xda,
    };
    let mut out = vec![0x78, flg];
    out.extend_from_slice(&body);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn zlib_unwrap(wrapped: &[u8]) -> (Vec<u8>, bool) {
    let n = wrapped.len();
    let body = &wrapped[2..n - 4];
    let t = &wrapped[n - 4..];
    let adler_trailer = (t[0] as u32) << 24 | (t[1] as u32) << 16 | (t[2] as u32) << 8 | t[3] as u32;
    let out = deflate_decompress(body);
    let ok = adler32(&out) == adler_trailer;
    (out, ok)
}

// 手工 gzip 容器：10B 头（mtime=0, XFL 按 level, OS=255）+ raw deflate
// + CRC32（小端）+ ISIZE（原长 mod 2^32，小端）。
fn gzip_wrap(level: u32, data: &[u8]) -> Vec<u8> {
    let body = deflate_compress(level, data);
    let xfl: u8 = if level == 9 {
        2
    } else if level <= 1 {
        4
    } else {
        0
    };
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, xfl, 255];
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn gzip_unwrap(wrapped: &[u8]) -> (Vec<u8>, bool) {
    let n = wrapped.len();
    let body = &wrapped[10..n - 8];
    let t = &wrapped[n - 8..n - 4];
    let crc_trailer = t[0] as u32 | (t[1] as u32) << 8 | (t[2] as u32) << 16 | (t[3] as u32) << 24;
    let out = deflate_decompress(body);
    let ok = crc32(&out) == crc_trailer;
    (out, ok)
}

fn main() {
    let data = make_data();
    println!("data len={} fnv={:016x}", data.len(), fnv1a(&data));

    // ① raw deflate × level 0/6/9：压缩长 / 压缩流 checksum / roundtrip 布尔
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

    // ② zlib 容器语义（手工容器 + 软件 adler32 trailer 校验）× level 0/6/9
    for level in [0u32, 6, 9] {
        let wrapped = zlib_wrap(level, &data);
        let (back, sum_ok) = zlib_unwrap(&wrapped);
        println!(
            "zlib level={level} clen={} cfnv={:016x} sum-ok={} roundtrip={}",
            wrapped.len(),
            fnv1a(&wrapped),
            sum_ok,
            back == data
        );
    }

    // ③ gzip 容器语义（手工容器 + 软件 CRC32 trailer 校验）× level 0/6/9
    for level in [0u32, 6, 9] {
        let wrapped = gzip_wrap(level, &data);
        let (back, sum_ok) = gzip_unwrap(&wrapped);
        println!(
            "gzip level={level} clen={} cfnv={:016x} sum-ok={} roundtrip={}",
            wrapped.len(),
            fnv1a(&wrapped),
            sum_ok,
            back == data
        );
    }

    // ④ 空输入边界：raw / zlib 容器 / gzip 容器各 roundtrip 一次
    let c = deflate_compress(6, b"");
    let back = deflate_decompress(&c);
    println!(
        "empty deflate clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back.is_empty()
    );
    let wrapped = zlib_wrap(6, b"");
    let (back, sum_ok) = zlib_unwrap(&wrapped);
    println!(
        "empty zlib clen={} sum-ok={} roundtrip={}",
        wrapped.len(),
        sum_ok,
        back.is_empty()
    );
    let wrapped = gzip_wrap(6, b"");
    let (back, sum_ok) = gzip_unwrap(&wrapped);
    println!(
        "empty gzip clen={} sum-ok={} roundtrip={}",
        wrapped.len(),
        sum_ok,
        back.is_empty()
    );

    // ⑤ 流式分块写入（7 字节小块跨 deflate 块边界）+ flush
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

    // ⑥ read 端编码器（反向 API 面）：read::DeflateEncoder 从 reader 拉流压缩
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

    // ⑦ 错误路径 a：reserved block type（BTYPE=0b11）→ inflate 立即报错
    let junk = [0xffu8; 4];
    let mut sink = Vec::new();
    let err = DeflateDecoder::new(&junk[..])
        .read_to_end(&mut sink)
        .unwrap_err();
    println!("bad-block kind={:?} msg={}", err.kind(), err);

    // ⑧ 错误路径 b：截断的 deflate 流 → 输出短于原文（行为由库决定，确定性对拍）
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
