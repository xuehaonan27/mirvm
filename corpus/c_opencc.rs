#!/usr/bin/env mirvm
---
[dependencies]
# opencc-rust =1.1.19（1.x 线最新；2.0.0 已于 2026-07-08 发布，本任务钉 1.x）。
# default features：不开 static-dictionaries（任务钉 default；该 feature 只是把
# 词典嵌进二进制，链接面不变）。crate 本体是 C++ OpenCC 的 extern "C" FFI
# binding（libc + pkg-config build），非纯 Rust。
opencc-rust = "=1.1.19"
---
// opencc-rust 1.1.19（OpenCC 简繁转换大表，C FFI）差分。
//
// 环境前置（本机无系统 libopencc，按任务纪律不装系统库）：
//   源码构建 OpenCC C++ ver.1.1.9（build.rs 的 pkg-config 版本区间锁
//   1.1.2..=1.2.0；上游最新 1.3/1.4 会被 max-version 拒绝）安装至
//   /tmp/opencc-local（libopencc.so + share/opencc/*.json/*.ocd2 词典）。
//   三维统一三个环境变量，缺一即回退 pkg-config 而 panic：
//     OPENCC_DIR=/tmp/opencc-local      build.rs 找 lib/include 根
//     OPENCC_LIBS=opencc                不设则 build.rs 仍以 pkg-config 猜库名
//     LD_LIBRARY_PATH=/tmp/opencc-local/lib   运行期 dlopen/native 动态链接
//   词典路径不依赖环境：opencc_open 相对名走库编译期 PKGDATADIR。
//
// 测试面（FFI 全链路）：
//   ① 四方向样本句逐字打印 + 锚定 assert_eq（发/髮、乾/幹、檯/台一对多分词
//      消歧，麵/麪 异体，ASCII/半角旁通）：四个 OpenCC 句柄（*mut c_void 不透明
//      指针，native 堆对象）并存互访。
//   ② convert_to_buffer：native 直接写入 guest String 缓冲（真实地址模型下
//      原生写回访客分配）；convert 返回串是 native malloc 的 C 串，guest
//      CStr::from_ptr 读 native 内存（无 AllocId 读路径）+ opencc_convert_utf8_free 归还。
//   ③ 错误路径：相对/绝对坏配置名 → Rust 侧静态 Err 串（稳定）。
//   ④ 一篇 ~1.3KB 简体长文：原文/s2t/s2tw/逆向 t2s 各阶段字节长度 + FNV-1a
//      指纹（长文本恰在最长匹配分词 + 大表查询上压）。
//
// 三维复跑（每维都以同一 env 前缀）：
//   A: OPENCC_DIR=/tmp/opencc-local OPENCC_LIBS=opencc LD_LIBRARY_PATH=/tmp/opencc-local/lib \
//        target/release/mirvm run corpus/c_opencc.rs
//   B: cd ~/.cache/mirvm/scripts/$(grep -rl 'name = "c_opencc"' ~/.cache/mirvm/scripts/*/Cargo.toml | head -1 | xargs dirname | xargs basename) && \
//        OPENCC_DIR=/tmp/opencc-local OPENCC_LIBS=opencc LD_LIBRARY_PATH=/tmp/opencc-local/lib \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 + A 维同 env。
//
// FRONTIER：无（先行 scratch 原生验证 FFI 链路全通；mirvm 真实地址模型预期覆盖
// native 读/写回访客所有四形态）。若日后引擎在 FFI 指针面回归，本 driver 即探针。
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

// ~1.3KB 简体长文（定长常量，含标点/数字/半角字母）。
const LONG: &str = "简繁转换的历史几乎与汉字信息化的历史等长。上世纪七十年代，中国大陆推行简化字方案之后，海峡两岸的计算机系统分别建立了各自的编码与字表，同一份文稿在两地之间流转时，需要逐字甚至逐词地对照替换。早期的转换程序大多只有一张单字映射表，遇到头发与发生、干燥与干部这类一对多的字，便会产生啼笑皆非的结果。现代的转换引擎引入分词与词组匹配，先按最长优先的策略把输入切分成词条，再在词典里查找每个词条对应的写法，因此能把方便面转换为方便麪，也能把服务器转换为伺服器。即便如此，地区词差异仍然棘手：软件在台湾常称软体，内存在台湾常称记忆体，而网络在香港又多写作網絡。优秀的转换器必须维护多套词库，并允许使用者自定义词条。本测试串约四百字，内含标点、数字 12345 与 abc 等半角符号，用于验证长文本转换在解释器、原生与即时编译三个维度上产生逐字节一致的结果。转换完成后再做一轮逆向转换，记录每一阶段输出的字节长度与哈希指纹，任何维度的分歧都会在指纹中现形。";

fn run(cc: &OpenCC, tag: &str, samples: &[(&str, &str)]) {
    for (input, expect) in samples {
        let out = cc.convert(input);
        assert_eq!(&out, expect, "{tag} anchor mismatch on {input}");
        println!("{tag} 「{input}」→「{out}」");
    }
}

fn main() {
    // 四个 native 句柄先全建（不透明指针表多个并存），再交错使用。
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

    // ① 四方向样本（逐字打印；锚点 = OpenCC 1.1.9 词典原生真值）
    run(&s2t, "s2t", S2T_SAMPLES);
    run(&t2s, "t2s", T2S_SAMPLES);
    run(&s2tw, "s2tw", S2TW_SAMPLES);
    run(&tw2s, "tw2s", TW2S_SAMPLES);

    // ② convert_to_buffer：native 写进 guest String 缓冲 + convert 读 native 串
    let head = tw2s.convert("涼風有訊");
    let buf = tw2s.convert_to_buffer("，秋月無邊", head.clone());
    assert_eq!(buf, format!("{}{}", tw2s.convert("涼風有訊"), tw2s.convert("，秋月無邊")));
    assert_eq!(buf, "凉风有讯，秋月无边");
    println!("buffer 「{head}」+=「，秋月無邊」→「{buf}」");

    // ③ 错误路径（Rust 侧静态串，稳定锚）
    for p in ["nope.json", "/nonexistent-dir-tw2sp/s2t.json"] {
        match OpenCC::new(p) {
            Ok(_) => println!("open {p} unexpected ok"),
            Err(e) => println!("open {p} err = {e}"),
        }
    }

    // ④ 长文：每阶段字节长度 + FNV-1a 指纹
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
