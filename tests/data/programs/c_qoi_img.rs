#!/usr/bin/env mirvm
---
[dependencies]
qoi = "0.4"
---
// qoi 0.4 (QOI "Quite Okay Image": integer-only, pure safe Rust, only bytemuck)
// differential over an in-memory 96x64 procedural image; five bands, one op each:
//   y<10   fine incremental gradient     -> QOI_OP_DIFF
//   y<21   medium green-biased gradient  -> QOI_OP_LUMA
//   y<35   xor texture                   -> QOI_OP_RGB / QOI_OP_RGBA (noisy alpha)
//   y<42   8-colour rotating palette     -> QOI_OP_INDEX
//   rest   constant horizontal bands     -> QOI_OP_RUN (cross-row runs, run==62 flush)
// Coverage: encode_to_vec/decode_to_vec free fns, Encoder (both with_colorspace
// values, channels/header/required_buf_len, encode_to_buf), Decoder (with_channels
// 3<->4 cross decode, channels/required_buf_len, decode_to_buf, trailing data()),
// decode_header, encode_max_len, encode_to_buf/decode_to_buf free fns,
// Header::n_pixels/n_bytes, streaming encode_to_stream/from_stream, the 1x1 edge;
// error paths: bad magic / bad channels / bad colorspace / truncated header or
// body / bad padding / pixel-count mismatch / zero dim / small output buffer (both).
// Output: dimensions, FNV-1a, per-pixel mismatch counts, op census -- integers only.
use qoi::{
    Channels, ColorSpace, Decoder, Encoder, decode_header, decode_to_buf, decode_to_vec,
    encode_max_len, encode_to_buf, encode_to_vec,
};

const W: u32 = 96;
const H: u32 = 64;

/// 8-colour rotating palette (INDEX band): the hash slots repeat, so QOI_OP_INDEX always hits.
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

/// Procedural RGB pattern (x,y -> pixel); the bands are described in the file header.
fn rgb_at(x: u32, y: u32) -> [u8; 3] {
    if y < 10 {
        // Per-pixel deltas ∈ {(0,1,0),(1,1,0),(0,1,-1),(1,1,-1)}: all inside the DIFF range
        let g = (x as u8).wrapping_add(y as u8);
        [g / 4, g, 200 - g / 2]
    } else if y < 21 {
        // Per-pixel delta (5,6,4): dg=6 ∈ LUMA range, dr-dg=-1 and db-dg=-2 ∈ [-8,7]
        let (r, g, b) = ((x * 5) as u8, (x * 6) as u8, (x * 4) as u8);
        [r, g.wrapping_add(y as u8), b]
    } else if y < 35 {
        // xor texture: large pseudorandom jumps -> RGB(A) ops; 32-colour quantization repeats INDEX
        let v = (((x * 3) ^ (y * 5) ^ x.wrapping_mul(y)) & 0xf8) as u8;
        [v, v ^ 0x5a, v.rotate_left(3)]
    } else if y < 42 {
        PALETTE[(x % 8) as usize]
    } else {
        // Constant-colour bands, 3 rows each, continuous across rows -> long RUNs (96*3 > 62)
        let band = ((y - 42) / 3) as u8;
        [
            band.wrapping_mul(40),
            200u8.wrapping_sub(band.wrapping_mul(9)),
            band.wrapping_mul(7),
        ]
    }
}

/// Alpha profile: 255 in the gradient/palette/band regions (keeping DIFF/LUMA/INDEX/RUN),
/// and following the texture noise in the xor region (changes per pixel -> QOI_OP_RGBA).
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

/// RGBA image with alpha fixed at 255, for the RGBA->RGB cross decode: qoi-rust 0.4
/// forces alpha to 255 in the index hash chain on a 4->3 channel decode (the encoder
/// used the real alpha), so once the stream changes alpha, later QOI_OP_INDEX probes
/// the wrong slot. Exact cross-channel checks are anchored on alpha-flat streams only.
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

/// Per-pixel comparison: (number of mismatching pixels, index of the first mismatch or -1).
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

/// Walk the op stream (skipping each op's payload bytes, stopping before the 8-byte
/// padding) and count the six ops -- a structural fingerprint of the encoder path.
fn op_census(enc: &[u8]) -> (u32, u32, u32, u32, u32, u32) {
    let (mut rgb, mut rgba, mut diff, mut luma, mut index, mut run) = (0, 0, 0, 0, 0, 0);
    let mut i = 14; // fixed 14-byte QOI header
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

/// ① One roundtrip group: encode -> size/FNV/op census -> decode and compare per pixel.
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

    // ① All combinations of 3/4 channels × the two colorspace values
    roundtrip("rgb/srgb  ", &rgb, ColorSpace::Srgb);
    roundtrip("rgb/linear", &rgb, ColorSpace::Linear);
    roundtrip("rgba/srgb ", &rgba, ColorSpace::Srgb);
    roundtrip("rgba/lin  ", &rgba, ColorSpace::Linear);
    println!("flags: srgb={} linear={} rgb={} rgba={}",
             ColorSpace::Srgb.is_srgb(), ColorSpace::Linear.is_linear(),
             Channels::Rgb.is_rgb(), Channels::Rgba.is_rgba());

    // ② Cross decode: RGBA (alpha fixed at 255) -> RGB exactly, and RGB -> RGBA (alpha=255)
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

    // ③ Header / buffer / streaming API surface
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

    // ④ edge case 1x1: a single pixel goes straight to the run tail flush + RGBA inference
    let one = [7u8, 8, 9, 255];
    let e1 = encode_to_vec(one, 1, 1).unwrap();
    let (h1, d1) = decode_to_vec(&e1).unwrap();
    println!("edge 1x1: enc={} fnv={:016x} ch={} eq={}", e1.len(), fnv1a(&e1), h1.channels.as_u8(), d1 == one);

    // ⑤ error paths (the Display text is all integers/byte arrays, so it is deterministic)
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
