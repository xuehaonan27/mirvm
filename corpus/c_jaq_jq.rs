#!/usr/bin/env mirvm
---
[dependencies]
jaq-core = "2"
jaq-std = "2"
jaq-json = { version = "1", features = ["serde_json"] }
serde_json = "1"
---
// jaq-core 2 / jaq-std 2 / jaq-json 1（纯 Rust jq 解释器，meta 查询语言）差分。
// 链路：Loader(jaq_std::defs + jaq_json::defs 作 prelude) parse →
// Compiler(with_funs std+json natives) compile → Filter::run 逐值输出。
// jaq-json 的 Val 用 IndexMap 存对象——插入序确定（keys 经 sort，
// keys_unsorted 经 hifijson/fromjson 保源序），数组序即 jq 语义序；
// 无 HashMap 随机序、无时间/地址/线程序。
//
// 覆盖：路径/索引/切片/迭代/替代（.a.b[1]、.[]、[0:2]、//）、map(select(.x>2))、
// sort/sort_by/group_by/unique/unique_by/min/max、reduce/foreach/$var 绑定、
// keys 与 keys_unsorted、to_entries/from_entries、paths/path_values、
// type 谱系与 is* 选择器、字符串（explode/implode/split/join/ltrimstr/
// startswith/contains/indices）、regex-lite（test/capture/gsub/splits）、
// 格式化（@csv/@tsv/@sh/@html/@uri/@base64 往返）、tojson/fromjson
// （hifijson 解析器：大整数保串、插入序保持）、libm 数学（sqrt/pow/log/exp/
// floor/ceil/round）与 0/0、±1/0 的 nan/inf 位型（浮点一律附 to_bits 锁位）、
// 时间（gmtime/mktime/strftime/strptime/fromdate/todate 固定历元，纯 chrono
// UTC 日期算术，不碰 now/localtime/env）、更新（|=、del、getpath）、
// walk/flatten/transpose/recurse、limit/range/inputs 空流、try/catch；
// 错误路径三类：parse/lex 错（期望 vs 实见 token 文本）、compile 错
// （未定义 filter/变量）、运行期错（类型算术错/error() 值载荷/has 非容器/
// 数组用字符串索引）。附 identity roundtrip（Val ⇄ serde_json::Value 等值）。
use jaq_core::load::{Arena, Error as LoadError, File, Loader};
use jaq_core::{Compiler, Ctx, Filter, Native, RcIter};
use jaq_json::Val;
use serde_json::{json, Value};

/// parse + compile；失败时逐条打印诊断文本（错误形状本身是差分对象）。
fn compile(src: &str) -> Option<Filter<Native<Val>>> {
    let program = File { code: src, path: () };
    let loader = Loader::new(jaq_std::defs().chain(jaq_json::defs()));
    let arena = Arena::default();
    let modules = match loader.load(&arena, program) {
        Ok(m) => m,
        Err(errs) => {
            for (_file, e) in &errs {
                match e {
                    LoadError::Lex(es) => {
                        for (exp, found) in es {
                            println!("  lex-err: expected {}, found {found:?}", exp.as_str());
                        }
                    }
                    LoadError::Parse(es) => {
                        for (exp, found) in es {
                            // opt_as_str 在 EOF 时返回空串
                            let f: &str = if found.is_empty() { "<eof>" } else { found };
                            println!("  parse-err: expected {}, found {f:?}", exp.as_str());
                        }
                    }
                    LoadError::Io(es) => {
                        for (path, msg) in es {
                            println!("  io-err: {path}: {msg}");
                        }
                    }
                }
            }
            return None;
        }
    };
    match Compiler::default()
        .with_funs(jaq_std::funs().chain(jaq_json::funs()))
        .compile(modules)
    {
        Ok(f) => Some(f),
        Err(errs) => {
            for (_file, es) in &errs {
                for (name, u) in es {
                    println!("  compile-err: {name} undefined {} {u:?}", u.as_str());
                }
            }
            None
        }
    }
}

/// 运行单个 filter，逐条打印输出值（浮点附 to_bits 锁位）或运行期错误。
fn run(doc: &Value, src: &str) {
    println!("== {src}");
    let Some(filter) = compile(src) else { return };
    let inputs = RcIter::new(core::iter::empty());
    let out = filter.run((Ctx::new([], &inputs), Val::from(doc.clone())));
    for r in out {
        match r {
            Ok(Val::Float(f)) => println!("  => {} bits={:016x}", Val::Float(f), f.to_bits()),
            Ok(v) => println!("  => {v}"),
            Err(e) => println!("  !> {e}"),
        }
    }
}

fn main() {
    let doc = json!({
        "a": {"b": [10, 20, 30], "c": {"d": null}},
        "items": [
            {"x": 1, "k": "b", "name": "pear"},
            {"x": 3, "k": "a", "name": "fig"},
            {"x": 5, "k": "b", "name": "apple"},
            {"x": 2, "k": "a", "name": "kiwi"},
            {"x": 4, "k": "b", "name": "fig"}
        ],
        "nums": [3, 1, 4, 1, 5, 9, 2, 6],
        "mixed": [1, "two", 2.5, true, null, [3], {"k": 4}],
        "unicode": "héllo 汉字 🦀",
        "words": ["pear", "apple", "fig", "kiwi", "avocado"],
        "csv_row": "a,b,,c",
        "greeting": "Hello World",
        "nested": [[1, [2, [3]]], 4],
        "empty_obj": {},
        "empty_arr": []
    });

    // ① 路径 / 索引 / 切片 / 迭代 / 替代
    for f in [
        ".a.b[1]",
        ".a.b[]",
        ".items[2].name",
        ".a.c.d",
        ".a.b[10]",
        ".items[-1].x",
        ".a.b[0:2]",
        ".missing // \"fallback\"",
    ] {
        run(&doc, f);
    }

    // ② map / select / 比较 / 布尔聚合
    for f in [
        ".items | map(select(.x > 2)) | map(.name)",
        ".nums | map(select(. >= 2 and . <= 6))",
        ".items | map(.x) | add",
        ".nums | any(. > 8)",
        ".nums | all(. > 0)",
        "isempty(.nums[] | select(. > 100))",
    ] {
        run(&doc, f);
    }

    // ③ sort / sort_by / group_by / unique / min / max
    for f in [
        ".nums | sort",
        ".items | sort_by(.x) | map(.name)",
        ".items | group_by(.k) | map({key: .[0].k, names: map(.name)})",
        ".nums | unique",
        ".items | unique_by(.name) | map(.name)",
        ".nums | [min, max]",
        ".mixed | [sort_by(type)[] | type] | unique",
    ] {
        run(&doc, f);
    }

    // ④ reduce / foreach / 变量绑定
    for f in [
        ".nums | reduce .[] as $x (0; . + $x)",
        ".nums | reduce .[] as $x (1; . * $x)",
        "[foreach .nums[] as $x (0; . + $x)]",
        ".items[0] as $o | ($o | to_entries | from_entries) == $o",
    ] {
        run(&doc, f);
    }

    // ⑤ keys / entries / paths
    for f in [
        ". | keys",
        ".items[0] | keys_unsorted",
        "\"{\\\"z\\\":1,\\\"a\\\":2,\\\"m\\\":3}\" | fromjson | keys_unsorted",
        ".a | paths",
        "[paths] | length",
        ".items[0] | to_entries",
        ".a | path_values",
    ] {
        run(&doc, f);
    }

    // ⑥ type 谱系 + is* 选择器
    for f in [
        ".mixed | map(type)",
        "[.mixed[] | numbers]",
        "[.mixed[] | strings]",
        "[.mixed[] | booleans]",
        "[.mixed[] | arrays]",
        "[.mixed[] | objects]",
        "[.mixed[] | nulls]",
        ".empty_arr, .empty_obj | type",
    ] {
        run(&doc, f);
    }

    // ⑦ 字符串 / 正则（regex-lite）
    for f in [
        ".unicode | length",
        ".unicode | explode | length",
        ".unicode | explode | implode",
        ".csv_row | split(\",\")",
        ".greeting | ascii_downcase",
        ".greeting | [startswith(\"He\"), endswith(\"ld\")]",
        "\"banana\" | indices(\"an\")",
        ".greeting | [ltrimstr(\"Hello \"), rtrimstr(\"World\")]",
        ".words | map(test(\"^a\"))",
        ".words | map(capture(\"^(?P<first>.)\"))",
        ".greeting | gsub(\"o\"; \"0\")",
        "\"a1b22c333\" | [splits(\"[0-9]+\")]",
        ".words | join(\", \")",
    ] {
        run(&doc, f);
    }

    // ⑧ 格式化 @*（aho-corasick / base64 / urlencoding）
    for f in [
        "[\"a,b\", \"c\\\"d\", null, 3] | @csv",
        "[\"x\ty\", 1, true, null] | @tsv",
        "[\"it's\", \"a b\", null] | @sh",
        "\"<a href=\\\"x\\\">&</a>\" | @html",
        ".unicode | @uri",
        ".unicode | @base64",
        ".unicode | @base64 | @base64d",
        ".items[0] | tojson",
    ] {
        run(&doc, f);
    }

    // ⑨ tojson/fromjson roundtrip（hifijson 解析：大整数保串、键序保源序）
    for f in [
        ".nums as $n | ($n | tojson | fromjson) == $n",
        "\"123456789012345678901234567890\" | fromjson",
        "\"[3, 1, 4]\" | fromjson | sort",
        "\"2.5e3\" | fromjson",
        ".unicode | tojson | fromjson",
    ] {
        run(&doc, f);
    }

    // ⑩ 数学 / 浮点位型（libm 精确位型 + nan/inf）
    for f in [
        "10 / 3",
        "7 % 3",
        "2 | sqrt",
        "pow(2; 10)",
        "10 | log",
        "1 | exp",
        "2.5 | floor",
        "2.5 | ceil",
        "-2.5 | round",
        "0 / 0",
        "1 / 0",
        "-1 / 0",
    ] {
        run(&doc, f);
    }

    // ⑪ 时间（固定历元，chrono 纯 UTC 日期算术）
    for f in [
        "0 | gmtime",
        "1234567890 | gmtime | mktime",
        "1234567890 | gmtime | strftime(\"%Y-%m-%dT%H:%M:%SZ\")",
        "\"2009-02-13T23:31:30Z\" | strptime(\"%Y-%m-%dT%H:%M:%SZ\") | mktime",
        "\"2009-02-13T23:31:30Z\" | fromdate",
        "1234567890 | todate",
    ] {
        run(&doc, f);
    }

    // ⑫ 更新 / 递归 / 发生器 / try-catch
    for f in [
        ".a.b[0] |= (. + 100) | .a.b",
        "del(.items[0]) | .items | length",
        "getpath([\"a\", \"b\", 2])",
        "getpath([\"a\", \"zz\", 0])",
        "[.. | numbers] | sort",
        ".nested | flatten",
        "[[1, 2], [3, 4, 5]] | transpose",
        "walk(if isnumber then . * 10 else . end) | .nums",
        "limit(3; .nums[])",
        "limit(0; .nums[])",
        "[range(2; 9; 3)]",
        "[inputs]",
        "try (.mixed | map(. + 1)) catch .",
        ".nums | [first, last]",
        "1, 2 | . + 10",
    ] {
        run(&doc, f);
    }

    // ⑬ 错误路径：parse / compile / 运行期
    for f in [
        ".a |",
        "(.",
        "no_such_filter_xyz",
        "$undefined_var",
        "1 + \"x\"",
        "\"str\" | keys",
        "{code: 42} | error",
        ".a | error(\"boom\")",
        "[1, 2] | .[\"a\"]",
    ] {
        run(&doc, f);
    }

    // ⑭ identity roundtrip：Val ⇄ serde_json::Value 等值不变量
    println!("== roundtrip");
    if let Some(filter) = compile(".") {
        let inputs = RcIter::new(core::iter::empty());
        let out: Vec<_> = filter
            .run((Ctx::new([], &inputs), Val::from(doc.clone())))
            .collect();
        let eq = match out.as_slice() {
            [Ok(v)] => Value::from(v.clone()) == doc,
            _ => false,
        };
        println!("  n={} eq={eq}", out.len());
    }
}
