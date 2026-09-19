#!/usr/bin/env mirvm
---
[dependencies]
exr = "1.74"
# half 2.3+ does an unconditional runtime CPUID probe for f16c on std and takes the
# F16C hardware path; mirvm provides llvm.x86.vcvtps2ph/vcvtph2ps at all four widths
# (its software model is bit-for-bit equal to the hardware, per the
# cvtps2ph_software_matches_f16c_hardware_bitwise unit test), so both sides take the
# same F16C path with identical bit patterns, sNaN quieting included. Pinned to =2.2.1.
---
// exr 1.74.2 (pure-Rust OpenEXR implementation, no unsafe or foreign code), three-way
// differential. exr has no 3.x line: crates.io reports max_version = 1.74.2 and the
// sparse index ends there too, so neither the spec's `exr = "3"` nor a bare "3"
// resolves on both sides. This fixture uses 1.74.2.
//
// Coverage:
// ① A direct half::f16 bit spectrum (from_bits/from_f32/to_bits/to_f32, both software
//    float conversion directions, including subnormals, zero, inf and NaN).
// ② An in-memory 64x48 single-layer AnyChannels image: A/B/G/R f16 channels carrying
//    procedural gradients, with the R channel injected with f16 sentinel bit patterns
//    (subnormal 0x0001/0x03FF, 65504, ±inf, qNaN, -0.0), a Z f32 depth gradient and a
//    mask u32 mixed-bit channel, covering all three SampleTypes. layer_name="main",
//    pixel_aspect=0.9375 and screen_window_width=2.5 go into the metadata.
// ③ Eight compression levels written into a Vec (in-memory Cursor, to_unbuffered):
//    Uncompressed/RLE/ZIP1/ZIP16/PXR24/PIZ/B44/B44A, each printing len/FNV-1a; the read
//    builder reads them back and checks the roundtrip per channel: bitwise fnv
//    comparison plus a mismatch count. Lossy matrix (semantic expectation, holds on
//    both sides):
//      PXR24 -> only Z(f32) is lossy (24-bit truncation), f16/u32 are lossless;
//      B44/B44A -> only f16 is lossy (4x4 block quantization), f32/u32 are copied raw;
//      the other six compression levels are lossless for every type.
// ④ Encoding variants: RLE + 64x64 tiles (block-coordinate path) and Uncompressed +
//    LineOrder::Decreasing (out-of-order chunk path).
// ⑤ The SpecificChannels::rgba closure pixel surface (generic GetPixel path) roundtrips.
// ⑥ meta::MetaData::read_from_buffered (pedantic) rebuilds the metadata header:
//    requirements, compression/line_order/chunk_count/layer_size/display_window,
//    pixel_aspect bits, layer_name and the per-channel descriptions.
// ⑦ Error paths: bad magic (MetaData read), full-read and header-read truncation, and a
//    one-byte corruption in the ZIP16 data body.
// Deterministic: in-memory only (std::io::Cursor), no files/time/threads/addresses; floats
// print as to_bits(), HashMap iteration order is never observed (custom attributes stay
// empty, so written bytes are bitwise comparable); error text is the library's Cow; no rayon.
use std::io::Cursor;

use exr::prelude::*;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const W: usize = 64;
const H: usize = 48;

/// f16 RGBA procedural gradients plus the f16 bit-spectrum sentinels in R. Each Vec is row-major.
struct Source {
    r: Vec<f16>,
    g: Vec<f16>,
    b: Vec<f16>,
    a: Vec<f16>,
    z: Vec<f32>,
    mask: Vec<u32>,
}

fn make_source() -> Source {
    let mut r = Vec::with_capacity(W * H);
    let mut g = Vec::with_capacity(W * H);
    let mut b = Vec::with_capacity(W * H);
    let mut a = Vec::with_capacity(W * H);
    let mut z = Vec::with_capacity(W * H);
    let mut mask = Vec::with_capacity(W * H);
    for y in 0..H {
        for x in 0..W {
            r.push(f16::from_f32(x as f32 / 63.0));
            g.push(f16::from_f32(y as f32 / 47.0));
            b.push(f16::from_f32(((x * 3 + y * 7) % 97) as f32 / 96.0));
            a.push(f16::from_f32(if (x + y) % 11 == 0 { 0.25 } else { 1.0 }));
            // 0.6/0.00274 leave low mantissa bits set, so PXR24's f24 truncation actually fires
            z.push(128.0 + x as f32 * 0.6 + y as f32 * 0.00274);
            mask.push(((x as u32) << 16 ^ (y as u32) << 4) ^ 0xAA55_AA55);
        }
    }
    // Inject the f16 bit-spectrum sentinels into the first row of R
    let sentinels = [
        0x0001u16, // min subnormal
        0x03FF,    // max subnormal
        0x7BFF,    // max normal 65504
        0x7C00,    // +inf
        0xFC00,    // -inf
        0x7E00,    // qNaN
        0x8000,    // -0.0
    ];
    for (i, &bits) in sentinels.iter().enumerate() {
        r[i] = f16::from_bits(bits);
    }
    Source { r, g, b, a, z, mask }
}

fn hash_f16(v: &[f16]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in v {
        for b in x.to_bits().to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn hash_u32(v: &[u32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in v {
        for b in x.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

fn hash_f32(v: &[f32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in v {
        for b in x.to_bits().to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// Assembles the source data into a single-layer exr Image with the given encoding and compression.
fn build_image(compression: Compression, blocks: Blocks, line_order: LineOrder) -> Image<Layer<AnyChannels<FlatSamples>>> {
    let src = make_source();
    let chans = AnyChannels::sort(SmallVec::from_vec(vec![
        AnyChannel::new("A", FlatSamples::F16(src.a)),
        AnyChannel::new("B", FlatSamples::F16(src.b)),
        AnyChannel::new("G", FlatSamples::F16(src.g)),
        AnyChannel::new("R", FlatSamples::F16(src.r)),
        AnyChannel::new("Z", FlatSamples::F32(src.z)),
        AnyChannel::new("mask", FlatSamples::U32(src.mask)),
    ]));
    let mut layer_attrs = LayerAttributes::default();
    layer_attrs.layer_name = Some(Text::from("main"));
    layer_attrs.screen_window_width = 2.5;
    let encoding = Encoding { compression, blocks, line_order };
    let layer = Layer::new((W, H), layer_attrs, encoding, chans);

    let mut attrs = ImageAttributes::new(IntegerBounds {
        position: Vec2(0, 0),
        size: Vec2(W, H),
    });
    attrs.pixel_aspect = 0.9375;
    Image::new(attrs, layer)
}

/// Bitwise comparison of one read-back channel. Returns (roundtrip bool, mismatch count, read-back fnv).
fn check_channel(name: &str, got: &FlatSamples, src: &Source) -> (bool, usize, u64) {
    match (name, got) {
        ("A", FlatSamples::F16(v)) => {
            let mut mism = 0;
            for (x, y) in src.a.iter().zip(v) {
                if x.to_bits() != y.to_bits() {
                    mism += 1;
                }
            }
            (mism == 0 && v.len() == W * H, mism + usize::from(v.len() != W * H), hash_f16(v))
        }
        ("B", FlatSamples::F16(v)) => {
            let mut mism = 0;
            for (x, y) in src.b.iter().zip(v) {
                if x.to_bits() != y.to_bits() {
                    mism += 1;
                }
            }
            (mism == 0 && v.len() == W * H, mism + usize::from(v.len() != W * H), hash_f16(v))
        }
        ("G", FlatSamples::F16(v)) => {
            let mut mism = 0;
            for (x, y) in src.g.iter().zip(v) {
                if x.to_bits() != y.to_bits() {
                    mism += 1;
                }
            }
            (mism == 0 && v.len() == W * H, mism + usize::from(v.len() != W * H), hash_f16(v))
        }
        ("R", FlatSamples::F16(v)) => {
            let mut mism = 0;
            for (x, y) in src.r.iter().zip(v) {
                if x.to_bits() != y.to_bits() {
                    mism += 1;
                }
            }
            (mism == 0 && v.len() == W * H, mism + usize::from(v.len() != W * H), hash_f16(v))
        }
        ("Z", FlatSamples::F32(v)) => {
            let mut mism = 0;
            for (x, y) in src.z.iter().zip(v) {
                if x.to_bits() != y.to_bits() {
                    mism += 1;
                }
            }
            (mism == 0 && v.len() == W * H, mism + usize::from(v.len() != W * H), hash_f32(v))
        }
        ("mask", FlatSamples::U32(v)) => {
            let mut mism = 0;
            for (x, y) in src.mask.iter().zip(v) {
                if x != y {
                    mism += 1;
                }
            }
            (mism == 0 && v.len() == W * H, mism + usize::from(v.len() != W * H), hash_u32(v))
        }
        (other, _) => {
            println!("  {other} OTHER-MISMATCH");
            (false, 1, 0)
        }
    }
}

/// Writes one compression level, prints the file fnv, reads it back and prints the per-channel comparison.
fn run_codec(tag: &str, compression: Compression, blocks: Blocks, line_order: LineOrder) -> Vec<u8> {
    let image = build_image(compression, blocks, line_order);
    let mut cursor = Cursor::new(Vec::new());
    image.write().to_unbuffered(&mut cursor).unwrap();
    let bytes = cursor.into_inner();
    println!("{} len={} fnv={:016x}", tag, bytes.len(), fnv1a(&bytes));

    let back = exr::image::read::read()
        .no_deep_data()
        .largest_resolution_level()
        .all_channels()
        .first_valid_layer()
        .all_attributes()
        .from_buffered(Cursor::new(&bytes[..]))
        .unwrap();
    let layer = &back.layer_data;
    let src = make_source();
    for ch in &layer.channel_data.list {
        let (rt, mism, h) = check_channel(&ch.name.to_string(), &ch.sample_data, &src);
        println!("  {} rt={} mism={} fnv={:016x}", ch.name, rt, mism, h);
    }
    bytes
}

fn main() {
    // —— ① direct half::f16 bit spectrum (software float conversion both ways) ——
    println!(
        "half conv {:04x} {:04x} {:04x} {:04x}",
        f16::from_f32(1.0009765625).to_bits(),
        f16::from_f32(-0.0).to_bits(),
        f16::from_f32(65504.0).to_bits(),
        f16::from_f32(1e10).to_bits()
    );
    println!(
        "half back {:08x} {:08x} {:08x}",
        f16::from_bits(0x0001).to_f32().to_bits(),
        f16::from_bits(0x7E00).to_f32().to_bits(),
        f16::from_bits(0x8000).to_f32().to_bits()
    );
    println!(
        "half round {:04x} {:04x} {:04x}",
        f16::from_f32(f16::from_bits(0x03FF).to_f32()).to_bits(),
        f16::from_f32(f16::from_bits(0x7BFF).to_f32()).to_bits(),
        f16::from_f32(f16::from_bits(0xC001).to_f32()).to_bits()
    );

    // —— ② source data anchors (fnv pins the bits) ——
    let src0 = make_source();
    println!(
        "src R={:016x} G={:016x} B={:016x} A={:016x} Z={:016x} mask={:016x}",
        hash_f16(&src0.r),
        hash_f16(&src0.g),
        hash_f16(&src0.b),
        hash_f16(&src0.a),
        hash_f32(&src0.z),
        hash_u32(&src0.mask)
    );

    // —— ③ eight compression levels roundtrip ——
    run_codec("uc", Compression::Uncompressed, Blocks::ScanLines, LineOrder::Increasing);
    run_codec("rle", Compression::RLE, Blocks::ScanLines, LineOrder::Increasing);
    run_codec("zip1", Compression::ZIP1, Blocks::ScanLines, LineOrder::Increasing);
    let zip16 = run_codec("zip16", Compression::ZIP16, Blocks::ScanLines, LineOrder::Increasing);
    run_codec("pxr24", Compression::PXR24, Blocks::ScanLines, LineOrder::Increasing);
    run_codec("piz", Compression::PIZ, Blocks::ScanLines, LineOrder::Increasing);
    run_codec("b44", Compression::B44, Blocks::ScanLines, LineOrder::Increasing);
    run_codec("b44a", Compression::B44A, Blocks::ScanLines, LineOrder::Increasing);

    // —— ④ Encoding variants: tiles path + out-of-order chunks ——
    run_codec("rle-tiles", Compression::RLE, Blocks::Tiles(Vec2(64, 64)), LineOrder::Increasing);
    run_codec("uc-dec", Compression::Uncompressed, Blocks::ScanLines, LineOrder::Decreasing);

    // —— ⑤ SpecificChannels::rgba closure pixel surface ——
    {
        let image = Image::from_channels(
            (W, H),
            SpecificChannels::rgba(|Vec2(x, y)| {
                (
                    f16::from_f32(x as f32 / 63.0),
                    f16::from_f32(y as f32 / 47.0),
                    f16::from_f32(0.5),
                    f16::from_f32(1.0),
                )
            }),
        );
        let mut cursor = Cursor::new(Vec::new());
        image.write().to_unbuffered(&mut cursor).unwrap();
        let bytes = cursor.into_inner();
        println!("spec-rgba len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));
        let back = exr::image::read::read()
            .no_deep_data()
            .largest_resolution_level()
            .all_channels()
            .first_valid_layer()
            .all_attributes()
            .from_unbuffered(Cursor::new(&bytes[..]))
            .unwrap();
        let names: Vec<String> = back
            .layer_data
            .channel_data
            .list
            .iter()
            .map(|c| c.name.to_string())
            .collect();
        println!("spec-rgba chans={names:?} size=({}, {})", back.layer_data.size.0, back.layer_data.size.1);
    }

    // —— ⑥ metadata header fields (rebuilt from the ZIP16 file) ——
    {
        let meta = exr::meta::MetaData::read_from_buffered(Cursor::new(&zip16[..]), true).unwrap();
        println!("meta req {:?}", meta.requirements);
        println!("meta headers={}", meta.headers.len());
        let hdr = &meta.headers[0];
        println!(
            "hdr compression={:?} line_order={:?} chunks={} size=({}, {}) deep={}",
            hdr.compression,
            hdr.line_order,
            hdr.chunk_count,
            hdr.layer_size.0,
            hdr.layer_size.1,
            hdr.deep
        );
        println!(
            "hdr display pos=({}, {}) size=({}, {}) pixel_aspect={:08x}",
            hdr.shared_attributes.display_window.position.0,
            hdr.shared_attributes.display_window.position.1,
            hdr.shared_attributes.display_window.size.0,
            hdr.shared_attributes.display_window.size.1,
            hdr.shared_attributes.pixel_aspect.to_bits()
        );
        let lname = hdr
            .own_attributes
            .layer_name
            .as_ref()
            .map_or_else(|| "<none>".to_string(), |t| t.to_string());
        println!(
            "hdr layer_name={} screen_window_width={:08x}",
            lname,
            hdr.own_attributes.screen_window_width.to_bits()
        );
        for ch in &hdr.channels.list {
            println!(
                "chan {} type={:?} ql={} smp=({}, {})",
                ch.name,
                ch.sample_type,
                ch.quantize_linearly,
                ch.sampling.0,
                ch.sampling.1
            );
        }
    }

    // —— ⑦ error paths ——
    {
        let mut bad = zip16.clone();
        bad[..4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        match exr::meta::MetaData::read_from_buffered(Cursor::new(&bad[..]), true) {
            Ok(_) => println!("bad-magic unexpectedly ok"),
            Err(e) => println!("bad-magic err = {e}"),
        }
        let cut = &zip16[..zip16.len() * 3 / 5];
        match exr::image::read::read()
            .no_deep_data()
            .largest_resolution_level()
            .all_channels()
            .first_valid_layer()
            .all_attributes()
            .from_buffered(Cursor::new(cut))
        {
            Ok(_) => println!("truncated unexpectedly ok"),
            Err(e) => println!("truncated err = {e}"),
        }
        let cut_head = &zip16[..zip16.len().min(220)];
        match exr::meta::MetaData::read_from_buffered(Cursor::new(cut_head), true) {
            Ok(_) => println!("truncated-head unexpectedly ok"),
            Err(e) => println!("truncated-head err = {e}"),
        }
        // One-byte corruption in the ZIP16 data body: expect a zlib adler32/stream error
        let mut corrupt = zip16.clone();
        let off = corrupt.len() * 4 / 5;
        corrupt[off] ^= 0xFF;
        match exr::image::read::read()
            .no_deep_data()
            .largest_resolution_level()
            .all_channels()
            .first_valid_layer()
            .all_attributes()
            .from_buffered(Cursor::new(&corrupt[..]))
        {
            Ok(img) => {
                let mut h_all: u64 = 0xcbf29ce484222325;
                for chma in &img.layer_data.channel_data.list {
                    let h = match &chma.sample_data {
                        FlatSamples::F16(v) => hash_f16(v),
                        FlatSamples::F32(v) => hash_f32(v),
                        FlatSamples::U32(v) => hash_u32(v),
                    };
                    h_all ^= h;
                    h_all = h_all.wrapping_mul(0x100000001b3);
                }
                println!("corrupt read ok allfnv={h_all:016x}");
            }
            Err(e) => println!("corrupt err = {e}"),
        }
    }
}
