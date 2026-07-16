#!/usr/bin/env mirvm
---
[dependencies]
deunicode = "1"
slug = "0.1"
# 注意包名大写：crates.io 上注册名为 `Inflector`（cargo 对 `inflector` 报
# "no matching package found, perhaps you meant: Inflector"）；其 [lib] 名仍是
# 小写 `inflector`，Rust 代码照常 `use inflector::...`。
Inflector = "0.11"
---
// deunicode 1.6 + slug 0.1 + Inflector 0.11（转写表偏门族）差分。
// deunicode：mapping.txt + pointers.bin 双静态表查表；ASCII 快路径 +
// Cow 借还；未知字符走 tofu 占位。slug：逐 char deunicode_char 查表，
// 非 [a-z0-9] 折叠为单个 '-'，去首尾 '-'。Inflector 0.11.4：case 族为
// 纯 Rust 字符扫描；pluralize/singularize 走 lazy_static + regex 规则表
// （24 条，rev 序首命中返回）+ special_cases 静态 match + UNACCONTABLE
// 202 词静态数组——本条同时压 lazy_static 初始化与 regex DFA 编译。
//
// 用户命名映射（crate 无 Rails 原名 API，取语义对应物）：
//   camelize  → to_camel_case / to_pascal_case
//   underscore→ to_snake_case（另覆盖 screaming_snake/kebab/train）
//   pluralize → to_plural；singularize → to_singular
//   humanize  → to_sentence_case（Inflector 无 humanize；sentence case 为
//               Rails humanize 的去分隔符 + 首字母大写语义子集，另附 title）
// 覆盖：deunicode/deunicode_with_tofu/deunicode_char 三 API ×12 串（拉丁
// 扩展/组合符/西里尔/西里尔扩展/希腊古稿/日汉假名/emoji/符号/全角/制表/
// 谚文/私用区未知）；slugify 全批 + 空串/全分隔/前缀虚线等边界；Inflector
// camelize、underscore 4 变体、sentence(humanize)/title、pluralize 50 词、
// singularize 50 词（含 ox/man/woman/die/foot/goose/quiz 不规则族、
// person~people、mouse~mice、-us→-i、-um/-on→-a、-ix→-ices、-f(e)→-ves、
// uncountable 谱系）、ordinalize/deordinalize、to_foreign_key、demodulize/
// deconstantize、class/table case、is_* 判定与 trait Inflector/
// InflectorNumbers 方法调用面；结尾全结果字节 fnv 指纹。
// 确定性：全表查询，无时间/地址/随机源；转写结果可能内嵌 \n（deunicode
// 明示），一律 {:?} 转义打印；不建临时文件；stderr 为空（零 warning）。
use deunicode::{deunicode, deunicode_char, deunicode_with_tofu};
// Inflector 0.11 顶层不 re-export 函数（lib.rs 内为私有 use），公开面是
// 模块路径函数 + Inflector/InflectorNumbers 两 trait。
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

/// 内联 FNV-1a（全结果字节锚定，不依赖打印逐字节比对之外的任何总表）。
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

    // ① deunicode 三 API：12 批 unicode 串
    let texts = [
        "Æsop Études Straße HŒLLO Čapek Ñandú Björk Þór", // 拉丁扩展
        "Déjà Vu: na\u{0303}o, fac\u{0327}ade, a\u{0301}gia", // 组合符叠加
        "Хрущёв пил квасъ в Москве",                     // 西里尔
        "Љубав Ґруши Њујорк Џеп",                        // 西里尔扩展
        "Ἰλιὰς Ὁμήρου, θάλασσα ποντίζεται",              // 希腊（古稿多调符）
        "日本語のテスト東京ひらがなカタカナ",            // 日汉假名
        "한국어 시험 서울",                              // 谚文
        "crab 🦀 party 🎉 rocket 🚀 bulb 💡",            // emoji
        "€5 £3 ¥100 ©2026 ®™ §4 µm ½×¾÷√∑",              // 符号/数学
        "ＦＵＬＬＷＩＤＴＨ　ｆｕｌｌ１２３ＡＢＣ",      // 全角（含全角空格）
        "┌─┐│═╗║╔╝ box drawing ─│┌┐",                    // 制表符
        "\u{F8FF}\u{E000}\u{10FFFF}\u{0378} priv use",   // 私用区/未分配 → tofu
    ];
    for (i, s) in texts.iter().enumerate() {
        let out = deunicode(s);
        println!("deu[{i}] chars={} out={:?}", s.chars().count(), out);
        sink.feed(&out);
        let sl = slugify(s);
        println!("slug[{i}] = {sl:?}");
        sink.feed(&sl);
    }
    // tofu 自定义占位 + Cow 借还路径（纯 ASCII 输入应原样借还）
    println!("tofu = {:?}", deunicode_with_tofu("private \u{F8FF} use", "{?}"));
    println!("tofu-empty = {:?}", deunicode_with_tofu("\u{F8FF}x", ""));
    println!("ascii-cow = {:?}", deunicode("plain ascii stays"));
    // deunicode_char：Some/None 双路径
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

    // ② slugify 边界：空串 / 全分隔符 / 已有虚线 / 大小写 / 换行制表
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

    // ③ camelize 谱系：snake/kebab/已驼峰/全大写/双分隔/首尾下划线
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

    // ④ underscore 谱系：驼峰/缩写连排/数字夹心 + 三分隔变体
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

    // ⑤ humanize 谱系：sentence case 承担 + title case 旁证（见头注映射）
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

    // ⑥ pluralize：不规则 special 族 + 各正则规则族 + uncountable
    #[rustfmt::skip]
    let singulars = [
        // special_cases 直查表
        "ox", "man", "woman", "die", "yes", "foot", "eave", "goose", "tooth", "quiz",
        // person~people / child~children 正则族
        "person", "child",
        // 常规 -s / -y→-ies / -ey 系
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
        // -ix/-ex → -ices；mice/louse 族
        "matrix", "vertex", "index", "appendix", "mouse", "louse",
        // -alias/-us/-gas/-ris → -es
        "bus", "gas", "alias", "status",
    ];
    for w in singulars {
        let p = to_plural(w);
        println!("plural {w} => {p}");
        sink.feed(&p);
    }
    // uncountable 直通
    for w in ["fish", "sheep", "deer", "information", "species", "series", "aircraft", "rice", "equipment"] {
        let p = to_plural(w);
        println!("plural-un {w} => {p}");
        sink.feed(&p);
    }

    // ⑦ singularize：上面各族的复数形 + 常见规则形 + uncountable
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

    // ⑨ class/table case（heavyweight：内部走 singular/plural + case 组合）
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

    // ⑩ trait 方法面：Inflector（&str/String）+ InflectorNumbers（整数）
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
