#!/usr/bin/env mirvm
---
[dependencies]
comfy-table = "7"
---
// comfy-table 7.2 (unicode table rendering). The full render is compared
// byte-for-byte with native, so the width table, grapheme boundaries and layout
// arithmetic must agree. Cells are measured through unicode-width 0.2
// (CJK/fullwidth = 2, halfwidth = 1, emoji and marks per the Unicode width table)
// and split into graphemes by unicode-segmentation (ZWJ clusters, selectors).
// (1) Cover the five border presets: UTF8_FULL / UTF8_BORDERS_ONLY / ASCII_FULL /
//     ASCII_MARKDOWN / NOTHING (borderless), all over one mixed CJK content table.
// (2) Cover alignment: per-cell Left/Center/Right plus a column-level
//     set_cell_alignment default, showing the precedence cell > column > Left.
// (3) Cover wrap: ContentArrangement::Dynamic + set_width x ColumnConstraint::
//     {UpperBoundary, Absolute, LowerBoundary}(Width::Fixed), splitting long
//     CJK/English at graphemes (split_long_word); DynamicFullWidth +
//     Width::Percentage (clamped above 100) for surplus distribution.
// (4) Anchor widths: CJK, half/fullwidth kana, emoji (single, ZWJ family cluster,
//     skin tone, flag, rainbow flag, variation selector), combining marks
//     (including enclosing), zero-width space, soft hyphen, three alignments; plus
//     multi-line cells (\\n and a custom delimiter) and Row::max_height truncation.
// (5) Force deterministic ANSI: force_no_tty + enforce_styling pin the crossterm
//     sequences (Color::{named,Rgb,AnsiValue} x Attribute::{Bold,Underlined,
//     Italic,Reverse}, fg/bg) with style_text_only on and off; ESC becomes a
//     literal "\\e" before printing. Also checks the no_tty path (zero ESC).
// (6) Edges: empty, header-only, single column, ragged rows, empty cells,
//     add_row_if/add_rows_if, column_max_content_widths, load_preset roundtrip,
//     apply_modifier, set_style/style/remove_style fallback; no rowspan/colspan.
// Determinism: every render goes through lines().join("\n") via Display and
// prints in full with bytes=/fnv1a anchors per table. main calls
// remove_var("NO_COLOR") first; force_no_tty turns off tty detection.
//
//
//
//
//
//
//
//
//
//
use comfy_table::{
    Attribute, Cell, CellAlignment, Color, ColumnConstraint, ContentArrangement, Row, Table,
    TableComponent, Width, modifiers::UTF8_ROUND_CORNERS, presets,
};

/// FNV-1a anchor over the rendered byte stream.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Prints the label, the byte count/FNV anchor and the full render.
fn show(label: &str, table: &Table) {
    let s = table.to_string();
    println!("== {label} bytes={} fnv={:016x} ==", s.len(), fnv1a(s.as_bytes()));
    if s.is_empty() {
        println!("<empty-render>");
    } else {
        println!("{s}");
    }
}

/// Mixed-content base table shared by the preset cases.
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
    // SAFETY: at the top of main, single-threaded, and before crossterm memoizes
    // NO_COLOR through parking_lot::Once; the environment is untouched after this.
    unsafe { std::env::remove_var("NO_COLOR") };

    // (1) the five border presets
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
    // trim_fmt variant of the borderless preset (strips trailing spaces)
    let mut t = base_table();
    t.load_preset(presets::NOTHING);
    let trimmed = t.trim_fmt();
    println!(
        "trim_fmt bytes={} fnv={:016x}",
        trimmed.len(),
        fnv1a(trimmed.as_bytes())
    );
    println!("{trimmed}");

    // (2) alignment: three cell-level states plus the column-level default
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
    // Columns 0/1 are right/centre aligned at column level: row0 col0 has no
    // cell-level setting, so the column wins; row1 col0 has one and overrides it.
    t.column_mut(0).unwrap().set_cell_alignment(CellAlignment::Right);
    t.column_mut(1).unwrap().set_cell_alignment(CellAlignment::Center);
    t.column_mut(2).unwrap().set_cell_alignment(CellAlignment::Right);
    show("align/precedence", &t);

    // (3a) Dynamic + Fixed constraints, wrapping
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

    // (3b) DynamicFullWidth + Percentage (120 clamped to 100)
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

    // (4) width anchors: CJK/kana/emoji/combining marks across three alignments
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

    // (4b) multi-line cells + a custom delimiter + max_height truncation
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

    // (5) conditional styling (enforce_styling locks ANSI; ESC becomes "\e")
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
    // no_tty suppression path: the same styled table without enforce renders no ESC
    let mut plain = Table::new();
    plain.force_no_tty();
    plain.load_preset(presets::UTF8_FULL);
    plain.set_header(vec![Cell::new("fg").fg(Color::Green)]);
    plain.add_row(vec![Cell::new("文字wide🦀").fg(Color::AnsiValue(202))]);
    let s_plain = plain.to_string();
    println!("styled/suppressed esc={}", s_plain.matches('\u{1b}').count());
    println!("{s_plain}");

    // (6) edge family
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

    // Ragged rows: short rows are padded, wide rows grow the table.
    let mut rg = Table::new();
    rg.force_no_tty();
    rg.load_preset(presets::UTF8_FULL);
    rg.set_header(vec!["a", "b", "c"]);
    rg.add_row(vec!["1"]);
    rg.add_row(vec!["x", "y", "z", "溢出列"]);
    rg.add_row(vec!["", ""]);
    println!("ragged cols={} rows={}", rg.column_count(), rg.row_count());
    show("edge/ragged", &rg);

    // Predicates add_row_if / add_rows_if (they see the current row count)
    let mut t = base_table();
    t.add_row_if(|n, row: &Vec<&str>| n < 4 && row[0].len() > 3, vec!["gate-pass", "谓词", "1"])
        .add_row_if(|n, _: &Vec<&str>| n >= 99, vec!["gate-fail", "谓词", "2"])
        .add_rows_if(
            |n, rows: &Vec<Vec<&str>>| n % 2 == 0 || rows.len() > 1,
            vec![vec!["bulk-a", "批", "3"], vec!["bulk-b", "量", "4"]],
        );
    show("edge/predicates", &t);

    // Style roundtrip: preset -> modifier -> export -> reload -> same render
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

    // set_style single-point override + style query + remove_style fallback
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
