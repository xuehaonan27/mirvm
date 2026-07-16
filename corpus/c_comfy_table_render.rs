#!/usr/bin/env mirvm
---
[dependencies]
comfy-table = "7"
---
// comfy-table 7.2（unicode 表格渲染）差分：「显示宽度」计算是本 driver 核心锚点——
// 单元格内容经 unicode-width 0.2（CJK/全角宽 2、半角宽 1、emoji/组合符按
// Unicode 宽度表）+ unicode-segmentation 的 grapheme 切分（ZWJ 簇/变体选择符），
// 对齐、折行、截断全部由显示宽度而非字节数驱动。渲染全文逐字节对拍
// native/mirvm 的宽度表、grapheme 边界与布局算术语义一致性。
//
// 覆盖：
// ① preset 边框族五路：UTF8_FULL / UTF8_BORDERS_ONLY / ASCII_FULL /
//    ASCII_MARKDOWN / NOTHING（无框），同一份 CJK 混合内容渲染全文打印。
// ② 对齐：单元格级 Left/Center/Right + 列级 set_cell_alignment 默认，
//    覆盖次序 cell > column > Left。
// ③ 列宽约束 wrap：ContentArrangement::Dynamic + set_width ×
//    ColumnConstraint::{UpperBoundary, Absolute, LowerBoundary}(Width::Fixed)，
//    长中/英文按 grapheme 退刀折行（split_long_word）；DynamicFullWidth +
//    Width::Percentage（含 >100 夹取）盈余分配。
// ④ 宽度锚点表：CJK、全/半角假名、emoji（单字/ZWJ 家庭簇/肤调/旗帜/彩虹旗/
//    变体选择符）、组合附加符（含圈符）、零宽空格、软连字符，三列三种对齐混排；
//    另含多行单元格（\n 与自定义 delimiter）与 Row::max_height + 自定义截断
//    指示符的 truncate_first_lines 路径。
// ⑤ 条件样式：force_no_tty + enforce_styling 使 crossterm ANSI 序列确定生成
//    （Color::{named,Rgb,AnsiValue} × Attribute::{Bold,Underlined,Italic,
//    Reverse}，fg/bg），style_text_only 开/关两形态；打印前把 ESC
//    替换为字面 "\\e"——输出保持可打印纯文本，ANSI 序列本身仍逐字节锚定。
//    另验证 no_tty 抑制路径（同款表渲染零 ESC）。
// ⑥ 边界：空表、仅表头、单列、参差行（短行占位填充 / 宽于表头自动扩列）、
//    空字符串单元格、add_row_if/add_rows_if 谓词、column_max_content_widths、
//    current_style_as_preset→load_preset roundtrip、apply_modifier
//    （UTF8_ROUND_CORNERS）、set_style/style 查询/remove_style 回退空格。
//
// 无跨行/跨列：comfy-table 7.2 API 面无 rowspan/colspan（grep 源码零命中），
// 本 driver 不涉及该维度。
//
// 确定性：全部渲染经 Display（lines().join("\n")）全文打印，每张表附
// bytes= + fnv1a 锚；force_no_tty 关闭 tty 探测（渲染行为与环境无关）；
// main 开头 remove_var("NO_COLOR")——crossterm 0.29 尊重 NO_COLOR 环境变量
// （memoized 抑制彩色 SGR 序列），移除后 ANSI 颜色路径全展开、与环境无关；
// 内部 style HashMap 在 crate 内只按键存取、无迭代序。
use comfy_table::{
    Attribute, Cell, CellAlignment, Color, ColumnConstraint, ContentArrangement, Row, Table,
    TableComponent, Width, modifiers::UTF8_ROUND_CORNERS, presets,
};

/// FNV-1a 锚定渲染字节流。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 打印标签、字节数/FNV 锚、渲染全文。
fn show(label: &str, table: &Table) {
    let s = table.to_string();
    println!("== {label} bytes={} fnv={:016x} ==", s.len(), fnv1a(s.as_bytes()));
    if s.is_empty() {
        println!("<empty-render>");
    } else {
        println!("{s}");
    }
}

/// 各 preset 共用的混合内容基准表。
fn base_table() -> Table {
    let mut t = Table::new();
    t.force_no_tty();
    t.set_header(vec!["Name", "语种", "Score"]);
    t.add_row(vec!["alpha", "拉丁", "98"]);
    t.add_row(vec!["汉字基准", "中文", "87"]);
    t.add_row(vec!["sigma-σ", "混排", "100"]);
    t
}

fn main() {
    // SAFETY: main 起点、单线程，且在 crossterm 的 NO_COLOR memoize（首次
    // styled 渲染时经 parking_lot::Once 快照环境）之前执行；此后不再触碰环境。
    unsafe { std::env::remove_var("NO_COLOR") };

    // ① preset 边框族五路
    for (label, preset) in [
        ("utf8_full", presets::UTF8_FULL),
        ("utf8_borders_only", presets::UTF8_BORDERS_ONLY),
        ("ascii_full", presets::ASCII_FULL),
        ("ascii_markdown", presets::ASCII_MARKDOWN),
        ("nothing", presets::NOTHING),
    ] {
        let mut t = base_table();
        t.load_preset(preset);
        show(&format!("preset/{label}"), &t);
    }
    // 无框 preset 的 trim_fmt 变体（去行尾空格）
    let mut t = base_table();
    t.load_preset(presets::NOTHING);
    let trimmed = t.trim_fmt();
    println!(
        "trim_fmt bytes={} fnv={:016x}",
        trimmed.len(),
        fnv1a(trimmed.as_bytes())
    );
    println!("{trimmed}");

    // ② 对齐：cell 级三态 + 列级默认，覆盖次序 cell > column
    let mut t = Table::new();
    t.force_no_tty();
    t.load_preset(presets::UTF8_FULL);
    t.set_header(vec!["cell-级", "cell-级", "列级"]);
    t.add_row(vec![
        Cell::new("left"),
        Cell::new("center").set_alignment(CellAlignment::Center),
        Cell::new("由列决定"),
    ]);
    t.add_row(vec![
        Cell::new("显式左对齐").set_alignment(CellAlignment::Left),
        Cell::new("宽宽宽").set_alignment(CellAlignment::Right),
        Cell::new("同样由列决定"),
    ]);
    // 第 0/1 列列级右对齐：row0 col0 无 cell 级设置 → 生效；row1 col0 有 → 被覆盖。
    // 第 2 列列级右对齐，两行都由列决定。
    t.column_mut(0).unwrap().set_cell_alignment(CellAlignment::Right);
    t.column_mut(1).unwrap().set_cell_alignment(CellAlignment::Center);
    t.column_mut(2).unwrap().set_cell_alignment(CellAlignment::Right);
    show("align/precedence", &t);

    // ③a Dynamic + Fixed 约束折行
    let mut t = Table::new();
    t.force_no_tty();
    t.load_preset(presets::UTF8_FULL);
    t.set_content_arrangement(ContentArrangement::Dynamic);
    t.set_width(44);
    t.set_header(vec!["Upper(12)", "Abs(16)", "Lower(4)"]);
    t.add_row(vec![
        "一段相当长的中文内容用于触发按显示宽度折行的行为观察",
        "a quite long english sentence to wrap across lines",
        "短",
    ]);
    t.add_row(vec!["second row短", "tiny", "下界列内容可以更宽一些试试"]);
    t.set_constraints(vec![
        ColumnConstraint::UpperBoundary(Width::Fixed(12)),
        ColumnConstraint::Absolute(Width::Fixed(16)),
        ColumnConstraint::LowerBoundary(Width::Fixed(4)),
    ]);
    show("wrap/dynamic-fixed", &t);

    // ③b DynamicFullWidth + Percentage（120 夹取到 100）
    let mut t = Table::new();
    t.force_no_tty();
    t.load_preset(presets::UTF8_FULL);
    t.set_content_arrangement(ContentArrangement::DynamicFullWidth);
    t.set_width(50);
    t.set_header(vec!["P120%", "P40%", "无约束"]);
    t.add_row(vec![
        "百分比约束列的内容会被夹取后的比例折行处理",
        "mid-width content here",
        "rest",
    ]);
    t.set_constraints(vec![
        ColumnConstraint::UpperBoundary(Width::Percentage(120)),
        ColumnConstraint::LowerBoundary(Width::Percentage(40)),
    ]);
    show("wrap/fullwidth-pct", &t);

    // ④ 宽度锚点：CJK/假名/emoji/组合符 × 左中右三列
    let mut t = Table::new();
    t.force_no_tty();
    t.load_preset(presets::UTF8_FULL);
    t.set_header(vec!["kind", "左", "中", "右"]);
    let width_rows: [(&str, [&str; 3]); 8] = [
        ("cjk", ["宽度测试一", "漢字仮名交じり文", "한국어텍스트"]),
        ("kana", ["ｶﾀｶﾅ半角", "カタカナ全角", "ｱｲｳｴｵ半"]),
        ("emoji", ["🦀🎉✨", "👨‍👩‍👧‍👦家庭", "✈️民航机"]),
        ("mark", ["e\u{301}a\u{308}", "किताब本", "a\u{20dd}圈"]),
        ("flag", ["🇨🇳旗", "👍🏽赞", "🏳️‍🌈虹"]),
        ("zero", ["a\u{200b}b零宽", "软\u{ad}连字", "x\u{fe0f}变体"]),
        ("mix", ["abc中文🦀xy", "12漢34🎉56", "附e\u{301}尾"]),
        ("empty", ["", " ", "非空"]),
    ];
    for (kind, [l, c, r]) in width_rows {
        t.add_row(vec![
            Cell::new(kind),
            Cell::new(l),
            Cell::new(c).set_alignment(CellAlignment::Center),
            Cell::new(r).set_alignment(CellAlignment::Right),
        ]);
    }
    println!("widths colmax={:?}", t.column_max_content_widths());
    show("width/anchor", &t);

    // ④b 多行单元格 + 自定义 delimiter + max_height 截断
    let mut t = Table::new();
    t.force_no_tty();
    t.load_preset(presets::UTF8_FULL);
    t.set_header(vec!["多行", "管道分隔", "裁断"]);
    let mut r = Row::new();
    r.add_cell(Cell::new("第一行\n第二行宽宽宽\n第三行\n被裁行"));
    r.add_cell(Cell::new("a|b宽|c").set_delimiter('|'));
    r.add_cell(Cell::new("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz"));
    t.add_row(r);
    t.add_row(vec!["短", "x", "y"]);
    t.set_truncation_indicator("…");
    t.row_mut(0).unwrap().max_height(3);
    show("multiline/truncate", &t);

    // ⑤ 条件样式（enforce_styling 锁定 ANSI 生成，ESC → "\e" 可打印化）
    let mut t = Table::new();
    t.force_no_tty();
    t.enforce_styling();
    t.load_preset(presets::UTF8_FULL);
    t.set_header(vec![
        Cell::new("fg").fg(Color::Green),
        Cell::new("bg宽").bg(Color::Rgb {
            r: 200,
            g: 40,
            b: 199,
        }),
        Cell::new("attr").add_attribute(Attribute::Bold),
    ]);
    t.add_row(vec![
        Cell::new("文字wide🦀").fg(Color::AnsiValue(202)).bg(Color::DarkGrey),
        Cell::new("混合😀样式").add_attributes(vec![
            Attribute::Italic,
            Attribute::Underlined,
            Attribute::Reverse,
        ]),
        Cell::new("无样式"),
    ]);
    let s_full = t.to_string();
    t.style_text_only();
    let s_text = t.to_string();
    for (label, s) in [("styled/full", &s_full), ("styled/text-only", &s_text)] {
        println!(
            "== {label} bytes={} fnv={:016x} esc={} ==",
            s.len(),
            fnv1a(s.as_bytes()),
            s.matches('\u{1b}').count()
        );
        println!("{}", s.replace('\u{1b}', "\\e"));
    }
    // no_tty 抑制路径：同款样式表不 enforce → 渲染零 ESC
    let mut plain = Table::new();
    plain.force_no_tty();
    plain.load_preset(presets::UTF8_FULL);
    plain.set_header(vec![Cell::new("fg").fg(Color::Green)]);
    plain.add_row(vec![Cell::new("文字wide🦀").fg(Color::AnsiValue(202))]);
    let s_plain = plain.to_string();
    println!("styled/suppressed esc={}", s_plain.matches('\u{1b}').count());
    println!("{s_plain}");

    // ⑥ 边界族
    let mut e = Table::new();
    e.force_no_tty();
    println!("empty is_empty={}", e.is_empty());
    show("edge/empty", &e);

    let mut h = Table::new();
    h.force_no_tty();
    h.load_preset(presets::UTF8_FULL);
    h.set_header(vec!["仅", "表头"]);
    println!("header-only rows={} is_empty={}", h.row_count(), h.is_empty());
    show("edge/header-only", &h);

    let mut s1 = Table::new();
    s1.force_no_tty();
    s1.load_preset(presets::UTF8_FULL);
    s1.set_header(vec!["单列"]);
    s1.add_row(vec!["一"]);
    s1.add_row(vec![Cell::new("两行\n单元格")]);
    s1.add_row(vec![""]);
    s1.add_row(vec!["全角宽度二字符"]);
    show("edge/single-col", &s1);

    // 参差行：短行占位填充、宽于表头自动扩列
    let mut rg = Table::new();
    rg.force_no_tty();
    rg.load_preset(presets::UTF8_FULL);
    rg.set_header(vec!["a", "b", "c"]);
    rg.add_row(vec!["1"]);
    rg.add_row(vec!["x", "y", "z", "溢出列"]);
    rg.add_row(vec!["", ""]);
    println!("ragged cols={} rows={}", rg.column_count(), rg.row_count());
    show("edge/ragged", &rg);

    // 谓词 add_row_if / add_rows_if（谓词可见当前行数与被加行）
    let mut t = base_table();
    t.add_row_if(|n, row: &Vec<&str>| n < 4 && row[0].len() > 3, vec!["gate-pass", "谓词", "1"])
        .add_row_if(|n, _: &Vec<&str>| n >= 99, vec!["gate-fail", "谓词", "2"])
        .add_rows_if(
            |n, rows: &Vec<Vec<&str>>| n % 2 == 0 || rows.len() > 1,
            vec![vec!["bulk-a", "批", "3"], vec!["bulk-b", "量", "4"]],
        );
    show("edge/predicates", &t);

    // 样式 roundtrip：preset → modifier → 导出 → 重载 → 渲染等价
    let mut t = base_table();
    t.load_preset(presets::UTF8_FULL);
    t.apply_modifier(UTF8_ROUND_CORNERS);
    let preset_str = t.current_style_as_preset();
    println!("roundtrip preset = {preset_str:?}");
    let mut t2 = base_table();
    t2.load_preset(&preset_str);
    let eq = t.to_string() == t2.to_string();
    println!("preset roundtrip eq = {eq}");
    show("style/round-corners", &t2);

    // set_style 单点覆盖 + style 查询 + remove_style 回退空格
    let mut t = base_table();
    t.load_preset(presets::ASCII_FULL);
    t.set_style(TableComponent::TopLeftCorner, '*');
    t.set_style(TableComponent::HeaderLines, '~');
    println!(
        "style q corner={:?} header={:?} right-corner={:?}",
        t.style(TableComponent::TopLeftCorner),
        t.style(TableComponent::HeaderLines),
        t.style(TableComponent::TopRightCorner)
    );
    t.remove_style(TableComponent::TopRightCorner);
    t.remove_style(TableComponent::RightBorder);
    show("style/override-remove", &t);
}
