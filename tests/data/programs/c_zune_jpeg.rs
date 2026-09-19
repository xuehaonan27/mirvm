#!/usr/bin/env mirvm
---
[dependencies]
zune-jpeg = "0.4"
image = { version = "0.25", default-features = false, features = ["jpeg"] }
jpeg-encoder = "0.7"
---
// zune-jpeg 0.4 (0.4.21, last of the 0.4.x line; pinned to 0.4 even though crates.io already
// has 0.5.x) pure-Rust JPEG decoding differential.
//
// Coverage:
// ① Two RGB images: G=64x48 gradient (r/g/b integer linear/quadratic, low AC); S=73x41
// sentinel (odd dims 73%16!=0, 41%8!=0 -> forces MCU edge padding + upsample edges; 8px
// checkerboard + xor + corner/edge sentinels, high AC energy for progressive refinement).
// ② Five encoded streams (byte-exact fnv anchors the encoder output):
//      G-base    image::codecs::jpeg::JpegEncoder q=92 (image official: always 4:2:2
//      subsampled, Baseline DCT);
//      S-base    image JpegEncoder q=80 (also 4:2:2 baseline);
//      G-prog    jpeg-encoder 0.7.0 q=88 + set_progressive(true) +
//      set_progressive_scans(4) + set_sampling_factor(F_2_2/4:2:0) +
//      set_restart_interval(99): the DRI segment exists (dri=99) but each 4:2:0 scan has
//      only 12 MCU < 99 -> no restart marker fires (see ⑦'s upstream hole);
//      S-prog    jpeg-encoder 0.7.0 q=95 + set_progressive(true) +
//      set_progressive_scans(5) + set_sampling_factor(F_1_1/4:4:4), RI=0;
//      G-seq-ri  jpeg-encoder 0.7.0 sequential q=88 + F_2_2 + RI=5: restart markers
//      really fire in a sequential scan and zune-jpeg 0.4.21 decodes it normally
//      (bit-identical to the other decode lines), covering the "restart path really
//      taken" face.
// Three chroma layouts (4:2:2/4:2:0/4:4:4) -> every decoder upsample branch.
// ③ Per frame: zune-jpeg 0.4.21 decode_headers -> info() (dims/components), in/out colorspace,
// sof Debug/progressive, then jpeg_set_out_colorspace(RGB) + decode() full frame.
// ④ decode info parses the restart interval itself: zune-jpeg 0.4.21's restart_interval is
// pub(crate) with no getter, so the driver walks JPEG segments (SOI -> each header segment ->
// DRI/FFDD before SOS), no entropy-data false positives (stuffed 0xFF00, RSTn=FFD0-D7 all
// !=DD; DRI is only in pre-scan headers). base frames=0 (image writes no DRI), G-prog=99,
// S-prog=0, G-seq-ri=5 -- the prog flag plus two dri values confirm both encoding paths are
// taken.
// ⑤ Pixel-by-pixel comparison against image 0.25.10 (load_from_memory_with_format -> to_rgb8).
// image's own jpeg backend is zune-jpeg 0.5.x (its Cargo.toml [dependencies].zune-jpeg
// version="0.5.5" resolves via caret to the newest 0.5.x and its default features cannot be
// stripped) -> a cross-version comparison of the 0.4.21 and 0.5.x decode lines; mismatch/first
// are printed per frame. Measured: 4:2:2 frames agree bit-for-bit (mism=0); 4:2:0 frames
// (G-prog/G-seq-ri) differ by 48 pixels (mism=48 first=62, all rounding in the vertical chroma
// upsampling region -- T.81 does not require bit-exact IDCT/upsampling, and each decoder
// version reproduces the same frame bit-identically across progressive and sequential -> the
// difference is in the 0.4<->0.5 decode lines, not the encoding path); 4:4:4's S-prog returns
// to zero. The count itself is deterministic and byte-identical across all three dimensions.
// ⑥ Per-frame FNV-1a (zune's decoded RGB frame) + sampled pixels ((0,0)/(w/3,h/2)/(w-1,h-1))
// as #rrggbb hex bits; the source raw bytes are fnv-anchored.
// ⑦ Upstream-hole probe (recorded faithfully, not on the main line): zune-jpeg 0.4.21 cannot
// decode progressive + restart-markers-firing streams (native probe matrix, same image):
//      seq+RI6    → z04 OK / z05 OK / image OK
//      prog+RI0   -> all OK
//      prog+RI2   -> z04 ERR "Error in decoding MCU. Reason Marker SOS found in
//                   bitstream, possibly corrupt jpeg" / z05 OK / image OK
//      prog+RI99  -> all OK (enc_len = RI0 + 6 bytes = DRI body, no restart markers)
//    zune-jpeg 0.5.x and image both decode the stream normally -> the hole is in zune
//    0.4.21's progressive restart handling (fixed in 0.5). It is printed with
//    deterministic error text (byte-identical across dimensions); G-prog's RI=99
//    sidesteps the hole while keeping dri nonzero.
//
// Determinism: in-memory integer math; no files/clock/randomness/threads/HashMap iteration;
// no floats (no to_string(f64) anywhere); error text is static strings; stderr empty.
//
// Workarounds / version pinning:
// ⚠ jpeg-encoder = "0.7" (0.7.0): image 0.25's JpegEncoder implements only the Baseline
//    standard (docs.rs image 0.25.9 codecs::jpeg: "This module implements the Baseline JPEG
//    standard."; its methods are only new/new_with_quality/set_pixel_density/encode*, no
//    progressive surface), so progressive streams must come from jpeg-encoder
//    (set_progressive/set_progressive_scans/set_restart_interval are public switches).
//    image still covers baseline encoding and reference decoding. jpeg-encoder
//    default=["std"] ("simd" is non-default -> all-scalar encode, no AVX2 branch).
// ⚠ zune-jpeg = "0.4" keeps default features (x86/neon/std): on x86 it keeps runtime
//    is_x86_feature_detected dispatch to the SSE/AVX decode fast path; all dimensions share
//    one feature set, and the mirvm side has guest CPUID dispatch plus the x86 helper
//    surface (src/vm/x86.rs), so hitting a not-yet-builtin intrinsic reddens ①
//    rather than pre-stripping the feature to dodge the probe surface.
// ⚠ image = "0.25" uses default-features=false + ["jpeg"]: this driver only uses the jpeg
//    encode/decode surface, so unrelated png/gif features are dropped to narrow the
//    dependency tree (behavior unaffected).
// ⚠ G-prog uses RI=99 plus a separate G-seq-ri frame: sidesteps the zune 0.4.21 upstream
//    hole of ⑦ (a dodge-an-upstream-hole case, not trimming of the spec main line -- both
//    progressive and baseline decode paths and the pixel comparison are fully retained, and
//    the hole itself is printed faithfully with deterministic text).
// Resolved versions: zune-jpeg 0.4.21 / image 0.25.10 / jpeg-encoder 0.7.0; zune-core 0.4.12
// (with 0.4.21); image's transitive decode backend zune-jpeg 0.5.15 / zune-core 0.5.1.

use image::ExtendedColorType;
use image::ImageFormat;
use image::codecs::jpeg::JpegEncoder as ImgJpegEncoder;
use jpeg_encoder::{ColorType as JeColor, Encoder as JeEncoder, SamplingFactor};
use zune_jpeg::JpegDecoder as ZuneDecoder;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const GW: u32 = 64;
const GH: u32 = 48;
const SW: u32 = 73;
const SH: u32 = 41;

/// G image: 64x48 smooth three-channel gradient (low AC energy).
fn build_gradient() -> Vec<u8> {
    let mut v = Vec::with_capacity((GW * GH * 3) as usize);
    for y in 0..GH {
        for x in 0..GW {
            let r = (x * 255 / (GW - 1) + y * 64 / (GH - 1)) & 0xff;
            let g = (y * 255 / (GH - 1)) & 0xff;
            let b = ((x * x + y * 3) % 192) & 0xff;
            v.push(r as u8);
            v.push(g as u8);
            v.push(b as u8);
        }
    }
    v
}

/// S image: 73x41 sentinel odd size, 8px checkerboard + xor high-frequency texture + edge sentinels.
fn build_sentinel() -> Vec<u8> {
    let mut v = Vec::with_capacity((SW * SH * 3) as usize);
    for y in 0..SH {
        for x in 0..SW {
            if x == 0 && y == 0 {
                v.extend_from_slice(&[0x00, 0x00, 0x00]); // top-left black sentinel
            } else if x == SW - 1 && y == 0 {
                v.extend_from_slice(&[0xff, 0xff, 0xff]); // top-right white sentinel
            } else if x == 0 && y == SH - 1 {
                v.extend_from_slice(&[0xff, 0x00, 0x00]); // bottom-left red sentinel
            } else if x == SW - 1 && y == SH - 1 {
                v.extend_from_slice(&[0x00, 0x00, 0xff]); // bottom-right blue sentinel
            } else if x == SW - 1 {
                v.extend_from_slice(&[0x00, 0xff, 0x00]); // last-column pure-green sentinel
            } else if y == SH - 1 {
                v.extend_from_slice(&[0xff, 0xff, 0x00]); // last-row pure-yellow sentinel
            } else {
                let tile = ((x / 8) + (y / 8)) % 2;
                let n = ((x * 37) ^ (y * 91) ^ (x.wrapping_mul(y))) & 0x3f;
                let base: u32 = tile * 0xc0;
                v.push((base ^ n) as u8);
                v.push((base.wrapping_add(0x20) ^ (n << 1)) as u8);
                v.push((0xff - base ^ (n >> 1)) as u8);
            }
        }
    }
    v
}

/// baseline path: image 0.25 JpegEncoder (officially always 4:2:2, Baseline DCT).
fn enc_baseline(data: &[u8], w: u32, h: u32, quality: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut enc = ImgJpegEncoder::new_with_quality(&mut buf, quality);
    enc.encode(data, w, h, ExtendedColorType::Rgb8).unwrap();
    buf
}

/// jpeg-encoder 0.7 generic encode (sequential/progressive, subsampling, restart interval all controllable).
/// Progressive frames must go through this path: image has no progressive encode surface (header ⚠).
fn enc_je(data: &[u8], w: u32, h: u32, quality: u8, progressive: bool, scans: u8, ri: u16, sf: SamplingFactor) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut enc = JeEncoder::new(&mut buf, quality);
    enc.set_progressive(progressive);
    if progressive {
        enc.set_progressive_scans(scans);
    }
    enc.set_restart_interval(ri);
    enc.set_sampling_factor(sf);
    enc.encode(data, w as u16, h as u16, JeColor::Rgb).unwrap();
    buf
}

/// Walk the JPEG segments to read DRI (zune-jpeg 0.4.21 has no public getter; see header ④).
/// Returns 0 = no DRI segment; u16::MAX / MAX-1 / MAX-2 = unexpected format (should not happen).
fn restart_interval(b: &[u8]) -> u16 {
    if b.len() < 4 || b[0] != 0xff || b[1] != 0xd8 {
        return u16::MAX;
    }
    let mut p = 2usize;
    loop {
        if p >= b.len() {
            return 0;
        }
        if b[p] != 0xff {
            return u16::MAX - 1;
        }
        while p < b.len() && b[p] == 0xff {
            p += 1; // fill bytes
        }
        if p >= b.len() {
            return 0;
        }
        let m = b[p];
        p += 1;
        if m == 0x00 || (0xd0..=0xd9).contains(&m) {
            continue; // standalone marker (stuffed 0/SOI/RSTn/EOI)
        }
        if m == 0xda {
            return 0; // SOS: entropy data starts, DRI is legal only before the scan
        }
        if p + 2 > b.len() {
            return 0;
        }
        let seg_len = u16::from_be_bytes([b[p], b[p + 1]]) as usize;
        if m == 0xdd && seg_len == 4 && p + 4 <= b.len() {
            return u16::from_be_bytes([b[p + 2], b[p + 3]]);
        }
        if seg_len < 2 {
            return u16::MAX - 2;
        }
        p += seg_len;
    }
}

/// Print a frame's full picture: zune decode + info + image comparison.
fn run_frame(tag: &str, enc: &[u8]) {
    println!("{}: enc len={} fnv={:016x}", tag, enc.len(), fnv1a(enc));
    let dri = restart_interval(enc);

    // ---- zune-jpeg 0.4.21 decode surface ----
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut dec = ZuneDecoder::new_with_options(&enc[..], opts);
    dec.decode_headers().unwrap();
    let info = dec.info().unwrap();
    let (zw, zh) = dec.dimensions().unwrap();
    println!(
        "{}: info {}x{} comp={} in_cs={:?} out_cs={:?} sof={:?} prog={} dri={}",
        tag,
        zw,
        zh,
        info.components,
        dec.get_input_colorspace().unwrap(),
        dec.get_output_colorspace().unwrap(),
        info.sof,
        info.sof.is_progressive(),
        dri
    );
    let zpix = dec.decode().unwrap();
    println!("{}: zune len={} fnv={:016x}", tag, zpix.len(), fnv1a(&zpix));

    // sampled pixel hex bits
    let (w, h) = (zw as usize, zh as usize);
    let coords = [(0usize, 0usize), (w / 3, h / 2), (w - 1, h - 1)];
    let mut line = String::new();
    for (x, y) in coords {
        let o = (y * w + x) * 3;
        line.push_str(&format!(
            " ({},{})#{:02x}{:02x}{:02x}",
            x, y, zpix[o], zpix[o + 1], zpix[o + 2]
        ));
    }
    println!("{}: samples{}", tag, line);

    // ---- image 0.25.10 reference decode (backend zune-jpeg 0.5.x -> cross-version comparison) ----
    let dimg = image::load_from_memory_with_format(&enc[..], ImageFormat::Jpeg).unwrap();
    let (iw, ih) = (dimg.width(), dimg.height());
    let ipix = dimg.to_rgb8().into_raw();
    let mut mism = 0usize;
    let mut first: i64 = -1;
    if zpix.len() == ipix.len() {
        for (i, (a, b)) in zpix.chunks_exact(3).zip(ipix.chunks_exact(3)).enumerate() {
            if a != b {
                mism += 1;
                if first < 0 {
                    first = i as i64;
                }
            }
        }
    } else {
        mism = usize::MAX;
        first = -2;
    }
    println!(
        "{}: xcmp image {}x{} ipsfnv={:016x} mism={} first={}",
        tag,
        iw,
        ih,
        fnv1a(&ipix),
        mism,
        first
    );
}

fn main() {
    let g = build_gradient();
    let s = build_sentinel();
    println!(
        "src: G {}x{} fnv={:016x} S {}x{} fnv={:016x}",
        GW,
        GH,
        fnv1a(&g),
        SW,
        SH,
        fnv1a(&s)
    );

    // five main-line frames
    run_frame("G-base   ", &enc_baseline(&g, GW, GH, 92));
    run_frame("S-base   ", &enc_baseline(&s, SW, SH, 80));
    run_frame("G-prog   ", &enc_je(&g, GW, GH, 88, true, 4, 99, SamplingFactor::F_2_2));
    run_frame("S-prog   ", &enc_je(&s, SW, SH, 95, true, 5, 0, SamplingFactor::F_1_1));
    run_frame("G-seq-ri ", &enc_je(&g, GW, GH, 88, false, 0, 5, SamplingFactor::F_2_2));

    // ---- ⑦ upstream-hole probe: zune 0.4.21 x progressive + restart markers really firing ----
    {
        let hole = enc_je(&g, GW, GH, 88, true, 4, 6, SamplingFactor::F_2_2);
        println!("hole: enc len={} fnv={:016x} dri={}", hole.len(), fnv1a(&hole), restart_interval(&hole));
        let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut dec = ZuneDecoder::new_with_options(&hole[..], opts);
        match dec.decode() {
            Ok(px) => println!("hole: zune04 unexpected-ok len={} fnv={:016x}", px.len(), fnv1a(&px)),
            Err(e) => println!("hole: zune04 err = {e}"),
        }
        // Same stream decodes normally under image (zune 0.5.x backend) -> hole is in the 0.4.21 decode line
        match image::load_from_memory_with_format(&hole[..], ImageFormat::Jpeg) {
            Ok(dimg) => {
                let px = dimg.to_rgb8().into_raw();
                println!("hole: image ok {}x{} ifnv={:016x}", dimg.width(), dimg.height(), fnv1a(&px));
            }
            Err(e) => println!("hole: image err = {e}"),
        }
    }
}
