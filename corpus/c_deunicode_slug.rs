#!/usr/bin/env mirvm
---
[dependencies]
deunicode = "1"
slug = "0.1"
# Package name is capitalized: crates.io registers it as `Inflector` (cargo
# reports "no matching package found, perhaps you meant: Inflector" for
# `inflector`); its [lib] name is lowercase, so code uses `use inflector::...`.
Inflector = "0.11"
---
// deunicode 1.6 + slug 0.1 + Inflector 0.11 (the transliteration-table backwater) differential.
// deunicode: table lookup over mapping.txt + pointers.bin; an ASCII fast path and
// Cow borrow/return; unknown characters become a tofu placeholder. slug: per-char
// deunicode_char lookup, folds anything outside [a-z0-9] into a single '-', then
// trims leading/trailing '-'. Inflector 0.11.4: the case family is a pure Rust
// char scan; pluralize/singularize go through a lazy_static + regex rule table
// (24 rules, first hit in reverse order wins) plus a special_cases static match
// and a 202-word UNACCONTABLE static array, so this case also exercises
// lazy_static initialization and regex DFA compilation.
// User-facing name mapping (the crate has no Rails-named API): camelize ->
// to_camel_case/to_pascal_case, underscore -> to_snake_case, pluralize -> to_plural,
// singularize -> to_singular, humanize -> to_sentence_case (Rails humanize's
// separator stripping + first-letter uppercasing), and title case too.
// Coverage: deunicode/deunicode_with_tofu/deunicode_char, three APIs x 12 strings
// (Latin extended / combining marks / Cyrillic / Greek / Japanese + kana / emoji /
// symbols / fullwidth / box drawing / Hangul / private-use unknowns); the slugify
// batch plus empty/all-separator/leading-dash edges; Inflector camelize, the 4
// underscore variants, sentence(humanize)/title, pluralize and singularize over 50
// words each (irregular families ox/man/woman/die/foot/goose/quiz, person~people,
// mouse~mice, -us->-i, -um/-on->-a, -ix->-ices, -f(e)->-ves, uncountables);
// ordinalize/deordinalize, to_foreign_key, demodulize/deconstantize, class/table
// case, the is_* predicates, and the trait method surface.
// Determinism: all table lookups, no time/address/random source; transliterations
// can embed \n, so everything prints via {:?}; no temp files; stderr is empty, and
// the run ends with an FNV fingerprint over all result bytes.
use deunicode::{deunicode, deunicode_char, deunicode_with_tofu};
// Inflector 0.11 does not re-export functions at the top level, so the public
// surface is module-path functions plus the Inflector/InflectorNumbers traits.
use inflector::cases::camelcase::to_camel_case;
use inflector::cases::kebabcase::to_kebab_case;
use inflector::cases::pascalcase::to_pascal_case;
use inflector::cases::screamingsnakecase::to_screaming_snake_case;
use inflector::cases::sentencecase::to_sentence_case;
use inflector::cases::snakecase::to_snake_case;
use inflector::cases::titlecase::to_title_case;
use inflector::cases::traincase::to_train_case;
use inflector::numbers::{deordinalize::deordinalize, ordinalize::ordinalize};
use inflector::string::{
    deconstantize::deconstantize, demodulize::demodulize, pluralize::to_plural,
    singularize::to_singular,
};
use inflector::suffix::foreignkey::to_foreign_key;
use inflector::{Inflector, InflectorNumbers};
use slug::slugify;

/// Inline FNV-1a (anchors every result byte; no table beyond the printed byte comparison).
struct Sink(u64);

impl Sink {
    fn feed(&mut self, s: &str) {
        for &b in s.as_bytes().iter().chain([0x1f].iter()) {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

fn main() {
    let mut sink = Sink(0xcbf29ce484222325);

    // ① the three deunicode APIs over 12 unicode strings
    let texts = [
        "Æsop Études Straße HŒLLO Čapek Ñandú Björk Þór", // Latin extended
        "Déjà Vu: na\u{0303}o, fac\u{0327}ade, a\u{0301}gia", // stacked combining marks
        "Хрущёв пил квасъ в Москве",                     // Cyrillic
        "Љубав Ґруши Њујорк Џеп",                        // Cyrillic extended
        "Ἰλιὰς Ὁμήρου, θάλασσα ποντίζεται",              // Greek (polytonic)
        "日本語のテスト東京ひらがなカタカナ",            // Japanese + kana
        "한국어 시험 서울",                              // Hangul
        "crab 🦀 party 🎉 rocket 🚀 bulb 💡",            // emoji
        "€5 £3 ¥100 ©2026 ®™ §4 µm ½×¾÷√∑",              // symbols / math
        "ＦＵＬＬＷＩＤＴＨ　ｆｕｌｌ１２３ＡＢＣ",      // fullwidth (incl. ideographic space)
        "┌─┐│═╗║╔╝ box drawing ─│┌┐",                    // box drawing
        "\u{F8FF}\u{E000}\u{10FFFF}\u{0378} priv use",   // private use / unassigned -> tofu
    ];
    for (i, s) in texts.iter().enumerate() {
        let out = deunicode(s);
        println!("deu[{i}] chars={} out={:?}", s.chars().count(), out);
        sink.feed(&out);
        let sl = slugify(s);
        println!("slug[{i}] = {sl:?}");
        sink.feed(&sl);
    }
    // Custom tofu placeholder + the Cow borrow/return path (pure ASCII should be borrowed as-is)
    println!("tofu = {:?}", deunicode_with_tofu("private \u{F8FF} use", "{?}"));
    println!("tofu-empty = {:?}", deunicode_with_tofu("\u{F8FF}x", ""));
    println!("ascii-cow = {:?}", deunicode("plain ascii stays"));
    // deunicode_char: the Some/None paths
    for ch in ['Æ', 'ß', 'š', '北', 'Ж', 'Ω', '🦀', 'A', '\u{0378}'] {
        println!(
            "char {:?} U+{:04X} => {:?}",
            ch,
            ch as u32,
            deunicode_char(ch).map(|r| {
                sink.feed(r);
                r
            })
        );
    }

    // ② slugify edges: empty string / all separators / existing dashes / case / newline+tab
    for (i, s) in [
        "",
        "  --__  ",
        "  --test_-_cool",
        "You & Me",
        "user@example.com",
        "test\nit\t now!",
        "ALREADY-UPPER SnOw",
        "version 2.0.1-rc1",
    ]
    .iter()
    .enumerate()
    {
        let sl = slugify(s);
        println!("slug-edge[{i}] = {sl:?}");
        sink.feed(&sl);
    }

    // ③ camelize family: snake/kebab/already-camel/ALLCAPS/double separator/leading+trailing underscore
    let case_words = [
        "active_record",
        "ActiveRecord",
        "XMLHttpRequest",
        "flag_shih_tzu",
        "user_id",
        "HttpServer",
        "kebab-case-words",
        "mixed-Style_String",
        "ALLCAPS",
        "camelCase",
        "_leading_under",
        "trailing_under_",
        "double__under",
        "with space",
    ];
    for w in case_words {
        let (c, p) = (to_camel_case(w), to_pascal_case(w));
        println!("camel {w:?} => {c:?} / {p:?}");
        sink.feed(&c);
        sink.feed(&p);
    }
    println!(
        "is_camel camelCase={} ActiveRecord={} active_record={}",
        "camelCase".is_camel_case(),
        "ActiveRecord".is_camel_case(),
        "active_record".is_camel_case()
    );
    println!(
        "is_pascal ActiveRecord={} camelCase={}",
        "ActiveRecord".is_pascal_case(),
        "camelCase".is_pascal_case()
    );

    // ④ underscore family: camelCase/acronym runs/digits in between + the three separator variants
    for w in [
        "ActiveRecord",
        "XMLHttpRequest",
        "HttpServer",
        "camelCase",
        "user2Id",
        "JSON2XML",
        "AB",
        "with space",
        "kebab-case",
        "PascalCase",
    ] {
        let u = to_snake_case(w);
        println!("under {w:?} => {u:?}");
        sink.feed(&u);
    }
    for w in ["active_record", "XMLHttpRequest", "flag_shih_tzu", "user2Id"] {
        let (ss, kb, tr) = (
            to_screaming_snake_case(w),
            to_kebab_case(w),
            to_train_case(w),
        );
        println!("variants {w:?} => {ss:?} / {kb:?} / {tr:?}");
        sink.feed(&ss);
        sink.feed(&kb);
        sink.feed(&tr);
    }
    println!(
        "is_snake active_record={} ActiveRecord={} SCREAMING={}",
        "active_record".is_snake_case(),
        "ActiveRecord".is_snake_case(),
        "SCREAMING".is_snake_case()
    );

    // ⑤ humanize family: sentence case carries it, with title case as corroboration (see the header mapping)
    for w in [
        "employee_salary",
        "author_id",
        "post_title",
        "user_name",
        "XML_document",
        "with-dash",
        "already Title",
    ] {
        let (h, t) = (to_sentence_case(w), to_title_case(w));
        println!("human {w:?} => {h:?} / {t:?}");
        sink.feed(&h);
        sink.feed(&t);
    }
    println!(
        "is_sentence Employee salary={} EMPLOYEE={} employee_salary={}",
        "Employee salary".is_sentence_case(),
        "EMPLOYEE".is_sentence_case(),
        "employee_salary".is_sentence_case()
    );

    // ⑥ pluralize: the irregular special family + each regex rule family + uncountable
    #[rustfmt::skip]
    let singulars = [
        // special_cases direct lookup table
        "ox", "man", "woman", "die", "yes", "foot", "eave", "goose", "tooth", "quiz",
        // person~people / child~children regex family
        "person", "child",
        // regular -s / -y->-ies / -ey family
        "cat", "book", "toy", "day", "key", "baby", "story", "hobby", "money", "valley",
        // -ch/-sh/-ss/-x/-zz → -es
        "church", "dish", "class", "box", "buzz",
        // -f/-fe → -ves
        "knife", "wife", "life", "wolf", "leaf", "loaf", "thief", "self",
        // -(her|at|gr)o → -oes
        "hero", "potato", "tomato",
        // -is → -es
        "axis", "testis", "crisis", "analysis",
        // -us → -i
        "octopus", "virus", "syllabus", "cactus", "alumnus", "locus",
        // -um/-on → -a；-a → -ae；-im
        "datum", "bacterium", "criterion", "phenomenon", "alumna", "vertebra", "seraph", "cherub",
        // -ix/-ex -> -ices; the mice/louse family
        "matrix", "vertex", "index", "appendix", "mouse", "louse",
        // -alias/-us/-gas/-ris → -es
        "bus", "gas", "alias", "status",
    ];
    for w in singulars {
        let p = to_plural(w);
        println!("plural {w} => {p}");
        sink.feed(&p);
    }
    // uncountable passthrough
    for w in ["fish", "sheep", "deer", "information", "species", "series", "aircraft", "rice", "equipment"] {
        let p = to_plural(w);
        println!("plural-un {w} => {p}");
        sink.feed(&p);
    }

    // ⑦ singularize: the plural forms of the families above + common regular forms + uncountable
    #[rustfmt::skip]
    let plurals = [
        "oxen", "men", "women", "dice", "yeses", "feet", "eaves", "geese", "teeth", "quizzes",
        "people", "children",
        "cats", "books", "toys", "days", "keys", "babies", "stories", "hobbies",
        "churches", "dishes", "classes", "boxes", "buzzes",
        "knives", "wives", "lives", "wolves", "leaves", "loaves", "thieves", "selves",
        "heroes", "potatoes", "tomatoes",
        "axes", "testes", "crises", "analyses",
        "octopi", "viri", "syllabi", "cacti", "alumni", "loci",
        "data", "bacteria", "criteria", "phenomena", "alumnae", "vertebrae", "seraphim", "cherubim",
        "matrices", "vertices", "indices", "appendices", "mice", "lice",
        "buses", "gases", "aliases", "statuses",
        "fish", "sheep", "information", "monies", "valleys",
    ];
    for w in plurals {
        let s = to_singular(w);
        println!("singular {w} => {s}");
        sink.feed(&s);
    }

    // ⑧ ordinalize/deordinalize + foreign_key + demodulize/deconstantize
    for n in [
        "0", "1", "2", "3", "4", "11", "12", "13", "21", "22", "23", "101", "111", "1003", "2.5",
    ] {
        let o = ordinalize(n);
        println!("ordinal {n} => {o}");
        sink.feed(&o);
    }
    for n in ["1st", "2nd", "3rd", "11th", "12003rd", "2.5th"] {
        println!("deordinal {n} => {}", deordinalize(n));
    }
    for w in ["Person", "Admin::Post", "user_id", "XMLHttpRequest"] {
        let fk = to_foreign_key(w);
        println!("fk {w:?} => {fk:?} (is_fk={})", w.is_foreign_key());
        sink.feed(&fk);
    }
    for w in ["Admin::User", "Top::Mid::Leaf", "plain"] {
        let (dm, dc) = (demodulize(w), deconstantize(w));
        println!("mod {w:?} => {dm:?} / {dc:?}");
        sink.feed(&dm);
        sink.feed(&dc);
    }

    // ⑨ class/table case (heavyweight: singular/plural + case composition inside)
    for w in ["posts", "line_items", "people", "data"] {
        let k = w.to_class_case();
        println!("class {w:?} => {k:?} (is_class={})", k.is_class_case());
        sink.feed(&k);
    }
    for w in ["Post", "LineItem", "Mouse", "Person"] {
        let t = w.to_table_case();
        println!("table {w:?} => {t:?} (is_table={})", t.is_table_case());
        sink.feed(&t);
    }

    // ⑩ trait method surface: Inflector (&str/String) + InflectorNumbers (integers)
    let t1 = "active_record".to_pascal_case();
    let t2 = String::from("XMLHttp_request").to_snake_case();
    let t3 = "shoes".to_singular();
    let t4 = 1u32.ordinalize();
    let t5 = 111u64.ordinalize();
    println!("trait {t1:?} {t2:?} {t3:?} {t4} {t5}");
    sink.feed(&t1);
    sink.feed(&t2);
    sink.feed(&t3);

    println!("corpus fnv = {:016x}", sink.0);
}
