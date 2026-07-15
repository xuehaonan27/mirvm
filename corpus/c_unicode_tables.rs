#!/usr/bin/env mirvm
---
[dependencies]
unicode-normalization = "0.1"
unicode-segmentation = "1"
unicode-bidi = "0.3"
unicode-width = "0.2"
---
// unicode 表驱动四门面：
//   normalization —— NFC/NFD/NFKC/NFKD 对组合字符/Hangul 音节/连字的归一（码点 hex）
//   segmentation  —— graphemes/words/sentences 切分（emoji+ZWJ、组合旗标、肤色修饰符）
//   bidi          —— 英阿混排 per-char levels + visual reorder
//   width         —— CJK/emoji/控制符宽度 + 大范围码点求和（大表查询压力）
use unicode_bidi::{BidiInfo, Level, get_base_direction};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{canonical_combining_class, compose, is_combining_mark};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn hex(s: &str) -> String {
    s.chars()
        .map(|c| format!("{:04X}", c as u32))
        .collect::<Vec<_>>()
        .join(" ")
}

fn main() {
    println!("unicode-version = {:?}", unicode_normalization::UNICODE_VERSION);

    // ==== ① normalization：组合字符 / Hangul / 连字 / 兼容字符 ====
    let norm_cases: &[(&str, &str)] = &[
        ("comb-acute", "e\u{0301}"),               // e + COMBINING ACUTE
        ("precomposed", "\u{00E9}"),               // é
        ("double-comb", "o\u{0302}\u{0301}"),      // ô + acute 叠加
        ("reorder", "a\u{0315}\u{0300}"),          // ccc 232 在前 230 在后 → NFD 重排
        ("hangul-jamo", "\u{1100}\u{1161}\u{11A8}"), // 각 → 각
        ("hangul-syll-trail", "\u{AC01}\u{11A8}"), // 각 + ᆨ → 갂
        ("hangul-word", "한국어"),                 // 预组合音节词
        ("fi-ligature", "\u{FB01}"),               // ﬁ → fi (仅兼容分解)
        ("circled-digits", "\u{2461}\u{2462}"),    // ②③ → 2 3
        ("superscript", "x\u{00B2}y\u{2075}"),     // x²y⁵ → x2y5
        ("square-kana", "\u{3300}"),               // ㌀ → カタカナ
        ("angstrom", "\u{212B}"),                  // Å(ANGSTROM) → 0041 030A
        ("fullwidth", "\u{FF21}\u{FF42}"),         // Ａｂ → Ab
        ("compat-cjk", "\u{FA10}"),                // 塚 → 塚 (canonical)
        ("arabic-lig", "\u{FDFA}"),                // ﷺ → 18 码点兼容展开
        ("devanagari", "\u{0915}\u{093C}\u{093F}"), // क + nukta + vowel sign
        ("tibetan-vowel", "\u{0F40}\u{0F71}\u{0F72}"), // ccc 129/130 排序
        ("emoji-untouched", "👍🏽\u{1F1E8}\u{1F1F3}"), // emoji/旗标不归一化
        ("mixed-line", "Cafe\u{0301} \u{AC00}\u{1100}\u{1161}\u{FB01}"),
    ];
    for &(name, input) in norm_cases {
        let nfc: String = input.nfc().collect();
        let nfd: String = input.nfd().collect();
        let nfkc: String = input.nfkc().collect();
        let nfkd: String = input.nfkd().collect();
        println!("norm[{name}] in   = {}", hex(input));
        println!("  nfc  = {}", hex(&nfc));
        println!("  nfd  = {}", hex(&nfd));
        println!("  nfkc = {}", hex(&nfkc));
        println!("  nfkd = {}", hex(&nfkd));
        // 不变量交给两侧机器各自计算，由 diff 裁决
        println!(
            "  inv nfc^2={} nfkc^2={} nfc(nfd)={} len={}/{}/{}/{}",
            input.nfc().nfc().collect::<String>() == nfc,
            input.nfkc().nfkc().collect::<String>() == nfkc,
            input.nfd().nfc().collect::<String>() == nfc,
            nfc.chars().count(),
            nfd.chars().count(),
            nfkc.chars().count(),
            nfkd.chars().count(),
        );
    }
    // 低层 char API：组合类 / 合成表 / 组合标记判定
    for c in ['\u{0301}', '\u{0315}', '\u{0300}', 'a', '\u{093C}', '\u{0F71}'] {
        println!(
            "ccc U+{:04X} = {} mark={}",
            c as u32,
            canonical_combining_class(c),
            is_combining_mark(c)
        );
    }
    println!("compose(a,grave) = {:?}", compose('a', '\u{0300}'));
    println!("compose(a,b) = {:?}", compose('a', 'b'));
    println!(
        "compose(hangul) = {:?}",
        compose('\u{1100}', '\u{1161}')
    );
    // Hangul 批量：64 个音节的 NFD 总码点数 + NFC roundtrip 计数
    let mut dcnt = 0usize;
    let mut rt = 0usize;
    for cp in 0xAC00u32..0xAC40 {
        let s: String = char::from_u32(cp).into_iter().collect();
        dcnt += s.nfd().count();
        if s.nfd().nfc().collect::<String>() == s {
            rt += 1;
        }
    }
    println!("hangul batch: nfd-total={dcnt} roundtrip={rt}/64");

    // ==== ② segmentation：graphemes / words / sentences ====
    let seg_cases: &[&str] = &[
        "👨‍👩‍👧‍👦",                      // ZWJ 家庭序列 = 1 grapheme
        "\u{1F1E8}\u{1F1F3}\u{1F1FA}\u{1F1F8}", // 🇨🇳🇺🇸 组合旗标 = 2
        "e\u{0301} cafe\u{0301} 日本語",       // 组合符 + CJK
        "Hello, world! It's a test.",
        "The quick (brown) fox can't jump. Really?! Yes.",
        "中文没有空格。第二句在这里！第三句吗？",
        "👍🏽👍 thumbs up",                    // 肤色修饰符
        "क्‍ष test 123",                     // 梵文合字(ZWJ) + latin
    ];
    for &t in seg_cases {
        let gs: Vec<&str> = t.graphemes(true).collect();
        println!("seg in = {}", hex(t));
        println!("  graphemes = {}", gs.len());
        for (off, g) in t.grapheme_indices(true) {
            println!("    @{off} {}", hex(g));
        }
        let ws: Vec<&str> = t.unicode_words().collect();
        println!("  words = {} {:?}", ws.len(), ws);
        let ss: Vec<&str> = t.unicode_sentences().collect();
        println!("  sentences = {} {:?}", ss.len(), ss);
    }

    // ==== ③ bidi：英阿/英希混排 levels + visual 序 ====
    let bidi_cases: &[(&str, Option<Level>)] = &[
        ("hello مرحبا world", None),
        ("abc 123 تجربة DEF", None),
        ("مرحبا بالعالم hello", None),
        ("hello مرحبا", Some(Level::rtl())),
        ("hello مرحبا", Some(Level::ltr())),
        ("123 456", None),
        ("אבג דהו xyz", None),
    ];
    for &(text, level) in bidi_cases {
        let info = BidiInfo::new(text, level);
        println!(
            "bidi {:?} default={:?} has_rtl={} base={:?} paras={}",
            text,
            level.map(|l| l.number()),
            info.has_rtl(),
            get_base_direction(text),
            info.paragraphs.len()
        );
        for para in &info.paragraphs {
            println!(
                "  para level={} range={}..{}",
                para.level.number(),
                para.range.start,
                para.range.end
            );
            // per-char level（取每字符首字节的 level）
            let per_char: Vec<u8> = text[para.range.clone()]
                .char_indices()
                .map(|(i, _)| info.levels[para.range.start + i].number())
                .collect();
            println!("  levels = {per_char:?}");
            let visual = info.reorder_line(para, para.range.clone());
            println!("  visual = {}", hex(&visual));
            let (runs_levels, runs) = info.visual_runs(para, para.range.clone());
            let desc: Vec<(usize, usize, u8)> = runs
                .iter()
                .enumerate()
                .map(|(i, r)| (r.start, r.end, runs_levels[i].number()))
                .collect();
            println!("  runs = {desc:?}");
        }
    }

    // ==== ④ width：CJK / emoji / 控制符 / 组合符 ====
    let width_chars: &[char] = &[
        'a', 'Z', '7', ' ', '汉', '字', 'カ', 'ｱ', 'Ａ',
        '😀', '👍', '\u{1F3FD}',           // emoji + 肤色修饰符
        '\u{0301}', '\u{200D}',            // 组合符 / ZWJ
        '\u{0}', '\u{7}', '\u{1B}', '\u{7F}', '\t', // 控制符
        '·', '§', '±',                     // Ambiguous（CJK 语境变宽）
        '\u{AD}', '·',                     // SOFT HYPHEN
        '\u{2028}', '\u{3000}',            // LINE SEP / 表意空格
    ];
    for &c in width_chars {
        println!(
            "width U+{:04X} = {:?} cjk = {:?}",
            c as u32,
            UnicodeWidthChar::width(c),
            UnicodeWidthChar::width_cjk(c)
        );
    }
    let width_strs: &[&str] = &[
        "hello world",
        "汉字テスト",
        "한국어",
        "😀👍🏽",
        "e\u{0301}",
        "\t\n",
        "abc汉字😀def",
        "👨‍👩‍👧‍👦 family 👪",
        "ｱｲｳｴｵ half",
    ];
    for &s in width_strs {
        println!(
            "strw {} = {} cjk {}",
            hex(s),
            UnicodeWidthStr::width(s),
            UnicodeWidthStr::width_cjk(s)
        );
    }
    // 大范围码点扫描求和：大表二分/区间查询的覆盖压力
    let ranges: &[(u32, u32)] = &[
        (0x20, 0x7F),       // ASCII printable
        (0x300, 0x370),     // combining marks
        (0x2E80, 0x3400),   // CJK radicals
        (0x4E00, 0xA000),   // CJK 统一表意（约 20K 码点）
        (0xAC00, 0xD800),   // Hangul 音节全表
        (0x1F300, 0x1F650), // emoji 主块
    ];
    let mut grand = 0usize;
    let mut grand_cjk = 0usize;
    for &(lo, hi) in ranges {
        let mut sum = 0usize;
        let mut sum_cjk = 0usize;
        let mut n = 0usize;
        for cp in lo..hi {
            if let Some(c) = char::from_u32(cp) {
                n += 1;
                sum += UnicodeWidthChar::width(c).unwrap_or(0);
                sum_cjk += UnicodeWidthChar::width_cjk(c).unwrap_or(0);
            }
        }
        grand += sum;
        grand_cjk += sum_cjk;
        println!("range {lo:04X}..{hi:04X} chars={n} width={sum} cjk={sum_cjk}");
    }
    println!("width grand = {grand} cjk {grand_cjk}");
}
