#!/usr/bin/env mirvm
---
[dependencies]
# 钉 0.20.4（0.20 系列末版）。default-features = false：默认 features 是
# progressbar(indicatif)/onig(Oniguruma C 库 FFI)/esaxx_fast(esaxx-rs C++
# 后缀数组)——三者对本 driver 的编码覆盖面无任何贡献却引入 FFI/线程风险。
# 关闭后依赖树全为纯 Rust：esaxx-rs 以 default-features=false 走纯 Rust sais.rs
# （仅 Unigram 训练用到），spm_precompiled 是 prost 预编译的 sentencepiece
# charsmap 解析器（纯 Rust）。
# 绕行记录：0.20.4 的 src/utils/mod.rs 中 `mod onig` 无条件编译
# （lib.rs 无 feature 门），通体依赖 `onig` crate——default-features=false 后
# 编译 E0432/E0433（utils/onig.rs:3 `use onig::Regex`）。onig 是 Oniguruma
# 的 C FFI（cc 编译 C 源码进 native-archive）。绕行 = 开 `unstable_wasm`
# feature：把 SysRegex 后端从 onig 换成 fancy-regex（纯 Rust 回溯正则，
# 支持 lookahead），语义等价——两侧同一份后端，逐字节对拍不受影响；
# feature 顺带开的 getrandom/js 只在 wasm32 目标下激活，native 无额外依赖。
tokenizers = { version = "=0.20.4", default-features = false, features = ["unstable_wasm"] }
---
// tokenizers 0.20.4（HuggingFace，unicode 重）差分。不引 fixture：全部模型经
// builder API 内存构造。三大模型齐上：
//   ① WordLevel + Normalizer Sequence(Lowercase→NFD→StripAccents)
//      + Whitespace pretok + WordPieceDecoder + BertProcessing 后处理
//      + AddedToken special mask（aho-corasick matcher 路径）
//   ② BPE：手工 vocab+merges（merge 秩序驱动子词拆分），whitespace pretok；
//      另一支 BPE + ByteLevel(add_prefix_space/trim_offsets/regex 三参数)
//      + ByteLevelDecoder（bytes_to_unicode 映射表路径，emoji/CJK 走字节化）
//   ③ Unigram：手工 (token, f64) 分数表 Viterbi 切分（浮点分不进输出，
//      只打印 ids/tokens/offsets，IEEE 确定性下两边逐位一致）
// 覆盖：encode 单句/pair、ids/tokens/offsets/type_ids/attention_mask/
// special_tokens_mask/word_ids 全字段、decode skip/不 skip special、
// truncation 谱系（max_length/stride/左右方向/三 strategy/非法参数错误路径）、
// padding 谱系（Fixed/BatchLongest/左右方向/pad_to_multiple_of）、
// token_to_id/id_to_token/get_vocab_size、to_string→from_str 内存 roundtrip
// （compact+pretty 两种 serde_json 序列化）、空串/空 decode/纯 OOV/纯空白边界、
// WordLevel/BPE builder 错误路径。
// 确定性：不迭代 get_vocab 的 HashMap——词汇表打印用 (id,token) 按 id 排序的
// 自身构造出场序；无 batch API（rayon-cond 维持在串行阈值内）；不触 dropout/
// thread_rng（getrandom 不被调用）；无时间/地址/线程序。
// 上游怪癖锚点：tok1/tok2 的 vocab 刻意留 id 洞（10+ 起跳、20+ 起跳），
// save/to_string 触发 tokenizers models/mod.rs:54 的字面 println!
// （"The OrderedVocab you are attempting to save contains holes ..."——不是
// log::warn! 管道，直接砸 stdout），洞 id 列表由 vocab 内容决定，两侧一致。
use std::collections::HashMap;
use std::str::FromStr;

use tokenizers::decoders::wordpiece::WordPiece as WordPieceDecoder;
use tokenizers::models::bpe::BPE;
use tokenizers::models::unigram::Unigram;
use tokenizers::models::wordlevel::WordLevel;
use tokenizers::normalizers::unicode::NFD;
use tokenizers::normalizers::{Lowercase, Sequence as NSeq, StripAccents};
use tokenizers::pre_tokenizers::whitespace::Whitespace;
use tokenizers::processors::bert::BertProcessing;
use tokenizers::tokenizer::Tokenizer;
use tokenizers::{
    AddedToken, Encoding, PaddingDirection, PaddingParams, PaddingStrategy, TruncationDirection,
    TruncationParams, TruncationStrategy,
};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// encode 全字段 dump：ids/tokens/offsets/type_ids/attention_mask/
/// special_tokens_mask/word_ids，以及两种 decode。
fn dump(tk: &Tokenizer, label: &str, input: &str) {
    let e = tk.encode(input, true).unwrap();
    dump_enc(label, &e);
    println!(
        "{label} dec/skip   = {:?}",
        tk.decode(e.get_ids(), true).unwrap()
    );
    println!(
        "{label} dec/keep   = {:?}",
        tk.decode(e.get_ids(), false).unwrap()
    );
}

fn dump_enc(label: &str, e: &Encoding) {
    println!("{label} ids      = {:?}", e.get_ids());
    println!("{label} toks     = {:?}", e.get_tokens());
    println!("{label} offs     = {:?}", e.get_offsets());
    println!("{label} type_ids = {:?}", e.get_type_ids());
    println!("{label} attn     = {:?}", e.get_attention_mask());
    println!("{label} special  = {:?}", e.get_special_tokens_mask());
    println!("{label} words    = {:?}", e.get_word_ids());
}

/// WordLevel 词汇表：special 小 id + 词大 id，(id, token) 出场序即确定性打印序。
fn wordlevel_vocab() -> (Vec<(u32, &'static str)>, HashMap<String, u32>) {
    let pairs: Vec<(u32, &str)> = vec![
        (0, "[PAD]"),
        (1, "[UNK]"),
        (2, "[CLS]"),
        (3, "[SEP]"),
        (10, "hello"),
        (11, "world"),
        (12, "rust"),
        (13, "mir"),
        (14, "vm"),
        (15, "cafe"),
        (16, "naive"),
        (17, "the"),
        (18, "quick"),
        (19, "brown"),
        (20, "fox"),
        (21, "jumps"),
        (22, "over"),
        (23, "lazy"),
        (24, "dog"),
        (25, "我爱"),
        (26, "自然语言"),
        (27, "处理"),
    ];
    let vocab: HashMap<String, u32> = pairs.iter().map(|(i, t)| (t.to_string(), *i)).collect();
    (pairs, vocab)
}

fn build_wordlevel() -> Tokenizer {
    let (pairs, vocab) = wordlevel_vocab();
    println!("tok1 vocab = {:?}", pairs);
    let wl = WordLevel::builder()
        .vocab(vocab)
        .unk_token("[UNK]".to_string())
        .build()
        .unwrap();
    let mut tk = Tokenizer::new(wl);
    // Lowercase → NFD → StripAccents：NFD 把 é 拆成 e+U+0301，strip 再摘掉组合符
    tk.with_normalizer(Some(NSeq::new(vec![
        Lowercase.into(),
        NFD.into(),
        StripAccents.into(),
    ])));
    tk.with_pre_tokenizer(Some(Whitespace::default()));
    tk.with_decoder(Some(WordPieceDecoder::new("##".to_string(), true)));
    tk.with_post_processor(Some(BertProcessing::new(
        ("[SEP]".to_string(), 3),
        ("[CLS]".to_string(), 2),
    )));
    // AddedToken special：😀 与 🤖 不经 normalizer、整体命中 special mask
    let n = tk.add_special_tokens(&[
        AddedToken::from("😀", true),
        AddedToken::from("🤖", true),
    ]);
    println!("tok1 added_special n = {n}");
    println!("tok1 vocab_size plain={} added={}", tk.get_vocab_size(false), tk.get_vocab_size(true));
    tk
}

/// BPE 手工 vocab+merges：秩序驱动——"hug"→[hug]；"pug"→[pug]；
/// "bugs" merges 无 b 头缀 → 拆成 b,u,g + unk(s)；"lower"→[lo(or low), w, er]。
fn build_bpe_ws() -> (Tokenizer, Vec<(u32, &'static str)>) {
    let pairs: Vec<(u32, &str)> = vec![
        (0, "<unk>"), (1, "h"), (2, "u"), (3, "g"), (4, "p"), (5, "n"),
        (6, "e"), (7, "l"), (8, "o"), (9, "w"), (10, "r"), (11, "d"),
        (12, "b"), (13, "s"), (20, "hu"), (21, "hug"), (22, "pu"),
        (23, "pug"), (24, "lo"), (25, "low"), (26, "er"), (30, "你"),
        (31, "好"), (32, "世"), (33, "界"),
    ];
    let vocab: HashMap<String, u32> = pairs.iter().map(|(i, t)| (t.to_string(), *i)).collect();
    let merges: Vec<(String, String)> = [
        ("h", "u"), ("hu", "g"), ("p", "u"), ("pu", "g"),
        ("l", "o"), ("lo", "w"), ("e", "r"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    println!("tok2 vocab = {:?}", pairs);
    println!("tok2 merges = {:?}", merges);
    let bpe = BPE::builder()
        .unk_token("<unk>".to_string())
        .vocab_and_merges(vocab, merges)
        .build()
        .unwrap();
    let mut tk = Tokenizer::new(bpe);
    tk.with_pre_tokenizer(Some(Whitespace::default()));
    (tk, pairs)
}

/// BPE + ByteLevel 全链：chars→bytes→unicode-alphabet pre-tokenize（regex 拆分），
/// ByteLevelDecoder 还原。emoji/CJK 全部走字节化路径。
fn build_bpe_bytelevel() -> Tokenizer {
    // ByteLevel 字节字母表的两个锚点token + 合并对：
    // "Ġthe" 之类词首合并——Ġ 是空格 0x20 的映射。
    let pairs: Vec<(u32, &str)> = vec![
        (0, "a"), (1, "b"), (2, "c"), (3, "d"), (4, "e"), (5, "f"),
        (6, "g"), (7, "h"), (8, "i"), (9, "j"), (10, "k"), (11, "l"),
        (12, "m"), (13, "n"), (14, "o"), (15, "p"), (16, "q"), (17, "r"),
        (18, "s"), (19, "t"), (20, "u"), (21, "v"), (22, "w"), (23, "x"),
        (24, "y"), (25, "z"), (26, "Ġ"), (27, "Ġa"), (28, "Ġb"),
        (29, "ab"), (30, "Ġab"), (31, "abc"), (32, "Ġabc"),
    ];
    let vocab: HashMap<String, u32> = pairs.iter().map(|(i, t)| (t.to_string(), *i)).collect();
    let merges: Vec<(String, String)> = [
        ("Ġ", "a"), ("Ġ", "b"), ("a", "b"), ("Ġ", "ab"), ("ab", "c"), ("Ġ", "abc"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    let bpe = BPE::builder()
        .vocab_and_merges(vocab, merges)
        .build()
        .unwrap();
    let mut tk = Tokenizer::new(bpe);
    tk.with_pre_tokenizer(Some(
        tokenizers::pre_tokenizers::byte_level::ByteLevel::new(false, true, true),
    ));
    tk.with_decoder(Some(
        tokenizers::decoders::byte_level::ByteLevel::default(),
    ));
    tk
}

/// Unigram：手工分数表，Viterbi 做 "hello"。分数仅参与内部比较，不打印。
fn build_unigram() -> Tokenizer {
    let vocab: Vec<(String, f64)> = [
        ("<unk>", 0.0),
        ("h", -0.10),
        ("e", -0.20),
        ("l", -0.15),
        ("o", -0.30),
        ("he", -0.45),
        ("ll", -0.25),
        ("lo", -0.28),
        ("llư", -9.0), // 干扰项：高惩罚路径
    ]
    .iter()
    .map(|(t, s)| (t.to_string(), *s))
    .collect();
    let uni = Unigram::from(vocab, Some(0), false).unwrap();
    Tokenizer::new(uni)
}

fn main() {
    // ================= tok1：WordLevel 全链 =================
    let mut tk1 = build_wordlevel();
    println!(
        "tok1 lookups = {:?} {:?} {:?} {:?}",
        tk1.token_to_id("hello"),
        tk1.token_to_id("不存在"),
        tk1.id_to_token(11),
        tk1.id_to_token(9999)
    );
    for (label, s) in [
        ("t1/s0 ascii", "Hello World RUST mir vm"),
        ("t1/s1 nfc", "Café Naïve café"), // NFC：é/ï 单码点
        ("t1/s2 nfd", "Cafe\u{301} Nai\u{308}ve"), // NFD：e+U+0301, i+U+0308
        ("t1/s3 cjk", "我爱 自然语言 处理 文字"), // 部分 OOV：文字
        ("t1/s4 emoji", "hello 😀 世界 🤖"), // special token 命中 + OOV
        ("t1/s5 empty", ""),
        ("t1/s6 blanks", "   \t  "),
    ] {
        dump(&tk1, label, s);
    }
    // NFC 与 NFD 归一后 ids 必须一致
    let a = tk1.encode("Café Naïve", true).unwrap();
    let b = tk1.encode("Cafe\u{301} Nai\u{308}ve", true).unwrap();
    println!("t1/norm-eq ids_a={:?} ids_b={:?} eq={}", a.get_ids(), b.get_ids(), a.get_ids() == b.get_ids());

    // pair + BertProcessing：type_ids 0/1 分界
    let pair = tk1.encode(("the quick brown fox", "jumps over lazy dog"), true).unwrap();
    dump_enc("t1/pair", &pair);

    // ---- truncation 谱系（LongestFirst / 两个方向 / stride 溢出块） ----
    let long_pair = ("the quick brown fox jumps over lazy dog cafe naive", "hello world rust mir vm");
    for (label, tp) in [
        ("tr/right s0", TruncationParams { max_length: 8, stride: 0, direction: TruncationDirection::Right, strategy: TruncationStrategy::LongestFirst }),
        ("tr/left  s0", TruncationParams { max_length: 8, stride: 0, direction: TruncationDirection::Left, strategy: TruncationStrategy::LongestFirst }),
        ("tr/right s2", TruncationParams { max_length: 9, stride: 2, direction: TruncationDirection::Right, strategy: TruncationStrategy::LongestFirst }),
        ("tr/only1", TruncationParams { max_length: 6, stride: 0, direction: TruncationDirection::Right, strategy: TruncationStrategy::OnlyFirst }),
        ("tr/only2", TruncationParams { max_length: 6, stride: 0, direction: TruncationDirection::Right, strategy: TruncationStrategy::OnlySecond }),
    ] {
        tk1.with_truncation(Some(tp)).unwrap();
        // Err 也打印：SequenceTooShort 是合法错误路径（max_length 小于
        // special tokens 占用后无可截内容时触发）
        match tk1.encode(long_pair, true) {
            Ok(e) => {
                println!(
                    "{label} ids={:?} toks={:?} overflow_n={}",
                    e.get_ids(),
                    e.get_tokens(),
                    e.get_overflowing().len()
                );
                for (i, ov) in e.get_overflowing().iter().enumerate() {
                    println!("{label} overflow{i} toks={:?}", ov.get_tokens());
                }
            }
            Err(e) => println!("{label} err = {e}"),
        }
    }
    // 错误路径 1：单句 + OnlySecond（无第二序列可截）
    tk1.with_truncation(Some(TruncationParams {
        max_length: 4,
        stride: 0,
        direction: TruncationDirection::Right,
        strategy: TruncationStrategy::OnlySecond,
    }))
    .unwrap();
    match tk1.encode(long_pair.0, true) {
        Ok(_) => println!("tr/only2-single unexpectedly ok"),
        Err(e) => println!("tr/only2-single err = {e}"),
    }
    // 错误路径 2：stride ≥ effective max length（= max_length - n_added）
    // → with_truncation 直接拒绝。注意 max_length=0 会撞上 tokenizers 上游
    // debug 构建的 subtract-overflow panic（mod.rs:622 无减法检查），不演示。
    match tk1.with_truncation(Some(TruncationParams {
        max_length: 3,
        stride: 3,
        direction: TruncationDirection::Right,
        strategy: TruncationStrategy::LongestFirst,
    })) {
        Ok(_) => println!("tr/zero-len unexpectedly ok"),
        Err(e) => println!("tr/zero-len err = {e}"),
    }
    tk1.with_truncation(None).unwrap();

    // ---- padding 谱系 ----
    for (label, pp) in [
        ("pd/fixed12-R", PaddingParams { strategy: PaddingStrategy::Fixed(12), direction: PaddingDirection::Right, pad_to_multiple_of: None, pad_id: 0, pad_type_id: 0, pad_token: "[PAD]".to_string() }),
        ("pd/fixed12-L", PaddingParams { strategy: PaddingStrategy::Fixed(12), direction: PaddingDirection::Left, pad_to_multiple_of: None, pad_id: 0, pad_type_id: 0, pad_token: "[PAD]".to_string() }),
        ("pd/mul8-batch", PaddingParams { strategy: PaddingStrategy::BatchLongest, direction: PaddingDirection::Right, pad_to_multiple_of: Some(8), pad_id: 0, pad_type_id: 0, pad_token: "[PAD]".to_string() }),
    ] {
        tk1.with_padding(Some(pp));
        let e = tk1.encode("hello world", true).unwrap();
        println!(
            "{label} ids={:?} toks={:?} attn={:?} type={:?}",
            e.get_ids(),
            e.get_tokens(),
            e.get_attention_mask(),
            e.get_type_ids()
        );
    }
    tk1.with_padding(None);

    // ---- to_string → from_str 内存 roundtrip（compact + pretty）----
    let js = tk1.to_string(false).unwrap();
    println!("t1/ser compact len={} fnv={:016x}", js.len(), fnv1a(js.as_bytes()));
    let tk1r = Tokenizer::from_str(&js).unwrap();
    let e1 = tk1.encode("hello world rust", true).unwrap();
    let e2 = tk1r.encode("hello world rust", true).unwrap();
    println!("t1/ser roundtrip eq={} ids={:?}", e1.get_ids() == e2.get_ids(), e2.get_ids());
    let jsp = tk1.to_string(true).unwrap();
    println!("t1/ser pretty len={} fnv={:016x}", jsp.len(), fnv1a(jsp.as_bytes()));
    let tk1p = Tokenizer::from_str(&jsp).unwrap();
    let e3 = tk1p.encode("hello world rust", true).unwrap();
    println!("t1/ser pretty-roundtrip eq={}", e1.get_ids() == e3.get_ids());

    // ================= tok2：BPE 手工 merges =================
    let (tk2, _) = build_bpe_ws();
    for (label, s) in [
        ("t2/s0 merge", "hug pug bugs"),
        ("t2/s1 subword", "lower lowered"),
        ("t2/s2 cjk", "你 好 世界 你好"),
        ("t2/s3 unk", "hugs pugs xqz"),
        ("t2/s4 empty", ""),
    ] {
        dump(&tk2, label, s);
    }
    // serde roundtrip：merges 表往返
    let js2 = tk2.to_string(false).unwrap();
    println!("t2/ser len={} fnv={:016x}", js2.len(), fnv1a(js2.as_bytes()));
    let tk2r = Tokenizer::from_str(&js2).unwrap();
    let f1 = tk2.encode("hug pug lower", true).unwrap();
    let f2 = tk2r.encode("hug pug lower", true).unwrap();
    println!("t2/ser roundtrip eq={} toks={:?}", f1.get_ids() == f2.get_ids(), f2.get_tokens());

    // ---- BPE builder 错误路径：merges 引用未在 vocab 中的合成 token ----
    let bad_vocab: HashMap<String, u32> = [("a".to_string(), 0u32), ("b".to_string(), 1u32)]
        .into_iter()
        .collect();
    match BPE::builder()
        .vocab_and_merges(bad_vocab, vec![("a".to_string(), "z".to_string())])
        .build()
    {
        Ok(_) => println!("t2/bad-merges unexpectedly ok"),
        Err(e) => println!("t2/bad-merges err = {e}"),
    }

    // ================= tok3：BPE + ByteLevel =================
    let tk3 = build_bpe_bytelevel();
    for (label, s) in [
        ("t3/s0 bpe", "abc abc ab"),
        ("t3/s1 lead-space", " abc  ab"),
        ("t3/s2 emoji", "a😀b 世界"),
        ("t3/s3 mixed", "abc\txyz"),
    ] {
        dump(&tk3, label, s);
    }

    // ================= tok4：Unigram =================
    let tk4 = build_unigram();
    println!(
        "t4 lookups = {:?} {:?} {:?}",
        tk4.token_to_id("he"),
        tk4.id_to_token(0),
        tk4.get_vocab_size(false)
    );
    for (label, s) in [("t4/s0 viterbi", "hello"), ("t4/s1 unk", "helloz"), ("t4/s2 empty", "")] {
        dump(&tk4, label, s);
    }

    // ---- WordLevel 无 unk_token 行为锚点：OOV 词触发 MissingUnkToken 错误 ----
    let v_nounk: HashMap<String, u32> = [("a".to_string(), 0u32)]
        .into_iter()
        .collect();
    let wl2 = WordLevel::builder().vocab(v_nounk).build().unwrap();
    let mut tk5 = Tokenizer::new(wl2);
    tk5.with_pre_tokenizer(Some(Whitespace::default()));
    match tk5.encode("a b a 轻轻", true) {
        Ok(e) => println!("t1/no-unk ids={:?} toks={:?}", e.get_ids(), e.get_tokens()),
        Err(e) => println!("t1/no-unk err = {e}"),
    }

    // ================= 公共边界 =================
    println!("edge decode-empty = {:?}", tk1.decode(&[], true).unwrap());
    println!("edge decode-unk   = {:?}", tk1.decode(&[1], true).unwrap());
    println!(
        "edge char_to_token = {:?} {:?}",
        a.char_to_token(0, 0),
        a.char_to_token(10_000, 0)
    );
    println!(
        "edge token_to_word = {:?} {:?}",
        a.token_to_word(0),
        a.token_to_word(999)
    );
}
