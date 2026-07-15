#!/usr/bin/env mirvm
---
[dependencies]
# 钉 0.10.0：0.10.2 新增 bytecount 依赖，其 num_chars 在 cut 热路径（任何
# ≥16 字节的 CJK 块）走 `llvm.x86.sse2.psad.bw` —— mirvm 未内建的 x86
# intrinsic（已知 avx2.psad.bw FRONTIER 的 SSE2 变体）。0.10.0 无此依赖，
# 字符计数为纯 Rust，API 面相同。
jieba-rs = "=0.10.0"
# jieba-rs 0.10.0 的 src/hmm.rs 只与 jieba-macros 0.10.0 生成的码表表示兼容
# （EMIT_PROBS 为 [Map<char, f64>; N]）；jieba-macros 0.10.1+ 改成
# Map<char, [f64; N]>，直接编译 E0308/E0277——上游 semver 破洞，钉死配对版本。
jieba-macros = "=0.10.0"
---
// jieba-rs 0.10：中文分词差分。内置大词典（include-flate 编译期 deflate、
// 运行期 libflate 纯 Rust 解压）→ cedarwood 双数组 trie；HMM 新词发现
// （jieba-macros 编译期 phf 码表）；posseg HMM 词性标注（运行期解析
// 256×256 对数转移矩阵）。
// 覆盖：cut(hmm 开/关) / cut_all / cut_for_search / tokenize(两 mode) /
// tag(含 guess_tag 的 eng/m/CJK 路径) / add_word(显式 freq、None→suggest_freq、
// 空词、重复加更新) / suggest_freq / has_word / empty+load_dict / with_dict /
// 坏词典错误路径；句子集含歧义句「研究生命起源」、中英混排、数字标点、emoji、
// OOV 人名、空串与纯标点边界。
// 确定性：只打印 Vec 顺序结果、计数、字节偏移与布尔——无 HashMap 迭代、
// 无地址/时间/线程序。
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

    // ① 词典健全性抽查：布尔 + suggest_freq 整数指纹（f64 对数加总路径）
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

    // ② 固定句子集 × 四种切法
    let sentences = [
        "研究生命起源",                       // 歧义：研究生/生命 vs 研究/生命
        "我来到北京清华大学",                 // 经典
        "他结婚了和尚未结婚的",               // 歧义：和 vs 和尚
        "我用Rust在GitHub上写了3个demo",      // 中英混排
        "总价3.14元,满100减20%,电话13812345678!", // 数字标点
        "今天天气不错😀我们去公园玩🎉",        // emoji 夹心
        "薛浩男在杭州西湖边写代码",           // OOV 人名（HMM）
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

    // ③ Token 偏移字段（unicode start/end + byte_start/byte_end）
    let toks = jieba.cut(sentences[1], true);
    for t in &toks {
        println!(
            "tok {} u={}..{} b={}..{}",
            t.word, t.start, t.end, t.byte_start, t.byte_end
        );
    }

    // ④ tokenize 两种 mode
    for mode in [TokenizeMode::Default, TokenizeMode::Search] {
        let toks = jieba.tokenize(sentences[0], mode, true);
        println!("tokenize {mode:?} n={} {}", toks.len(), join(&toks));
    }

    // ⑤ tag：经典句（词典词性）+ 中英混排（guess_tag 的 eng/m）+ OOV CJK（posseg HMM）
    for (label, s) in [
        ("classic", "我来到北京清华大学"),
        ("mixed", "我用Rust写了3个demo"),
        ("oov", "薛浩男在杭州西湖边写代码"),
    ] {
        let tags = jieba.tag(s, true);
        println!("tag/{label} n={} {}", tags.len(), join_tags(&tags));
    }

    // ⑥ add_word：自定义词前后对切同一句
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
    // None freq → suggest_freq 路径；空词 → 0；重复加 → 更新 freq
    let f2 = jieba.add_word("浩男分词器", None, Some("nz"));
    println!("add 浩男分词器 freq={f2}");
    let f3 = jieba.add_word("", None, None);
    println!("add empty ret={f3}");
    let f4 = jieba.add_word("研究生命", Some(60000), None);
    println!("re-add 研究生命 freq={f4}");
    let after2 = jieba.cut(s, true);
    println!("after-readd cut n={} {}", after2.len(), join(&after2));

    // ⑦ 内存自定义词典：empty + load_dict / with_dict / 坏行错误路径
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

    // ⑧ 边界：空串四 API、纯标点串
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
