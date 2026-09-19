#!/usr/bin/env mirvm
---
[dependencies]
# Pinned to =0.9.3: the fontdue latest stable in the crates.io sparse index.
# Bypass note (official backend switch, not a trim): default-features=false disables
# the default simd feature, so fontdue uses its built-in simd_core pure-f32 scalar
# fallback (fontdue's own equivalent implementation, whose internal tests assert agreement
# with basic f32 arithmetic; both oracles use the same feature set, scalar).
# On x86_64 the simd path uses 14 core::arch SSE intrinsics such as _mm_add_ps/
# _mm_sqrt_ps/_mm_div_ps/_mm_cvttps_epi32 (fontdue-0.9.3/src/platform/simd_x86.rs),
# the same family as the tiny-skia SIMD TRAP (mirvm has no llvm.x86.sse externals).
# Optional hashbrown/rayon stay off: parallel adds thread scheduling order; cache noise changes no output.
fontdue = { version = "=0.9.3", default-features = false }
---
// fontdue 0.9.3 font rasterization differential (scalar backend; see the pin note above).
// Coverage:
// ① Font loading: hard-coded /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf (the local
//    2016 DejaVuSans). A missing file prints fixed text to stderr and exits 2 with no
//    conditional skip; a parse failure exits 3; raw bytes print len + FNV-1a to pin both oracles.
// ② Font-level probes: name (Name ID 4), units_per_em bits, glyph_count, charmap entry count
//    (length only, never a HashMap iteration), scale_factor bits, and the four components of
//    the horizontal/vertical line metrics at 24px (DejaVu has no vertical metrics -> none).
// ③ Coverage probes (deterministic selection, not skips): three CJK chars (U+4E2D/6587/5B57,
//    absent from DejaVu -> probe miss) and the ligatures U+FB00..FB03 (ﬀ/ﬁ/ﬂ/ﬃ, rasterized when
//    present). The main run is ASCII + Latin-1 Supplement + Latin Extended-A; any probe char
//    the font covers joins the rasterize set.
// ④ Main raster: fixed run x 8 sizes [6,9,12,15,18,24,36,60]px (the fontdue size argument is px),
//    one line per glyph: gid, integer metrics xmin/ymin/width/height, advance_width/height bits,
//    the four OutlineBounds f32 bit patterns, and bitmap length + FNV-1a. Zero-area glyphs such
//    as the space still print (the bitmap len=0 boundary).
// ⑤ kern: three horizontal pairs (A-V / T-o / f-i) at 24px, None -> none, Some -> bits, covering
//    the ttf-parser opentype-layout GPOS/kern table paths.
// ⑥ Total line: every glyph line's text rolls byte-by-byte into one aggregate FNV-1a.
// Determinism: all inputs are fixed constants, every f32 prints via to_bits, and the loop order
//    is the two explicit array orders; no randomness/time/HashMap iteration/addresses; stderr empty.
use fontdue::{Font, FontSettings};

const FONT_PATH: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
const SIZES: [f32; 8] = [6.0, 9.0, 12.0, 15.0, 18.0, 24.0, 36.0, 60.0];

// Main run: ASCII (spaces, punctuation and digits mix zero-area and outline glyphs)
const ASCII_RUN: &str = "Hello, World! 0123456789";
// Latin-1 Supplement + Latin Extended-A
const LATIN_RUN: &str = "ÀáÂäÇçÉèÑñÿĀāČčŒœŠšŽž";
// Probe segment: CJK (expected miss; pins probe behavior) and ligature pairs (rasterized on hit)
const CJK_PROBE: &str = "中文字";
const LIG_PROBE: &str = "\u{FB00}\u{FB01}\u{FB02}\u{FB03}";

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let bytes = match std::fs::read(FONT_PATH) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("FATAL: cannot read {FONT_PATH}: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "file path='{}' len={} fnv={:016x}",
        FONT_PATH,
        bytes.len(),
        fnv1a(&bytes)
    );
    let font = match Font::from_bytes(bytes, FontSettings::default()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("FATAL: cannot parse {FONT_PATH}: {e}");
            std::process::exit(3);
        }
    };

    // ---- font-level probes ----
    println!("font name={:?}", font.name());
    println!(
        "font upem_bits={:08x} glyphs={} charmap={} sf24_bits={:08x}",
        font.units_per_em().to_bits(),
        font.glyph_count(),
        font.chars().len(),
        font.scale_factor(24.0).to_bits()
    );
    match font.horizontal_line_metrics(24.0) {
        Some(lm) => println!(
            "lm24 asc={:08x} desc={:08x} gap={:08x} nls={:08x}",
            lm.ascent.to_bits(),
            lm.descent.to_bits(),
            lm.line_gap.to_bits(),
            lm.new_line_size.to_bits()
        ),
        None => println!("lm24 none"),
    }
    match font.vertical_line_metrics(24.0) {
        Some(lm) => println!(
            "vlm24 asc={:08x} desc={:08x} gap={:08x} nls={:08x}",
            lm.ascent.to_bits(),
            lm.descent.to_bits(),
            lm.line_gap.to_bits(),
            lm.new_line_size.to_bits()
        ),
        None => println!("vlm24 none"),
    }

    // ---- kern path ----
    for (l, r) in [('A', 'V'), ('T', 'o'), ('f', 'i')] {
        match font.horizontal_kern(l, r, 24.0) {
            Some(k) => println!("kern U+{:04X} U+{:04X} bits={:08x}", l as u32, r as u32, k.to_bits()),
            None => println!("kern U+{:04X} U+{:04X} none", l as u32, r as u32),
        }
    }

    // ---- coverage probes + rasterize-set assembly (explicit array order) ----
    let mut run: Vec<char> = ASCII_RUN.chars().chain(LATIN_RUN.chars()).collect();
    for c in CJK_PROBE.chars() {
        let gid = font.lookup_glyph_index(c);
        if gid == 0 {
            println!("probe U+{:04X} gid=0 miss", c as u32);
        } else {
            println!("probe U+{:04X} gid={} hit", c as u32, gid);
            run.push(c);
        }
    }
    for c in LIG_PROBE.chars() {
        let gid = font.lookup_glyph_index(c);
        if gid == 0 {
            println!("probe U+{:04X} gid=0 miss", c as u32);
        } else {
            println!("probe U+{:04X} gid={} hit", c as u32, gid);
            run.push(c);
        }
    }
    println!("rune count={}", run.len());

    // ---- main raster loop: 8 sizes x all runes ----
    let mut agg: u64 = 0xcbf29ce484222325;
    let mut n: u64 = 0;
    for &px in &SIZES {
        for &c in &run {
            let gid = font.lookup_glyph_index(c);
            let (m, bmp) = font.rasterize(c, px);
            let bh = fnv1a(&bmp);
            let line = format!(
                "p{:02} U+{:04X} gid={} x={} y={} w={} h={} aw={:08x} ah={:08x} bx={:08x} by={:08x} bw={:08x} bh={:08x} len={} fnv={:016x}",
                px as u32,
                c as u32,
                gid,
                m.xmin,
                m.ymin,
                m.width,
                m.height,
                m.advance_width.to_bits(),
                m.advance_height.to_bits(),
                m.bounds.xmin.to_bits(),
                m.bounds.ymin.to_bits(),
                m.bounds.width.to_bits(),
                m.bounds.height.to_bits(),
                bmp.len(),
                bh
            );
            for &b in line.as_bytes() {
                agg ^= b as u64;
                agg = agg.wrapping_mul(0x100000001b3);
            }
            n += 1;
            println!("{line}");
        }
    }
    println!("total glyphs={} fnv={:016x}", n, agg);
}
