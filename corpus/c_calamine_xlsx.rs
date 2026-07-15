#!/usr/bin/env mirvm
---
[dependencies]
rust_xlsxwriter = "0.80"
calamine = "0.26"
---
// rust_xlsxwriter 0.80 + calamine 0.26 自洽闭环：内存 Vec<u8> 建 xlsx → Cursor 读回。
// 写侧（rust_xlsxwriter）：多 sheet（Data/Calc/Series/Empty）、数字/文本（CJK+emoji）
// /布尔/公式（含字符串缓存结果 t="str"）/日期（固定值 + 显式日期格式）/合并单元格
// /列宽（字符单位 + 像素两 API）；DocProperties::set_creation_datetime 钉死
// dcterms:created，zip 无 time feature（entry mtime 恒 1980-01-01）→ 整档字节可复现，
// 打印 len + fnv 锚定。
// 读侧（calamine）：sheet_names、逐 sheet worksheet_range（Range 元数据 + cells()
// 行主序逐格打印 类型+值，浮点/日期序列值一律 to_bits 锁位）、worksheet_formula
// （公式文本面）、load_merged_regions + merged_regions(_by_sheet)、边界（空 sheet
// 元数据、range.get/get_value 越界 None、不存在 sheet 的错误路径）。
// 确定性：输出只含 Vec 序/计数/位型/布尔断言，无时间/地址/HashMap 序。
//
// 已知 FRONTIER（mirvm 默认维与 JIT 维同址 TRAP，exit 70，stdout 空）：
//   mirvm[m4-engine]: TRAP: foreign `llvm.x86.pclmulqdq`（LLVM 内部符号，按需内建）
//   （fn core::core_arch::x86::pclmulqdq::__mm_clmulepi64_si128 @ crc32fast）
// 机理：rust_xlsxwriter 与 calamine 都经 zip 2.x → crc32fast 算 entry CRC32；
// crc32fast 带 std 时运行期 cpuid 探测（guest 直通宿主特性位）选 pclmulqdq
// 硬件路径，单次 update ≥128B 即执行 _mm_clmulepi64_si128——mirvm 未内建该
// llvm.x86 intrinsic（corpus.md M5.x 欠账队列已列此条）。任务预判的 psad.bw
// （simd-adler32）不会撞：zip 的 deflate 是 flate2 raw deflate（c_zip_arch 已
// 实证），真正的必经阻塞是 crc32fast/pclmulqdq。绕行排查：① stored 压缩无效——
// Stored entry 同样算 CRC32；② driver 控不住分块——写侧 rust_xlsxwriter 对每个
// XML 部件 write_all 整缓冲（最小工作簿的 [Content_Types].xml 也 >128B，空
// workbook 即撞），读侧 calamine 以 8KB BufReader 包 ZipFile 的 Crc32Reader，
// 两侧写/读块长都在 crate 内部，无法像 c_zip_arch 那样 64B 分块；③ 强制
// crc32fast 可移植基线路径需其 no_std 编译期分支，但 zip 默认 features 并集
// 必带 std（运行期探测），下游无法减。native 输出两跑逐字节一致（含整档
// fnv），作参考基准保留。
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

/// Data 变体的确定序标签；浮点与日期序列值按位打印。
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
    // 钉死文档创建时间，否则 core.xml 嵌入 utc_now → 整档字节不可复现。
    let created = ExcelDateTime::from_ymd(2024, 3, 14)
        .unwrap()
        .and_hms(15, 9, 26)
        .unwrap();
    wb.set_properties(&DocProperties::new().set_creation_datetime(&created));

    // ---- Sheet 1 "Data"：混合类型 + 合并单元格 + 列宽 ----
    let ws = wb.add_worksheet();
    ws.set_name("Data").unwrap();
    ws.write_string(0, 0, "hello").unwrap();
    ws.write_string(0, 1, "汉字 🦀 混合").unwrap();
    ws.write_number(1, 0, 42.0).unwrap();
    ws.write_number(1, 1, -3.5).unwrap();
    ws.write_number(1, 2, 1e300).unwrap();
    ws.write_boolean(2, 0, true).unwrap();
    ws.write_boolean(2, 1, false).unwrap();
    // 固定日期值 + 显式日期数字格式（无格式时 calamine 只会看到 Float 序列值）。
    let date_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    let dt = ExcelDateTime::from_ymd(2024, 3, 14)
        .unwrap()
        .and_hms(15, 9, 26)
        .unwrap();
    ws.write_datetime_with_format(3, 0, &dt, &date_fmt).unwrap();
    ws.merge_range(5, 0, 6, 2, "merged 合并", &Format::new()).unwrap();
    ws.set_column_width(0, 24).unwrap();
    ws.set_column_width_pixels(1, 120).unwrap();

    // ---- Sheet 2 "Calc"：公式 + 缓存结果（数值与字符串两类） ----
    let ws = wb.add_worksheet();
    ws.set_name("Calc").unwrap();
    ws.write_number(0, 0, 1.0).unwrap();
    ws.write_number(1, 0, 2.0).unwrap();
    ws.write_number(2, 0, 3.0).unwrap();
    ws.write_formula(0, 1, &Formula::new("=SUM(A1:A3)").set_result("6"))
        .unwrap();
    ws.write_formula(1, 1, &Formula::new("=A1*A2+A3").set_result("5"))
        .unwrap();
    // 字符串缓存结果 → t="str" 单元格。
    ws.write_formula(
        2,
        1,
        &Formula::new("=CONCATENATE(\"ab\",\"cd\")").set_result("abcd"),
    )
    .unwrap();

    // ---- Sheet 3 "Series"：确定性数值块（50×4），给 deflate 真实负载 ----
    let ws = wb.add_worksheet();
    ws.set_name("Series").unwrap();
    for r in 0..50u32 {
        for c in 0..4u16 {
            let v = (r as u64 * 4 + c as u64) as f64 * 0.25 - 12.5;
            ws.write_number(r, c, v).unwrap();
        }
    }

    // ---- Sheet 4 "Empty"：空 sheet 边界 ----
    let ws = wb.add_worksheet();
    ws.set_name("Empty").unwrap();

    wb.save_to_buffer().unwrap()
}

fn main() {
    // ===== 写侧 =====
    let bytes = build_xlsx();
    println!("xlsx len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));

    // ===== 读侧 =====
    let mut xls: Xlsx<_> = Xlsx::new(Cursor::new(bytes)).unwrap();

    // ① sheet 名列表
    let names = xls.sheet_names();
    println!("sheets = {names:?}");

    // ② 逐 sheet：Range 元数据 + 行主序逐格 类型+值
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

    // ③ 公式文本面（缓存值已在 ② 的 Calc 格中打印）
    let frange = xls.worksheet_formula("Calc").unwrap();
    println!("formula sheet: empty = {}", frange.is_empty());
    for (r, c, f) in frange.cells() {
        if !f.is_empty() {
            println!("  f({r},{c}) {f}");
        }
    }

    // ④ 合并区域（加载后全量 + 按 sheet 过滤两 API）
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

    // ⑤ 边界：越界访问 / 空 sheet / 不存在 sheet 错误路径
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
    // 损坏档错误路径：截断的 zip 打不开。
    match Xlsx::new(Cursor::new(b"not an xlsx at all".to_vec())) {
        Ok(_) => println!("junk xlsx: unexpected ok"),
        Err(e) => println!("junk xlsx err: {e}"),
    }
}
