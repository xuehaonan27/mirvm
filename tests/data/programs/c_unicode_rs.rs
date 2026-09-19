#!/usr/bin/env mirvm
---
[dependencies]
unicode-segmentation = "1"
unicode-normalization = "0.1"
---
// Differential fixture for unicode-segmentation 1.13 + unicode-normalization 0.1.25.
//
// Coverage:
//   ① grapheme_indices extended clusters: ZWJ family, ZWJ+VS16 composite (kiss),
//     RI flag pairs, keycap, VS16, skin tone, Devanagari conjunct, stacked
//     combining marks, CRLF, CJK, Hangul, RTL (Hebrew/Arabic), digit context,
//     unassigned + noncharacters, bare default-ignorables -- [start,end) + escape_debug.
//   ② split_word_bound_indices + unicode_words: decimal/thousands/currency/percent,
//     scientific notation and radix, abbreviation dots/hyphens/apostrophes,
//     email/URL, CJK and RTL mixed with digits, emoji runs, ZWSP/SOFT HYPHEN edges.
//   ③ unicode_sentences: Mr./Dr./St./Vol./Prof., ellipsis, question and exclamation
//     marks, CJK sentence punctuation, Hebrew punctuation, Arabic question mark.
//   ④ NFD/NFC/NFKD/NFKC: precomposed/fully decomposed, the Å/ANGSTROM/A+ring
//     triple, Hangul syllables <-> conjoining jamo, ligatures, superscripts,
//     fullwidth, circled digits, fractions and square units (compat-only),
//     decomposable Devanagari precomposed, unassigned/noncharacter and
//     default-ignorable identity; is_nfd/nfc/nfkd/nfkc quick check (top-level
//     free fn in 0.1.25) + is_nfc_quick enum; round trips; cjk_compat_variants.
//   ⑤ char::{compose, decompose_canonical, decompose_compatible,
//     canonical_combining_class, is_combining_mark}: Hangul arithmetic composition and None.
//   ⑥ streaming equals one-shot: whole-buffer nfc baseline; per-char incremental
//     pull == one-shot; safe cut (space starter) chunk-wise nfc, bare and
//     stream_safe-wrapped concatenation both == one-shot; hazardous cut (through
//     a combining sequence) diverges bare and stream_safe does not rescue it
//     (UAX15-D4 inserts CGJ only after >30 non-starters), printed as-is; 31
//     consecutive U+0301 counts the CGJ insert; stream-safe booleans.
//   ⑦ single-code-point rows for unassigned/noncharacter/private-use/default-
//     ignorable: grapheme count, word count, is_public_assigned, mark, ccc, nfkc rewrite.
//   Trailing FNV fingerprint over every result byte.
//
// Determinism: every input is a fixed literal; output is only counts, byte offsets,
//   escape_debug text, booleans, enum Debug and version constants; no floats, no
//   randomness, no time, no addresses, no threads, no HashMap iteration; no temp files; stderr empty.
// Bypasses/version pins: none beyond these. unicode-segmentation 1.13.3 /
//   unicode-normalization 0.1.25 are pure-Rust static tables (normalization depends
//   only on pure-Rust tinyvec); both resolve to Unicode 17.0.0, and all three dimensions share one Cargo.lock and build directory.
use unicode_normalization::char::{
    canonical_combining_class, compose, decompose_canonical, decompose_compatible,
    is_combining_mark, is_public_assigned,
};
use unicode_normalization::{
    is_nfc, is_nfc_quick, is_nfc_stream_safe, is_nfd, is_nfd_stream_safe, is_nfkc, is_nfkd,
    UnicodeNormalization, UNICODE_VERSION,
};
use unicode_segmentation::UnicodeSegmentation;

/// Inline FNV-1a over every result byte, used to anchor the output.
struct Sink(u64);

impl Sink {
    fn feed(&mut self, s: &str) {
        for &b in s.as_bytes().iter().chain([0x1f].iter()) {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

/// Shorthand for `escape_debug` over a segment's text.
fn ed(s: &str) -> String {
    s.escape_debug().collect()
}

/// Single char -> "U+xxxx".
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

    // ① grapheme cluster segmentation
    println!("== graphemes ==");
    #[rustfmt::skip]
    let gtexts: &[&str] = &[
        "नमस्ते दुनिया",                            // Devanagari (includes conjunct)
        "क्‍ष ज्ञ क्ष",                                 // virama+ZWJ explicit conjunct
        "👨\u{200D}👩\u{200D}👧\u{200D}👦",                     // ZWJ family
        "👩\u{200D}❤\u{FE0F}‍\u{200D}💋\u{200D}👨",               // ZWJ+VS16 composite
        "🇨🇳🇺🇸",                             // two RI flag pairs
        "1️⃣2️⃣ ✔️ 👍🏽 ❤\u{FE0F}",                    // keycap/VS16/skin tone
        "e\u{0301}cole a\u{0300}\u{0301}\u{0302}ma", // stacked combining marks
        "Ångström Mötley Crüe",                     // precomposed Latin
        "שלום world שלום12",                        // Hebrew RTL mixed run
        "مرحبا بالعالم ٣٤٥",                        // Arabic RTL + Arabic-Indic digits
        "日本語のテキスト、テスト123",               // CJK with digits
        "한국어 한글 테스트",                        // Hangul
        "abc\r\ndef\tghi",                          // CRLF single cluster + TAB control
        "\u{0378}\u{2FE1}\u{FDD0}\u{10FFFF}",       // unassigned/noncharacter
        "\u{AD}\u{034F}\u{200B}\u{200D}\u{2060}\u{FE0F}", // bare default-ignorable concatenation
        "3.14 & 1,000,000",                         // digit context
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

    // ② word boundaries (unicode_words content words + split_word_bounds over all boundaries)
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

    // ③ sentence boundaries
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

    // ④ the four forms + quick check + round trips + cjk_compat_variants
    println!("== normalize ==");
    #[rustfmt::skip]
    let ntexts: &[&str] = &[
        "Ângström Å for café",                 // precomposed; Å Latin
        "\u{212B}\u{00C5}A\u{030A} triple",    // ANGSTROM / Å / A+ring triple
        "e\u{0301} a\u{0300}\u{0301} n\u{0303} o\u{0308}", // fully decomposed sequence
        "한 국 어 한글",                         // Hangul syllable (already composed)
        "\u{1100}\u{1161}\u{11A8}\u{1101}\u{1161}\u{11A8}", // conjoining jamo raw string
        "ﬁle ﬂow ofﬁce ﬀ",                     // ligature (compat-only)
        "x² + ³√2 ≈ Åℌ ℝ",                     // superscript/double-struck (compat-only)
        "Ｆｕｌｌｗｉｄｔｈ　ＡＢＣ１２３",       // fullwidth + ideographic space
        "①⑫㉑ ½⅜ ㍈㎡ ㌀",                      // circled/fraction/square units
        "\u{0958}\u{09DC}\u{0A33} deva",       // decomposable Indic precomposed
        "\u{0378}\u{FDD0}\u{10FFFF}",          // unassigned/noncharacter identity
        "\u{034F}\u{200D}\u{2060}\u{FE0F}\u{AD}", // default-ignorable identity
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
        // round-trip booleans between the four forms
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
    // cjk_compat_variants: compat ideograph -> standard form + variation selector
    for c in ['\u{F901}', '\u{2F801}', '任', '語'] {
        let v: String = [c].into_iter().cjk_compat_variants().collect();
        println!("cjkvar {} => c={} {}", cu(c), v.chars().count(), ed(&v));
        sink.feed(&v);
    }

    // ⑤ char:: free-function surface
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

    // ⑥ streaming vs one-shot equivalence
    println!("== streaming ==");
    // whole-buffer one-shot nfc baseline (mix: combining marks + precomposed + jamo + word-break context)
    let whole = "Que\u{0301}rie a\u{0308}nsi \u{1101}\u{1161} ko\u{0303}x co\u{0315}\u{0300}m";
    let oneshot: String = whole.nfc().collect();
    println!("stream oneshot = {}", ed(&oneshot));
    // per-char incremental pull (manual steps over the same iterator) == one-shot
    let mut inc = String::new();
    for ch in whole.nfc() {
        inc.push(ch);
    }
    println!("stream pull-1eq = {}", inc == oneshot);
    // safe cut point: split into two chunks at a space (starter boundary), nfc each;
    // bare concatenation and stream_safe-wrapped concatenation both match one-shot
    let (s1, s2) = whole.split_once(' ').unwrap();
    let s2ws = format!(" {s2}"); // second chunk keeps the leading space so the bytes are restored
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
    // hazardous cut point: split between starter 'e' and its combining mark U+0301
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
    // UAX15-D4: >30 consecutive non-starters -> stream_safe inserts U+034F CGJ
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

    // ⑦ single-code-point rows: unassigned / noncharacter / private-use / default-ignorable
    println!("== edge-cp ==");
    for c in [
        '\u{0378}', // unassigned (hole in the Greek block)
        '\u{0382}',
        '\u{0838}',
        '\u{2FE1}', // unassigned
        '\u{FDD0}', // noncharacter
        '\u{FFFE}',
        '\u{FFFF}',
        '\u{10FFFF}', // noncharacter (top of plane)
        '\u{E000}',   // private-use area
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
