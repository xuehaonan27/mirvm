#!/usr/bin/env mirvm
---
[dependencies]
png = "0.17"
---
// png 0.17 真实二进制 roundtrip：程序化 96x64 RGBA（x*y 正弦/异或图案）→
// Encoder（含两个 tEXt chunk，Best 压缩）→ Vec<u8> → Decoder 读回逐像素对拍。
// 覆盖：IHDR 字段、tEXt 写入/读回、deflate 压缩字节、OutputInfo、坏签名/截断两条错误路径。
use png::{BitDepth, ColorType, Compression, Decoder, Encoder};

const W: usize = 96;
const H: usize = 64;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// r = x*y 正弦，g = x^(3y) 异或，b = 二者混合，a = 高位固定 + 低位异或。
fn gen_image() -> Vec<u8> {
    let mut px = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) * 4;
            let s = ((x * y) as f64 * 0.07).sin();
            px[i] = ((s * 0.5 + 0.5) * 255.0) as u8;
            px[i + 1] = (x as u32 ^ (y as u32 * 3)) as u8;
            px[i + 2] = ((x * y) as u32 ^ (x as u32 + y as u32)) as u8;
            px[i + 3] = 0xC0 | ((x ^ y) & 0x3F) as u8;
        }
    }
    px
}

fn encode(px: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = Encoder::new(&mut out, W as u32, H as u32);
        enc.set_color(ColorType::Rgba);
        enc.set_depth(BitDepth::Eight);
        enc.set_compression(Compression::Best);
        enc.add_text_chunk("Software".to_string(), "mirvm-corpus png_round".to_string())
            .unwrap();
        enc.add_text_chunk("Pattern".to_string(), "sin(x*y) ^ xor".to_string())
            .unwrap();
        let mut w = enc.write_header().unwrap();
        w.write_image_data(px).unwrap();
        w.finish().unwrap();
    }
    out
}

fn main() {
    let px = gen_image();
    println!("src {}x{} rgba bytes = {}", W, H, px.len());
    println!("src fnv1a = {:016x}", fnv1a(&px));

    let png = encode(&px);
    println!("png bytes = {}", png.len());
    println!("png fnv1a = {:016x}", fnv1a(&png));
    println!("png magic = {:02x?}", &png[..8]);

    // ---- decode + 逐像素比对 ----
    let mut reader = Decoder::new(&png[..]).read_info().unwrap();
    let cap = reader.output_buffer_size();
    println!("decoder buffer size = {cap}");
    let mut dec = vec![0u8; cap];
    let info = reader.next_frame(&mut dec).unwrap();
    println!(
        "dec info = {}x{} {:?}/{:?} line {}",
        info.width, info.height, info.color_type, info.bit_depth, info.line_size
    );
    println!("dec bytes = {} fnv1a = {:016x}", dec.len(), fnv1a(&dec));
    for t in &reader.info().uncompressed_latin1_text {
        println!("tEXt {} = {}", t.keyword, t.text);
    }

    let mut mism = 0usize;
    for (a, b) in px.iter().zip(dec.iter()) {
        if a != b {
            mism += 1;
        }
    }
    println!("roundtrip = {}", mism == 0 && px.len() == dec.len());
    println!("byte mismatches = {mism}");
    for &(x, y) in &[(0usize, 0usize), (95, 63), (17, 31), (64, 16), (1, 62)] {
        let i = (y * W + x) * 4;
        println!(
            "px({x},{y}) = {:02x}{:02x}{:02x}{:02x}",
            dec[i],
            dec[i + 1],
            dec[i + 2],
            dec[i + 3]
        );
    }

    // ---- 错误路径 ①：坏 PNG 签名 ----
    let mut bad = png.clone();
    bad[1] = b'X';
    match Decoder::new(&bad[..]).read_info() {
        Ok(_) => println!("bad magic: unexpectedly ok"),
        Err(e) => println!("bad magic err = {e}"),
    }

    // ---- 错误路径 ②：截断的 IDAT ----
    let cut = &png[..png.len() - 40];
    let r: Result<(), png::DecodingError> = (|| {
        let mut reader = Decoder::new(cut).read_info()?;
        let mut o = vec![0u8; reader.output_buffer_size()];
        reader.next_frame(&mut o)?;
        Ok(())
    })();
    match r {
        Ok(()) => println!("truncated: unexpectedly ok"),
        Err(e) => println!("truncated err = {e}"),
    }
}
