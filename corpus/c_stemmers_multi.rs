#!/usr/bin/env mirvm
---
[dependencies]
rust-stemmers = "1"
---
// rust-stemmers 1.2（Snowball 词干算法，编译期生成的表驱动状态机）多语差分。
// 纯计算 crate：静态 Among 表 + SnowballEnv 字符串游标，无 IO / 无随机 /
// 无 HashMap 迭代序，输出天然确定。stem() 返回 Cow<str>——顺带探测
// Cow 的 Borrowed/Owned 判别（输入未被修改时借用返回）。
//
// 覆盖：
//  ① 全族 15 算法（en/fr/de/it/pt/ru/fi/nl/sv/es/da/no/hu/ro/tr）× 各自
//     固定词表（拉丁/西里尔/芬兰-乌戈尔/突厥谱系，含变音符与长尾形态）；
//  ② 英文不规则谱系 running/ran/better/geese/caresses 及 Porter 经典集；
//  ③ 边界：空串 / 单字符 / 纯数字 / 纯标点 / 连字符 / 撇号 / 未 lowercase
//     大写 / 200 字符长词 / 跨文字混入（西里尔词喂英文算法）；
//  ④ 同词跨算法对照：4 个泛拉丁形态词 × 全 15 算法；
//  ⑤ 全行内联 FNV-1a 聚合指纹 + 总词数收尾（任何一字节漂移即指纹变）。
use rust_stemmers::{Algorithm, Stemmer};
use std::borrow::Cow;

/// (短码, 算法, 固定词表)。输入按 crate 文档要求预 lowercase。
const FAMILY: &[(&str, Algorithm, &[&str])] = &[
    ("en", Algorithm::English, &[
        "running", "caresses", "ponies", "agreed", "plastered",
        "motoring", "conflated", "happily",
    ]),
    ("fr", Algorithm::French, &[
        "mangeaient", "chevaux", "parlions", "finirions",
        "nationalement", "heureuses", "grandir", "bâtiments",
    ]),
    ("de", Algorithm::German, &[
        "gegangen", "laufenden", "häusern", "schnellsten",
        "möglichkeiten", "gearbeitet", "kindern", "autobahnen",
    ]),
    ("it", Algorithm::Italian, &[
        "cantavano", "canzoni", "nazionalmente", "mangiatori",
        "bellissima", "finiremo", "andando", "espressioni",
    ]),
    ("pt", Algorithm::Portuguese, &[
        "cantavam", "canções", "nacionalmente", "faladores",
        "bonitas", "comeríamos", "trabalhando", "ações",
    ]),
    ("ru", Algorithm::Russian, &[
        "машины", "быстро", "домами", "сказавший",
        "интересного", "работает", "приветствия", "национального",
    ]),
    ("fi", Algorithm::Finnish, &[
        "taloissa", "koiriemme", "nopeasti", "kirjastoista",
        "juokseminen", "kaupungissa", "syödään", "tulevat",
    ]),
    ("nl", Algorithm::Dutch, &[
        "lopende", "huizen", "snelheden", "werkelijkheden",
        "gekocht", "spelende", "kinderen", "mooiste",
    ]),
    ("sv", Algorithm::Swedish, &[
        "springande", "husens", "snabbheten", "hästarnas",
        "arbetande", "köpte", "vackrast", "nationalitet",
    ]),
    ("es", Algorithm::Spanish, &[
        "cantaban", "canciones", "nacionalmente", "habladores",
        "comíamos", "trabajando", "acciones", "rápidamente",
    ]),
    ("da", Algorithm::Danish, &[
        "løbende", "husenes", "hurtigste", "bilernes",
        "arbejdende", "købte", "smukkest", "nationalitet",
    ]),
    ("no", Algorithm::Norwegian, &[
        "løpende", "husenes", "kjøligste", "hestenes",
        "arbeidende", "kjøpte", "vakrest", "nasjonalitet",
    ]),
    ("hu", Algorithm::Hungarian, &[
        "házakban", "gyorsabban", "szépségével", "autóinkkal",
        "dolgozóknak", "könyveket", "beszélek", "országok",
    ]),
    ("ro", Algorithm::Romanian, &[
        "națională", "frumuseți", "câinilor", "lucrând",
        "cumpărat", "vitezei", "cărțile", "vorbind",
    ]),
    ("tr", Algorithm::Turkish, &[
        "evlerimizden", "hızlıca", "kitaplarımız", "çocuklardan",
        "çalışarak", "güzellik", "arabaların", "gelmiştim",
    ]),
];

/// Porter 英文经典谱系：任务点名的不规则词 + 原论文分步变形样例 +
/// A_1 撇号表的三条（' / 's' / 's）。
const EN_SUITE: &[&str] = &[
    "running", "ran", "better", "geese", "caresses",
    "ponies", "ties", "cats", "feed", "agreed",
    "bled", "sing", "hopping", "tanned", "falling",
    "hissing", "fizzed", "failing", "filing", "sky",
    "news", "inning", "outing", "proceed", "succeed",
    "o's", "o's'", "o'clock",
];

/// 边界样例：空 / 单字符 / 非字母 / 混合符号 / 未 lowercase / 长词 / 跨文字。
const EDGE: &[&str] = &[
    "", "a", "x", "z",
    "123", "!!!", "---", "a-b", "it's",
    "Running", "CARESSES", "ПоНеДеЛьНиК",
    "ing", "ization", "supercalifragilisticexpialidocious",
];

fn fnv1a(h: &mut u64, data: &[u8]) {
    for &b in data {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// 打印 `word => stem (len, B|O)` 并喂入聚合 hasher。
/// B = Cow::Borrowed（输入原样返回），O = Owned（发生了修改）。
fn record(h: &mut u64, st: &Stemmer, word: &str) {
    let out: Cow<str> = st.stem(word);
    let tag = match out {
        Cow::Borrowed(_) => "B",
        Cow::Owned(_) => "O",
    };
    let line = format!("{word} => {out} (len={}, {tag})", out.len());
    fnv1a(h, line.as_bytes());
    fnv1a(h, b"\n");
    println!("{line}");
}

fn main() {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut total = 0u32;

    // ① 全族 15 算法 × 各自固定词表
    println!("== family ==");
    fnv1a(&mut h, b"== family ==\n");
    for &(code, algo, words) in FAMILY {
        let st = Stemmer::create(algo);
        let head = format!("[{code} {algo:?}]");
        fnv1a(&mut h, head.as_bytes());
        fnv1a(&mut h, b"\n");
        println!("{head}");
        for w in words {
            record(&mut h, &st, w);
            total += 1;
        }
    }

    // ② 英文不规则谱系 + Porter 经典集（单 stemmer 复用跨词）
    println!("== english-suite ==");
    fnv1a(&mut h, b"== english-suite ==\n");
    let en = Stemmer::create(Algorithm::English);
    for w in EN_SUITE {
        record(&mut h, &en, w);
        total += 1;
    }

    // ③ 边界：同一边界词集喂 en/ru/fi/tr 四个谱系代表
    println!("== edge ==");
    fnv1a(&mut h, b"== edge ==\n");
    const EDGE_ALGOS: &[(&str, Algorithm)] = &[
        ("en", Algorithm::English),
        ("ru", Algorithm::Russian),
        ("fi", Algorithm::Finnish),
        ("tr", Algorithm::Turkish),
    ];
    for &(code, algo) in EDGE_ALGOS {
        let st = Stemmer::create(algo);
        println!("[edge/{code}]");
        fnv1a(&mut h, code.as_bytes());
        fnv1a(&mut h, b"\n");
        for w in EDGE {
            record(&mut h, &st, w);
            total += 1;
        }
        // 200 字符长词：'a' 重复 + 后缀，探测长缓冲区路径
        let long = format!("{}{}", "a".repeat(200), w_long_suffix(code));
        record(&mut h, &st, &long);
        total += 1;
    }

    // ④ 同词跨算法对照：泛拉丁形态词 × 全 15 算法
    println!("== cross ==");
    fnv1a(&mut h, b"== cross ==\n");
    let stemmers: Vec<(&str, Stemmer)> =
        FAMILY.iter().map(|&(c, a, _)| (c, Stemmer::create(a))).collect();
    for w in ["nationale", "running", "amente", "izations"] {
        let mut line = format!("{w}:");
        for (code, st) in &stemmers {
            let out = st.stem(w);
            line.push_str(&format!(" {code}={out}"));
            total += 1;
        }
        fnv1a(&mut h, line.as_bytes());
        fnv1a(&mut h, b"\n");
        println!("{line}");
    }

    println!("total={total} fnv={h:016x}");
}

/// 各谱系有代表性的长尾后缀（长词用例用）。
fn w_long_suffix(code: &str) -> &'static str {
    match code {
        "en" => "izations",
        "ru" => "ами",
        "fi" => "issa",
        "tr" => "larımızdan",
        _ => "",
    }
}
