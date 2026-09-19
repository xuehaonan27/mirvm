#!/usr/bin/env mirvm
---
[dependencies]
# The core encoder has no dependencies; the image/svg/pic renderers are optional features,
# and this driver uses only the built-in string/unicode renderer and the to_colors matrix.
# default-features=false drops the image crate dependency without changing the API surface.
qrcode = { version = "0.14", default-features = false }
# rqrr's img feature pulls in the image crate, which is not needed: PreparedImage::
# prepare_from_greyscale takes a closure grayscale image. default-features=false leaves g2p + lru.
rqrr = { version = "0.9", default-features = false }
---
// QR closed-loop differential: qrcode 0.14 generates, rqrr 0.9 detects and decodes.
// Coverage:
//  ① fixed text x four (version, EC level) pairs (v1L/v2M/v4Q/v5H, v5H with CJK UTF-8):
//     qrcode::with_version encodes -> width/dark count/01-spectrum FNV -> self-rendered gray
//     image -> rqrr prepare_from_greyscale -> detect_grids -> decode -> byte-for-byte equal
//     to the source text + MetaData(version/ecc/mask).
//  ② QrCode::new auto version + optimize.rs mode segmentation (Numeric/Alphanumeric/Byte/
//     Kanji/mixed text) + with_error_correction_level.
//  ③ three renderer surfaces: &str ██ block render (with quiet zone), char render (quiet zone
//     off, the 21x21 matrix printed verbatim), unicode::Dense1x2, min_dimensions scaling.
//  ④ rqrr image paths: a centered 5x5 module flip still decodes (deep Reed-Solomon correction),
//     inverted/blank images detect 0 grids, two codes in one frame detect 2 grids and decode in
//     sorted order, decode_to(writer) compares bytes.
//  ⑤ error paths: DataTooLong, InvalidVersion (Normal 0 / Normal 41 / Micro1+H / Micro4+H);
//     Micro QR normal modes (micro1L/micro2M/micro3M) encode.
//
// Known pitfalls (upstream limits, semantically bypassed):
//  - rqrr 0.9 needs modules >=2px: scale=1 (1px modules) panics inside rqrr's
//    identify/grid.rs with `assertion failed: scan >= 1`, so everything renders at scale=2.
//  - qrcode's Version::Micro(0) panics directly in upstream cast.rs (not a QrError), so it is
//    not used as an error-path case.
//  - rqrr MetaData.ecc_level is the raw 2-bit QR format value (M=0/L=1/H=2/Q=3), numbered
//    differently from qrcode EcLevel, so it prints as-is with no mapping.
// Determinism: pure computation, no time/threads/hash order; counts, bools, fixed strings, FNV hex.
use qrcode::render::unicode;
use qrcode::{Color, EcLevel, QrCode, QrResult, Version};
use rqrr::PreparedImage;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Row-major 01 spectrum of the matrix (dark=1) plus the dark-module count.
fn spectrum(code: &QrCode) -> (usize, String) {
    let colors = code.to_colors();
    let mut dark = 0usize;
    let mut s = String::with_capacity(colors.len());
    for c in &colors {
        if *c == Color::Dark {
            dark += 1;
            s.push('1');
        } else {
            s.push('0');
        }
    }
    (dark, s)
}

/// Self-rendered grayscale bitmap: dark modules 0 / light 255, each module scale x scale
/// pixels, surrounded by a qz-module quiet zone. Returns (pixel buffer, side length).
fn render_gray(code: &QrCode, scale: usize, qz: usize) -> (Vec<u8>, usize) {
    let n = code.width();
    let colors = code.to_colors();
    let w = (n + 2 * qz) * scale;
    let mut img = vec![255u8; w * w];
    for my in 0..n {
        for mx in 0..n {
            let v = colors[my * n + mx].select(0u8, 255u8);
            for dy in 0..scale {
                for dx in 0..scale {
                    img[((qz + my) * scale + dy) * w + (qz + mx) * scale + dx] = v;
                }
            }
        }
    }
    (img, w)
}

/// Detect + decode summary line. `expect` is the text to decode (None = report the grid count only).
fn probe(img: &[u8], w: usize, h: usize, expect: Option<&str>) -> String {
    let mut prep = PreparedImage::prepare_from_greyscale(w, h, |x, y| img[y * w + x]);
    let grids = prep.detect_grids();
    let mut out = format!("grids={}", grids.len());
    if let Some(g) = grids.first() {
        match g.decode() {
            Ok((meta, s)) => {
                let eq = expect.is_some_and(|t| s == t);
                out += &format!(
                    " eq={eq} rv={} recc={} rmask={}",
                    meta.version.0, meta.ecc_level, meta.mask
                );
            }
            Err(e) => out += &format!(" decode-err={e:?}"),
        }
    }
    out
}

/// Error text for a QrResult (QrCode does not implement Debug, so unwrap_err is unavailable).
fn es(r: QrResult<QrCode>) -> String {
    match r {
        Ok(_) => "unexpected-ok".to_string(),
        Err(e) => format!("{e:?}"),
    }
}

fn main() {
    // ---- ① encode x closed-loop decode suite ----
    let cases: [(&str, &str, Version, EcLevel); 4] = [
        ("v1L", "MIRVM-QR-01", Version::Normal(1), EcLevel::L),
        ("v2M", "mirvm qr round v2M", Version::Normal(2), EcLevel::M),
        (
            "v4Q",
            "mirvm qr roundtrip payload 0123456789",
            Version::Normal(4),
            EcLevel::Q,
        ),
        (
            "v5H",
            "mirvm 闭环 QR emoji-free 汉字 混合",
            Version::Normal(5),
            EcLevel::H,
        ),
    ];
    for (label, text, v, ec) in cases {
        let code = QrCode::with_version(text, v, ec).unwrap();
        let (dark, spec) = spectrum(&code);
        let (img, w) = render_gray(&code, 2, 4);
        println!(
            "{label} width={} dark={} spec={:016x} round {}",
            code.width(),
            dark,
            fnv1a(spec.as_bytes()),
            probe(&img, w, w, Some(text))
        );
    }

    // ---- ② auto version + mode segmentation ----
    let modes: [(&str, &str); 5] = [
        ("numeric", "0123456789012345678901234567890123456789"),
        ("alnum", "HELLO WORLD $%*+-./: MIRVM 42"),
        ("byte-cjk", "差分测试：字节模式走 UTF-8 混合。"),
        ("kanji", "漢字漢字漢字漢字漢字漢字漢字漢字"),
        ("mixed", "ORDER-3421 漢字 mix 9876543210 tail"),
    ];
    for (label, text) in modes {
        let code = QrCode::new(text).unwrap();
        let (dark, spec) = spectrum(&code);
        println!(
            "mode/{label} width={} dark={} spec={:016x}",
            code.width(),
            dark,
            fnv1a(spec.as_bytes())
        );
    }
    let h = QrCode::with_error_correction_level("MIRVM-EC-OVERRIDE-42", EcLevel::H).unwrap();
    let (dark, spec) = spectrum(&h);
    println!(
        "ec-override width={} dark={} spec={:016x}",
        h.width(),
        dark,
        fnv1a(spec.as_bytes())
    );

    // ---- ③ three renderer surfaces (small v1L code) ----
    let r1 = QrCode::with_version("MIRVM-QR-01", Version::Normal(1), EcLevel::L).unwrap();
    let s_block = r1.render::<&str>().dark_color("██").light_color("  ").build();
    println!(
        "render/block lines={} len={} fnv={:016x}",
        s_block.lines().count(),
        s_block.len(),
        fnv1a(s_block.as_bytes())
    );
    let s_char = r1
        .render::<char>()
        .dark_color('#')
        .light_color('.')
        .quiet_zone(false)
        .build();
    println!(
        "render/char lines={} len={} fnv={:016x}",
        s_char.lines().count(),
        s_char.len(),
        fnv1a(s_char.as_bytes())
    );
    println!("{s_char}");
    println!("render/char-end");
    let s_uni = r1.render::<unicode::Dense1x2>().build();
    println!(
        "render/unicode lines={} len={} fnv={:016x}",
        s_uni.lines().count(),
        s_uni.len(),
        fnv1a(s_uni.as_bytes())
    );
    let s_min = r1.render::<char>().min_dimensions(40, 40).build();
    println!(
        "render/mindim lines={} len={} fnv={:016x}",
        s_min.lines().count(),
        s_min.len(),
        fnv1a(s_min.as_bytes())
    );

    // ---- ④ rqrr image paths (v5H base image) ----
    let ecc_text = "mirvm 闭环 QR emoji-free 汉字 混合";
    let ecc_code = QrCode::with_version(ecc_text, Version::Normal(5), EcLevel::H).unwrap();
    // Centered 5x5 module flip (within H-level 30% correction budget) -> must still decode
    let (mut dmg, w) = render_gray(&ecc_code, 2, 4);
    for my in 10..15 {
        for mx in 10..15 {
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = ((4 + my) * 2 + dy) * w + (4 + mx) * 2 + dx;
                    dmg[p] = if dmg[p] == 0 { 255 } else { 0 };
                }
            }
        }
    }
    println!("damaged {}", probe(&dmg, w, w, Some(ecc_text)));
    // Inverted image: finder patterns reversed -> no grid detected
    let (img, w) = render_gray(&ecc_code, 2, 4);
    let inv: Vec<u8> = img.iter().map(|&b| 255 - b).collect();
    println!("inverted {}", probe(&inv, w, w, None));
    // Blank image
    let blank = vec![255u8; 64 * 64];
    println!("blank {}", probe(&blank, 64, 64, None));
    // Two codes in one frame: v1M + v2M side by side, 2 grids detected, decoded output sorted
    let g1 = QrCode::with_version("FIRST-GRID", Version::Normal(1), EcLevel::M).unwrap();
    let g2 = QrCode::with_version("SECOND GRID 22", Version::Normal(2), EcLevel::M).unwrap();
    let (i1, w1) = render_gray(&g1, 2, 4);
    let (i2, w2) = render_gray(&g2, 2, 4);
    let gap = 16;
    let (tw, th) = (w1 + gap + w2, w1.max(w2));
    let mut two = vec![255u8; tw * th];
    for y in 0..w1 {
        for x in 0..w1 {
            two[y * tw + x] = i1[y * w1 + x];
        }
    }
    for y in 0..w2 {
        for x in 0..w2 {
            two[y * tw + w1 + gap + x] = i2[y * w2 + x];
        }
    }
    let mut prep = PreparedImage::prepare_from_greyscale(tw, th, |x, y| two[y * tw + x]);
    let grids = prep.detect_grids();
    let mut decoded: Vec<String> = grids
        .iter()
        .map(|g| {
            g.decode()
                .map(|(_, s)| s)
                .unwrap_or_else(|e| format!("ERR{e:?}"))
        })
        .collect();
    decoded.sort();
    println!("two-grid n={} decoded={decoded:?}", grids.len());
    // decode_to(writer) byte comparison + the consuming into_colors API
    let mut prep = PreparedImage::prepare_from_greyscale(w, w, |x, y| img[y * w + x]);
    let grids = prep.detect_grids();
    let mut out = Vec::new();
    let meta = grids[0].decode_to(&mut out).unwrap();
    let colors = ecc_code.into_colors();
    println!(
        "decode_to bytes={} eq={} rv={} colors={}",
        out.len(),
        out == ecc_text.as_bytes(),
        meta.version.0,
        colors.len()
    );

    // ---- ⑤ error paths + Micro QR ----
    println!(
        "err/toolong {}",
        es(QrCode::with_version(
            "this payload is way too long for version one",
            Version::Normal(1),
            EcLevel::L
        ))
    );
    println!("err/v0 {}", es(QrCode::with_version("x", Version::Normal(0), EcLevel::L)));
    println!("err/v41 {}", es(QrCode::with_version("x", Version::Normal(41), EcLevel::M)));
    println!(
        "err/micro1H {}",
        es(QrCode::with_version("1", Version::Micro(1), EcLevel::H))
    );
    println!(
        "err/micro4H {}",
        es(QrCode::with_version("12345", Version::Micro(4), EcLevel::H))
    );
    for (mv, ec) in [(1, EcLevel::L), (2, EcLevel::M), (3, EcLevel::M)] {
        let c = QrCode::with_version("12345", Version::Micro(mv), ec).unwrap();
        let (dark, spec) = spectrum(&c);
        println!(
            "micro{mv} width={} dark={} spec={:016x}",
            c.width(),
            dark,
            fnv1a(spec.as_bytes())
        );
    }
}
