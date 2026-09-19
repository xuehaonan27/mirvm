#!/usr/bin/env mirvm
---
[dependencies]
barcoders = { version = "1", features = ["ascii", "json", "svg"] }
---
// barcoders 1.0.2: 1D barcode generation differential. No datamatrix (pure 1D library), so
// all 9 symbology modules are covered: code39 (with_checksum mod-43 check), ean13 (12/13
// digit dual length), ean8, codabar, code93 (C+K dual check; upstream CHARS table has a
// duplicate '[' bug, so ']' is illegal), code11 (>10 chars appends K), code128 (À/Ɓ/Ć
// prefix + mod-103), tf (interleaved pads odd length mod-10), ean_supp (EAN2/EAN5).
// UPCA/JAN/Bookland/USD8 aliases each run once. ascii/json/svg generators: ascii row size
// + per-row FNV + raw '# ' spectrum; json full text; svg rect count + viewBox + fill-
// opacity float format. encode() prints every 01 spectrum row in full (>160 modules
// truncated head/tail). Check digits: EAN reverse-looks-up through the pub ENCODINGS
// table, code39 extracts the 12-module check pattern, code128/tf/code93/code11 are fnv-
// anchored. Error paths: Character (illegal chars, incl. code128 missing prefix / C-set
// odd position / ']' hitting the upstream bug) and Length (empty/over-limit/boundary
// 256/257). Determinism: fixed inputs; only integers/hex/bools and deterministic text.
use barcoders::error::Result as BResult;
use barcoders::generators::ascii::ASCII;
use barcoders::generators::json::JSON;
use barcoders::generators::svg::{Color, SVG};
use barcoders::sym::codabar::Codabar;
use barcoders::sym::code11::{Code11, USD8};
use barcoders::sym::code128::Code128;
use barcoders::sym::code39::Code39;
use barcoders::sym::code93::Code93;
use barcoders::sym::ean13::{Bookland, EAN13, JAN, UPCA, ENCODINGS};
use barcoders::sym::ean8::EAN8;
use barcoders::sym::ean_supp::EANSUPP;
use barcoders::sym::tf::TF;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Module 0/1 spectrum: short spectra in full, long spectra truncated head/tail.
fn pr_row(label: &str, enc: &[u8]) {
    let s: String = enc.iter().map(|&d| (b'0' + d) as char).collect();
    if s.len() <= 160 {
        println!("{label} row[{s}]");
    } else {
        println!("{label} row[{}..{}]", &s[..80], &s[s.len() - 16..]);
    }
}

/// Uniform fingerprint: module count + FNV + 01 spectrum.
fn fp(label: &str, enc: &[u8]) {
    println!("{label} len={} fnv={:016x}", enc.len(), fnv1a(enc));
    pr_row(label, enc);
}

/// Uniform error-path printing (Error variant names come from Debug).
fn dump_err<T, F: FnOnce(&str) -> BResult<T>>(label: &str, input: &str, f: F) {
    match f(input) {
        Ok(_) => println!("{label} err[{}]=unexpected-ok", show(input)),
        Err(e) => println!("{label} err[{}]={e:?}", show(input)),
    }
}

/// Deterministic display of error input: control chars as \u{xx}, long input truncated to 40 chars + rest.
fn show(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        if c.is_control() {
            o.push_str(&format!("\\u{{{:02x}}}", c as u32));
        } else {
            o.push(c);
        }
    }
    let n = o.chars().count();
    if n > 40 {
        let t: String = o.chars().take(40).collect();
        format!("{t}..(+{})", n - 40)
    } else {
        o
    }
}

/// Reverse-lookup of the EAN13/EAN8 check digit: the final 10 modules are "check 7 modules
/// + right guard 3", resolved via the public ENCODINGS table. -1 means lookup failed (never expected).
fn ean_cd(enc: &[u8]) -> i32 {
    let seg = &enc[enc.len() - 10..enc.len() - 3];
    for (d, pat) in ENCODINGS[2].iter().enumerate() {
        if *seg == *pat {
            return d as i32;
        }
    }
    -1
}

fn main() {
    // ---- ① EAN-13: regular + 12/13 dual-length quirk + all-0/all-9 boundaries ----
    for s in [
        "750103131130",
        "590123412345",
        "9780306406157", // 13-digit input still accepted (valid_len 12..=13); enc becomes 102 modules
        "000000000000",
        "999999999999",
    ] {
        let b = EAN13::new(s).unwrap();
        fp(&format!("ean13[{s}]"), &b.encode());
    }
    // Sweeping the last digit 0..10 walks the mod-10 check digit, reverse-looked-up from the encoding.
    for d in 0..10u32 {
        let s = format!("10000000000{d}");
        let e = EAN13::new(&s).unwrap().encode();
        println!("ean13 cd[{s}]={} fnv={:016x}", ean_cd(&e), fnv1a(&e));
    }
    // Type-alias surface: UPC-A (leading 0) / JAN (49) / Bookland (978)
    let upc: UPCA = EAN13::new("016600069865").unwrap();
    fp("upca[016600069865]", &upc.encode());
    let jan: JAN = EAN13::new("491234567890").unwrap();
    fp("jan[491234567890]", &jan.encode());
    let bk: Bookland = EAN13::new("978030640615").unwrap();
    fp("bookland[978030640615]", &bk.encode());
    // Error paths
    dump_err("ean13", "12345678901", |s: &str| EAN13::new(s)); // 11 digits -> Length
    dump_err("ean13", "12345678901234", |s: &str| EAN13::new(s)); // 14 digits -> Length
    dump_err("ean13", "75010313113A", |s: &str| EAN13::new(s)); // non-digit -> Character
    dump_err("ean13", "", |s: &str| EAN13::new(s)); // empty -> Length

    // ---- ② EAN-8: 7/8 dual length + boundaries ----
    for s in ["55123457", "1234567", "0000000", "9992227"] {
        let b = EAN8::new(s).unwrap();
        fp(&format!("ean8[{s}]"), &b.encode());
    }
    for d in 0..10u32 {
        let s = format!("000000{d}");
        let e = EAN8::new(&s).unwrap().encode();
        println!("ean8 cd[{s}]={} fnv={:016x}", ean_cd(&e), fnv1a(&e));
    }
    dump_err("ean8", "123456", |s: &str| EAN8::new(s)); // 6 digits -> Length
    dump_err("ean8", "123456789", |s: &str| EAN8::new(s)); // 9 digits -> Length
    dump_err("ean8", "12345A6", |s: &str| EAN8::new(s)); // -> Character

    // ---- ③ Codabar: start/stop chars + all symbol chars + length boundaries 1/256/257 ----
    for s in ["A98B", "B12354999A", "A5675+++3$$B", "C401.56:92/1D", "A"] {
        let b = Codabar::new(s).unwrap();
        fp(&format!("codabar[{s}]"), &b.encode());
    }
    let max_ok = "A".repeat(256);
    let b = Codabar::new(&max_ok).unwrap();
    println!(
        "codabar[A*256] len={} fnv={:016x}",
        b.encode().len(),
        fnv1a(&b.encode())
    );
    dump_err("codabar", &"A".repeat(257), |s: &str| Codabar::new(s)); // over 256 -> Length
    dump_err("codabar", "A12Q", |s: &str| Codabar::new(s)); // 'Q' -> Character
    dump_err("codabar", "a98b", |s: &str| Codabar::new(s)); // lowercase start/stop -> Character
    dump_err("codabar", "", |s: &str| Codabar::new(s)); // -> Length

    // ---- ④ Code39: plain vs with_checksum + check-digit module spectrum ----
    for s in ["TEST8052", "MIRVM-2026", "CODE 39/$+%."] {
        let p = Code39::new(s).unwrap().encode();
        let c = Code39::with_checksum(s).unwrap().encode();
        fp(&format!("code39[{s}]"), &p);
        println!(
            "code39+ck[{s}] len={} fnv={:016x}",
            c.len(),
            fnv1a(&c)
        );
    }
    // Extraction of the mod-43 check character's 12-module pattern: after guard(12)+0+13n
    // and before the trailing guard. Covers boundaries ('0'=index 0, '%'=index 42) and all 43 chars.
    for s in ["0", "%", "MIRVM", "WIKIPEDIA", "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-. $/+%"] {
        let c = Code39::with_checksum(s).unwrap().encode();
        let n = s.chars().count();
        let chk: String = c[13 + 13 * n..25 + 13 * n]
            .iter()
            .map(|&d| (b'0' + d) as char)
            .collect();
        println!(
            "code39 chk[{s}] len={} mods={chk} fnv={:016x}",
            c.len(),
            fnv1a(&c)
        );
    }
    dump_err("code39", "1212s", |s: &str| Code39::new(s)); // lowercase -> Character
    dump_err("code39", "", |s: &str| Code39::new(s)); // -> Length
    dump_err("code39", &"X".repeat(257), |s: &str| Code39::new(s)); // -> Length

    // ---- ⑤ Code93: auto C+K dual check; ']' hits the upstream CHARS-table duplicate bug -> Character ----
    for s in ["TEST93", "CIVIC VIDEO", "BAR(93)RANGE"] {
        let b = Code93::new(s).unwrap();
        fp(&format!("code93[{s}]"), &b.encode());
    }
    dump_err("code93", "ABC]", |s: &str| Code93::new(s)); // ']' -> Character
    dump_err("code93", "", |s: &str| Code93::new(s)); // -> Length

    // ---- ⑥ Code11/USD8: C check always present, >10 chars appends K ----
    for s in ["12-9", "9923-1111", "1234567890", "12345678901", "122333444455556666"] {
        let b = Code11::new(s).unwrap();
        fp(&format!("code11[{s}]"), &b.encode());
    }
    let usd: USD8 = Code11::new("7-3").unwrap();
    fp("usd8[7-3]", &usd.encode());
    dump_err("code11", "12A", |s: &str| Code11::new(s)); // -> Character
    dump_err("code11", "", |s: &str| Code11::new(s)); // -> Length

    // ---- ⑦ Code128: three charsets + mid-stream set switch + mod-103 ----
    for s in ["ÀHELLO", "ƁHE1234A*1", "ÀHE@$AĆ123456", "Ć123456", "À"] {
        let b = Code128::new(s).unwrap();
        fp(&format!("code128[{s}]"), &b.encode());
    }
    dump_err("code128", "HELLO", |s: &str| Code128::new(s)); // missing charset prefix -> Character
    dump_err("code128", "Ć123", |s: &str| Code128::new(s)); // C-set odd position -> Character
    dump_err("code128", "À\u{007F}", |s: &str| Code128::new(s)); // DEL not in A set -> Character
    dump_err("code128", "", |s: &str| Code128::new(s)); // len<2 -> Length

    // ---- ⑧ TF: interleaved pads odd length with mod-10 / standard as-is ----
    for s in ["1234567", "1234", "98766543561"] {
        let i = TF::interleaved(s).unwrap().encode();
        let t = TF::standard(s).unwrap().encode();
        println!("itf[{s}] len={} fnv={:016x}", i.len(), fnv1a(&i));
        println!("stf[{s}] len={} fnv={:016x}", t.len(), fnv1a(&t));
    }
    dump_err("tf", "12A4", |s: &str| TF::interleaved(s)); // -> Character
    dump_err("tf", "", |s: &str| TF::standard(s)); // -> Length

    // ---- ⑨ EANSUPP: EAN2/EAN5 + middle lengths fall through to the match fallback Length ----
    for s in ["34", "50799", "00", "99999"] {
        let b = EANSUPP::new(s).unwrap();
        fp(&format!("eansupp[{s}]"), &b.encode());
    }
    dump_err("eansupp", "123", |s: &str| EANSUPP::new(s)); // 3 digits -> match fallback Length
    dump_err("eansupp", "1234", |s: &str| EANSUPP::new(s)); // 4 digits -> same
    dump_err("eansupp", "1", |s: &str| EANSUPP::new(s)); // Length at the parse layer
    dump_err("eansupp", "5A", |s: &str| EANSUPP::new(s)); // -> Character

    // ---- ⑩ Generators: ASCII matrix / JSON full text / SVG ----
    let e13 = EAN13::new("750103131130").unwrap().encode();
    let g = ASCII::new().generate(&e13).unwrap();
    let lines: Vec<&str> = g.split('\n').collect();
    println!(
        "ascii/ean13 bytes={} lines={} rows_eq={} fnv={:016x}",
        g.len(),
        lines.len(),
        lines.iter().all(|l| *l == lines[0]),
        fnv1a(g.as_bytes())
    );
    for (i, l) in lines.iter().enumerate().take(2) {
        println!("ascii/ean13 line{i} len={} fnv={:016x}", l.len(), fnv1a(l.as_bytes()));
    }
    println!("ascii/ean13 line0 |{}|", lines[0]);
    let g2 = ASCII { height: 3, xdim: 3 }
        .generate(Code39::new("MIRVM").unwrap().encode())
        .unwrap();
    let lines2: Vec<&str> = g2.split('\n').collect();
    println!(
        "ascii/code39 bytes={} lines={} fnv={:016x}",
        g2.len(),
        lines2.len(),
        fnv1a(g2.as_bytes())
    );
    println!("ascii/code39 line0 |{}|", lines2[0]);

    let j1 = JSON::new().generate(Codabar::new("A98B").unwrap().encode()).unwrap();
    println!("json/codabar bytes={} fnv={:016x}", j1.len(), fnv1a(j1.as_bytes()));
    println!("json/codabar {j1}");
    let j2 = JSON { height: 4, xdim: 2 }
        .generate(EAN8::new("1234567").unwrap().encode())
        .unwrap();
    println!("json/ean8 bytes={} fnv={:016x}", j2.len(), fnv1a(j2.as_bytes()));

    let s1 = SVG::new(60).generate(Code93::new("TEST93").unwrap().encode()).unwrap();
    println!(
        "svg/code93 bytes={} rects={} fnv={:016x}",
        s1.len(),
        s1.matches("<rect").count(),
        fnv1a(s1.as_bytes())
    );
    let s2 = SVG {
        height: 40,
        xdim: 2,
        foreground: Color::new([255, 38, 42, 120]),
        background: Color::new([10, 20, 30, 255]),
    }
    .generate(EAN8::new("5512345").unwrap().encode())
    .unwrap();
    let vb_end = s2.find('>').unwrap() + 1;
    let opa = s2.find("fill-opacity").unwrap();
    println!(
        "svg/ean8-semi bytes={} rects={} fnv={:016x}",
        s2.len(),
        s2.matches("<rect").count(),
        fnv1a(s2.as_bytes())
    );
    println!("svg/ean8-semi root |{}|", &s2[..vb_end]);
    println!("svg/ean8-semi opa |{}|", &s2[opa - 16..opa + 26]);
}
