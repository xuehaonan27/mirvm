#!/usr/bin/env mirvm
---
[dependencies]
unicode-segmentation = "1"
unicode-normalization = "0.1"
---
// unicode-segmentation 1.13 + unicode-normalization 0.1.25（UAX 系查表双雄）差分。
//
// 覆盖清单：
//   ① grapheme_indices 扩展簇分段：ZWJ 家庭、ZWJ+VS16 复合（吻）、RI 旗对、
//     keycap、VS16、肤色修饰、天城文 conjunct、组合叠加、CRLF、CJK、谚文、
//     RTL(希/阿) 混排、数字环境、未分配码点+非字符、default-ignorable 裸连
//     ——逐段 [start,end) + escape_debug。
//   ② split_word_bound_indices + unicode_words：小数/千分位/货币/百分号/
//     科学计数/进制、缩写点/连字符/撇号、email/URL、CJK 混数字、RTL 混数字、
//     emoji 串、ZWSP 与 SOFT HYPHEN 边界。
//   ③ unicode_sentences：Mr./Dr./St./Vol./Prof. 缩写、省略号、问号叹号、
//     CJK 句读、希伯来句读、阿拉伯问号。
//   ④ NFD/NFC/NFKD/NFKC 四形式：预组合/全组合序列、Å/ANGSTROM/A+ring 三衣、
//     谚文音节与 conjoining jamo 互转、连字/上标/全角/带圈数字/分数/平方单位
//     （compat 专属）、deva 可分解预组合、未分配与非字符恒等、
//     default-ignorable 恒等；is_nfd/nfc/nfkd/nfkc quick check（0.1.25 顶层
//     自由函数）+ is_nfc_quick 三态枚举；四形式互转往返布尔断言；
//     cjk_compat_variants 变异选择子替换。
//   ⑤ char::{compose, decompose_canonical, decompose_compatible,
//     canonical_combining_class, is_combining_mark}：含谚文算术组合与 None。
//   ⑥ 流式与一次性等价：整块一次性 nfc 基准；逐 char 增量拉流 == 一次性断言；
//     安全切点（空格 starter 边界）逐块 nfc 裸露拼接与 stream_safe 包裹拼接
//     均 == 一次性断言；危险切点（斩在组合序列腰上）裸露拼接发散、
//     stream_safe 不担急救（UAX15-D4 只管 >30 连非起始符插 CGJ）如实打印；
//     31 连 U+0301 触发 stream_safe 插入 U+034F CGJ 的计数断言；
//     is_nfc_stream_safe/is_nfd_stream_safe 布尔。
//   ⑦ 未分配/非字符/私用/default-ignorable 单码点专测行：grapheme 数、
//     word 数、is_public_assigned、is_combining_mark、ccc、nfkc 是否改写。
//   末尾全结果字节 fnv 指纹。
//
// 确定性：全部输入为定值字面量；输出仅计数/字节偏移/escape_debug 文本/布尔/
//   枚举 Debug/版本号常量；无浮点、无随机、无时间、无地址、无线程、
//   无 HashMap 迭代；不建临时文件；stderr 为空。
// 绕行/钉版本：无。unicode-segmentation 1.13.3 / unicode-normalization 0.1.25
//   均纯 Rust 静态表（normalization 仅依赖纯 Rust tinyvec）；两 crate 本次
//   均解析到 Unicode 17.0.0 表，三维使用同一 Cargo.lock 与构建目录。
use unicode_normalization::char::{
    canonical_combining_class, compose, decompose_canonical, decompose_compatible,
    is_combining_mark, is_public_assigned,
};
use unicode_normalization::{
    is_nfc, is_nfc_quick, is_nfc_stream_safe, is_nfd, is_nfd_stream_safe, is_nfkc, is_nfkd,
    UnicodeNormalization, UNICODE_VERSION,
};
use unicode_segmentation::UnicodeSegmentation;

/// 内联 FNV-1a（全结果字节锚定）。
struct Sink(u64);

impl Sink {
    fn feed(&mut self, s: &str) {
        for &b in s.as_bytes().iter().chain([0x1f].iter()) {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

/// escape_debug 的短路函数（段文本安全形式）。
fn ed(s: &str) -> String {
    s.escape_debug().collect()
}

/// 单字符 → "U+xxxx"。
fn cu(c: char) -> String {
    format!("U+{:04X}", c as u32)
}

fn main() {
    let mut sink = Sink(0xcbf29ce484222325);
    println!(
        "tables segmentation={:?} normalization={:?}",
        unicode_segmentation::UNICODE_VERSION,
        UNICODE_VERSION
    );

    // ① grapheme 簇分段
    println!("== graphemes ==");
    #[rustfmt::skip]
    let gtexts: &[&str] = &[
        "नमस्ते दुनिया",                            // 天城文（含 conjunct）
        "क्‍ष ज्ञ क्ष",                                 // virama+ZWJ 显式 conjunct
        "👨\u{200D}👩\u{200D}👧\u{200D}👦",                     // ZWJ 家庭
        "👩\u{200D}❤\u{FE0F}‍\u{200D}💋\u{200D}👨",               // ZWJ+VS16 复合
        "🇨🇳🇺🇸",                             // 两对 RI 旗
        "1️⃣2️⃣ ✔️ 👍🏽 ❤\u{FE0F}",                    // keycap/VS16/肤色
        "e\u{0301}cole a\u{0300}\u{0301}\u{0302}ma", // 组合叠加
        "Ångström Mötley Crüe",                     // 预组合拉丁
        "שלום world שלום12",                        // 希伯来 RTL 混排
        "مرحبا بالعالم ٣٤٥",                        // 阿拉伯 RTL + 阿拉伯数字
        "日本語のテキスト、テスト123",               // CJK 混数字
        "한국어 한글 테스트",                        // 谚文
        "abc\r\ndef\tghi",                          // CRLF 单簇 + TAB 控制
        "\u{0378}\u{2FE1}\u{FDD0}\u{10FFFF}",       // 未分配/非字符
        "\u{AD}\u{034F}\u{200B}\u{200D}\u{2060}\u{FE0F}", // default-ignorable 裸连
        "3.14 & 1,000,000",                         // 数字环境
    ];
    for (ti, s) in gtexts.iter().enumerate() {
        let gs: Vec<(usize, &str)> = s.grapheme_indices(true).collect();
        println!(
            "g[{ti}] bytes={} chars={} graphemes={}",
            s.len(),
            s.chars().count(),
            gs.len()
        );
        for (i, (off, seg)) in gs.iter().enumerate() {
            println!("g[{ti}].{i} [{}, {}) {}", off, off + seg.len(), ed(seg));
            sink.feed(seg);
        }
    }

    // ② word 边界（unicode_words 实词 + split_word_bounds 全界两遍）
    println!("== words ==");
    #[rustfmt::skip]
    let wtexts: &[&str] = &[
        "The quick brown fox jumps over 3.14 lazy dogs.",
        "don't split well-known words, Mr. O'Brien's e-mail",
        "价格¥1,299.99打9折，满200.00减30%，共12345件",
        "שלום עולם peace שלום12 תודה",
        "مرحبا بالعالم سنة ٢٠٢٦ م.",
        "email@example.com https://rust-lang.org/crates/unicode",
        "🚀🚀 blast off 3...2...1! (go) #tag @user",
        "\u{200B}zero\u{200B}width soft\u{AD}hyphen\u{200B}",
        "1.5e10 0xFF 100% $9.99 +42 (3.5)",
    ];
    for (ti, s) in wtexts.iter().enumerate() {
        let words: Vec<&str> = s.unicode_words().collect();
        println!("w[{ti}] bytes={} words={}", s.len(), words.len());
        for (i, w) in words.iter().enumerate() {
            println!("w[{ti}].word.{i} = {}", ed(w));
            sink.feed(w);
        }
        let bs: Vec<(usize, &str)> = s.split_word_bound_indices().collect();
        println!("w[{ti}] bounds={}", bs.len());
        for (i, (off, seg)) in bs.iter().enumerate() {
            println!("w[{ti}].bound.{i} [{}, {}) {}", off, off + seg.len(), ed(seg));
            sink.feed(seg);
        }
    }

    // ③ sentence 边界
    println!("== sentences ==");
    #[rustfmt::skip]
    let stexts: &[&str] = &[
        "Mr. Smith bought cheapshoes.com for $1.0 million. Then Dr. Jones left!",
        "Wait... what?! OK done. (Yes.) End",
        "第一句是测试。第二句！第三句吗？「嗯。」哈哈。",
        "מה יש? כלום! בסדר גמור.",
        "مرحبا. كيف حالك؟ أنا بخير! شكرا.",
        "St. Augustine wrote. Vol. 2 was revised... Prof. X agreed? Yes!",
    ];
    for (ti, s) in stexts.iter().enumerate() {
        let ss: Vec<&str> = s.unicode_sentences().collect();
        println!("s[{ti}] bytes={} sentences={}", s.len(), ss.len());
        for (i, seg) in ss.iter().enumerate() {
            println!("s[{ti}].{i} = {}", ed(seg));
            sink.feed(seg);
        }
    }

    // ④ 四形式 + quick check + 互转往返 + cjk_compat_variants
    println!("== normalize ==");
    #[rustfmt::skip]
    let ntexts: &[&str] = &[
        "Ângström Å for café",                 // 预组合；Å 拉丁
        "\u{212B}\u{00C5}A\u{030A} triple",    // ANGSTROM / Å / A+ring 三方
        "e\u{0301} a\u{0300}\u{0301} n\u{0303} o\u{0308}", // 全组合序列
        "한 국 어 한글",                         // 谚文音节（已组合）
        "\u{1100}\u{1161}\u{11A8}\u{1101}\u{1161}\u{11A8}", // conjoining jamo 原串
        "ﬁle ﬂow ofﬁce ﬀ",                     // 连字（compat 专属）
        "x² + ³√2 ≈ Åℌ ℝ",                     // 上标/双线体（compat 专属）
        "Ｆｕｌｌｗｉｄｔｈ　ＡＢＣ１２３",       // 全角+全角空格
        "①⑫㉑ ½⅜ ㍈㎡ ㌀",                      // 带圈/分数/平方单位
        "\u{0958}\u{09DC}\u{0A33} deva",       // Indic 可分解预组合
        "\u{0378}\u{FDD0}\u{10FFFF}",          // 未分配/非字符恒等
        "\u{034F}\u{200D}\u{2060}\u{FE0F}\u{AD}", // default-ignorable 恒等
        "\u{F901}\u{2F801}",                   // CJK compatibility ideograph
    ];
    for (ti, s) in ntexts.iter().enumerate() {
        let nfd: String = s.nfd().collect();
        let nfc: String = s.nfc().collect();
        let nfkd: String = s.nfkd().collect();
        let nfkc: String = s.nfkc().collect();
        println!("n[{ti}] in   c={} {}", s.chars().count(), ed(s));
        println!("n[{ti}] nfd  c={} {}", nfd.chars().count(), ed(&nfd));
        println!("n[{ti}] nfc  c={} {}", nfc.chars().count(), ed(&nfc));
        println!("n[{ti}] nfkd c={} {}", nfkd.chars().count(), ed(&nfkd));
        println!("n[{ti}] nfkc c={} {}", nfkc.chars().count(), ed(&nfkc));
        println!(
            "n[{ti}] qc nfd={} nfc={} nfkd={} nfkc={} nfcq={:?}",
            is_nfd(s),
            is_nfc(s),
            is_nfkd(s),
            is_nfkc(s),
            is_nfc_quick(s.chars())
        );
        // 四形式互转往返断言
        let d2c: String = nfd.chars().nfc().collect();
        let c2d: String = nfc.chars().nfd().collect();
        let kd2kc: String = nfkd.chars().nfkc().collect();
        let kc2kd: String = nfkc.chars().nfkd().collect();
        println!(
            "n[{ti}] rt d2c={} c2d={} kd2kc={} kc2kd={}",
            d2c == nfc,
            c2d == nfd,
            kd2kc == nfkc,
            kc2kd == nfkd
        );
        sink.feed(&nfd);
        sink.feed(&nfc);
        sink.feed(&nfkd);
        sink.feed(&nfkc);
    }
    // cjk_compat_variants：compat ideograph → 标准形 + 变异选择子
    for c in ['\u{F901}', '\u{2F801}', '任', '語'] {
        let v: String = [c].into_iter().cjk_compat_variants().collect();
        println!("cjkvar {} => c={} {}", cu(c), v.chars().count(), ed(&v));
        sink.feed(&v);
    }

    // ⑤ char:: 自由函数面
    println!("== char-compose ==");
    for (a, b) in [
        ('e', '\u{0301}'),
        ('A', '\u{0308}'),
        ('o', '\u{0302}'),
        ('n', '\u{0303}'),
        ('a', 'b'),
        ('\u{1100}', '\u{1161}'),
        ('가', '\u{11A8}'),
        (' ', '\u{0301}'),
    ] {
        match compose(a, b) {
            Some(c) => println!("compose {} {} => {}", cu(a), cu(b), cu(c)),
            None => println!("compose {} {} => -", cu(a), cu(b)),
        }
    }
    for c in ['Å', '\u{212B}', 'é', '긱', 'ﬁ', '²', '任', '\u{FDD0}'] {
        let mut dc: Vec<String> = Vec::new();
        decompose_canonical(c, |d| dc.push(cu(d)));
        let mut dk: Vec<String> = Vec::new();
        decompose_compatible(c, |d| dk.push(cu(d)));
        println!(
            "decompose {} canon={} compat={}",
            cu(c),
            dc.join(" "),
            dk.join(" ")
        );
        sink.feed(&dc.join(" "));
        sink.feed(&dk.join(" "));
    }
    for c in ['a', '\u{0301}', '\u{0315}', '\u{0345}', '天', '\u{034F}'] {
        println!(
            "lookup {} ccc={} mark={} assigned={}",
            cu(c),
            canonical_combining_class(c),
            is_combining_mark(c),
            is_public_assigned(c)
        );
    }

    // ⑥ 流式与一次性等价
    println!("== streaming ==");
    // 整块一次性 nfc 基准（混搭：组合符 + 预组合 + jamo + 分词环境）
    let whole = "Que\u{0301}rie a\u{0308}nsi \u{1101}\u{1161} ko\u{0303}x co\u{0315}\u{0300}m";
    let oneshot: String = whole.nfc().collect();
    println!("stream oneshot = {}", ed(&oneshot));
    // 逐 char 增量拉流（同迭代器手工步进累计）== 一次性
    let mut inc = String::new();
    for ch in whole.nfc() {
        inc.push(ch);
    }
    println!("stream pull-1eq = {}", inc == oneshot);
    // 安全切点：空格处（starter 边界）分两块逐块 nfc，裸露拼接与
    // stream_safe 包裹拼接均应与一次性一致
    let (s1, s2) = whole.split_once(' ').unwrap();
    let s2ws = format!(" {s2}"); // 第二块含前导空格，还原字节
    let safeb_naive: String = [s1, &s2ws].iter().map(|c| c.nfc().collect::<String>()).collect();
    let safeb_ss: String = [s1, &s2ws]
        .iter()
        .map(|c| c.stream_safe().nfc().collect::<String>())
        .collect();
    println!(
        "stream cut-safe naive eq={} safe eq={}",
        safeb_naive == oneshot,
        safeb_ss == oneshot
    );
    // 危险切点：斩在 starter 'e' 与其组合符 U+0301 之间
    let (h1, h2) = (&whole[..3], &whole[3..]); // "Que" | "\u{0301}rie ..."
    let haz_naive: String = [h1, h2].iter().map(|c| c.nfc().collect::<String>()).collect();
    let haz_ss: String = [h1, h2]
        .iter()
        .map(|c| c.stream_safe().nfc().collect::<String>())
        .collect();
    let haz_ss_stripped: String = haz_ss.chars().filter(|&c| c != '\u{034F}').collect();
    println!("stream cut-haz naive = {} eq={}", ed(&haz_naive), haz_naive == oneshot);
    println!(
        "stream cut-haz safe  = {} eq={}",
        ed(&haz_ss),
        haz_ss_stripped == oneshot
    );
    sink.feed(&oneshot);
    sink.feed(&safeb_ss);
    sink.feed(&haz_ss);
    // UAX15-D4：连非起始符 >30 → stream_safe 插入 U+034F CGJ
    let runs: Vec<String> = [30usize, 31, 45, 61]
        .iter()
        .map(|&n| core::iter::repeat_n('\u{0301}', n).collect())
        .collect();
    for r in &runs {
        let ss: String = r.chars().stream_safe().collect();
        let cgj = ss.chars().filter(|&c| c == '\u{034F}').count();
        println!(
            "stream run n={} ss-c={} cgj={} nfc-ss-true={}",
            r.chars().count(),
            ss.chars().count(),
            cgj,
            is_nfc_stream_safe(&ss)
        );
    }
    println!(
        "stream-safe qc nfc={} nfd={}",
        is_nfc_stream_safe(whole),
        is_nfd_stream_safe(whole)
    );

    // ⑦ 未分配 / 非字符 / 私用 / default-ignorable 单码点专测
    println!("== edge-cp ==");
    for c in [
        '\u{0378}', // 未分配（希腊区洞）
        '\u{0382}',
        '\u{0838}',
        '\u{2FE1}', // 未分配
        '\u{FDD0}', // 非字符
        '\u{FFFE}',
        '\u{FFFF}',
        '\u{10FFFF}', // 非字符（面顶）
        '\u{E000}',   // 私用区
        '\u{AD}',     // SOFT HYPHEN
        '\u{034F}',   // CGJ
        '\u{200B}',   // ZWSP
        '\u{200D}',   // ZWJ
        '\u{2060}',   // WORD JOINER
        '\u{FE0F}',   // VS16
        '\u{180E}',   // MONGOLIAN VOWEL SEPARATOR
        '\u{E0001}',  // LANGUAGE TAG
    ] {
        let s = c.to_string();
        let nfkc: String = s.nfkc().collect();
        println!(
            "cp {} g={} w={} assigned={} mark={} ccc={} qc={}{}{}{} kc-ch={}",
            cu(c),
            s.graphemes(true).count(),
            s.unicode_words().count(),
            is_public_assigned(c),
            is_combining_mark(c),
            canonical_combining_class(c),
            is_nfd(&s) as u8,
            is_nfc(&s) as u8,
            is_nfkd(&s) as u8,
            is_nfkc(&s) as u8,
            nfkc != s
        );
        sink.feed(&s);
    }

    println!("corpus fnv = {:016x}", sink.0);
}
