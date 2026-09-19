#!/usr/bin/env mirvm
---
[dependencies]
printpdf = "0.7"
lopdf = { version = "0.34", default-features = false, features = ["nom_parser"] }
---
// printpdf 0.7 generation plus lopdf 0.34 parsing of one PDF: generate,
// byte-anchor, parse, re-save, parse again. Built-in Helvetica/Helvetica-Bold
// (no embedded fonts), two pages (A4 portrait + A4 landscape), six text lines
// (including the PDF literal-string escape boundaries ( ) \ % $), three
// rectangles (Fill, FillStroke and Stroke PaintMode), one open and one closed
// polyline, and Rgb fill/outline colors with a line width.
// lopdf: load_mem, version, get_pages, a get_and_decode_page_content operator
// histogram (BTreeMap order), extract_text compared against the source after
// whitespace normalization, an objects variant walk with counts, a save_to
// roundtrip re-extract, and three error paths (junk file, truncation and an
// out-of-range page number).
// Determinism: printpdf's document_id and the trailer instance_id both come
// from the fixed-seed global xorshift in its utils.rs (RAND_SEED=2100, SeqCst
// fetch_add), so sequential single-threaded calls repeat the same sequence.
// PdfMetadata::new would stamp OffsetDateTime::now_utc() (wall clock) into the
// Info dictionary, so the driver pins all three dates to a fixed epoch.
// printpdf prunes and compresses streams only in release builds, and both
// harnesses run debug (cargo run without --release), so both sides see an
// uncompressed stream.
use std::collections::BTreeMap;

use printpdf::path::PaintMode;
use printpdf::{BuiltinFont, Color, Line, Mm, PdfDocument, Point, Rect, Rgb};

/// Inline FNV-1a, used to anchor the binary content without printing raw bytes.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Whitespace normalization, applied identically on extraction and source:
/// extract_text folds each ET into '\n' and spaces TJ array elements.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Top-level lopdf Object variant classification, used by the object walk tally.
fn class(o: &lopdf::Object) -> &'static str {
    use lopdf::Object::*;
    match o {
        Null => "null",
        Boolean(_) => "bool",
        Integer(_) => "int",
        Real(_) => "real",
        Name(_) => "name",
        String(..) => "string",
        Array(_) => "array",
        Dictionary(_) => "dict",
        Stream(_) => "stream",
        Reference(_) => "ref",
    }
}

fn main() {
    // ---- (1) printpdf generation: 2 pages / 6 text lines / 3 rects / 2 lines ----
    let fixed = printpdf::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    let (doc, page1, layer1) =
        PdfDocument::new("mirvm corpus pdf_pair", Mm(210.0), Mm(297.0), "Layer 1");
    let doc = doc
        .with_title("mirvm corpus pdf_pair")
        .with_creation_date(fixed)
        .with_metadata_date(fixed)
        .with_mod_date(fixed);

    let helv = doc.add_builtin_font(BuiltinFont::Helvetica).unwrap();
    let helv_bold = doc.add_builtin_font(BuiltinFont::HelveticaBold).unwrap();

    let lines_p1 = [
        "mirvm corpus pdf_pair: page one",
        "Helvetica 12pt second line",
        "literal escapes: (paren) back\\slash end",
        "96.5% ascii-safe symbols $ & # @ !",
    ];
    let lines_p2 = [
        "page two landscape Helvetica-Bold",
        "second page (escaped) chars: \\ ( ) done",
    ];

    {
        let layer = doc.get_page(page1).get_layer(layer1);
        layer.use_text(lines_p1[0], 24.0, Mm(15.0), Mm(275.0), &helv);
        layer.use_text(lines_p1[1], 12.0, Mm(15.0), Mm(262.0), &helv);
        layer.use_text(lines_p1[2], 10.5, Mm(15.0), Mm(249.0), &helv);
        layer.use_text(lines_p1[3], 9.0, Mm(15.0), Mm(236.0), &helv);

        layer.set_fill_color(Color::Rgb(Rgb::new(0.2, 0.7, 0.1, None)));
        layer.add_rect(Rect::new(Mm(15.0), Mm(210.0), Mm(65.0), Mm(230.0)));
        layer.set_outline_color(Color::Rgb(Rgb::new(0.9, 0.1, 0.1, None)));
        layer.set_outline_thickness(1.5);
        layer.add_rect(
            Rect::new(Mm(75.0), Mm(210.0), Mm(125.0), Mm(230.0)).with_mode(PaintMode::FillStroke),
        );
        layer.add_rect(
            Rect::new(Mm(135.0), Mm(210.0), Mm(185.0), Mm(230.0)).with_mode(PaintMode::Stroke),
        );

        layer.add_line(Line {
            points: vec![
                (Point::new(Mm(15.0), Mm(190.0)), false),
                (Point::new(Mm(105.0), Mm(190.0)), false),
                (Point::new(Mm(105.0), Mm(200.0)), false),
            ],
            is_closed: false,
        });
        layer.add_line(Line {
            points: vec![
                (Point::new(Mm(120.0), Mm(190.0)), false),
                (Point::new(Mm(150.0), Mm(200.0)), false),
                (Point::new(Mm(180.0), Mm(190.0)), false),
            ],
            is_closed: true,
        });
    }

    let (page2, layer2) = doc.add_page(Mm(297.0), Mm(210.0), "Layer 2");
    {
        let layer = doc.get_page(page2).get_layer(layer2);
        layer.use_text(lines_p2[0], 16.0, Mm(20.0), Mm(180.0), &helv_bold);
        layer.use_text(lines_p2[1], 11.0, Mm(20.0), Mm(165.0), &helv);
        layer.set_fill_color(Color::Rgb(Rgb::new(0.1, 0.2, 0.8, None)));
        layer.add_rect(Rect::new(Mm(20.0), Mm(140.0), Mm(80.0), Mm(155.0)));
    }

    let bytes = doc.save_to_bytes().unwrap();
    println!("generated len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));

    // ---- (2) lopdf parse: version / pages / per-page operator histogram / text ----
    let parsed = lopdf::Document::load_mem(&bytes).unwrap();
    println!("version = {}", parsed.version);
    let pages = parsed.get_pages();
    println!("pages = {}", pages.len());
    for (num, id) in &pages {
        let content = parsed.get_and_decode_page_content(*id).unwrap();
        let mut hist: BTreeMap<&str, usize> = BTreeMap::new();
        for op in &content.operations {
            *hist.entry(op.operator.as_str()).or_default() += 1;
        }
        print!("page {num} ({},{}) ops = {}:", id.0, id.1, content.operations.len());
        for (k, v) in &hist {
            print!(" {k}x{v}");
        }
        println!();
    }

    let expected = normalize(&[lines_p1.join(" "), lines_p2.join(" ")].join(" "));
    let extracted = parsed.extract_text(&[1, 2]).unwrap();
    let norm = normalize(&extracted);
    println!("extracted = {norm:?}");
    println!("extracted-ok = {}", norm == expected);

    // ---- (3) object walk counts (BTreeMap order) ----
    let mut hist: BTreeMap<&str, usize> = BTreeMap::new();
    for obj in parsed.objects.values() {
        *hist.entry(class(obj)).or_default() += 1;
    }
    println!("objects = {}", parsed.objects.len());
    for (k, v) in &hist {
        println!("  {k} = {v}");
    }

    // ---- (4) lopdf re-save roundtrip: byte anchor + reload and re-extract ----
    let mut redoc = lopdf::Document::load_mem(&bytes).unwrap();
    let mut resaved = Vec::new();
    redoc.save_to(&mut resaved).unwrap();
    println!("resaved len={} fnv={:016x}", resaved.len(), fnv1a(&resaved));
    let reparsed = lopdf::Document::load_mem(&resaved).unwrap();
    let reextract = reparsed.extract_text(&[1, 2]).unwrap();
    println!("resave-extract-ok = {}", normalize(&reextract) == expected);

    // ---- (5) error paths: junk file / truncation / page number out of range ----
    match lopdf::Document::load_mem(b"mirvm: definitely not a pdf payload") {
        Ok(_) => println!("junk load unexpectedly ok"),
        Err(e) => println!("junk load err = {e}"),
    }
    match lopdf::Document::load_mem(&bytes[..bytes.len() / 2]) {
        Ok(_) => println!("truncated load unexpectedly ok"),
        Err(e) => println!("truncated load err = {e}"),
    }
    match parsed.extract_text(&[99]) {
        Ok(_) => println!("page-99 extract unexpectedly ok"),
        Err(e) => println!("page-99 extract err = {e}"),
    }
}
