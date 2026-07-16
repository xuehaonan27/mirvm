#!/usr/bin/env mirvm
---
[dependencies]
zxcvbn = "3"
---
// zxcvbn 3.1（Dropbox zxcvbn 的 Rust 移植，密码强度估计）差分：真正的表驱动
// 语义——30k 密码词典 + 英文维基词表 + 美国人口普查姓名表（lazy_static 运行时
// 构建 HashMap<&str, usize> 排名表）+ qwerty/dvorak/keypad 邻接图；8 个 matcher
// （词典/反向词典/l33t 替换/键位/重复（fancy-regex)/Unicode 码位序列/regex/
// 日期）→ omnimatch 全模式扫描 → 动态规划选最小猜测序列 → 时间估算分级。
// 内部还有隐藏时钟面：DatePattern/recent_year 的 year_space 依赖 lib 内
// REFERENCE_YEAR（lazy_static 取当前 UTC 年）——两侧同跑同年，diff 不受影响。
//
// 覆盖：Entropy 全 getter（score/guesses/guesses_log10/crack_times 四场景/
// feedback/sequence）；MatchPattern 全部 7 变体（dictionary/spatial/repeat/
// sequence/regex/date/bruteforce）及各自公有问题字段；feedback 的 warning 14
// 档与 suggestions 13 档按集内密码尽量踩中；空串早退路径（guesses=0、
// log10=-inf、默认 feedback）；chars().take(100) 截断边界（恰好 100 vs 107 超界
// 截到 100）；saturating_mul 溢出路径（guesses=u64::MAX）；user_inputs 注入前后
// 对照（UserInputs 词典 rank=1 命中顶掉 bruteforce、user input 小写化消毒、
// 内置词典命中与注入词条并列 min-submatch-guesses 时的钳位语义）。
// 密码集：空 / top-10 / top-100 / 反写 / passphrase / 键盘直行 / 键盘转弯 /
// shift 键位 / keypad / 升降序列 / 单字符重复 / 单词重复 / 无分隔日期 / 带分隔
// 日期 / unicode 日期（märz）/ 近年份 regex / l33t 三例 / crate 自测锚定值
// （"TestMeNow!" guesses=372_010_000、"r0sebudmaelstrom11/20/91aaaa" score 4）/
// 溢出饱和 / 单字符 / 中文混排 / 纯 CJK / 非 BMP（𐰊 古突厥文）/ 定种伪随机长串 /
// 截断边界两例。
//
// 确定性：一切 f64（guesses_log10、crack_times 的 Float 场景原值）打印
// to_bits() 锁位型；crack_times 另打印 Display 文本分级（"5 hours" 族）；
// calculation_time() 是墙钟 Duration，绝不打印；l33t 的 sub 表是
// HashMap<char,char>，排序后打印，绝不打印 lib 预拼的 sub_display（HashMap
// 迭代序拼接）；不对 Match/pattern 用 {:?}（Debug 含 sub_display）；词典名经
// DictionaryType 的 derive(Debug) 打印（该类型在私有模块不可命名，只能 {:?}）；
// 长随机密码由定种 xorshift64* 生成（两侧同序列）；stderr 为空（driver 零
// warning）。
//
// 确定性坑（上游 exact-tie nondeterminism，driver 输入侧躲开，非 mirvm 发现）：
// 随机串原取 24 字符时，native 连跑 5 次第 2/3 次与首次不一致，翻转行：
//   <   seq n=1 / m0 [0..23] tok=[…Osla] g=18446744073709551615 bruteforce
//   >   seq n=2 / m0 [0..19] bruteforce(MAX) + m1 [20..23] tok=[Osla] g=88
//   >   dict word=[also] rank=22 name=English rev=true …
// 根因：尾 4 字符 "Osla" 反写命中 English 词典 "also"；bruteforce ≥19 字符时
// 10^k saturating_mul 必撞 u64::MAX，len=1 全段（1×MAX）与 len=2 分解
// （2×(MAX×88) sat）的序列猜测**精确并列 MAX**；scoring.rs 的 unwind 对
// optimal.g[k]（HashMap<usize,u64>）迭代取严格最小，exact-tie 谁先生效取决于
// 进程随机种子 → 输出抖动（native 对 native 不稳定；本 driver A/B/C 三维恰好
// 三进程同种子才全绿）。语义改动为零的躲开方式：随机串取 18 字符
// （bruteforce 上界 10^18 < u64::MAX，一切分解离开饱和区，并列在代数上不再
// 成立），落地后经 native 10 连跑逐字节验证稳定。
use zxcvbn::matching::patterns::MatchPattern;
use zxcvbn::matching::Match;
use zxcvbn::time_estimates::CrackTimeSeconds;
use zxcvbn::{zxcvbn, Entropy};

/// 定种 xorshift64*（native/mirvm 同序列）。
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

/// CrackTimeSeconds：原值（整数原样 / 浮点锁位型）+ Display 文本分级双打印。
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
            // sub 是 HashMap<char,char>：排序后打印，不用 lib 的 sub_display（HashMap 序）。
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
    // 定种伪随机长密码（[a-zA-Z0-9]×18），两侧逐字节同串；18 = 全段 bruteforce
    // 猜测上界 10^18 仍在 u64 内，躲开 ≥19 字符必撞 u64::MAX 的饱和并列（见头注）。
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let alphabet: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let rand_pw: String = (0..18)
        .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize] as char)
        .collect();
    // chars().take(100) 截断边界：恰好 100（词全保留）vs 107（截到只剩 p）。
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

    // user_inputs 注入前后对照：u0 注入词不在任何内置词典 → post 可见
    // name=UserInputs rank=1 的词典命中顶掉 bruteforce；u1 顺带验证 user input
    // 的小写化消毒（注入 "XueHaoNan" 命中 "xuehaonan"）；u2 的内置词典命中
    // （Surnames/Passwords）与注入词条并列 min-submatch-guesses=50，UserInputs
    // 在 omnimatch 里恒被显式排在内置词典之后 push，update 的 ≤ 保留先见者——
    // 注入不改 guesses，如实打印这一钳位语义。
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
