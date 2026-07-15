#!/usr/bin/env mirvm
---
[dependencies]
lz4_flex = "0.11"
snap = "1"
---
// lz4_flex 0.11 + snap 1（均零依赖纯 Rust）：LZ4 与 Snappy 的块级 + frame/stream
// 两级 roundtrip 差分。压缩器对同输入同参数逐字节确定，压缩流 FNV 直接锚定；
// 两端错误 Display 文本逐字节一致同样是断言面。
//
// 已知 FRONTIER（无法绕行，原样保留复现）：snap 的 frame 层（write::FrameEncoder /
// read::FrameDecoder / read::FrameEncoder）每个数据块都要算掩码 crc32c，其
// CheckSummer 在 x86_64 上按 `is_x86_feature_detected!("sse4.2")` 运行期派发到
// `crc32c_sse`（core::arch::_mm_crc32_u64/_mm_crc32_u8）——mirvm 未内建对应
// intrinsic，首个数据块写入即 TRAP 进程退出（exit 70）：
//   mirvm[m4-engine]: TRAP: foreign `llvm.x86.sse42.crc32.64.64`（LLVM 内部符号，
//   按需内建）（fn ...core_arch...sse42...__mm_crc32_u64...）
// 与 aesni 同属「运行期探测」类：mirvm 的 guest CPUID 透报宿主 sse4.2，crate 内
// 无 force-soft 开关；软表路径（crc32c_slice16）与硬件路径算值相同但选择不可外部
// 干预；snap 全部 1.x 版本（1.0.0/1.0.1/1.0.5/1.1.0/1.1.1 逐一核实）均带此 SSE
// 派发，无版本钉可钉；frame 格式校验不可跳过（空输入挡无数据块、不触发校验，
// 但属退化覆盖，不采用）。lz4_flex 全线（block+frame，xxhash 为纯 Rust）与
// snap::raw（raw 格式无校验）不受此 FRONTIER 影响。
//
// 覆盖：
//  * lz4_flex::block —— compress/decompress（裸块，显式尺寸）、
//    compress_prepend_size/decompress_size_prepended（4B LE 尺寸头）、
//    compress_into/decompress_into（调用方缓冲）、get_maximum_output_size；
//    错误路径：垃圾字面量 / 越界 offset / 截断流 / 坏尺寸头。
//  * lz4_flex::frame —— FrameEncoder/FrameDecoder（Write/Read 流式，1000B 喂块
//    × 997B 读块跨块边界）、with_frame_info 自定义头（Max64KB / Linked /
//    块校验 + 内容校验 + 显式 content_size，96KB 文本 → 多 block）；
//    错误路径：坏魔数 WrongMagicNumber / 截断 / 改内容校验字节。
//  * snap::raw —— Encoder/Decoder 的 compress_vec/decompress_vec 与
//    compress/decompress 调用方缓冲两路、max_compress_len/decompress_len；
//    错误路径：空输入 Empty / 空头 Header / 超大 varint TooBig / 有头无体 /
//    截断流 / 数据字节损坏。
//  * snap::write::FrameEncoder + snap::read::FrameDecoder（snappy framing：
//    64KB 块 × 掩码 crc32c）、snap::read::FrameEncoder（读端压缩反向 API）；
//    错误路径：坏魔数 StreamHeader / 截断 mid-chunk / 改数据字节触发校验错。
//
// 输入档：结构化重复文本 96KB / 全零 16KB / 定种 xorshift64* 随机 32KB /
// 单字节 / 空。输出：长度 + FNV-1a + roundtrip 布尔 + 错误文本。
// 无时间/地址/HashMap 序；成功路径 stderr 为空。
use std::io::{Read, Write};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
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

/// 结构化重复文本：定长记录号 + 小周期字段（高压缩率且跨块有重复可引）。
fn make_text() -> Vec<u8> {
    let mut d = Vec::new();
    let mut i = 0u32;
    while d.len() < 96 * 1024 {
        let line = format!(
            "row {i:05} | user={:03} | tag={} | msg={}\n",
            i % 251,
            ["alpha", "beta", "gamma", "delta"][(i % 4) as usize],
            "lz4snap".repeat((i % 9 + 1) as usize)
        );
        d.extend_from_slice(line.as_bytes());
        i += 1;
    }
    d
}

/// 小缓冲循环读到 EOF：返回完整输出或第一个错误（错误文本亦参与对拍）。
fn read_chunked<R: Read>(mut r: R, chunk: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        match r.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return Err(e),
        }
    }
}

fn report(tag: &str, orig: &[u8], c: &[u8], back: &[u8]) {
    println!(
        "{tag} clen={} cfnv={:016x} roundtrip={}",
        c.len(),
        fnv1a(c),
        back == orig
    );
}

fn main() {
    let text = make_text();
    let zeros = vec![0u8; 16 * 1024];
    let rand = Rng(0x9E3779B97F4A7C15).bytes(32 * 1024);
    let one = b"Q".to_vec();
    let empty: Vec<u8> = Vec::new();
    let tiers: [(&str, &[u8]); 5] = [
        ("text", &text),
        ("zeros", &zeros),
        ("rand", &rand),
        ("one", &one),
        ("empty", &empty),
    ];
    for &(name, p) in &tiers {
        println!("payload {name} len={} fnv={:016x}", p.len(), fnv1a(p));
    }

    // ---- ① lz4 块级：裸块（显式尺寸）× 全输入档 ----
    for &(name, p) in &tiers {
        let c = lz4_flex::block::compress(p);
        let back = lz4_flex::block::decompress(&c, p.len()).unwrap();
        report(&format!("lz4-block {name}"), p, &c, &back);
    }

    // ---- ② lz4 块级：4B LE 尺寸头变体 ----
    for &(name, p) in &tiers {
        let c = lz4_flex::block::compress_prepend_size(p);
        let back = lz4_flex::block::decompress_size_prepended(&c).unwrap();
        report(&format!("lz4-block-sp {name}"), p, &c, &back);
    }

    // ---- ③ lz4 块级：调用方缓冲 API（compress_into/decompress_into）----
    {
        let mut cbuf = vec![0u8; lz4_flex::block::get_maximum_output_size(text.len())];
        let n = lz4_flex::block::compress_into(&text, &mut cbuf).unwrap();
        let mut out = vec![0u8; text.len()];
        let m = lz4_flex::block::decompress_into(&cbuf[..n], &mut out).unwrap();
        println!(
            "lz4-block-into text max={} clen={n} dlen={m} roundtrip={}",
            cbuf.len(),
            out == text
        );
    }

    // ---- ④ lz4 块级错误路径 ----
    // 0xff 起始：字面量长度扩展字节一路越界 → ExpectedAnotherByte
    match lz4_flex::block::decompress(b"\xff\xff\xff\xff\xff", 64) {
        Ok(v) => println!("lz4-junk-lit ok len={}", v.len()),
        Err(e) => println!("lz4-junk-lit err: {e}"),
    }
    // token 0x00：0 字面量后 offset=1 但输出为空 → OffsetOutOfBounds
    match lz4_flex::block::decompress(&[0x00, 0x01, 0x00, 0x00], 16) {
        Ok(v) => println!("lz4-junk-off ok len={}", v.len()),
        Err(e) => println!("lz4-junk-off err: {e}"),
    }
    let lz4_text_c = lz4_flex::block::compress(&text);
    let cut = &lz4_text_c[..lz4_text_c.len() * 3 / 5];
    match lz4_flex::block::decompress(cut, text.len()) {
        Ok(v) => println!("lz4-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("lz4-trunc err: {e}"),
    }
    // 尺寸头声称 5 字节，体只有 2 字节
    match lz4_flex::block::decompress_size_prepended(b"\x05\x00\x00\x00zz") {
        Ok(v) => println!("lz4-sp-short ok len={}", v.len()),
        Err(e) => println!("lz4-sp-short err: {e}"),
    }
    // 尺寸头本身不足 4 字节
    match lz4_flex::block::decompress_size_prepended(b"\x01\x02") {
        Ok(v) => println!("lz4-sp-tiny ok len={}", v.len()),
        Err(e) => println!("lz4-sp-tiny err: {e}"),
    }

    // ---- ⑤ lz4 frame：流式 Write/Read × 全输入档（1000B 喂 × 997B 读）----
    for &(name, p) in &tiers {
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        for chunk in p.chunks(1000) {
            enc.write_all(chunk).unwrap();
        }
        let c = enc.finish().unwrap();
        let back = read_chunked(lz4_flex::frame::FrameDecoder::new(&c[..]), 997).unwrap();
        report(&format!("lz4-frame {name}"), p, &c, &back);
    }

    // ---- ⑥ lz4 frame：自定义头（多块 linked + 双校验 + content_size）----
    let fi = lz4_flex::frame::FrameInfo::new()
        .block_size(lz4_flex::frame::BlockSize::Max64KB)
        .block_mode(lz4_flex::frame::BlockMode::Linked)
        .block_checksums(true)
        .content_checksum(true)
        .content_size(Some(text.len() as u64));
    let mut enc = lz4_flex::frame::FrameEncoder::with_frame_info(fi, Vec::new());
    enc.write_all(&text).unwrap();
    let lz4fi_c = enc.finish().unwrap();
    let back = read_chunked(lz4_flex::frame::FrameDecoder::new(&lz4fi_c[..]), 4096).unwrap();
    report("lz4-frame-fi text", &text, &lz4fi_c, &back);

    // ---- ⑦ lz4 frame 错误路径 ----
    match read_chunked(
        lz4_flex::frame::FrameDecoder::new(&b"\xde\xad\xbe\xef not an lz4 frame"[..]),
        64,
    ) {
        Ok(v) => println!("lz4-frame-badmagic ok len={}", v.len()),
        Err(e) => println!("lz4-frame-badmagic err: {e}"),
    }
    let cut = &lz4fi_c[..lz4fi_c.len() / 3];
    match read_chunked(lz4_flex::frame::FrameDecoder::new(cut), 4096) {
        Ok(v) => println!("lz4-frame-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("lz4-frame-trunc err: {e}"),
    }
    // 末 4 字节是内容校验：翻转末字节 → ContentChecksumError
    let mut bad = lz4fi_c.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xFF;
    match read_chunked(lz4_flex::frame::FrameDecoder::new(&bad[..]), 4096) {
        Ok(v) => println!("lz4-frame-corrupt ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("lz4-frame-corrupt err: {e}"),
    }

    // ---- ⑧ snap raw：Encoder/Decoder × 全输入档 ----
    for n in [0usize, 1, 1000, 96 * 1024] {
        println!("snap-max_compress_len({n}) = {}", snap::raw::max_compress_len(n));
    }
    for &(name, p) in &tiers {
        let c = snap::raw::Encoder::new().compress_vec(p).unwrap();
        let dlen = snap::raw::decompress_len(&c).unwrap();
        let back = snap::raw::Decoder::new().decompress_vec(&c).unwrap();
        println!(
            "snap-raw {name} clen={} cfnv={:016x} dlen={} roundtrip={}",
            c.len(),
            fnv1a(&c),
            dlen,
            back == p
        );
    }

    // ---- ⑨ snap raw：调用方缓冲 API（compress/decompress）----
    {
        let mut cbuf = vec![0u8; snap::raw::max_compress_len(text.len())];
        let n = snap::raw::Encoder::new()
            .compress(&text, &mut cbuf)
            .unwrap();
        let mut out = vec![0u8; text.len()];
        let m = snap::raw::Decoder::new()
            .decompress(&cbuf[..n], &mut out)
            .unwrap();
        println!(
            "snap-raw-into text max={} clen={n} dlen={m} roundtrip={}",
            cbuf.len(),
            out == text
        );
    }

    // ---- ⑩ snap raw 错误路径 ----
    match snap::raw::Decoder::new().decompress_vec(b"") {
        Ok(v) => println!("snap-empty ok len={}", v.len()),
        Err(e) => println!("snap-empty err: {e}"),
    }
    match snap::raw::decompress_len(b"") {
        Ok(n) => println!("snap-hdrlen-empty ok {n}"),
        Err(e) => println!("snap-hdrlen-empty err: {e}"),
    }
    // 5×0xff 的 varint 头 ≈ 3.4e10 > 2^32-1 → TooBig（先于任何分配）
    match snap::raw::Decoder::new().decompress_vec(&[0xff; 5]) {
        Ok(v) => println!("snap-toobig ok len={}", v.len()),
        Err(e) => println!("snap-toobig err: {e}"),
    }
    // varint 头声称 128 字节但无数据体
    match snap::raw::Decoder::new().decompress_vec(&[0x80, 0x01]) {
        Ok(v) => println!("snap-nobody ok len={}", v.len()),
        Err(e) => println!("snap-nobody err: {e}"),
    }
    let snap_text_c = snap::raw::Encoder::new().compress_vec(&text).unwrap();
    let cut = &snap_text_c[..snap_text_c.len() / 2];
    match snap::raw::Decoder::new().decompress_vec(cut) {
        Ok(v) => println!("snap-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-trunc err: {e}"),
    }
    let mut bad = snap_text_c.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0xFF;
    match snap::raw::Decoder::new().decompress_vec(&bad) {
        Ok(v) => println!("snap-corrupt ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-corrupt err: {e}"),
    }

    // ---- ⑪ snap frame：write::FrameEncoder + read::FrameDecoder × 全输入档 ----
    for &(name, p) in &tiers {
        let mut enc = snap::write::FrameEncoder::new(Vec::new());
        for chunk in p.chunks(1000) {
            enc.write_all(chunk).unwrap();
        }
        let c = enc.into_inner().unwrap();
        let back = read_chunked(snap::read::FrameDecoder::new(&c[..]), 997).unwrap();
        report(&format!("snap-frame {name}"), p, &c, &back);
    }

    // ---- ⑫ snap frame：读端压缩器（read::FrameEncoder 反向 API）----
    let mut rside = Vec::new();
    snap::read::FrameEncoder::new(&text[..])
        .read_to_end(&mut rside)
        .unwrap();
    let back = read_chunked(snap::read::FrameDecoder::new(&rside[..]), 2048).unwrap();
    report("snap-frame-readside text", &text, &rside, &back);

    // ---- ⑬ snap frame 错误路径 ----
    match read_chunked(
        snap::read::FrameDecoder::new(&b"definitely not a snappy stream!!"[..]),
        64,
    ) {
        Ok(v) => println!("snap-frame-badmagic ok len={}", v.len()),
        Err(e) => println!("snap-frame-badmagic err: {e}"),
    }
    let mut enc = snap::write::FrameEncoder::new(Vec::new());
    enc.write_all(&text).unwrap();
    let snap_frame_c = enc.into_inner().unwrap();
    // 96KB 文本 → 两个 64KB 数据块；1/3 处截断落在首块中段
    let cut = &snap_frame_c[..snap_frame_c.len() / 3];
    match read_chunked(snap::read::FrameDecoder::new(cut), 4096) {
        Ok(v) => println!("snap-frame-trunc ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-frame-trunc err: {e}"),
    }
    // 翻转数据块中段一字节：raw 解码错或 crc32c 校验错
    let mut bad = snap_frame_c.clone();
    let at = bad.len() * 2 / 3;
    bad[at] ^= 0xFF;
    match read_chunked(snap::read::FrameDecoder::new(&bad[..]), 4096) {
        Ok(v) => println!("snap-frame-corrupt ok len={} eq={}", v.len(), v == text),
        Err(e) => println!("snap-frame-corrupt err: {e}"),
    }
}
