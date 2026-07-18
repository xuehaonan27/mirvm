#!/usr/bin/env mirvm
---
[dependencies]
rust_xlsxwriter = "=0.96.0"
calamine = { version = "=0.36.0", features = ["picture"] }
---
// c_xlsxwriter_rw —— rust_xlsxwriter 0.96 写 + calamine 0.36 读的内存闭环三维差分
// （批10 波2）。与批3 c_calamine_xlsx（0.80/0.26 基础四表闭环）的关系：本 driver
// 走**现行稳定线**并压 0.80→0.96 / 0.26→0.36 之间的新 API 面（表格 table/autofilter/
// 冻结窗格/隐藏表/超链接/富文本/数组公式/未来函数转义/布尔与错误缓存结果/嵌入图片
// /批注 note/defined names/sheets_metadata/pictures），且读回侧在 full dump 之外
// 增加**逐格对拍**（写侧期望值模型 vs calamine 读回逐格比较）。
//
// 版本钉（相容组合证据）：
//   * rust_xlsxwriter =0.96.0：crates.io 2026-07-18 最新稳定（0.96.0 为 newest），
//     rust-version 1.83 << 本机 nightly-2026-07-02；默认 features 为空集（ryu/zmij/
//     chrono/jiff/serde 全不进场），硬依赖仅 zip ^7.2（default-features=false +
//     features=["deflate"]）。
//   * calamine =0.36.0：crates.io 2026-07-18 最新稳定，rust-version 1.88；默认
//     features 为空集，显式开 "picture"（Reader::pictures 按 feature 门控）以压
//     图片读回面；硬依赖 zip ^8.6（default-features=false + features=["deflate"]）、
//     quick-xml/atoi_simd/fast-float2/encoding_rs/codepage 等纯 Rust。
//   * 双 zip 大版本（xlsxwriter→zip7 / calamine→zip8）并存合法：cargo 按 semver
//     多版本共存；两 zip 的 deflate 特性在各自大版本内统一解析到 flate2 的
//     zlib-rs 后端（zip 7.2/8.6 的 deflate = zopfli + deflate-flate2-zlib-rs），
//     全图 flate2/libz-rs-sys 同一实例，无 C 依赖、无 build.rs 外部工具。
//   * 实测闭包 35 包（Cargo.lock 三维共享）：zip 7.2.0 + zip 8.6.0 双大版本、
//     crc32fast 1.5.0（entry CRC；≥128B 单块必走 pclmulqdq 硬件路径——该
//     intrinsic 已内建，本 driver 三维无 Trap 实证）、flate2 1.1.9 + zlib-rs
//     0.6.6（deflate 后端）+ zopfli 0.8.3 + miniz_oxide 0.8.9/simd-adler32
//     0.3.10（psad.bw 族已内建）、quick-xml 0.41/atoi_simd 0.18/fast-float2
//     0.2 等纯 Rust，零 C 依赖、零 build.rs 外部工具。
//
// 确定性说明：
//   * DocProperties::set_creation_datetime 钉死 dcterms:created=2031-01-02T03:04:05Z
//     （否则 utc_now 进 core.xml 整档不可复现——c_calamine_xlsx 同款纪律）。
//   * zip default-features=false 且未开 time feature → 全部 entry mtime 恒
//     1980-01-01 00:00:00，zip 容器字节可复现。
//   * deflate（flate2 zlib-rs）同库版本同输入同压缩级别 → 输出字节确定；
//     A/B/C 三维同 Cargo.toml（B 维直接复用 A 维物化的 script dir），依赖解析一致。
//   * 全部单元格数据/公式/格式/图片（78B 内嵌 PNG 常量）/批注文本为编译期常量；
//     无 OS 随机、无壁钟（除上钉死的文档属性）、无 HashMap 迭代序打印
//     （defined_names 按 workbook.xml 文件序=插入序）、无裸地址；浮点与日期
//     序列值一律 to_bits 十六进制锚定。
//   * stderr 真空：calamine 的 log crate 无 logger 时静默；rust_xlsxwriter 不打日志。
//
// 复红定因参照（若未来变红按此序排查）：
//   ① 运行期 SIMD 派发面：crc32fast 的 pclmulqdq（entry CRC ≥128B 单块必经）
//      与 flate2/zlib-rs·simd-adler32 的 avx2 maddubs·madd·psad.bw 族——全部
//      在 2026-07-15 内建清单内（corpus.md §5 欠账队列核销段），本 driver 三维
//      实证无 Trap；若新增未内建兄弟（如 avx512 系）即此处炸。
//   ② calamine 的 atoi_simd（SSE4.1 面）与 quick-xml 大 XML 解析——同族已绿先例
//      多（c_calamine_xlsx/c_zip_arch/c_crc32fast/c_zopfli_deep）。
//   ③ 语义数据点（非红）：calamine 读回类型由 XML 单元格 t= 与 numFmt 共同决定。
//      0.26→0.36 行为变更实测（本 driver 首跑对拍压出，native/mirvm 同文）：
//      a) 工作表数值格一律读作 Float（cells_reader.rs format_excel_f64_ref 只走
//         f64；Data::Int 仅剩 PivotCache 面）——0.26 的「无小数点则 Int」已移除；
//      b) 数组公式覆盖区的非锚格物化为 Float(0.0)；c) 合并区尾行无 XML 单元格时
//         Range 终点停在最后有格行（越界 get_value=None）。其余编码：日期格式读作
//         DateTime、超链接读作显示文本、富文本退化纯文本、空字符串不落盘
//         （rust_xlsxwriter 对无格式空串静默跳过）。逐格对拍期望值已按此编码，
//         全部 ok=true。
//
// 覆盖清单：
//   写侧（rust_xlsxwriter 0.96，七表）：
//   ① Types——转义字符串/CJK+emoji/空串（静默跳过语义锚）/整值·小数·1e300·
//     2.5e-10·大整数/布尔/两种日期写法（write_datetime_with_format 与显式序列值
//     +自定义日期格式 dd-mmm-yy）/带格式空白格/合并区域/列宽（字符+像素）/行高/
//     tab 颜色；② Calc——公式缓存结果四型（数值 Int/字符串 t="str"/布尔 t="b"/
//     错误 t="e" #DIV/0!）+ 数组公式（C6:E6 锚格缓存）+ 未来函数 =IFS 的 _xlfn
//     转义锚；③ Grid——40×6 混合 Int/Float 确定性数值块（deflate 真实负载）+
//     autofilter + 冻结窗格；④ Table——add_table 命名表 Sales（Medium9 样式 +
//     三列表头，CJK/emoji 数据行）；⑤ Hidden——set_hidden 隐藏表可读；
//     ⑥ Media——insert_image（78B 定值 PNG + alt 文本）/insert_note（含作者）/
//     write_url（URL 含 & 转义 + 显示文本 + tip）/write_rich_string 双格式段；
//     ⑦ Empty——空表边界。workbook 级：DocProperties 六字段 + define_name
//     全局/表局部各一。
//   读侧（calamine 0.36）：sheet_names/sheets_metadata（typ+visible）/
//     defined_names/逐表 Range 元数据 + 行主序全格 dump（九变体 data_tag，
//     浮点·日期 to_bits）/worksheet_formula 公式文本面/merge_cells_by_sheet_name
//     （+_by_id）合并区域/pictures 图片字节 fnv 闭环/越界 get_value/
//     空表/不存在表/垃圾字节两错误路径/逐格对拍（41 项期望值，mismatch 全零）。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_xlsxwriter_rw.rs
//   B: d=$(grep -l 'name = "c_xlsxwriter_rw"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_xlsxwriter_rw.rs
//
// 三维实测（2026-07-19，全绿）：A/B/C 三进程 stdout 逐字节一致（349 行：
// xlsx len=14236 fnv=01247f537aafa3a8；七表全格 dump + formula 面（_xlfn.IFS
// 转义与数组公式 ref 锚定）+ merged/pictures（pic ext=png len=78
// fnv=3ab250953e4202cd）+ 逐格对拍 41 项 mismatch 全零），stderr 全真空
// （0 字节）、exit 全 0；A/A2、B/B2 各自复跑逐字节一致（文档创建时间钉死 +
// zip 无 time feature，整档字节可复现实证成立）。时长：A 冷跑（含 35 crate
// 依赖闭包首次构建降级）约 1min、热跑 1.6s；B 首跑 11.3s（script dir target
// 已由 A 维物化期预建，增量链接+运行）、热跑 0.1s；C（JIT=1）1.5-2.1s。
// 依赖闭包 35 crate（zip 7.2.0/8.6.0 双大版本并存）。无 FRONTIER、无引擎
// bug 信号；压出的是 calamine 0.26→0.36 上游语义数据点三条（见上「复红定因
// 参照」③），非 mirvm 分叉。

use calamine::{Data, Reader, Xlsx};
use rust_xlsxwriter::{
    Color, DocProperties, ExcelDateTime, Format, FormatAlign, Formula, Image, Note, Table,
    TableColumn, TableStyle, Url, Workbook,
};
use std::io::Cursor;

/// 78 字节定值 3x2 truecolor PNG（像素 R,G,B / Y,C,M，zlib level 9 固定）。
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

/// Data 九变体的确定序标签；浮点与日期序列值按位打印。
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

/// 逐格对拍期望值：按 calamine 读回语义编码（见头注「复红定因参照」③）。
/// 注意无 Int 变体：calamine 0.36 的 xlsx 工作表数值路径一律走 f64
/// （cells_reader.rs format_excel_f64_ref），Data::Int 仅剩 PivotCache 面——
/// 整值数字读回 = Float。
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

/// (sheet, row, col, 期望值) 四元组；与写侧代码同序登记。
type Expect = (&'static str, u32, u16, Exp);

fn build_xlsx() -> (Vec<u8>, Vec<Expect>) {
    let mut exp: Vec<Expect> = Vec::new();
    let mut wb = Workbook::new();

    // 钉死文档创建时间；其余属性字段也全定值。
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

    // ---- Sheet 1 "Types"：类型矩阵 + 格式/合并/空白/列宽行高/tab 色 ----
    let ws = wb.add_worksheet();
    ws.set_name("Types").unwrap();
    ws.set_tab_color(Color::RGB(0x00C0_00CC));
    ws.write_string(0, 0, "esc <&> \"' 混合").unwrap();
    exp.push(("Types", 0, 0, Exp::Str("esc <&> \"' 混合")));
    ws.write_string(0, 1, "汉字 🦀 CJK").unwrap();
    exp.push(("Types", 0, 1, Exp::Str("汉字 🦀 CJK")));
    // 无格式空串被 rust_xlsxwriter 静默跳过（不落盘）——语义锚，读回为 Empty。
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
    // 日期两条写法：ExcelDateTime 对象 + 显式序列值（格式驱动 DateTime 读回）。
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
    // 合并区尾行 (6,0)/(6,1) 无 XML 单元格，calamine Range 终点停在 (5,4)，
    // get_value 越界得 None——形状由 merged 区域行锚定，此处不设期望。
    ws.set_column_width(0, 24).unwrap();
    ws.set_column_width_pixels(1, 120).unwrap();
    ws.set_row_height(3, 30.0).unwrap();

    // ---- Sheet 2 "Calc"：公式缓存结果四型 + 数组公式 + 未来函数转义 ----
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
    // 数组公式 C6:E6：缓存值只落锚格；非锚两格 calamine 0.36 按数组覆盖区
    // 物化为 Float(0.0)（0.26 行为数据点见头注）。
    ws.write_array_formula(5, 0, 5, 2, &Formula::new("=SUM(A1:A3)").set_result("6"))
        .unwrap();
    exp.push(("Calc", 5, 0, Exp::F64Bits((6.0f64).to_bits())));
    exp.push(("Calc", 5, 1, Exp::F64Bits((0.0f64).to_bits())));
    exp.push(("Calc", 5, 2, Exp::F64Bits((0.0f64).to_bits())));
    // 未来函数：写侧转义为 _xlfn.IFS（公式文本面锚定）。
    ws.write_formula(6, 1, &Formula::new("=IFS(A1>0,\"pos\")").set_result("pos"))
        .unwrap();
    exp.push(("Calc", 6, 1, Exp::Str("pos")));

    // ---- Sheet 3 "Grid"：40x6 混合 Int/Float 数值块 + autofilter + 冻结 ----
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

    // ---- Sheet 4 "Table"：命名表 + 样式 + 表头列 ----
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

    // ---- Sheet 5 "Hidden"：隐藏表仍可读 ----
    let ws = wb.add_worksheet();
    ws.set_name("Hidden").unwrap();
    ws.set_hidden(true);
    ws.write_string(0, 0, "secret 隐藏").unwrap();
    exp.push(("Hidden", 0, 0, Exp::Str("secret 隐藏")));
    ws.write_number(0, 1, 7.0).unwrap();
    exp.push(("Hidden", 0, 1, Exp::F64Bits((7.0f64).to_bits())));

    // ---- Sheet 6 "Media"：图片/批注/超链接/富文本 ----
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

    // ---- Sheet 7 "Empty"：空表边界 ----
    let ws = wb.add_worksheet();
    ws.set_name("Empty").unwrap();

    (wb.save_to_buffer().unwrap(), exp)
}

fn main() {
    // ===== 写侧 =====
    let (bytes, exp) = build_xlsx();
    println!("xlsx len={} fnv={:016x}", bytes.len(), fnv1a(&bytes));

    // ===== 读侧 =====
    let mut xls: Xlsx<_> = Xlsx::new(Cursor::new(bytes)).unwrap();

    // ① sheet 名列表 + 元数据（类型/可见性，Hidden 表锚定）
    let names = xls.sheet_names();
    println!("sheets = {names:?}");
    for s in xls.sheets_metadata() {
        println!("meta {:?} typ={:?} visible={:?}", s.name, s.typ, s.visible);
    }

    // ② defined names（workbook.xml 文件序 = 插入序）
    let dn = xls.defined_names();
    println!("defined_names = {dn:?}");

    // ③ 逐表：Range 元数据 + 行主序逐格 类型+值
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

    // ④ 公式文本面（_xlfn 转义与数组公式 ref 在此锚定；缓存值已在 ③ 打印）
    let frange = xls.worksheet_formula("Calc").unwrap();
    println!("formula sheet: empty = {}", frange.is_empty());
    for (r, c, f) in frange.cells() {
        if !f.is_empty() {
            println!("  f({r},{c}) {f}");
        }
    }

    // ⑤ 合并区域（0.36 推荐 API：按名 / 按 id 两路锚定）
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

    // ⑥ 图片读回闭环（写侧 78B 定值 PNG）
    match xls.pictures() {
        Some(pics) => {
            println!("pictures = {}", pics.len());
            for (ext, data) in &pics {
                println!("pic ext={ext} len={} fnv={:016x}", data.len(), fnv1a(data));
            }
        }
        None => println!("pictures = none"),
    }

    // ⑦ 逐格对拍：写侧期望 vs 读回实际（get_value 绝对坐标）
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

    // ⑧ 边界：越界 / 空表 / 不存在表 / 垃圾字节
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
