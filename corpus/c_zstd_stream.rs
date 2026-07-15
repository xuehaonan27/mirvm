#!/usr/bin/env mirvm
---
[dependencies]
zstd = "0.13"
---
// zstd 0.13（FFI 绑定 C zstd 1.5.7：zstd-sys 用 cc 把 lib/*.c +
// huf_decompress_amd64.S 编成静态 .a，mirvm 走「静态归档 → .so → RTLD_NOW」
// 通道加载）三维差分。压缩/解压的计算主体在 native C 里跑，两侧同源同参 →
// 帧字节天然确定；Rust 侧（zstd/zstd-safe 薄封装、io glue、错误串映射）才是
// 被解释/JIT 的对象。
//
// 覆盖：
// ① bulk::{compress,decompress,compress_to_buffer,decompress_to_buffer} ×
//    level -3/1/9/19 × {结构化重复日志, 定种 xorshift 随机} roundtrip。
// ② stream::{Encoder,Decoder} 同矩阵：333B 分块写 / 777B 分块读。
// ③ 便捷面 encode_all/decode_all/copy_encode/copy_decode；双帧拼接下默认
//    Decoder 拼到 EOF vs single_frame 只取首帧。
// ④ 字典：raw-content dict（stream Encoder/Decoder::with_dictionary）、
//    prepared EncoderDictionary/DecoderDictionary（bulk with_prepared_dictionary
//    复用同一 ctx 连压 6 条小记录）、无字典解码字典帧的错误路径。
// ⑤ 错误路径：垃圾数据 bulk/流解码、截断 bulk 帧、截断流（read 到 EOF 撞
//    "incomplete frame"）、非法 level=99、容量不足、空输入 encode/decode。
//
// 确定性：只打印长度/FNV-1a/布尔断言与 zstd C 库错误串（ZSTD_getErrorString
// 常量）；随机用定种 xorshift64*；无时间/地址/HashMap 序；stderr 为零。
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

/// 结构化重复日志（高压缩率；定长序号保证确定性）。
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

/// 分块读到 EOF（返回 Err 时把已读长度一并给出，供错误路径打印）。
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

    // ① bulk 两函数 × 四 level × 两数据集
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

    // ①b to_buffer 变体 + compress_bound
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

    // ② stream Encoder/Decoder 同矩阵（分块写、分块读）
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

    // ③ 便捷面 + 多帧拼接 / single_frame
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

    // ④ 字典：6 条相似小记录；prepared dict + bulk ctx 复用；raw dict + stream
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

    // ⑤ 错误路径与边界
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
