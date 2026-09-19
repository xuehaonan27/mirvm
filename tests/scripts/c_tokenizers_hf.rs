#!/usr/bin/env mirvm
---
[dependencies]
# Pinned 0.20.4 (last of the 0.20 series). default-features = false drops the
# progressbar (indicatif), onig (Oniguruma C FFI) and esaxx_fast (esaxx-rs C++
# suffix array) features: none contributes to this driver's encoding coverage,
# but each adds FFI/threading risk. With them off the tree is pure Rust
# (esaxx-rs uses pure-Rust sais.rs, needed only by the Unigram trainer;
# spm_precompiled is a prost-precompiled sentencepiece charsmap parser).
# Workaround: 0.20.4's src/utils/mod.rs compiles `mod onig` unconditionally
# (lib.rs has no feature gate) and depends on the `onig` crate throughout, so
# default-features=false fails to build with E0432/E0433 (utils/onig.rs:3);
# onig is Oniguruma's C FFI, compiled by cc into a native archive. Switching
# to pure-Rust fancy-regex via `unstable_wasm` (a backtracking regex with
# lookahead) is semantically equivalent, so both sides share one backend; the
# getrandom/js feature it also enables activates only on wasm32, native adds none.
tokenizers = { version = "=0.20.4", default-features = false, features = ["unstable_wasm"] }
---
// tokenizers 0.20.4 (HuggingFace, unicode-heavy) differential. No fixture: every
// model is built in memory through the builder API. Three models:
//   ① WordLevel + Normalizer Sequence(Lowercase -> NFD -> StripAccents)
//      + Whitespace pretok + WordPieceDecoder + BertProcessing post-processor
//      + AddedToken special mask (aho-corasick matcher path)
//   ② BPE: hand-written vocab+merges (merge rank drives subword splitting),
//      whitespace pretok; a second BPE + ByteLevel (add_prefix_space /
//      trim_offsets / regex) + ByteLevelDecoder (bytes_to_unicode table path;
//      emoji/CJK take the byte path)
//   ③ Unigram: hand-written (token, f64) score table, Viterbi segmentation
//      (scores stay internal; only ids/tokens/offsets print, IEEE-deterministic)
// Covers: encode single/pair, all ids/tokens/offsets/type_ids/attention_mask/
// special_tokens_mask/word_ids fields, decode skipping or keeping specials,
// truncation spectrum (max_length/stride/direction/strategies/invalid args),
// padding spectrum (Fixed/BatchLongest/direction/pad_to_multiple_of),
// token_to_id/id_to_token/get_vocab_size, to_string -> from_str roundtrip
// (compact + pretty), empty/edge inputs, WordLevel/BPE builder error paths.
// Determinism: get_vocab's HashMap is never iterated -- the vocab prints from
// the (id, token) pairs in construction order sorted by id; no batch API
// (rayon-cond stays serial); getrandom is never called; no time/address dumps.
// Upstream quirk anchor: tok1/tok2 vocabs deliberately leave id holes (start at
// 10+, 20+); save/to_string triggers the literal println! at tokenizers
// models/mod.rs:54 ("The OrderedVocab you are attempting to save contains
// holes ..."), straight to stdout (not log::warn!); hole ids are vocab-derived.
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

/// Dump every encode field: ids/tokens/offsets/type_ids/attention_mask/
/// special_tokens_mask/word_ids, plus both decode variants.
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

/// WordLevel vocab: small ids for specials, large for words; (id, token) order is deterministic.
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
    // Lowercase -> NFD -> StripAccents: NFD splits é into e+U+0301, then strip removes it
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
    // AddedToken specials: 😀 and 🤖 skip the normalizer and hit the special mask whole
    let n = tk.add_special_tokens(&[
        AddedToken::from("😀", true),
        AddedToken::from("🤖", true),
    ]);
    println!("tok1 added_special n = {n}");
    println!("tok1 vocab_size plain={} added={}", tk.get_vocab_size(false), tk.get_vocab_size(true));
    tk
}

/// Hand-written BPE vocab+merges: rank-driven -- "hug" -> [hug], "pug" -> [pug];
/// "bugs" has no b-initial merge -> b,u,g + unk(s); "lower" -> [lo|low, w, er].
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

/// Full BPE + ByteLevel chain: chars -> bytes -> unicode-alphabet pre-tokenize
/// (regex split), restored by ByteLevelDecoder. emoji/CJK all take the byte path.
fn build_bpe_bytelevel() -> Tokenizer {
    // Two anchor tokens of the ByteLevel byte alphabet plus merge pairs:
    // word-initial merges like "Ġthe" -- Ġ maps to space 0x20.
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

/// Unigram: hand-written score table; Viterbi segments "hello". Scores stay internal.
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
        ("llư", -9.0), // distractor: high-penalty path
    ]
    .iter()
    .map(|(t, s)| (t.to_string(), *s))
    .collect();
    let uni = Unigram::from(vocab, Some(0), false).unwrap();
    Tokenizer::new(uni)
}

fn main() {
    // ================= tok1: full WordLevel chain =================
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
        ("t1/s1 nfc", "Café Naïve café"), // NFC: é/ï are single code points
        ("t1/s2 nfd", "Cafe\u{301} Nai\u{308}ve"), // NFD: e+U+0301, i+U+0308
        ("t1/s3 cjk", "我爱 自然语言 处理 文字"), // partial OOV: the last word
        ("t1/s4 emoji", "hello 😀 世界 🤖"), // special token hits + OOV
        ("t1/s5 empty", ""),
        ("t1/s6 blanks", "   \t  "),
    ] {
        dump(&tk1, label, s);
    }
    // NFC and NFD must normalize to identical ids
    let a = tk1.encode("Café Naïve", true).unwrap();
    let b = tk1.encode("Cafe\u{301} Nai\u{308}ve", true).unwrap();
    println!("t1/norm-eq ids_a={:?} ids_b={:?} eq={}", a.get_ids(), b.get_ids(), a.get_ids() == b.get_ids());

    // pair + BertProcessing: type_ids split at 0/1
    let pair = tk1.encode(("the quick brown fox", "jumps over lazy dog"), true).unwrap();
    dump_enc("t1/pair", &pair);

    // ---- truncation spectrum (LongestFirst / both directions / stride overflow) ----
    let long_pair = ("the quick brown fox jumps over lazy dog cafe naive", "hello world rust mir vm");
    for (label, tp) in [
        ("tr/right s0", TruncationParams { max_length: 8, stride: 0, direction: TruncationDirection::Right, strategy: TruncationStrategy::LongestFirst }),
        ("tr/left  s0", TruncationParams { max_length: 8, stride: 0, direction: TruncationDirection::Left, strategy: TruncationStrategy::LongestFirst }),
        ("tr/right s2", TruncationParams { max_length: 9, stride: 2, direction: TruncationDirection::Right, strategy: TruncationStrategy::LongestFirst }),
        ("tr/only1", TruncationParams { max_length: 6, stride: 0, direction: TruncationDirection::Right, strategy: TruncationStrategy::OnlyFirst }),
        ("tr/only2", TruncationParams { max_length: 6, stride: 0, direction: TruncationDirection::Right, strategy: TruncationStrategy::OnlySecond }),
    ] {
        tk1.with_truncation(Some(tp)).unwrap();
        // Errors are printed too: SequenceTooShort is a valid path (fired when
        // max_length leaves nothing to truncate after the special tokens)
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
    // Error path 1: single sequence + OnlySecond (no second sequence to truncate)
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
    // Error path 2: stride >= effective max length (= max_length - n_added),
    // which with_truncation rejects outright. NOTE: max_length=0 hits an
    // upstream subtract-overflow panic in debug builds (mod.rs:622), so it is omitted.
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

    // ---- padding spectrum ----
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

    // ---- to_string -> from_str in-memory roundtrip (compact + pretty) ----
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

    // ================= tok2: hand-written BPE merges =================
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
    // serde roundtrip: the merges table survives the trip
    let js2 = tk2.to_string(false).unwrap();
    println!("t2/ser len={} fnv={:016x}", js2.len(), fnv1a(js2.as_bytes()));
    let tk2r = Tokenizer::from_str(&js2).unwrap();
    let f1 = tk2.encode("hug pug lower", true).unwrap();
    let f2 = tk2r.encode("hug pug lower", true).unwrap();
    println!("t2/ser roundtrip eq={} toks={:?}", f1.get_ids() == f2.get_ids(), f2.get_tokens());

    // ---- BPE builder error path: a merge references a synthesized token not in the vocab ----
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

    // ================= tok3: BPE + ByteLevel =================
    let tk3 = build_bpe_bytelevel();
    for (label, s) in [
        ("t3/s0 bpe", "abc abc ab"),
        ("t3/s1 lead-space", " abc  ab"),
        ("t3/s2 emoji", "a😀b 世界"),
        ("t3/s3 mixed", "abc\txyz"),
    ] {
        dump(&tk3, label, s);
    }

    // ================= tok4: Unigram =================
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

    // ---- WordLevel without unk_token anchor: an OOV word raises MissingUnkToken ----
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

    // ================= shared edges =================
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
