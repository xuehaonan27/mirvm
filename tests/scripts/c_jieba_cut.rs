#!/usr/bin/env mirvm
---
[dependencies]
# Pinned to 0.10.0: 0.10.2 adds a bytecount dependency whose num_chars hits
# `llvm.x86.sse2.psad.bw` on the cut hot path (any CJK chunk >=16 bytes), an x86 intrinsic
# mirvm does not build (the SSE2 variant of the known avx2.psad.bw FRONTIER). 0.10.0 has
# no such dependency; its character counting is pure Rust and the API surface is the same.
jieba-rs = "=0.10.0"
# jieba-rs 0.10.0 src/hmm.rs is compatible only with the code-table representation that
# jieba-macros 0.10.0 generates (EMIT_PROBS as [Map<char, f64>; N]); jieba-macros 0.10.1+
# switched to Map<char, [f64; N]>, which fails to compile with E0308/E0277, so the pair is pinned.
jieba-macros = "=0.10.0"
---
// jieba-rs 0.10: Chinese word-segmentation differential. The bundled dictionary
// (include-flate compile-time deflate, libflate pure-Rust runtime inflate) builds a
// cedarwood double-array trie; HMM new-word discovery uses jieba-macros compile-time
// phf tables; posseg HMM tagging parses a 256x256 log transition matrix at runtime.
// Coverage: cut(hmm on/off) / cut_all / cut_for_search / tokenize(both modes) /
// tag (incl. guess_tag's eng/m/CJK paths) / add_word (explicit freq, None -> suggest_freq,
// empty word, re-add updates) / suggest_freq / has_word / empty+load_dict / with_dict /
// bad-dict error path; sentences cover ambiguous segmentation, mixed Chinese/Latin text,
// digits and punctuation, emoji, an OOV personal name, and empty/punctuation-only boundaries.
// Determinism: only Vec-ordered results, counts, byte offsets and bools are printed --
// no HashMap iteration, addresses, time, or thread order.
use jieba_rs::{Jieba, Tag, Token, TokenizeMode};
use std::io::BufReader;

fn join(toks: &[Token]) -> String {
    toks.iter().map(|t| t.word).collect::<Vec<_>>().join("/")
}

fn join_tags(tags: &[Tag]) -> String {
    tags.iter()
        .map(|t| format!("{}:{}", t.word, t.tag))
        .collect::<Vec<_>>()
        .join("/")
}

fn main() {
    let mut jieba = Jieba::new();

    // ① dictionary sanity probes: bools + suggest_freq integer fingerprints (the f64 log-sum path)
    println!(
        "probe has 中国={} 开源={} 研究生={} 不存在的词={}",
        jieba.has_word("中国"),
        jieba.has_word("开源"),
        jieba.has_word("研究生"),
        jieba.has_word("不存在的词")
    );
    println!(
        "suggest 研究生命={} 生命起源={}",
        jieba.suggest_freq("研究生命"),
        jieba.suggest_freq("生命起源")
    );

    // ② fixed sentence set x four segmentation modes
    let sentences = [
        "研究生命起源",                       // ambiguous: grad-student/life vs research/life
        "我来到北京清华大学",                 // classic example
        "他结婚了和尚未结婚的",               // ambiguous: "and" vs the word "monk"
        "我用Rust在GitHub上写了3个demo",      // mixed Chinese/Latin
        "总价3.14元,满100减20%,电话13812345678!", // digits and punctuation
        "今天天气不错😀我们去公园玩🎉",        // emoji in the middle
        "薛浩男在杭州西湖边写代码",           // OOV personal name (HMM)
    ];
    for (i, s) in sentences.iter().enumerate() {
        let a = jieba.cut(s, true);
        let b = jieba.cut(s, false);
        let c = jieba.cut_all(s);
        let d = jieba.cut_for_search(s, true);
        println!("s{i} chars={}", s.chars().count());
        println!("s{i} cut/hmm   n={} {}", a.len(), join(&a));
        println!("s{i} cut/nohmm n={} {}", b.len(), join(&b));
        println!("s{i} cut_all   n={} {}", c.len(), join(&c));
        println!("s{i} search    n={} {}", d.len(), join(&d));
    }

    // ③ Token offset fields (unicode start/end + byte_start/byte_end)
    let toks = jieba.cut(sentences[1], true);
    for t in &toks {
        println!(
            "tok {} u={}..{} b={}..{}",
            t.word, t.start, t.end, t.byte_start, t.byte_end
        );
    }

    // ④ tokenize in both modes
    for mode in [TokenizeMode::Default, TokenizeMode::Search] {
        let toks = jieba.tokenize(sentences[0], mode, true);
        println!("tokenize {mode:?} n={} {}", toks.len(), join(&toks));
    }

    // ⑤ tag: classic sentence (dictionary POS) + mixed text (guess_tag eng/m) + OOV CJK (posseg HMM)
    for (label, s) in [
        ("classic", "我来到北京清华大学"),
        ("mixed", "我用Rust写了3个demo"),
        ("oov", "薛浩男在杭州西湖边写代码"),
    ] {
        let tags = jieba.tag(s, true);
        println!("tag/{label} n={} {}", tags.len(), join_tags(&tags));
    }

    // ⑥ add_word: segment the same sentence before and after adding a custom word
    let s = "研究生命起源";
    let f1 = jieba.add_word("研究生命", Some(50000), Some("nz"));
    println!(
        "add 研究生命 freq={f1} has={}",
        jieba.has_word("研究生命")
    );
    let after = jieba.cut(s, true);
    println!("after-add cut n={} {}", after.len(), join(&after));
    let tags = jieba.tag(s, true);
    println!("after-add tag n={} {}", tags.len(), join_tags(&tags));
    // None freq -> suggest_freq path; empty word -> 0; re-add -> update freq
    let f2 = jieba.add_word("浩男分词器", None, Some("nz"));
    println!("add 浩男分词器 freq={f2}");
    let f3 = jieba.add_word("", None, None);
    println!("add empty ret={f3}");
    let f4 = jieba.add_word("研究生命", Some(60000), None);
    println!("re-add 研究生命 freq={f4}");
    let after2 = jieba.cut(s, true);
    println!("after-readd cut n={} {}", after2.len(), join(&after2));

    // ⑦ in-memory custom dictionaries: empty + load_dict / with_dict / bad-line error path
    let mut d = Jieba::empty();
    let mut br = BufReader::new("自定义词 100 nz\n另一个词 50\n无频词\n".as_bytes());
    d.load_dict(&mut br).unwrap();
    println!(
        "custom has 自定义词={} 中国={}",
        d.has_word("自定义词"),
        d.has_word("中国")
    );
    let ct = d.cut("这是自定义词和另一个词", true);
    println!("custom cut n={} {}", ct.len(), join(&ct));
    let ct2 = d.tag("这是自定义词和另一个词", true);
    println!("custom tag n={} {}", ct2.len(), join_tags(&ct2));
    let mut br2 = BufReader::new("苹果 10 n\n香蕉 20 n\n".as_bytes());
    let d2 = Jieba::with_dict(&mut br2).unwrap();
    let ft = d2.cut("苹果和香蕉", false);
    println!("with_dict cut n={} {}", ft.len(), join(&ft));
    let mut br3 = BufReader::new("好词 10 n\n坏词 abc x\n".as_bytes());
    match Jieba::with_dict(&mut br3) {
        Ok(_) => println!("bad-dict: unexpected ok"),
        Err(e) => println!("bad-dict err: {e}"),
    }

    // ⑧ boundaries: empty string on four APIs, punctuation-only string
    println!(
        "empty cut={} all={} search={} tag={}",
        jieba.cut("", true).len(),
        jieba.cut_all("").len(),
        jieba.cut_for_search("", true).len(),
        jieba.tag("", true).len()
    );
    let p = jieba.cut("，。！？、", true);
    println!("punct cut n={} {}", p.len(), join(&p));
}
