#!/usr/bin/env mirvm
---
[dependencies]
zopfli = "0.8"
flate2 = "1"
---
// zopfli 0.8（纯 Rust 高压缩率 deflate；重计算 = JIT 压力）：三档输入
// （768B 结构化重复文本 / 384B 定种随机 / 768B 混合）× 三容器格式
// （Zlib/Gzip/Deflate）compress() 全矩阵，iterations 钉 2；zopfli 输出对同
// 参数同输入逐字节确定，压缩流以长度+FNV-1a 锚定。roundtrip 用 flate2 真容器
// 解码（ZlibDecoder/GzDecoder/DeflateDecoder 走 crc32fast/simd-adler32 硬件
// 路径——pclmulqdq/psad.bw 均已内建，无损校验）。覆盖：顶层 compress 三
// Format、Options 三字段变体（iteration_count / maximum_block_splits /
// iterations_without_improvement）、BlockType::Fixed/Uncompressed 直驱
// DeflateEncoder、流式 7 字节小块写（buffered 与非 buffered 两条通路）、
// get_ref/get_mut、empty/单字节边界、截断/垃圾/格式交叉三条解码错误路径。
// 确定性：定种 xorshift64*；不打印时间/地址/路径；二进制只印 len+fnv+布尔。
// 尺寸/iterations 按 MIRVM_JIT_THRESHOLD=1 <8 分钟预算钉（实测 C 维 ≈4.6
// 分钟）：zopfli 单次压缩在 mirvm 下约 10-50s（rand 形状最贵），任务建议的
// 50KB/20KB/100KB+iter5 仅 native debug 运行就需 ~3.5 分钟，mirvm 必超时，
// 故按规则先减输入尺寸。
use std::io::{Read, Write};
use std::num::NonZeroU64;

use zopfli::{BlockType, DeflateEncoder, Format, GzipEncoder, Options, ZlibEncoder};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 定种 xorshift64*（native/mirvm 同序列）。
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

// 结构化重复文本：定长记录号 + 小周期字段（高可压性，squeeze 迭代收益大）。
// 尺寸按 MIRVM_JIT_THRESHOLD=1 <8 分钟预算钉（超时先减输入，见任务约束）。
const TEXT_LEN: usize = 768;
// 定种随机（不可压形状，zopfli 仍全量跑 LZ77/squeeze 代价——mirvm 下最贵形状）。
const RAND_LEN: usize = 384;
// 混合：文本段与随机段交替（考验 block splitting 与块类型切换）。
const MIXED_LEN: usize = 768;
// 随机段原料池（须 ≥ 最长 rl 片段，见 make_mixed）。
const RAND_POOL: usize = 768;

fn make_text() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < TEXT_LEN {
        let line = format!(
            "row {i:05} | user={:03} | tag={} | msg={}\n",
            i % 251,
            ["alpha", "beta", "gamma", "delta"][(i % 4) as usize],
            "payload".repeat((i % 5 + 1) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

fn make_mixed() -> Vec<u8> {
    let text = make_text();
    let rand = Rng(0x9E3779B97F4A7C15).bytes(RAND_POOL);
    let mut d = Vec::new();
    let (mut ti, mut ri) = (0usize, 0usize);
    while d.len() < MIXED_LEN {
        let tl = 120 + (ti % 5) * 34;
        let s = ti % (text.len() - tl);
        d.extend_from_slice(&text[s..s + tl]);
        ti += tl;
        let rl = 60 + (ri % 3) * 26;
        let s = ri % (rand.len() - rl);
        d.extend_from_slice(&rand[s..s + rl]);
        ri += rl;
    }
    d
}

/// 主 options：iterations 钉 2（时长预算内最小多轮 squeeze；默认 15 时
/// MIRVM_JIT_THRESHOLD=1 超 8 分钟），其余默认（block splitting 开）。
fn zopts() -> Options {
    Options {
        iteration_count: NonZeroU64::new(2).unwrap(),
        ..Options::default()
    }
}

/// flate2 真容器解码（错误路径返回 Err 由调用方打印）。
fn inflate(fmt: &Format, c: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    match fmt {
        Format::Deflate => {
            flate2::read::DeflateDecoder::new(c).read_to_end(&mut out)?;
        }
        Format::Zlib => {
            flate2::read::ZlibDecoder::new(c).read_to_end(&mut out)?;
        }
        Format::Gzip => {
            flate2::read::GzDecoder::new(c).read_to_end(&mut out)?;
        }
    }
    Ok(out)
}

/// 顶层 compress() 一次压缩 + flate2 解码 roundtrip，打印锚定行。
fn run_compress(tag: &str, opts: Options, fmt: &Format, data: &[u8]) -> Vec<u8> {
    let mut c = Vec::new();
    zopfli::compress(opts, *fmt, &data[..], &mut c).unwrap();
    let back = inflate(fmt, &c).unwrap();
    println!(
        "{tag} clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == data
    );
    c
}

fn main() {
    let text = make_text();
    let rand = Rng(0xDEADBEEFCAFEF00D).bytes(RAND_LEN);
    let mixed = make_mixed();
    for (name, p) in [("text", &text), ("rand", &rand), ("mixed", &mixed)] {
        println!("payload {name} len={} fnv={:016x}", p.len(), fnv1a(p));
    }

    // ① 主矩阵：3 payload × Format::{Zlib,Gzip,Deflate}，iterations=5
    for (name, p) in [("text", &text), ("rand", &rand), ("mixed", &mixed)] {
        for fmt in [Format::Zlib, Format::Gzip, Format::Deflate] {
            let fname = match fmt {
                Format::Zlib => "zlib",
                Format::Gzip => "gzip",
                Format::Deflate => "deflate",
            };
            run_compress(&format!("matrix {name}/{fname}"), zopts(), &fmt, p);
        }
    }

    // ② Options 字段变体（text/deflate）：iteration 8 更优但同口径确定；
    //    maximum_block_splits=0 禁块切分；iterations_without_improvement=1 早停。
    let mut o8 = zopts();
    o8.iteration_count = NonZeroU64::new(8).unwrap();
    run_compress("opts iter=8", o8, &Format::Deflate, &text);
    let mut os0 = zopts();
    os0.maximum_block_splits = 0;
    run_compress("opts splits=0", os0, &Format::Deflate, &text);
    let mut oi1 = zopts();
    oi1.iterations_without_improvement = NonZeroU64::new(1).unwrap();
    run_compress("opts no-imp=1", oi1, &Format::Deflate, &text);

    // ③ BlockType 直驱 DeflateEncoder（绕开 zopfli squeeze 的两条非动态路径）
    for (bt, btname) in [(BlockType::Fixed, "fixed"), (BlockType::Uncompressed, "uncompressed")] {
        let mut e = DeflateEncoder::new(zopts(), bt, Vec::new());
        e.write_all(&text).unwrap();
        let c = e.finish().unwrap();
        let back = inflate(&Format::Deflate, &c).unwrap();
        println!(
            "blocktype {btname} clen={} cfnv={:016x} roundtrip={}",
            c.len(),
            fnv1a(&c),
            back == text
        );
    }

    // ④ 流式小块写：非 buffered DeflateEncoder 按 7B 写——非 buffered 的 Write
    //    语义是每 chunk 一次完整 compress_chunk（LZ77+squeeze 全路径），故只取
    //    前 280B（40 次全路径调用），时长可控且压力形状独特。
    //    buffered GzipEncoder 按 7B 写（走 BufWriter 聚合通路，同 compress 内部）。
    let stream_in = &text[..280];
    let mut e = DeflateEncoder::new(zopts(), BlockType::Dynamic, Vec::new());
    for chunk in stream_in.chunks(7) {
        e.write_all(chunk).unwrap();
    }
    println!("stream deflate get_ref-len={}", e.get_ref().len());
    let c = e.finish().unwrap();
    let back = inflate(&Format::Deflate, &c).unwrap();
    println!(
        "stream-7B deflate clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == stream_in
    );
    // buffered 通路：new_buffered 返回 BufWriter<GzipEncoder>，小块经聚合后喂入。
    let mut bw = GzipEncoder::new_buffered(zopts(), BlockType::Dynamic, Vec::new()).unwrap();
    for chunk in text.chunks(7) {
        bw.write_all(chunk).unwrap();
    }
    let enc = match bw.into_inner() {
        Ok(e) => e,
        Err(_) => panic!("gzip buffered flush failed"),
    };
    let gz = enc.finish().unwrap();
    let back = inflate(&Format::Gzip, &gz).unwrap();
    println!(
        "stream-7B gzip clen={} cfnv={:016x} roundtrip={}",
        gz.len(),
        fnv1a(&gz),
        back == text
    );
    // ZlibEncoder::new（非 buffered）单调用通路 + get_mut 探针。
    let kib = &text[..512];
    let mut ze = ZlibEncoder::new(zopts(), BlockType::Dynamic, Vec::new()).unwrap();
    ze.get_mut().reserve(64);
    ze.write_all(kib).unwrap();
    let c = ze.finish().unwrap();
    let back = inflate(&Format::Zlib, &c).unwrap();
    println!(
        "oneshot zlib-1K clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(&c),
        back == kib
    );

    // ⑤ 边界：empty × 3 格式 + 单字节 deflate
    let empty: Vec<u8> = Vec::new();
    for fmt in [Format::Zlib, Format::Gzip, Format::Deflate] {
        let fname = match fmt {
            Format::Zlib => "zlib",
            Format::Gzip => "gzip",
            Format::Deflate => "deflate",
        };
        run_compress(&format!("empty {fname}"), zopts(), &fmt, &empty);
    }
    let one = b"x".to_vec();
    run_compress("one-byte deflate", zopts(), &Format::Deflate, &one);

    // ⑥ 错误路径（flate2 解码端，文本为库内固定字符串）：截断 / 垃圾 / 格式交叉
    let full_gz = run_compress("errbase text/gzip", zopts(), &Format::Gzip, &text[..512]);
    let cut = &full_gz[..full_gz.len() / 2];
    let err = inflate(&Format::Gzip, cut).unwrap_err();
    println!("truncated-gzip kind={:?} msg={}", err.kind(), err);
    let junk = Rng(0x123456789ABCDEF0).bytes(512);
    let err = inflate(&Format::Zlib, &junk).unwrap_err();
    println!("junk-zlib kind={:?} msg={}", err.kind(), err);
    let zl = run_compress("errbase text/zlib", zopts(), &Format::Zlib, &text[..512]);
    let err = inflate(&Format::Gzip, &zl).unwrap_err();
    println!("cross gzip<-zlib kind={:?} msg={}", err.kind(), err);
}
