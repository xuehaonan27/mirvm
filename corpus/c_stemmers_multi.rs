#!/usr/bin/env mirvm
---
[dependencies]
rust-stemmers = "1"
---
// rust-stemmers 1.2 (Snowball stemming, compile-time generated table-driven state machines)
// across languages. A pure-computation crate: static Among tables plus a SnowballEnv string
// cursor; no IO, randomness or HashMap iteration order, so output is deterministic. stem()
// returns Cow<str>, which also probes Borrowed/Owned (borrowed when the input is unmodified).
//
// Coverage:
//  ① All 15 algorithms (en/fr/de/it/pt/ru/fi/nl/sv/es/da/no/hu/ro/tr) x their own fixed
//     word lists (Latin/Cyrillic/Finno-Ugric/Turkic, with diacritics and long-tail forms);
//  ② the English irregular set running/ran/better/geese/caresses plus the classic Porter set;
//  ③ boundaries: empty string / single char / digits only / punctuation only / hyphen / apostrophe /
//     uppercase (not lowercased) / 200-char word / cross-script input (Cyrillic words into English);
//  ④ same word across algorithms: 4 pan-Latin inflected words x all 15 algorithms;
//  ⑤ an inline FNV-1a aggregate over every line plus a total word count (any byte drift changes it).
use rust_stemmers::{Algorithm, Stemmer};
use std::borrow::Cow;

/// (short code, algorithm, fixed word list). Inputs are pre-lowercased as the crate docs require.
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

/// Classic Porter English set: the irregular words, the paper's step-by-step examples, and
/// three entries of the A_1 apostrophe table (' / 's' / 's).
const EN_SUITE: &[&str] = &[
    "running", "ran", "better", "geese", "caresses",
    "ponies", "ties", "cats", "feed", "agreed",
    "bled", "sing", "hopping", "tanned", "falling",
    "hissing", "fizzed", "failing", "filing", "sky",
    "news", "inning", "outing", "proceed", "succeed",
    "o's", "o's'", "o'clock",
];

/// Boundary samples: empty / single char / non-letter / mixed symbols / not lowercased / long word / cross-script.
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

/// Print `word => stem (len, B|O)` and feed the print into the aggregate hasher.
/// B = Cow::Borrowed (input returned unchanged), O = Cow::Owned (a modification happened).
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

    // ① all 15 algorithms x their own word lists
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

    // ② English irregular set + classic Porter set (one stemmer reused across words)
    println!("== english-suite ==");
    fnv1a(&mut h, b"== english-suite ==\n");
    let en = Stemmer::create(Algorithm::English);
    for w in EN_SUITE {
        record(&mut h, &en, w);
        total += 1;
    }

    // ③ boundaries: the same edge word set fed to the en/ru/fi/tr representatives
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
        // 200-char word: repeated 'a' plus a suffix, probing the long-buffer path
        let long = format!("{}{}", "a".repeat(200), w_long_suffix(code));
        record(&mut h, &st, &long);
        total += 1;
    }

    // ④ same word across algorithms: pan-Latin inflected words x all 15 algorithms
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

/// A representative long-tail suffix per family (used by the long-word case).
fn w_long_suffix(code: &str) -> &'static str {
    match code {
        "en" => "izations",
        "ru" => "ами",
        "fi" => "issa",
        "tr" => "larımızdan",
        _ => "",
    }
}
