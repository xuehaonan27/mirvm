#!/usr/bin/env mirvm
---
[dependencies]
zxcvbn = "3"
---
// zxcvbn 3.1 (the Rust port of Dropbox's zxcvbn password strength estimator)
// differential, exercising the table-driven semantics: a 30k password dictionary, an
// English Wikipedia wordlist and US census name tables (built at runtime by lazy_static
// into HashMap<&str, usize> rank tables) plus qwerty/dvorak/keypad adjacency graphs;
// eight matchers (dictionary, reverse dictionary, l33t substitution, spatial, repeat via
// fancy-regex, Unicode code-point sequence, regex, date) feed the omnimatch scan, then
// dynamic programming picks the minimum-guess sequence and the time estimate is graded.
//
// One hidden clock surface: DatePattern/recent_year's year_space depends on the library's
// REFERENCE_YEAR (lazy_static, the current UTC year). Both sides run in the same year, so
// the differential is unaffected.
//
// Coverage: every Entropy getter (score/guesses/guesses_log10/the four crack_times
// scenarios/feedback/sequence); all seven MatchPattern variants (dictionary/spatial/
// repeat/sequence/regex/date/bruteforce) with their public problem fields; the 14 warning
// and 13 suggestion feedback grades; the empty string early exit (guesses=0, log10=-inf,
// default feedback); the chars().take(100) truncation boundary (exactly 100 vs 107 clamped
// to 100); the saturating_mul overflow path (guesses=u64::MAX); and the user_inputs
// before/after comparison (a UserInputs dictionary hit at rank 1 displaces bruteforce,
// user input is lowercased and sanitised, and a built-in dictionary hit ties an injected
// entry at min-submatch-guesses, showing the clamping semantics).
// Password set: empty / top-10 / top-100 / reversed / passphrase / keyboard row /
// keyboard turns / shifted keys / keypad / ascending-descending sequence / single-char
// repeat / word repeat / unseparated date / separated date / unicode date (märz) / recent
// year regex / three l33t cases / the crate's own test anchors ("TestMeNow!"
// guesses=372_010_000, "r0sebudmaelstrom11/20/91aaaa" score 4) / overflow saturation /
// single char / mixed CJK / pure CJK / non-BMP (𐰊 Old Turkic) / seeded pseudo-random
// long string / two truncation boundary cases.
//
// Determinism: every f64 (guesses_log10 and the Float crack_times values) prints
// to_bits() to pin the bit pattern; crack_times also prints its Display grading (the
// "5 hours" family); calculation_time() is a wall-clock Duration and is never printed;
// the l33t sub table is a HashMap<char,char> and is printed sorted, never the library's
// pre-joined sub_display (HashMap iteration order); Match/pattern are never printed with
// {:?} because Debug includes sub_display; dictionary names go through DictionaryType's
// derived Debug (the type is private, so {:?} is the only option); the long random
// password comes from a seeded xorshift64* (same sequence on both sides); stderr is empty.
//
// Exact-tie hazard avoided on the input side (upstream nondeterminism, not a mirvm
// finding): the random password is 18 characters, not 24. At 24 the trailing "Osla"
// reversed hits the English dictionary word "also"; and for brute force of 19 or more
// characters 10^k saturating_mul always reaches u64::MAX, so the len=1 and len=2
// decompositions tie exactly at MAX, leaving the scoring unwind's HashMap<usize,u64>
// iteration order (process-random) to decide between equal optima. At 18 the bruteforce
// bound 10^18 < u64::MAX, so no such tie can form.
use zxcvbn::matching::patterns::MatchPattern;
use zxcvbn::matching::Match;
use zxcvbn::time_estimates::CrackTimeSeconds;
use zxcvbn::{zxcvbn, Entropy};

/// Seeded xorshift64*, the same sequence on native and mirvm.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// CrackTimeSeconds: prints both the raw value (integer as-is, float pinned by bits) and the Display grading.
fn fmt_secs(s: CrackTimeSeconds) -> String {
    match s {
        CrackTimeSeconds::Integer(v) => format!("i={v} t=[{s}]"),
        CrackTimeSeconds::Float(f) => format!("b=0x{:016x} t=[{s}]", f.to_bits()),
    }
}

fn fmt_match(m: &Match) -> String {
    let g = match m.guesses {
        Some(v) => v.to_string(),
        None => "none".to_string(),
    };
    let head = format!("[{}..{}] tok=[{}] g={}", m.i, m.j, m.token, g);
    match &m.pattern {
        MatchPattern::Dictionary(p) => {
            // sub is a HashMap<char,char>: print it sorted, not the library's sub_display (HashMap order).
            let mut subs: Vec<(char, char)> = match &p.sub {
                Some(h) => h.iter().map(|(&a, &b)| (a, b)).collect(),
                None => Vec::new(),
            };
            subs.sort();
            let subs = subs
                .iter()
                .map(|(a, b)| format!("{a}>{b}"))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{head} dict word=[{}] rank={} name={:?} rev={} l33t={} base={} upvar={} l33tvar={} subs=[{}]",
                p.matched_word,
                p.rank,
                p.dictionary_name,
                p.reversed,
                p.l33t,
                p.base_guesses,
                p.uppercase_variations,
                p.l33t_variations,
                subs
            )
        }
        MatchPattern::Spatial(p) => format!(
            "{head} spatial graph={} turns={} shifted={}",
            p.graph, p.turns, p.shifted_count
        ),
        MatchPattern::Repeat(p) => format!(
            "{head} repeat base=[{}] reps={} bg={}",
            p.base_token, p.repeat_count, p.base_guesses
        ),
        MatchPattern::Sequence(p) => format!(
            "{head} seq name={} space={} asc={}",
            p.sequence_name, p.sequence_space, p.ascending
        ),
        MatchPattern::Regex(p) => format!(
            "{head} regex name={} m=[{}]",
            p.regex_name,
            p.regex_match.join(",")
        ),
        MatchPattern::Date(p) => format!(
            "{head} date y={} m={} d={} sep='{}'",
            p.year, p.month, p.day, p.separator
        ),
        MatchPattern::BruteForce => format!("{head} bruteforce"),
    }
}

fn dump(tag: &str, pw: &str, ui: &str, e: &Entropy) {
    println!("case {tag} pw=[{pw}] ui=[{ui}] chars={}", pw.chars().count());
    println!(
        "  score={} guesses={} log10=0x{:016x}",
        u8::from(e.score()),
        e.guesses(),
        e.guesses_log10().to_bits()
    );
    let ct = e.crack_times();
    println!("  100/hr  {}", fmt_secs(ct.online_throttling_100_per_hour()));
    println!("  10/s    {}", fmt_secs(ct.online_no_throttling_10_per_second()));
    println!("  1e4/s   {}", fmt_secs(ct.offline_slow_hashing_1e4_per_second()));
    println!(
        "  1e10/s  {}",
        fmt_secs(ct.offline_fast_hashing_1e10_per_second())
    );
    match e.feedback() {
        None => println!("  feedback none"),
        Some(fb) => {
            let w = fb
                .warning()
                .map(|w| w.to_string())
                .unwrap_or_else(|| "-".to_string());
            let sugg = fb
                .suggestions()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            println!("  feedback warn=[{w}] sugg n={} [{}]", sugg.len(), sugg.join(" | "));
        }
    }
    let seq = e.sequence();
    println!("  seq n={}", seq.len());
    for (k, m) in seq.iter().enumerate() {
        println!("    m{k} {}", fmt_match(m));
    }
}

fn main() {
    // Seeded pseudo-random long password ([a-zA-Z0-9]x18), the same bytes on both sides; 18
    // keeps the whole-segment bruteforce bound 10^18 inside u64 (see the header note).
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let alphabet: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let rand_pw: String = (0..18)
        .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize] as char)
        .collect();
    // chars().take(100) truncation boundary: exactly 100 (the word is kept whole) vs 107 (clamped).
    let trunc100 = format!("{}password", "b".repeat(92));
    let trunc107 = format!("{}password", "b".repeat(99));

    let fixed: Vec<(&str, &str)> = vec![
        ("s00_empty", ""),
        ("s01_top10", "password"),
        ("s02_top100", "test"),
        ("s03_reversed", "drowssap"),
        ("s04_passphrase", "correcthorsebatterystaple"),
        ("s05_kb_row", "qwertyuiop"),
        ("s06_kb_turns", "1qaz2wsx"),
        ("s07_kb_shifted", "6tfGHJ"),
        ("s08_keypad", "/8520"),
        ("s09_seq_lower", "fghijk"),
        ("s10_seq_digits_desc", "97531"),
        ("s11_repeat_char", "aaaaaaaa"),
        ("s12_repeat_word", "abcabcabc"),
        ("s13_date_nosep", "11201991"),
        ("s14_date_sep", "11/20/1991"),
        ("s15_date_unicode", "08märz2010"),
        ("s16_recent_year", "happy2024"),
        ("s17_l33t", "p@ssw0rd"),
        ("s18_l33t_pure", "m0th3r"),
        ("s19_l33t_caps", "P4$$w0rd"),
        ("s20_rosebud", "r0sebudmaelstrom11/20/91aaaa"),
        ("s21_testmenow", "TestMeNow!"),
        ("s22_overflow", "!QASW@#EDFR$%TGHY^&UJKI*(OL"),
        ("s23_single", "a"),
        ("s24_cjk_mix", "密码password2024"),
        ("s25_cjk_pure", "我爱北京天安门"),
        ("s26_nonbmp", "𐰊𐰂𐰄𐰀𐰁"),
        ("s27_random18", &rand_pw),
        ("s28_trunc100", &trunc100),
        ("s29_trunc107", &trunc107),
    ];
    println!("== fixed ==");
    for (tag, pw) in &fixed {
        let e = zxcvbn(pw, &[]);
        dump(tag, pw, "-", &e);
    }

    // user_inputs before/after comparison: u0's injected word is in no built-in dictionary,
    // so only post shows a name=UserInputs rank=1 hit displacing bruteforce; u1 also checks
    // lowercasing/sanitising (injecting "XueHaoNan" hits "xuehaonan"); for u2 a built-in
    // dictionary hit (Surnames/Passwords) ties the injected entry at min-submatch-guesses
    // because UserInputs is always pushed after the built-in dictionaries in omnimatch and
    // the update keeps the first-seen on <=, so injection does not change guesses.
    println!("== user_inputs ==");
    let ui_cases: Vec<(&str, &str, &[&str])> = vec![
        ("u0_haonanxue", "haonanxue88", &["haonanxue", "xueba"]),
        ("u1_xuehaonan", "xuehaonan2024", &["XueHaoNan"]),
        ("u2_smith", "smith1990", &["smith"]),
    ];
    for (tag, pw, ui) in &ui_cases {
        let pre = zxcvbn(pw, &[]);
        dump(&format!("{tag}_pre"), pw, "-", &pre);
        let post = zxcvbn(pw, ui);
        dump(&format!("{tag}_post"), pw, &ui.join(","), &post);
        println!(
            "delta {tag}: score {}->{} guesses {}->{} seq {}->{}",
            u8::from(pre.score()),
            u8::from(post.score()),
            pre.guesses(),
            post.guesses(),
            pre.sequence().len(),
            post.sequence().len()
        );
    }
    println!("== done ==");
}
