#!/usr/bin/env mirvm
---
[dependencies]
qoi = "0.4"
---
// qoi 0.4（QOI "Quite Okay Image" 格式：纯整数、纯 safe Rust，唯一依赖 bytemuck）
// 差分：内存生成 96x64 程序化图案，五段分区各对准一条编码 op 路径——
//   y<10   微增量渐变   → QOI_OP_DIFF
//   y<21   中量绿偏渐变 → QOI_OP_LUMA
//   y<35   xor 纹理     → QOI_OP_RGB / QOI_OP_RGBA（RGBA 版 alpha 随纹理噪声）
//   y<42   8 色循环调色板 → QOI_OP_INDEX
//   其余   横向恒色条带  → QOI_OP_RUN（跨行长 run，触发 run==62 截断刷新）
// 覆盖：encode_to_vec/decode_to_vec free fn、Encoder（with_colorspace 双值、
// channels/header/required_buf_len、encode_to_buf）、Decoder（with_channels
// 3↔4 交叉解码、channels/required_buf_len、decode_to_buf、data() 尾）、
// decode_header、encode_max_len、encode_to_buf/decode_to_buf free fn、
// Header::n_pixels/n_bytes、流式 encode_to_stream/from_stream、1x1 边界；
// 错误路径：坏魔数 / 非法 channels / 非法 colorspace / 截断头 / 截断体 /
// 坏 padding / 像素数不符 / 零维度 / 输出缓冲过小（编码+解码）。
// 输出：尺寸、FNV-1a、逐像素比对计数、op 普查——全整数全确定，无浮点。
use qoi::{
    Channels, ColorSpace, Decoder, Encoder, decode_header, decode_to_buf, decode_to_vec,
    encode_max_len, encode_to_buf, encode_to_vec,
};

const W: u32 = 96;
const H: u32 = 64;

/// 8 色循环调色板（INDEX 区）：hash 槽复用，必然命中 QOI_OP_INDEX。
const PALETTE: [[u8; 3]; 8] = [
    [0x11, 0x22, 0x33],
    [0xaa, 0xbb, 0xcc],
    [0x00, 0x80, 0xff],
    [0xff, 0x80, 0x00],
    [0x12, 0x34, 0x56],
    [0x65, 0x43, 0x21],
    [0xde, 0xad, 0xbe],
    [0xca, 0xfe, 0xba],
];

/// 程序化 RGB 图案（x,y → 像素），分区见文件头注释。
fn rgb_at(x: u32, y: u32) -> [u8; 3] {
    if y < 10 {
        // 逐像素增量 ∈ {(0,1,0),(1,1,0),(0,1,-1),(1,1,-1)}：全落在 DIFF 域
        let g = (x as u8).wrapping_add(y as u8);
        [g / 4, g, 200 - g / 2]
    } else if y < 21 {
        // 逐像素增量 (5,6,4)：dg=6 ∈ LUMA 域，dr-dg=-1、db-dg=-2 ∈ [-8,7]
        let (r, g, b) = ((x * 5) as u8, (x * 6) as u8, (x * 4) as u8);
        [r, g.wrapping_add(y as u8), b]
    } else if y < 35 {
        // xor 纹理：大伪随机跳变 → RGB(A) op；量化到 32 色制造 INDEX 复用
        let v = (((x * 3) ^ (y * 5) ^ x.wrapping_mul(y)) & 0xf8) as u8;
        [v, v ^ 0x5a, v.rotate_left(3)]
    } else if y < 42 {
        PALETTE[(x % 8) as usize]
    } else {
        // 横向恒色条带：每 3 行一色，行内/行间连续 → 长 RUN（96*3 > 62 必截断）
        let band = ((y - 42) / 3) as u8;
        [
            band.wrapping_mul(40),
            200u8.wrapping_sub(band.wrapping_mul(9)),
            band.wrapping_mul(7),
        ]
    }
}

/// alpha 谱系：渐变/调色板/条带区恒 255（保住 DIFF/LUMA/INDEX/RUN），
/// xor 纹理区随图案噪声（逐像素变 → 强制 QOI_OP_RGBA）。
fn alpha_at(x: u32, y: u32) -> u8 {
    if (21..35).contains(&y) {
        (((x * 3) ^ (y * 5) ^ x.wrapping_mul(y)) & 0xf8) as u8
    } else {
        255
    }
}

fn build_rgb() -> Vec<u8> {
    let mut v = Vec::with_capacity((W * H * 3) as usize);
    for y in 0..H {
        for x in 0..W {
            v.extend_from_slice(&rgb_at(x, y));
        }
    }
    v
}

fn build_rgba() -> Vec<u8> {
    let mut v = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            v.extend_from_slice(&rgb_at(x, y));
            v.push(alpha_at(x, y));
        }
    }
    v
}

/// alpha 恒 255 的 RGBA 图：供 RGBA→RGB 交叉解码用。qoi-rust 0.4 的 (3,4)
/// 跨通道解码把 index hash 链的 alpha 强制为 255（encode 侧用真实 alpha），
/// 流中一旦有 alpha 变化，后续 QOI_OP_INDEX 会查错槽——上游语义偏差，
/// 故跨通道精确性检查只在 alpha 不变的流上锚定。
fn build_rgba_flat() -> Vec<u8> {
    let mut v = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            v.extend_from_slice(&rgb_at(x, y));
            v.push(255);
        }
    }
    v
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 逐像素比对：返回 (不匹配像素数, 首个不匹配下标或 -1)。
fn pixel_diff(a: &[u8], b: &[u8], ch: usize) -> (usize, i64) {
    if a.len() != b.len() {
        return (usize::MAX, -2);
    }
    let (mut n, mut first) = (0usize, -1i64);
    for i in 0..a.len() / ch {
        if a[i * ch..(i + 1) * ch] != b[i * ch..(i + 1) * ch] {
            n += 1;
            if first < 0 {
                first = i as i64;
            }
        }
    }
    (n, first)
}

/// 沿 op 流走一遍（跳过各 op 负载字节，止于 8 字节 padding 前），
/// 返回六种 op 的出现次数——编码路径覆盖的结构化指纹。
fn op_census(enc: &[u8]) -> (u32, u32, u32, u32, u32, u32) {
    let (mut rgb, mut rgba, mut diff, mut luma, mut index, mut run) = (0, 0, 0, 0, 0, 0);
    let mut i = 14; // QOI 头固定 14 字节
    while i < enc.len() - 8 {
        let b = enc[i];
        i += 1;
        match b {
            0xfe => {
                rgb += 1;
                i += 3;
            }
            0xff => {
                rgba += 1;
                i += 4;
            }
            0x00..=0x3f => index += 1,
            0x40..=0x7f => diff += 1,
            0x80..=0xbf => {
                luma += 1;
                i += 1;
            }
            0xc0..=0xfd => run += 1,
        }
    }
    (rgb, rgba, diff, luma, index, run)
}

/// ① 单组 roundtrip：编码 → 尺寸/FNV/op 普查 → 解码逐像素比对。
fn roundtrip(tag: &str, raw: &[u8], cs: ColorSpace) {
    let enc = Encoder::new(raw, W, H)
        .unwrap()
        .with_colorspace(cs)
        .encode_to_vec()
        .unwrap();
    let (hdr, dec) = decode_to_vec(&enc).unwrap();
    let (m, f) = pixel_diff(raw, &dec, hdr.channels.as_u8() as usize);
    let (rgb, rgba, diff, luma, index, run) = op_census(&enc);
    println!(
        "{tag}: raw={} rfnv={:016x} enc={} efnv={:016x}",
        raw.len(),
        fnv1a(raw),
        enc.len(),
        fnv1a(&enc)
    );
    println!(
        "{tag}: hdr {}x{} ch={} cs={} mismatch={m} first={f}",
        hdr.width,
        hdr.height,
        hdr.channels.as_u8(),
        hdr.colorspace.as_u8()
    );
    println!("{tag}: ops rgb={rgb} rgba={rgba} diff={diff} luma={luma} index={index} run={run}");
}

fn main() {
    let rgb = build_rgb();
    let rgba = build_rgba();

    // ① 通道数 3/4 × colorspace 双值 全组合 roundtrip
    roundtrip("rgb/srgb  ", &rgb, ColorSpace::Srgb);
    roundtrip("rgb/linear", &rgb, ColorSpace::Linear);
    roundtrip("rgba/srgb ", &rgba, ColorSpace::Srgb);
    roundtrip("rgba/lin  ", &rgba, ColorSpace::Linear);
    println!("flags: srgb={} linear={} rgb={} rgba={}",
             ColorSpace::Srgb.is_srgb(), ColorSpace::Linear.is_linear(),
             Channels::Rgb.is_rgb(), Channels::Rgba.is_rgba());

    // ② 交叉解码：RGBA(alpha 恒 255)→RGB 精确还原、RGB→RGBA（alpha=255）
    let rgba_flat = build_rgba_flat();
    let enc_rgba = encode_to_vec(&rgba_flat, W, H).unwrap();
    let mut d = Decoder::new(&enc_rgba).unwrap().with_channels(Channels::Rgb);
    println!("xdec rgba->rgb: ch={} req={}", d.channels().as_u8(), d.required_buf_len());
    let dec = d.decode_to_vec().unwrap();
    let (m, f) = pixel_diff(&dec, &rgb, 3);
    println!("xdec rgba->rgb: len={} fnv={:016x} mismatch={m} first={f}", dec.len(), fnv1a(&dec));

    let enc_rgb = encode_to_vec(&rgb, W, H).unwrap();
    let dec = Decoder::new(&enc_rgb)
        .unwrap()
        .with_channels(Channels::Rgba)
        .decode_to_vec()
        .unwrap();
    let mut expect = Vec::with_capacity(rgba.len());
    for px in rgb.chunks_exact(3) {
        expect.extend_from_slice(px);
        expect.push(255);
    }
    let (m, f) = pixel_diff(&dec, &expect, 4);
    println!("xdec rgb->rgba: len={} fnv={:016x} mismatch={m} first={f}", dec.len(), fnv1a(&dec));

    // ③ Header/缓冲/流式 API 面
    let h = decode_header(&enc_rgb).unwrap();
    println!(
        "header: {}x{} ch={} cs={} px={} bytes={}",
        h.width,
        h.height,
        h.channels.as_u8(),
        h.colorspace.as_u8(),
        h.n_pixels(),
        h.n_bytes()
    );
    println!("max_len: est={} actual={}", encode_max_len(W, H, 3), enc_rgb.len());

    let e = Encoder::new(&rgb, W, H).unwrap();
    println!("encoder: ch={} cs={} req={}", e.channels().as_u8(), e.header().colorspace.as_u8(), e.required_buf_len());
    let mut buf = vec![0u8; e.required_buf_len()];
    let n = e.encode_to_buf(&mut buf).unwrap();
    buf.truncate(n);
    println!("enc_to_buf: n={n} eq_vec={}", buf == enc_rgb);
    let mut buf2 = vec![0u8; encode_max_len(W, H, 3)];
    let n2 = encode_to_buf(&mut buf2, &rgb, W, H).unwrap();
    println!("enc_to_buf/free: n={n2} eq={}", buf2[..n2] == enc_rgb[..]);

    let mut d = Decoder::new(&enc_rgb).unwrap();
    let mut out = vec![0u8; d.required_buf_len()];
    let nd = d.decode_to_buf(&mut out).unwrap();
    println!("dec_to_buf: n={nd} eq={} tail={}", out == rgb, d.data().len());
    let mut out2 = vec![0u8; (W * H * 3) as usize];
    let h2 = decode_to_buf(&mut out2, &enc_rgb).unwrap();
    println!("dec_to_buf/free: {}x{} eq={}", h2.width, h2.height, out2 == rgb);

    let mut s: Vec<u8> = Vec::new();
    let ns = Encoder::new(&rgb, W, H).unwrap().encode_to_stream(&mut s).unwrap();
    let mut ds = Decoder::from_stream(&s[..]).unwrap();
    let outs = ds.decode_to_vec().unwrap();
    println!("stream: n={ns} len={} enc_eq={} dec_eq={}", s.len(), s == enc_rgb, outs == rgb);

    // ④ 边界：1x1（单像素直接走 run 尾刷新 + RGBA 通道推断）
    let one = [7u8, 8, 9, 255];
    let e1 = encode_to_vec(one, 1, 1).unwrap();
    let (h1, d1) = decode_to_vec(&e1).unwrap();
    println!("edge 1x1: enc={} fnv={:016x} ch={} eq={}", e1.len(), fnv1a(&e1), h1.channels.as_u8(), d1 == one);

    // ⑤ 错误路径（Display 文本全为整数/字节数组，确定）
    let mut bad = enc_rgb.clone();
    bad[0] = 0;
    println!("err magic: {}", decode_to_vec(&bad).unwrap_err());
    let mut bad = enc_rgb.clone();
    bad[12] = 5;
    println!("err channels: {}", decode_to_vec(&bad).unwrap_err());
    let mut bad = enc_rgb.clone();
    bad[13] = 7;
    println!("err colorspace: {}", decode_to_vec(&bad).unwrap_err());
    println!("err trunc-hdr: {}", decode_to_vec(&enc_rgb[..10]).unwrap_err());
    println!("err trunc-body: {}", decode_to_vec(&enc_rgb[..enc_rgb.len() - 20]).unwrap_err());
    let mut bad = enc_rgb.clone();
    let l = bad.len();
    bad[l - 1] ^= 1;
    println!("err padding: {}", decode_to_vec(&bad).unwrap_err());
    println!("err img-len: {}", encode_to_vec(&rgb[..rgb.len() - 1], W, H).unwrap_err());
    println!("err zero-dim: {}", encode_to_vec(&rgb, 0, H).unwrap_err());
    println!("err small-enc-buf: {}", encode_to_buf(vec![0u8; 8], &rgb, W, H).unwrap_err());
    let mut small = vec![0u8; 8];
    println!("err small-dec-buf: {}", decode_to_buf(&mut small, &enc_rgb).unwrap_err());
}
