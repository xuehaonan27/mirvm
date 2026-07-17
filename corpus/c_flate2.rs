#!/usr/bin/env mirvm
---
[dependencies]
flate2 = "1"
---
// flate2（默认 miniz_oxide 纯 Rust 后端）：Deflate × level 0/6/9 压缩 ~100KB
// 结构化重复文本并 roundtrip；位打包与 32KB 滑动窗口匹配密集。
// 2026-07-18（open-issues G4②）：ZlibEncoder/GzEncoder 原生容器恢复——
// 容器校验和路径的 simd-adler32（llvm.x86.sse2.psad.bw）与 crc32fast 固定
// pclmulqdq 路径均已内建，原先的手工容器等价覆盖（Deflate 载荷 + 软件
// adler32/crc32 trailer）退役，原文见 git 历史。
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

// 手工容器已随 G4② 退役（zlib/gzip 原生 API 恢复，软件 adler32/crc32 助手删除）。

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

    // ② zlib 原生容器（ZlibEncoder/ZlibDecoder，simd-adler32 校验和）× level 0/6/9
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

    // ③ gzip 原生容器（GzEncoder/GzDecoder，crc32fast 校验和）× level 0/6/9
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

    // ④ 空输入边界：raw / zlib 容器 / gzip 容器各 roundtrip 一次
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
