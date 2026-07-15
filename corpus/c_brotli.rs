#!/usr/bin/env mirvm
---
[dependencies]
brotli = "8"
---
// brotli 8（纯 Rust brotli）：一次性 BrotliCompress/BrotliDecompress 与流式
// CompressorWriter/Decompressor/CompressorReader 三条 API 通路；质量 1/9 ×
// 多档 lgwin 窗口 × 四种 payload 形状（结构化重复文本/全零/伪随机/混合）。
// 编码器是大型状态机 + 熵编码（Huffman、上下文建模、静态字典、块切分），
// 压缩流对同参数同输入逐字节确定——好压力。
// 输出：原始/压缩长度 + FNV-1a checksum + roundtrip 布尔；错误路径只打印
// is_err 与 out-len（不断言具体错误文本，两端一致即可）。
use std::io::{Read, Write};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// xorshift64 固定种子伪随机字节（不可压缩 payload 用）。
fn prng_bytes(n: usize, mut s: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.truncate(n);
    out
}

// 结构化重复文本：定长记录号 + 小周期字段，保证质量 1 与 9 行为差异明显。
fn make_text() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < 32 * 1024 {
        let line = format!(
            "row {i:05} | user={:03} | tag={} | pad={}\n",
            i % 251,
            ["alpha", "beta", "gamma", "delta"][(i % 4) as usize],
            "xyz".repeat((i % 17) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

// 混合 payload：文本段与伪随机段交替，考验块切分/上下文切换。
fn make_mixed() -> Vec<u8> {
    let text = make_text();
    let rand = prng_bytes(12 * 1024, 0x9e3779b97f4a7c15);
    let mut d = Vec::new();
    let (mut ti, mut ri) = (0usize, 0usize);
    while d.len() < 24 * 1024 {
        let tl = 700 + (ti % 5) * 130;
        d.extend_from_slice(&text[ti % (text.len() - tl)..ti % (text.len() - tl) + tl]);
        ti += tl;
        let rl = 300 + (ri % 3) * 90;
        d.extend_from_slice(&rand[ri % (rand.len() - rl)..ri % (rand.len() - rl) + rl]);
        ri += rl;
    }
    d
}

// 一次性压缩：params 走 BrotliEncoderInitParams 改 quality/lgwin。
fn oneshot_compress(data: &[u8], q: i32, lgwin: i32) -> (Vec<u8>, usize) {
    let mut params = brotli::enc::BrotliEncoderInitParams();
    params.quality = q;
    params.lgwin = lgwin;
    let mut out = Vec::new();
    let written = brotli::BrotliCompress(&mut &data[..], &mut out, &params).unwrap();
    (out, written)
}

fn oneshot_decompress(c: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::BrotliDecompress(&mut &c[..], &mut out).unwrap();
    out
}

// 流式压缩：CompressorWriter 按 chunk 字节分块喂，into_inner 收尾（FINISH）。
fn stream_compress(data: &[u8], chunk: usize, q: u32, lgwin: u32) -> Vec<u8> {
    let mut w = brotli::CompressorWriter::new(Vec::new(), 4096, q, lgwin);
    for c in data.chunks(chunk) {
        w.write_all(c).unwrap();
    }
    w.into_inner()
}

// 流式解压：Decompressor reader 手工按 read_chunk 字节小缓冲循环 read。
fn stream_decompress(c: &[u8], read_chunk: usize) -> Vec<u8> {
    let mut r = brotli::Decompressor::new(&c[..], 4096);
    let mut out = Vec::new();
    let mut buf = vec![0u8; read_chunk];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => panic!("stream decompress failed: {e:?}"),
        }
    }
    out
}

fn report(tag: &str, orig: &[u8], c: &[u8], extra: &str, back: &[u8]) {
    println!(
        "{tag} clen={} cfnv={:016x}{} roundtrip={}",
        c.len(),
        fnv1a(c),
        extra,
        back == orig
    );
}

fn main() {
    let text = make_text();
    let zeros = vec![0u8; 32 * 1024];
    let rand = prng_bytes(16 * 1024, 0xdeadbeefcafef00d);
    let mixed = make_mixed();
    let one = b"x".to_vec();
    let empty: Vec<u8> = Vec::new();
    for (name, p) in [
        ("text", &text),
        ("zeros", &zeros),
        ("rand", &rand),
        ("mixed", &mixed),
        ("one", &one),
        ("empty", &empty),
    ] {
        println!("payload {name} len={} fnv={:016x}", p.len(), fnv1a(p));
    }

    // ① 一次性两路：text/mixed 跑 quality{1,9} × lgwin{10,24} 全矩阵
    for (name, p) in [("text", &text), ("mixed", &mixed)] {
        for q in [1, 9] {
            for w in [10, 24] {
                let (c, written) = oneshot_compress(p, q, w);
                let back = oneshot_decompress(&c);
                let extra = format!(" ret={}", written);
                report(&format!("oneshot {name} q={q} lgwin={w}"), p, &c, &extra, &back);
            }
        }
    }
    // ② 其余 payload 形状各跑两档参数（低质量小窗 / 高质量大窗）
    for (name, p) in [("zeros", &zeros), ("rand", &rand), ("one", &one), ("empty", &empty)] {
        for (q, w) in [(1, 10), (9, 24)] {
            let (c, written) = oneshot_compress(p, q, w);
            let back = oneshot_decompress(&c);
            let extra = format!(" ret={}", written);
            report(&format!("oneshot {name} q={q} lgwin={w}"), p, &c, &extra, &back);
        }
    }

    // ③ 流式 writer（7 字节小块跨元块边界）+ 流式 reader（13 字节小缓冲）
    for (name, p, q, w) in [
        ("text", &text, 1u32, 12u32),
        ("text", &text, 9, 22),
        ("mixed", &mixed, 9, 22),
    ] {
        let c = stream_compress(p, 7, q, w);
        let back = stream_decompress(&c, 13);
        report(&format!("stream-7B {name} q={q} lgwin={w}"), p, &c, "", &back);
        // 同一压缩流的一次性解压也必须成立（容器格式互通）
        let back2 = oneshot_decompress(&c);
        println!("stream-7B {name} q={q} lgwin={w} oneshot-decode roundtrip={}", back2 == *p);
    }

    // ④ read 端压缩器（反向 API 面）：CompressorReader 从 reader 拉流压缩
    let mut cr = brotli::CompressorReader::new(&text[..], 4096, 9, 22);
    let mut c = Vec::new();
    cr.read_to_end(&mut c).unwrap();
    let back = oneshot_decompress(&c);
    report("readside text q=9 lgwin=22", &text, &c, "", &back);

    // ⑤ 错误路径 a：伪随机垃圾输入 → 解压应失败（或产出短输出）
    let junk = prng_bytes(256, 0x123456789abcdef0);
    let mut sink = Vec::new();
    let r = brotli::Decompressor::new(&junk[..], 4096).read_to_end(&mut sink);
    println!("junk-decode ok={} out-len={}", r.is_ok(), sink.len());

    // ⑥ 错误路径 b：截断的高质量压缩流 → 读提前结束
    let (full, _) = oneshot_compress(&text, 9, 22);
    let cut = &full[..full.len() / 3];
    let mut sink = Vec::new();
    let r = brotli::Decompressor::new(cut, 4096).read_to_end(&mut sink);
    println!(
        "truncated-decode ok={} out-len={} (orig {})",
        r.is_ok(),
        sink.len(),
        text.len()
    );
}
