#!/usr/bin/env mirvm
---
[dependencies]
libflate = "2"
---
// libflate 2.3（纯 Rust DEFLATE + zlib/gzip 容器——jieba 词典同款解压器）：
// 三容器压缩/解压 roundtrip + 流式分块 + 头字段内省 + 错误路径。
// 覆盖：zlib::{Encoder, Decoder, EncodeOptions, Header, Lz77WindowSize,
// FlushMode}（默认 / no_compression / fixed_huffman_codes / block_size /
// flush_mode(Sync) 五变体）、gzip::{Encoder, Decoder, MultiDecoder,
// EncodeOptions, HeaderBuilder, Header, Os, ExtraField, ExtraSubField}
// （钉死 mtime/filename/comment/extra/verify 的完整头，解码端逐字段读回）、
// deflate::{Encoder, Decoder}；输入三档：结构化重复文本 / 定种 xorshift
// 随机 / 空；流式：7B 分块写 + 13B 分块读；MultiDecoder 拼接双 member 与
// 单 member Decoder 行为对比；错误路径：zlib 坏 FLG 校验位 / 截断 / adler
// 篡改，gzip 坏魔数 / 截断 / crc 篡改，deflate 保留块型。
//
// 确定性：gzip HeaderBuilder 默认 mtime 取墙钟（UNIX_EPOCH.elapsed()）——
// 所有 gzip 编码一律 EncodeOptions::new().header(钉死 mtime 的头)；只打印
// 长度 / FNV-1a / 布尔 / 固定头字段；无时间 / 地址 / 线程序 / HashMap 序。
//
// 绕行记录（上游 libflate 行为，native 已验证同现，语义不变仅拆用例）：
// F_TEXT 与 F_HCRC 同置的 gzip 头在上游即无法解码——gzip::Header::read_from
// 不把 FLG 的 F_TEXT 位回填到 is_text，FHCRC 校验时 this.crc16() 重算的
// flags 缺 F_TEXT 位，crc16 必不匹配（报 "CRC16 of GZIP header mismatched"
// 的 InvalidData）。故 verify() 与 text() 拆到两个独立头分别覆盖：
// ④⑤ 全字段头不带 text，⑤b 单独验证 text() 编码落盘 / 解码端 is_text
// 读回（上游同样不回填，解出为 false——两侧一致对拍）。
// 另注：mirvm 未内建 dyn Error+Send+Sync → dyn Debug 的上溯 vtable 变换
// （M4.2+ 债务）——io::Error 的 Debug fmt 会 TRAP；本 driver 错误路径一律
// match + 只打印 kind({:?} 于 ErrorKind) 与 msg({} Display，已验证可用)，
// 不 {:?} 打印 io::Error 本体。
use std::ffi::CString;
use std::io::{self, Read, Write};

use libflate::gzip::{
    EncodeOptions as GzipEncodeOptions, ExtraField, ExtraSubField, HeaderBuilder, MultiDecoder,
    Os,
};
use libflate::zlib::{EncodeOptions as ZlibEncodeOptions, FlushMode, Lz77WindowSize};
use libflate::{deflate, gzip, zlib};

/// 钉死的 gzip mtime（HeaderBuilder 默认取 UNIX_EPOCH.elapsed()，必须覆盖）。
const MTIME: u32 = 0x0DDC_0FFE;

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

/// 结构化重复日志（高压缩率；定长记录号保证确定性）。
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

/// 钉死全部字段的 gzip 头（mtime/os/verify/filename/comment/extra；
/// 不带 text——见文件头绕行记录）。
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

/// 分 chunk 字节块读到 EOF（流式读路径）。
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

/// 错误路径统一报告：只打印 kind 与 Display msg（不 {:?} io::Error 本体，
/// 见文件头 dyn 上溯注）。
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

    // ① zlib 容器 × 三档输入：压缩流 checksum + roundtrip 布尔
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

    // ② zlib 头内省：window_size / compression_level / Lz77WindowSize 换算梯
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

    // ③ raw deflate 容器 × 三档输入
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

    // ④ gzip 容器 × 三档输入（钉死的全字段头，无 text——见绕行记录）
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

    // ⑤ gzip 头解码端逐字段读回（含 extra subfield 与 FHCRC 校验通过）
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

    // ⑤b F_TEXT 单独覆盖（与 verify 互斥，见绕行记录）：编码端 text=true，
    // 解码端上游不回填 is_text → false（native 已验证同值，对拍一致）。
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

    // ⑥ zlib EncodeOptions 变体 × log roundtrip（每 1KiB 块写后 flush）
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

    // ⑦ 流式：7B 分块写 + 13B 分块读（跨 deflate 块边界），zlib 与 gzip 各一
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

    // ⑧ MultiDecoder：拼接双 member 一次读尽；单 member Decoder 只读第一个
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

    // ⑨ 错误路径 a：zlib 坏 FLG 校验位（CMF*256+FLG 非 31 倍数）
    match zlib::Decoder::new(&b"jk"[..]) {
        Ok(_) => println!("zlib-bad-hdr unexpectedly ok"),
        Err(e) => println!("zlib-bad-hdr kind={:?} msg={}", e.kind(), e),
    }
    // 错误路径 b：zlib 截断（trailer read_exact → UnexpectedEof）
    let c = zlib_compress(&log);
    report_err("zlib-truncated", zlib_decompress(&c[..c.len() * 3 / 5]));
    // 错误路径 c：zlib adler32 trailer 篡改 → EOF 校验报错
    let mut bad = c.clone();
    let n = bad.len();
    bad[n - 1] ^= 0xFF;
    report_err("zlib-bad-adler", zlib_decompress(&bad));

    // 错误路径 d：gzip 坏魔数
    match gzip::Decoder::new(&b"not a gzip stream"[..]) {
        Ok(_) => println!("gzip-bad-magic unexpectedly ok"),
        Err(e) => println!("gzip-bad-magic kind={:?} msg={}", e.kind(), e),
    }
    // 错误路径 e：gzip 截断
    let g = gzip_compress(&log);
    report_err("gzip-truncated", gzip_decompress(&g[..g.len() * 3 / 5]));
    // 错误路径 f：gzip crc32 trailer 篡改 → EOF CRC 校验报错
    let mut badg = g.clone();
    let n = badg.len();
    badg[n - 6] ^= 0xFF; // trailer = crc32(4B LE) + isize(4B LE)，落在 crc 字段内
    report_err("gzip-bad-crc", gzip_decompress(&badg));

    // 错误路径 g：deflate 保留块型（BTYPE=0b11）→ inflate 立即报错
    report_err("deflate-bad-block", deflate_decompress(&[0xff; 4]));
}
