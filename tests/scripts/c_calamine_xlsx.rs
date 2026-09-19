#!/usr/bin/env mirvm
---
[dependencies]
rust_xlsxwriter = "0.80"
calamine = "0.26"
---
// rust_xlsxwriter 0.80 + calamine 0.26 self-contained loop: build an xlsx in an in-memory
// Vec<u8> and read it back through a Cursor.
// Write side (rust_xlsxwriter): several sheets (Data/Calc/Series/Empty); numbers / text
// (CJK + emoji) / booleans / formulas (including a cached string result, t="str") / dates
// (fixed value plus an explicit date format) / merged cells / column widths (character-unit
// and pixel API); DocProperties::set_creation_datetime pins dcterms:created and zip has no
// time feature (entry mtime is always 1980-01-01), so the archive is byte-reproducible and
// its len + fnv are the printed anchor; native output is byte-identical across runs.
// Read side (calamine): sheet_names; per sheet worksheet_range (Range metadata plus cells()
// printed row-major as type+value, float and date serial values pinned via to_bits);
// worksheet_formula (formula text); load_merged_regions and merged_regions(_by_sheet);
// boundaries (empty-sheet metadata, out-of-range get / get_value -> None, missing sheet).
// Deterministic: output is only Vec order, counts, bit patterns and boolean assertions --
// no time, addresses or HashMap order.
//
// Known limitation: both the default and the JIT dimension TRAP at the same address with exit
// 70 and empty stdout -- mirvm[m4-engine]: TRAP: foreign `llvm.x86.pclmulqdq` (LLVM-internal,
// must be built in on demand), in pclmulqdq::__mm_clmulepi64_si128 called from crc32fast.
// Mechanism: rust_xlsxwriter and calamine both go through zip 2.x -> crc32fast for an entry's
// CRC32; with std, crc32fast probes cpuid at runtime (the guest passes host feature bits
// through) and picks the pclmulqdq path, running _mm_clmulepi64_si128 as soon as one update
// reaches 128 B. The psad.bw (simd-adler32) path is not hit, because zip's deflate is flate2
// raw deflate (as c_zip_arch shows); crc32fast/pclmulqdq is the real blocker. No workaround
// holds: stored compression still computes CRC32; write and read block sizes both live inside
// the crates (the writer emits each XML part in one buffer, calamine wraps the Crc32Reader
// in an 8 KB BufReader), so neither can be chunked at 64 B as c_zip_arch does;
// forcing crc32fast's portable baseline needs its no_std compile-time branch, but zip's
// default feature union always pulls in std, so a downstream driver cannot select it.
use calamine::{Data, Reader, Xlsx};
use rust_xlsxwriter::{DocProperties, ExcelDateTime, Format, Formula, Workbook};
use std::io::Cursor;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Deterministic label for each Data variant; float and date serial values print as bits.
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

fn build_xlsx() -> Vec<u8> {
    let mut wb = Workbook::new();
    // Pin the document creation time; otherwise core.xml embeds utc_now and the archive is not reproducible.
    let created = ExcelDateTime::from_ymd(2024, 3, 14)
        .unwrap()
        .and_hms(15, 9, 26)
        .unwrap();
    wb.set_properties(&DocProperties::new().set_creation_datetime(&created));

    // ---- Sheet 1 "Data": mixed types + merged cells + column widths ----
    let ws = wb.add_worksheet();
    ws.set_name("Data").unwrap();
    ws.write_string(0, 0, "hello").unwrap();
    ws.write_string(0, 1, "汉字 🦀 混合").unwrap();
    ws.write_number(1, 0, 42.0).unwrap();
    ws.write_number(1, 1, -3.5).unwrap();
    ws.write_number(1, 2, 1e300).unwrap();
    ws.write_boolean(2, 0, true).unwrap();
    ws.write_boolean(2, 1, false).unwrap();
    // Fixed date value plus an explicit date number format (without one calamine sees only a Float serial).
    let date_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    let dt = ExcelDateTime::from_ymd(2024, 3, 14)
        .unwrap()
        .and_hms(15, 9, 26)
        .unwrap();
    ws.write_datetime_with_format(3, 0, &dt, &date_fmt).unwrap();
    ws.merge_range(5, 0, 6, 2, "merged 合并", &Format::new()).unwrap();
    ws.set_column_width(0, 24).unwrap();
    ws.set_column_width_pixels(1, 120).unwrap();

    // ---- Sheet 2 "Calc": formulas + cached results (numeric and string) ----
    let ws = wb.add_worksheet();
    ws.set_name("Calc").unwrap();
    ws.write_number(0, 0, 1.0).unwrap();
    ws.write_number(1, 0, 2.0).unwrap();
    ws.write_number(2, 0, 3.0).unwrap();
    ws.write_formula(0, 1, &Formula::new("=SUM(A1:A3)").set_result("6"))
        .unwrap();
    ws.write_formula(1, 1, &Formula::new("=A1*A2+A3").set_result("5"))
        .unwrap();
    // A cached string result -> a t="str" cell.
    ws.write_formula(
        2,
        1,
        &Formula::new("=CONCATENATE(\"ab\",\"cd\")").set_result("abcd"),
    )
    .unwrap();

    // ---- Sheet 3 "Series": a deterministic numeric block (50×4) to give deflate real payload ----
    let ws = wb.add_worksheet();
    ws.set_name("Series").unwrap();
    for r in 0..50u32 {
        for c in 0..4u16 {
            let v = (r as u64 * 4 + c as u64) as f64 * 0.25 - 12.5;
            ws.write_number(r, c, v).unwrap();
        }
    }

    // ---- Sheet 4 "Empty": the empty-sheet boundary ----
    let ws = wb.add_worksheet();
    ws.set_name("Empty").unwrap();

    wb.save_to_buffer().unwrap()
}

fn main() {
    // ===== Write side =====
    let bytes = build_xlsx();
    println!("xlsx len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));

    // ===== Read side =====
    let mut xls: Xlsx<_> = Xlsx::new(Cursor::new(bytes)).unwrap();

    // ① Sheet name list
    let names = xls.sheet_names();
    println!("sheets = {names:?}");

    // ② Per sheet: Range metadata + row-major per-cell type+value
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

    // ③ Formula text surface (cached values were already printed by ② for the Calc cells)
    let frange = xls.worksheet_formula("Calc").unwrap();
    println!("formula sheet: empty = {}", frange.is_empty());
    for (r, c, f) in frange.cells() {
        if !f.is_empty() {
            println!("  f({r},{c}) {f}");
        }
    }

    // ④ Merged regions (both the loaded full set and the per-sheet filter API)
    xls.load_merged_regions().unwrap();
    println!("merged count = {}", xls.merged_regions().len());
    for (sheet, _path, dims) in xls.merged_regions() {
        println!("merged {sheet} {:?}..{:?}", dims.start, dims.end);
    }
    println!(
        "merged_by_sheet(Data) = {} merged_by_sheet(Calc) = {}",
        xls.merged_regions_by_sheet("Data").len(),
        xls.merged_regions_by_sheet("Calc").len()
    );

    // ⑤ Boundaries: out-of-range access / empty sheet / missing-sheet error path
    let range = xls.worksheet_range("Data").unwrap();
    println!("inbounds get(1,1) = {}", data_tag(range.get((1, 1)).unwrap()));
    println!("oob get(100,100) is_some = {}", range.get((100, 100)).is_some());
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
    // Corrupt-archive error path: a truncated zip will not open.
    match Xlsx::new(Cursor::new(b"not an xlsx at all".to_vec())) {
        Ok(_) => println!("junk xlsx: unexpected ok"),
        Err(e) => println!("junk xlsx err: {e}"),
    }
}
