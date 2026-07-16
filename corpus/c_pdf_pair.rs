#!/usr/bin/env mirvm
---
[dependencies]
printpdf = "0.7"
lopdf = { version = "0.34", default-features = false, features = ["nom_parser"] }
---
// printpdf 0.7 生成 + lopdf 0.34 解析的 PDF 闭环（生成→逐字节锚定→解析→重存
// →再解析）。内建 Helvetica/Helvetica-Bold（不嵌字体）：两页（A4 portrait +
// A4 landscape）/ 六文本行（含 PDF 字面字符串转义边界 ( ) \ % $）/ 三矩形
// （Fill/FillStroke/Stroke 三 PaintMode）/ 开合两条多段线 / Rgb 填充+描边色 +
// 线宽。lopdf 面：load_mem / version / get_pages / get_and_decode_page_content
// 操作码直方图（BTreeMap 序）/ extract_text 空白归一化后与原文比对 / objects
// 变体遍历计数 / save_to 重存+重载再提取 roundtrip / 坏文件、截断、页号越界
// 三条错误路径。
//
// 确定性记录：① printpdf 的 document_id 与 trailer instance_id 全走其
// utils.rs 的定种全局 xorshift（RAND_SEED=2100，SeqCst fetch_add），单线程
// 顺序调用下逐 run 同一序列——非真随机，无需绕行。② PdfMetadata::new 默认
// 刻 OffsetDateTime::now_utc() 三枚时间戳进 Info 字典（壁钟！），driver 用
// with_creation_date/with_metadata_date/with_mod_date 钉到固定 epoch。
// ③ 默认 Custom conformance 不嵌 XMP/ICC；内建字体不嵌子集。④ lopdf 钉
// default-features=false+nom_parser：默认 features 含 rayon（无必要负载）
// 与 chrono_time；pom/nom 任一即提供 parser_aux（extract_text）。
// ⑤ printpdf 的 save_to_bytes 在 release 才 prune+compress——mirvm/native 两
// harness 均 debug profile（cargo run 无 --release），两侧同为未压缩流。
use std::collections::BTreeMap;

use printpdf::path::PaintMode;
use printpdf::{BuiltinFont, Color, Line, Mm, PdfDocument, Point, Rect, Rgb};

/// 内联 FNV-1a（二进制内容锚定，不打印原始字节）。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 空白归一化（两侧同码即确定）：extract_text 把每个 ET 结尾折成 '\n'、
/// TJ 数组元素间插空格，比对原文前统一折叠任意空白。
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// lopdf Object 顶层变体分类（对象遍历计数用）。
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
    // ---- ① printpdf 生成：两页 / 六文本行 / 三矩形 / 两条线 ----
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

    // ---- ② lopdf 解析：版本 / 页数 / 逐页操作码直方图 / 文本提取比对 ----
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

    // ---- ③ 对象遍历计数（BTreeMap 确定序）----
    let mut hist: BTreeMap<&str, usize> = BTreeMap::new();
    for obj in parsed.objects.values() {
        *hist.entry(class(obj)).or_default() += 1;
    }
    println!("objects = {}", parsed.objects.len());
    for (k, v) in &hist {
        println!("  {k} = {v}");
    }

    // ---- ④ lopdf 重存 roundtrip：字节锚定 + 重载再提取 ----
    let mut redoc = lopdf::Document::load_mem(&bytes).unwrap();
    let mut resaved = Vec::new();
    redoc.save_to(&mut resaved).unwrap();
    println!("resaved len={} fnv={:016x}", resaved.len(), fnv1a(&resaved));
    let reparsed = lopdf::Document::load_mem(&resaved).unwrap();
    let reextract = reparsed.extract_text(&[1, 2]).unwrap();
    println!("resave-extract-ok = {}", normalize(&reextract) == expected);

    // ---- ⑤ 错误路径：坏文件 / 截断 / 页号越界 ----
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
