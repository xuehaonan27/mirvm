#!/usr/bin/env mirvm
---
[dependencies]
# opencc-rust =1.1.19 (latest on the 1.x line; 2.0.0 was released 2026-07-08 and this
# fixture stays pinned to 1.x). Default features, so static-dictionaries is off: it only
# embeds the dictionaries and leaves the link surface unchanged. A C++ OpenCC FFI crate
# (libc + pkg-config build), not pure Rust.
opencc-rust = "=1.1.19"
---
// Differential driver for OpenCC's simplified/traditional conversion tables (the
// opencc-rust 1.1.19 crate) behind a C FFI.
// Environment: no system libopencc on this host, and installing system libraries is out
// of scope. Build the OpenCC C++ source ver.1.1.9 and install it to /tmp/opencc-local
// (libopencc.so + share/opencc/*.json/*.ocd2 dictionaries). build.rs pins the
// pkg-config range to 1.1.2..=1.2.0, so upstream 1.3/1.4 are rejected.
// All three dimensions must share these variables; missing one means a pkg-config
// fallback and panic:
//   OPENCC_DIR=/tmp/opencc-local           root in which build.rs finds lib/include
//   OPENCC_LIBS=opencc                     otherwise build.rs still guesses a library
//   LD_LIBRARY_PATH=/tmp/opencc-local/lib  runtime dlopen/native dynamic linking
// Dictionary paths need no environment: opencc_open resolves relative names through the
// library's compile-time PKGDATADIR.
// What it exercises: four-direction sample sentences printed verbatim with anchored
// assert_eq, covering one-to-many segmentation disambiguation, variant characters and
// ASCII/half-width bypass, through four OpenCC handles (opaque *mut c_void native heap
// objects) that coexist and are used interleaved; convert_to_buffer, where native code
// writes straight into a guest String buffer and convert returns a native malloc'd C
// string that the guest reads with CStr::from_ptr (no AllocId read path) and frees with
// opencc_convert_utf8_free; relative and absolute bad config names, which produce a
// stable Rust-side Err; and a ~1.3KB simplified-Chinese article, whose per-stage byte
// lengths and FNV-1a fingerprints stress longest-match segmentation plus large lookups.
// Each dimension re-runs this fixture with the env prefix above:
//   A: OPENCC_DIR=/tmp/opencc-local OPENCC_LIBS=opencc LD_LIBRARY_PATH=/tmp/opencc-local/lib \
//        target/release/mirvm run corpus/c_opencc.rs
//   B: the cached script's Cargo project with dimension A's env, RUSTC and cargo
//      pointing at the nightly-2026-07-02 toolchain, `cargo run -q`
//   C: MIRVM_JIT_THRESHOLD=1 with dimension A's env.
// The native scratch runs took this FFI chain end to end, and the real address model is
// expected to cover all four native-to-guest read/write forms. If the engine ever
// regresses on the FFI pointer surface, this driver is the probe.
use opencc_rust::*;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const S2T_SAMPLES: &[(&str, &str)] = &[
    ("头发的发展历史", "頭髮的發展歷史"),
    ("计算机软件发展历史", "計算機軟件發展歷史"),
    ("鼠标移动服务器，方便面打印出来", "鼠標移動服務器，方便麪打印出來"),
    ("OpenCC 开放中文转换 v123 seq。", "OpenCC 開放中文轉換 v123 seq。"),
];
const T2S_SAMPLES: &[(&str, &str)] = &[
    ("頭髮的發展歷史", "头发的发展历史"),
    ("計算機軟體與網際網路", "计算机软体与网际网路"),
    ("臺灣、香港、澳門與釣魚臺", "台湾、香港、澳门与钓鱼台"),
    ("OpenCC 開放中文轉換 v456 seq。", "OpenCC 开放中文转换 v456 seq。"),
];
const S2TW_SAMPLES: &[(&str, &str)] = &[
    ("软件、服务器、内存、网络、鼠标", "軟件、服務器、內存、網絡、鼠標"),
    ("里面、头发、干燥、回复、面条", "裡面、頭髮、乾燥、回覆、麵條"),
    ("占领与台历，干杯中的乾坤", "佔領與檯曆，乾杯中的乾坤"),
];
const TW2S_SAMPLES: &[(&str, &str)] = &[
    ("軟體、伺服器、記憶體、網路、滑鼠", "软体、伺服器、记忆体、网路、滑鼠"),
    ("裡面、頭髮、乾燥、回覆、麵條", "里面、头发、干燥、回复、面条"),
    ("臺灣正體中文轉換測試字串", "台湾正体中文转换测试字串"),
];

// ~1.3KB simplified-Chinese article: fixed length, with punctuation, digits and
// half-width letters.
const LONG: &str = "简繁转换的历史几乎与汉字信息化的历史等长。上世纪七十年代，中国大陆推行简化字方案之后，海峡两岸的计算机系统分别建立了各自的编码与字表，同一份文稿在两地之间流转时，需要逐字甚至逐词地对照替换。早期的转换程序大多只有一张单字映射表，遇到头发与发生、干燥与干部这类一对多的字，便会产生啼笑皆非的结果。现代的转换引擎引入分词与词组匹配，先按最长优先的策略把输入切分成词条，再在词典里查找每个词条对应的写法，因此能把方便面转换为方便麪，也能把服务器转换为伺服器。即便如此，地区词差异仍然棘手：软件在台湾常称软体，内存在台湾常称记忆体，而网络在香港又多写作網絡。优秀的转换器必须维护多套词库，并允许使用者自定义词条。本测试串约四百字，内含标点、数字 12345 与 abc 等半角符号，用于验证长文本转换在解释器、原生与即时编译三个维度上产生逐字节一致的结果。转换完成后再做一轮逆向转换，记录每一阶段输出的字节长度与哈希指纹，任何维度的分歧都会在指纹中现形。";

fn run(cc: &OpenCC, tag: &str, samples: &[(&str, &str)]) {
    for (input, expect) in samples {
        let out = cc.convert(input);
        assert_eq!(&out, expect, "{tag} anchor mismatch on {input}");
        println!("{tag} 「{input}」→「{out}」");
    }
}

fn main() {
    // Build all four native handles first, then use them interleaved.
    let s2t = OpenCC::new(DefaultConfig::S2T).unwrap();
    let t2s = OpenCC::new(DefaultConfig::T2S).unwrap();
    let s2tw = OpenCC::new(DefaultConfig::S2TW).unwrap();
    let tw2s = OpenCC::new(DefaultConfig::TW2S).unwrap();
    println!(
        "cfg = {:?}",
        [
            DefaultConfig::S2T.get_file_name(),
            DefaultConfig::T2S.get_file_name(),
            DefaultConfig::S2TW.get_file_name(),
            DefaultConfig::TW2S.get_file_name(),
        ]
    );

    // (1) Four-direction samples, printed verbatim; the anchors are OpenCC 1.1.9's native
    // dictionary values.
    run(&s2t, "s2t", S2T_SAMPLES);
    run(&t2s, "t2s", T2S_SAMPLES);
    run(&s2tw, "s2tw", S2TW_SAMPLES);
    run(&tw2s, "tw2s", TW2S_SAMPLES);

    // (2) convert_to_buffer: native code writes into a guest String buffer, and convert
    // reads back a native string.
    let head = tw2s.convert("涼風有訊");
    let buf = tw2s.convert_to_buffer("，秋月無邊", head.clone());
    assert_eq!(buf, format!("{}{}", tw2s.convert("涼風有訊"), tw2s.convert("，秋月無邊")));
    assert_eq!(buf, "凉风有讯，秋月无边");
    println!("buffer 「{head}」+=「，秋月無邊」→「{buf}」");

    // (3) Error paths; the Rust-side strings are static, so the anchors are stable.
    for p in ["nope.json", "/nonexistent-dir-tw2sp/s2t.json"] {
        match OpenCC::new(p) {
            Ok(_) => println!("open {p} unexpected ok"),
            Err(e) => println!("open {p} err = {e}"),
        }
    }

    // (4) Long article: per-stage byte length plus FNV-1a fingerprint.
    assert!(LONG.len() < 2048);
    let long_t = s2t.convert(LONG);
    let long_tw = s2tw.convert(LONG);
    let long_rt = t2s.convert(&long_t);
    for (tag, s) in [
        ("orig", LONG),
        ("s2t", long_t.as_str()),
        ("s2tw", long_tw.as_str()),
        ("t2s(s2t)", long_rt.as_str()),
    ] {
        println!("long {tag} bytes={} fnv={:016x}", s.len(), fnv1a(s.as_bytes()));
    }
}
