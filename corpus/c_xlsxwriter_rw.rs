#!/usr/bin/env mirvm
---
[dependencies]
rust_xlsxwriter = "=0.96.0"
calamine = { version = "=0.36.0", features = ["picture"] }
---
// c_xlsxwriter_rw -- an in-memory write/read closure differential: rust_xlsxwriter 0.96
// writes and calamine 0.36 reads. Unlike the earlier c_calamine_xlsx (the 0.80/0.26
// four-sheet closure), this driver targets the current stable line and exercises the new
// API surface between 0.80->0.96 and 0.26->0.36 (tables/autofilter/freeze panes/hidden
// sheets/hyperlinks/rich text/array formulas/future-function escaping/boolean and error
// cached results/embedded images/notes/defined names/sheets_metadata/pictures), and the
// read side adds per-cell comparison beyond the full dump (a write-side expected-value
// model checked against each cell calamine reads back).
//
// Version pins (compatible-combination evidence):
//   * rust_xlsxwriter =0.96.0: the newest stable on crates.io, rust-version 1.83 (well
//     under this machine's nightly-2026-07-02); default features are empty (ryu/zmij/
//     chrono/jiff/serde never enter), and the only hard dependency is zip ^7.2 with
//     default-features=false and features=["deflate"].
//   * calamine =0.36.0: the newest stable on crates.io, rust-version 1.88; default
//     features are empty, and "picture" is enabled explicitly (Reader::pictures is
//     feature-gated) to cover the image readback surface; its hard dependencies are
//     zip ^8.6 (default-features=false + features=["deflate"]) plus quick-xml/atoi_simd/
//     fast-float2/encoding_rs/codepage, all pure Rust.
//   * Two zip major versions (xlsxwriter->zip7 / calamine->zip8) may coexist legally:
//     cargo allows multiple semver versions, and both zips' deflate features resolve
//     within their own major to flate2's zlib-rs backend (zip 7.2/8.6 deflate = zopfli +
//     deflate-flate2-zlib-rs), so the whole graph shares one flate2/libz-rs-sys instance
//     with no C dependency and no external build.rs tool.
//   * Measured closure of 35 packages (one Cargo.lock shared by all three dimensions):
//     zip 7.2.0 + zip 8.6.0, crc32fast 1.5.0 (entry CRC; any single block >=128B takes
//     the pclmulqdq hardware path, which is built in -- this driver shows no trap),
//     flate2 1.1.9 + zlib-rs 0.6.6 (the deflate backend) + zopfli 0.8.3 + miniz_oxide
//     0.8.9/simd-adler32 0.3.10 (the psad.bw family is built in) and quick-xml 0.41/
//     atoi_simd 0.18/fast-float2 0.2 and so on -- all pure Rust.
//
// Determinism:
//   * DocProperties::set_creation_datetime pins dcterms:created=2031-01-02T03:04:05Z
//     (otherwise utc_now leaks into core.xml and the whole archive is unreproducible --
//     the same discipline as c_calamine_xlsx).
//   * zip has default-features=false and the time feature off, so every entry mtime stays
//     1980-01-01 00:00:00 and the zip container bytes are reproducible.
//   * deflate (flate2 zlib-rs) with the same library version, input and level produces
//     identical bytes; all three dimensions share the Cargo.toml (B reuses the script dir
//     A materialized), so dependency resolution matches.
//   * Every cell value/formula/format/image (the 78B inline PNG constant) and note text is a
//     compile-time constant; no OS randomness, no wall clock, no HashMap iteration order
//     printed (defined_names follow workbook.xml file order) and no raw addresses; floats
//     and date serials are anchored as to_bits hex.
//   * stderr is empty: calamine's log crate stays silent with no logger, and
//     rust_xlsxwriter logs nothing.
//
// Failure triage reference (follow this order if it ever goes red):
//   1) Runtime SIMD dispatch: crc32fast's pclmulqdq (any single entry CRC block >=128B) and
//      the flate2/zlib-rs plus simd-adler32 avx2 maddubs/madd/psad.bw family are all on the
//      built-in list, so this driver shows no trap; a new unbuilt sibling would blow up here.
//      calamine's atoi_simd (SSE4.1) and quick-xml parsing have green precedents
//      (c_calamine_xlsx/c_zip_arch/c_crc32fast/c_zopfli_deep).
//   3) Semantic data points (not red): calamine's readback type follows the XML cell's t=
//      plus numFmt. Measured 0.26->0.36 changes (identical on native and mirvm): a) numeric
//      worksheet cells always read as Float (format_excel_f64_ref takes only f64; Data::Int
//      is left to PivotCache), the 0.26 "Int when no decimal point" rule gone; b) array
//      formula non-anchor cells materialize as Float(0.0); c) a merged range with no XML
//      cell in its trailing rows ends at the last populated row (out-of-range None). Date
//      formats read as DateTime, hyperlinks as display text, rich text as plain text, and
//      empty strings are not written, so the expected values all compare ok=true.
//
// Coverage -- write side (rust_xlsxwriter 0.96, seven sheets):
//   1) Types: escaped strings/CJK+emoji/empty string (the silently-skipped anchor)/integer
//      and decimal 1e300/2.5e-10/large integers/boolean/two date forms/blank formatted
//      cell/merged range/column width/row height/tab color; 2) Calc: the four cached
//      formula result kinds (Int/str/b/bool/error #DIV/0!) + an array formula (C6:E6 anchor
//      cached) + the _xlfn escaping anchor for =IFS; 3) Grid: a deterministic 40x6 mixed
//      Int/Float block (a real deflate payload) + autofilter + freeze panes; 4) Table:
//      add_table named Sales (Medium9 + three CJK/emoji-header columns); 5) Hidden: a
//      set_hidden sheet stays readable; 6) Media: insert_image (78B PNG + alt text)/
//      insert_note/write_url (escaping & + display text + tip)/write_rich_string runs;
//      7) Empty: the empty-sheet edge; workbook level: six DocProperties fields + a global
//      and a sheet-local define_name.
// Coverage -- read side (calamine 0.36): sheet_names/sheets_metadata/defined_names/
//     per-sheet Range metadata + a row-major full-cell dump (nine data_tag variants,
//     floats and dates as to_bits)/worksheet_formula text/merge_cells_by_sheet_name (+
//     _by_id)/pictures byte-fnv closure/out-of-range get_value/empty sheet/missing sheet
//     and junk-bytes errors/per-cell comparison (41 expected values, zero mismatches).
//
// Three-way rerun:
//   A: target/release/mirvm run corpus/c_xlsxwriter_rw.rs
//   B: d=$(grep -l 'name = "c_xlsxwriter_rw"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_xlsxwriter_rw.rs
//
// Measured all-green baseline: A/B/C agree byte-for-byte on 349 stdout lines (xlsx
// len=14236 fnv=01247f537aafa3a8; the seven-sheet full-cell dump + the formula surface
// with the _xlfn.IFS escape and array-formula ref anchors + merged/pictures (pic ext=png
// len=78 fnv=3ab250953e4202cd) + 41 per-cell comparisons with zero mismatches), stderr
// empty (0 bytes) and exit 0 everywhere; A/A2 and B/B2 each agree byte-for-byte on a
// rerun (the pinned document creation time plus zip having no time feature make the whole
// archive byte-reproducible). Timings: A cold (first build of the 35-crate closure) about
// 1min, 1.6s warm; B first 11.3s, 0.1s warm; C (JIT=1) 1.5-2.1s. No FRONTIER signal.

use calamine::{Data, Reader, Xlsx};
use rust_xlsxwriter::{
    Color, DocProperties, ExcelDateTime, Format, FormatAlign, Formula, Image, Note, Table,
    TableColumn, TableStyle, Url, Workbook,
};
use std::io::Cursor;

/// A fixed 78-byte 3x2 truecolor PNG (pixels R,G,B / Y,C,M, zlib level 9 fixed).
const TINY_PNG: [u8; 78] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0x12, 0x16, 0xf1,
    0x4d, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xc0,
    0x00, 0xc1, 0xff, 0x81, 0xd4, 0x7f, 0x20, 0xf1, 0x1f, 0x00, 0x4a, 0xc9, 0x08, 0xf8, 0x5f, 0xb3,
    0x7d, 0x75, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Deterministic labels for the nine Data variants; floats and date serials print by bit.
fn data_tag(d: &Data) -> String {
    match d {
        Data::Int(i) => format!("Int({i})"),
        Data::Float(f) => format!("Float(0x{:016x})", f.to_bits()),
        Data::String(s) => format!("Str({s})"),
        Data::Bool(b) => format!("Bool({b})"),
        Data::DateTime(dt) => format!("DateTime(0x{:016x})", dt.as_f64().to_bits()),
        Data::DateTimeIso(s) => format!("DateTimeIso({s})"),
        Data::DurationIso(s) => format!("DurationIso({s})"),
        Data::Error(e) => format!("Error({e:?})"),
        Data::Empty => "Empty".to_string(),
    }
}

/// Per-cell expected values, encoded to calamine's readback semantics (see triage note 3).
/// There is no Int variant: calamine 0.36's xlsx worksheet numeric path always goes through
/// f64 (cells_reader.rs format_excel_f64_ref) and Data::Int is left to the PivotCache
/// surface, so an integer number reads back as Float.
enum Exp {
    F64Bits(u64),
    Str(&'static str),
    Bool(bool),
    DateBits(u64),
    ErrTag(&'static str),
    Empty,
}

impl Exp {
    fn matches(&self, d: &Data) -> bool {
        match (self, d) {
            (Exp::F64Bits(b), Data::Float(f)) => *b == f.to_bits(),
            (Exp::Str(s), Data::String(d)) => s == &d.as_str(),
            (Exp::Bool(b), Data::Bool(d)) => b == d,
            (Exp::DateBits(b), Data::DateTime(dt)) => *b == dt.as_f64().to_bits(),
            (Exp::ErrTag(t), Data::Error(e)) => *t == format!("{e:?}"),
            (Exp::Empty, Data::Empty) => true,
            _ => false,
        }
    }

    fn show(&self) -> String {
        match self {
            Exp::F64Bits(b) => format!("Float(0x{b:016x})"),
            Exp::Str(s) => format!("Str({s})"),
            Exp::Bool(b) => format!("Bool({b})"),
            Exp::DateBits(b) => format!("DateTime(0x{b:016x})"),
            Exp::ErrTag(t) => format!("Error({t})"),
            Exp::Empty => "Empty".to_string(),
        }
    }
}

/// A (sheet, row, col, expected value) tuple; registered in the same order as the write side.
type Expect = (&'static str, u32, u16, Exp);

fn build_xlsx() -> (Vec<u8>, Vec<Expect>) {
    let mut exp: Vec<Expect> = Vec::new();
    let mut wb = Workbook::new();

    // Pin the document creation time; every other property field is a fixed value too.
    let created = ExcelDateTime::from_ymd(2031, 1, 2)
        .unwrap()
        .and_hms(3, 4, 5)
        .unwrap();
    wb.set_properties(
        &DocProperties::new()
            .set_title("mirvm xlsxwriter_rw")
            .set_subject("三维差分 subject")
            .set_author("mirvm corpus")
            .set_manager("mgr 钉")
            .set_company("mirvm")
            .set_creation_datetime(&created),
    );
    wb.define_name("ProjectConst", "=42").unwrap();
    wb.define_name("Calc!SubTotal", "=Calc!$B$1").unwrap();

    // ---- Sheet 1 "Types": type matrix + formats/merge/blank/column widths/row heights/tab color ----
    let ws = wb.add_worksheet();
    ws.set_name("Types").unwrap();
    ws.set_tab_color(Color::RGB(0x00C0_00CC));
    ws.write_string(0, 0, "esc <&> \"' 混合").unwrap();
    exp.push(("Types", 0, 0, Exp::Str("esc <&> \"' 混合")));
    ws.write_string(0, 1, "汉字 🦀 CJK").unwrap();
    exp.push(("Types", 0, 1, Exp::Str("汉字 🦀 CJK")));
    // Unformatted empty strings are silently skipped by rust_xlsxwriter and read back as Empty.
    ws.write_string(0, 2, "").unwrap();
    exp.push(("Types", 0, 2, Exp::Empty));
    ws.write_number(1, 0, 42.0).unwrap();
    exp.push(("Types", 1, 0, Exp::F64Bits((42.0f64).to_bits())));
    ws.write_number(1, 1, -3.5).unwrap();
    exp.push(("Types", 1, 1, Exp::F64Bits((-3.5f64).to_bits())));
    ws.write_number(1, 2, 1e300).unwrap();
    exp.push(("Types", 1, 2, Exp::F64Bits((1e300f64).to_bits())));
    ws.write_number(1, 3, 2.5e-10).unwrap();
    exp.push(("Types", 1, 3, Exp::F64Bits((2.5e-10f64).to_bits())));
    ws.write_number(1, 4, 123456789012.0).unwrap();
    exp.push(("Types", 1, 4, Exp::F64Bits((123456789012.0f64).to_bits())));
    ws.write_boolean(2, 0, true).unwrap();
    exp.push(("Types", 2, 0, Exp::Bool(true)));
    ws.write_boolean(2, 1, false).unwrap();
    exp.push(("Types", 2, 1, Exp::Bool(false)));
    // Two date forms: an ExcelDateTime object and an explicit serial value the format drives.
    let date_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    let dt = ExcelDateTime::from_ymd(2031, 1, 2)
        .unwrap()
        .and_hms(3, 4, 5)
        .unwrap();
    ws.write_datetime_with_format(3, 0, &dt, &date_fmt).unwrap();
    exp.push(("Types", 3, 0, Exp::DateBits(dt.to_excel().to_bits())));
    let serial_fmt = Format::new().set_num_format("dd-mmm-yy");
    ws.write_number_with_format(3, 1, 45000.75, &serial_fmt).unwrap();
    exp.push(("Types", 3, 1, Exp::DateBits((45000.75f64).to_bits())));
    let blank_fmt = Format::new().set_num_format("0.00%");
    ws.write_blank(4, 0, &blank_fmt).unwrap();
    exp.push(("Types", 4, 0, Exp::Empty));
    let center = Format::new().set_align(FormatAlign::Center);
    ws.merge_range(5, 0, 6, 1, "合并 merged", &center).unwrap();
    exp.push(("Types", 5, 0, Exp::Str("合并 merged")));
    exp.push(("Types", 5, 1, Exp::Empty));
    // The merged range's trailing rows (6,0)/(6,1) have no XML cell, so calamine's Range ends at
    // (5,4) and out-of-range get_value yields None -- the shape is anchored by the merged rows.
    ws.set_column_width(0, 24).unwrap();
    ws.set_column_width_pixels(1, 120).unwrap();
    ws.set_row_height(3, 30.0).unwrap();

    // ---- Sheet 2 "Calc": four cached formula result kinds + array formula + future-function escaping ----
    let ws = wb.add_worksheet();
    ws.set_name("Calc").unwrap();
    ws.write_number(0, 0, 1.0).unwrap();
    exp.push(("Calc", 0, 0, Exp::F64Bits((1.0f64).to_bits())));
    ws.write_number(1, 0, 2.0).unwrap();
    exp.push(("Calc", 1, 0, Exp::F64Bits((2.0f64).to_bits())));
    ws.write_number(2, 0, 3.0).unwrap();
    exp.push(("Calc", 2, 0, Exp::F64Bits((3.0f64).to_bits())));
    ws.write_formula(0, 1, &Formula::new("=SUM(A1:A3)").set_result("6"))
        .unwrap();
    exp.push(("Calc", 0, 1, Exp::F64Bits((6.0f64).to_bits())));
    ws.write_formula(1, 1, &Formula::new("=A1*A2+A3").set_result("5"))
        .unwrap();
    exp.push(("Calc", 1, 1, Exp::F64Bits((5.0f64).to_bits())));
    ws.write_formula(
        2,
        1,
        &Formula::new("=CONCATENATE(\"ab\",\"cd\")").set_result("abcd"),
    )
    .unwrap();
    exp.push(("Calc", 2, 1, Exp::Str("abcd")));
    ws.write_formula(
        3,
        1,
        &Formula::new("=IF(A1>0,TRUE,FALSE)").set_result("TRUE"),
    )
    .unwrap();
    exp.push(("Calc", 3, 1, Exp::Bool(true)));
    ws.write_formula(4, 1, &Formula::new("=1/0").set_result("#DIV/0!"))
        .unwrap();
    exp.push(("Calc", 4, 1, Exp::ErrTag("Div0")));
    // Array formula C6:E6: the cached value lands only on the anchor cell; the two non-anchor cells
    // materialize as Float(0.0) under calamine 0.36 (the 0.26 behavior is a data point in the header).
    ws.write_array_formula(5, 0, 5, 2, &Formula::new("=SUM(A1:A3)").set_result("6"))
        .unwrap();
    exp.push(("Calc", 5, 0, Exp::F64Bits((6.0f64).to_bits())));
    exp.push(("Calc", 5, 1, Exp::F64Bits((0.0f64).to_bits())));
    exp.push(("Calc", 5, 2, Exp::F64Bits((0.0f64).to_bits())));
    // Future function: the write side escapes it to _xlfn.IFS (anchored in the formula text).
    ws.write_formula(6, 1, &Formula::new("=IFS(A1>0,\"pos\")").set_result("pos"))
        .unwrap();
    exp.push(("Calc", 6, 1, Exp::Str("pos")));

    // ---- Sheet 3 "Grid": 40x6 mixed Int/Float numeric block + autofilter + freeze ----
    let ws = wb.add_worksheet();
    ws.set_name("Grid").unwrap();
    for r in 0..40u32 {
        for c in 0..6u16 {
            let v = (r as u64 * 6 + c as u64) as f64 * 0.5 - 60.0;
            ws.write_number(r, c, v).unwrap();
        }
    }
    ws.autofilter(0, 0, 39, 5).unwrap();
    ws.set_freeze_panes(1, 0).unwrap();
    exp.push(("Grid", 0, 0, Exp::F64Bits((-60.0f64).to_bits())));
    exp.push(("Grid", 0, 1, Exp::F64Bits((-59.5f64).to_bits())));
    exp.push(("Grid", 39, 4, Exp::F64Bits((59.0f64).to_bits())));
    exp.push(("Grid", 39, 5, Exp::F64Bits((59.5f64).to_bits())));

    // ---- Sheet 4 "Table": named table + style + header columns ----
    let ws = wb.add_worksheet();
    ws.set_name("Table").unwrap();
    let cols = [
        TableColumn::new().set_header("item"),
        TableColumn::new().set_header("qty"),
        TableColumn::new().set_header("price"),
    ];
    ws.add_table(
        1,
        1,
        4,
        3,
        &Table::new()
            .set_name("Sales")
            .set_style(TableStyle::Medium9)
            .set_columns(&cols),
    )
    .unwrap();
    let items = ["apple 苹果", "banana", "cherry 🍒"];
    let qty = [3.0, 5.0, 8.0];
    let price = [1.5, 0.75, 2.25];
    for (i, ((it, q), p)) in items.iter().zip(qty).zip(price).enumerate() {
        let r = 2 + i as u32;
        ws.write_string(r, 1, *it).unwrap();
        ws.write_number(r, 2, q).unwrap();
        ws.write_number(r, 3, p).unwrap();
    }
    exp.push(("Table", 1, 1, Exp::Str("item")));
    exp.push(("Table", 1, 3, Exp::Str("price")));
    exp.push(("Table", 2, 1, Exp::Str("apple 苹果")));
    exp.push(("Table", 2, 2, Exp::F64Bits((3.0f64).to_bits())));
    exp.push(("Table", 3, 3, Exp::F64Bits((0.75f64).to_bits())));
    exp.push(("Table", 4, 2, Exp::F64Bits((8.0f64).to_bits())));

    // ---- Sheet 5 "Hidden": a hidden sheet is still readable ----
    let ws = wb.add_worksheet();
    ws.set_name("Hidden").unwrap();
    ws.set_hidden(true);
    ws.write_string(0, 0, "secret 隐藏").unwrap();
    exp.push(("Hidden", 0, 0, Exp::Str("secret 隐藏")));
    ws.write_number(0, 1, 7.0).unwrap();
    exp.push(("Hidden", 0, 1, Exp::F64Bits((7.0f64).to_bits())));

    // ---- Sheet 6 "Media": image/note/hyperlink/rich text ----
    let ws = wb.add_worksheet();
    ws.set_name("Media").unwrap();
    let img = Image::new_from_buffer(&TINY_PNG)
        .unwrap()
        .set_alt_text("定值 3x2 PNG");
    ws.insert_image(1, 1, &img).unwrap();
    let note = Note::new("note 定值 ✓").set_author("mirvm");
    ws.insert_note(0, 2, &note).unwrap();
    let url = Url::new("https://example.com/x?a=1&b=2")
        .set_text("链接 text")
        .set_tip("tip 提示");
    ws.write_url(0, 0, &url).unwrap();
    exp.push(("Media", 0, 0, Exp::Str("链接 text")));
    let bold = Format::new().set_bold();
    let red = Format::new().set_font_color(Color::RGB(0x00FF_0000));
    ws.write_rich_string(1, 0, &[(&bold, "半"), (&red, "rich✗")])
        .unwrap();
    exp.push(("Media", 1, 0, Exp::Str("半rich✗")));

    // ---- Sheet 7 "Empty": the empty-sheet edge ----
    let ws = wb.add_worksheet();
    ws.set_name("Empty").unwrap();

    (wb.save_to_buffer().unwrap(), exp)
}

fn main() {
    // ===== write side =====
    let (bytes, exp) = build_xlsx();
    println!("xlsx len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));

    // ===== read side =====
    let mut xls: Xlsx<_> = Xlsx::new(Cursor::new(bytes)).unwrap();

    // ① sheet name list + metadata (type/visibility, anchored by the Hidden sheet)
    let names = xls.sheet_names();
    println!("sheets = {names:?}");
    for s in xls.sheets_metadata() {
        println!("meta {:?} typ={:?} visible={:?}", s.name, s.typ, s.visible);
    }

    // ② defined names (workbook.xml file order = insertion order)
    let dn = xls.defined_names();
    println!("defined_names = {dn:?}");

    // ③ per sheet: Range metadata + row-major per-cell type+value
    for name in &names {
        let range = xls.worksheet_range(name).unwrap();
        println!(
            "sheet {name}: start={:?} end={:?} w={} h={} empty={}",
            range.start(),
            range.end(),
            range.width(),
            range.height(),
            range.is_empty()
        );
        for (r, c, cell) in range.cells() {
            println!("  ({r},{c}) {}", data_tag(cell));
        }
    }

    // ④ formula text surface (the _xlfn escaping and array-formula refs anchor here; cached values printed in ③)
    let frange = xls.worksheet_formula("Calc").unwrap();
    println!("formula sheet: empty = {}", frange.is_empty());
    for (r, c, f) in frange.cells() {
        if !f.is_empty() {
            println!("  f({r},{c}) {f}");
        }
    }

    // ⑤ merged ranges (0.36's recommended API: anchored both by name and by id)
    let m_types = xls.merge_cells_by_sheet_name("Types").unwrap();
    println!("merged(Types) = {}", m_types.len());
    for d in &m_types {
        println!("merged Types {:?}..{:?}", d.start, d.end);
    }
    println!(
        "merged(Calc) = {} merged_by_id(0) = {}",
        xls.merge_cells_by_sheet_name("Calc").unwrap().len(),
        xls.merge_cells_by_sheet_id(0).unwrap().len()
    );

    // ⑥ picture readback closure (the 78B fixed PNG from the write side)
    match xls.pictures() {
        Some(pics) => {
            println!("pictures = {}", pics.len());
            for (ext, data) in &pics {
                println!("pic ext={ext} len={} fnv={:016x}", data.len(), fnv1a(data));
            }
        }
        None => println!("pictures = none"),
    }

    // ⑦ per-cell comparison: write-side expectation vs readback (absolute get_value coordinates)
    let mut total = 0usize;
    let mut bad = 0usize;
    for name in &names {
        let items: Vec<&Expect> = exp.iter().filter(|(s, ..)| s == name).collect();
        if items.is_empty() {
            continue;
        }
        let range = xls.worksheet_range(name).unwrap();
        let mut mism = 0usize;
        for (sheet, r, c, e) in items {
            total += 1;
            let got = range.get_value((*r, u32::from(*c)));
            let ok = matches!(got, Some(d) if e.matches(d));
            if !ok {
                mism += 1;
                bad += 1;
                let g = got.map(data_tag).unwrap_or_else(|| "MISSING".to_string());
                println!("  mismatch {sheet}({r},{c}) expect={} got={g}", e.show());
            }
        }
        let checked = exp.iter().filter(|(s, ..)| s == name).count();
        println!("cmp {name}: checked={checked} mismatch={mism} ok={}", mism == 0);
    }
    println!("cmp total: checked={total} mismatch={bad} ok={}", bad == 0);

    // ⑧ edges: out of range / empty sheet / missing sheet / junk bytes
    let range = xls.worksheet_range("Types").unwrap();
    println!("inbounds get_value((1,1)) = {}", data_tag(range.get_value((1, 1)).unwrap()));
    println!(
        "oob get_value((999,999)) is_some = {}",
        range.get_value((999, 999)).is_some()
    );
    let empty = xls.worksheet_range("Empty").unwrap();
    println!(
        "empty sheet: empty={} w={} h={} cells={} get(0,0)={}",
        empty.is_empty(),
        empty.width(),
        empty.height(),
        empty.cells().count(),
        empty.get((0, 0)).is_some()
    );
    match xls.worksheet_range("NoSuch") {
        Ok(_) => println!("missing sheet: unexpected ok"),
        Err(e) => println!("missing sheet err: {e}"),
    }
    match Xlsx::new(Cursor::new(b"definitely not an xlsx".to_vec())) {
        Ok(_) => println!("junk xlsx: unexpected ok"),
        Err(e) => println!("junk xlsx err: {e}"),
    }
}
