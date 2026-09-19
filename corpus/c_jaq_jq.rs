#!/usr/bin/env mirvm
---
[dependencies]
jaq-core = "2"
jaq-std = "2"
jaq-json = { version = "1", features = ["serde_json"] }
serde_json = "1"
---
// jaq-core 2 / jaq-std 2 / jaq-json 1 differential (pure-Rust jq interpreter, meta query language).
// Chain: Loader(jaq_std::defs + jaq_json::defs as prelude) parse ->
// Compiler(with_funs std+json natives) compile -> Filter::run emits one value at a time.
// jaq-json stores object entries in a Val as an IndexMap -- insertion order is deterministic
// (keys sorts, keys_unsorted keeps source order via hifijson/fromjson), array order is jq
// semantic order; no HashMap random order, no time/address/thread order.
//
// Covers: path/index/slice/iteration/alternative (.a.b[1], .[], [0:2], //), map(select(.x>2)),
// sort/sort_by/group_by/unique/unique_by/min/max, reduce/foreach/$var binding,
// keys and keys_unsorted, to_entries/from_entries, paths/path_values,
// type lineage and is* selectors, strings (explode/implode/split/join/ltrimstr/
// startswith/contains/indices), regex-lite (test/capture/gsub/splits),
// formatting (@csv/@tsv/@sh/@html/@uri/@base64 roundtrip), tojson/fromjson
// (hifijson: big integers stay strings, insertion order preserved), libm math (sqrt/pow/log/
// exp/floor/ceil/round) and the nan/inf bit patterns of 0/0 and ±1/0 (floats carry to_bits),
// time (gmtime/mktime/strftime/strptime/fromdate/todate at a fixed epoch, pure chrono
// UTC date arithmetic, never touching now/localtime/env), updates (|=, del, getpath),
// walk/flatten/transpose/recurse, empty limit/range/inputs streams, try/catch;
// three error paths: parse/lex errors (expected vs actual token text), compile errors
// (undefined filter/variable), runtime errors (bad type arithmetic, error() payload, has on a
// non-container, bad string index). Plus an identity roundtrip: Val <-> serde_json::Value.
use jaq_core::load::{Arena, Error as LoadError, File, Loader};
use jaq_core::{Compiler, Ctx, Filter, Native, RcIter};
use jaq_json::Val;
use serde_json::{json, Value};

/// Parse and compile; on failure prints each diagnostic (the error shape is the differential).
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
                            // opt_as_str returns an empty string at EOF
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

/// Runs one filter, printing each output value (floats carry to_bits) or its runtime error.
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

    // ① path / index / slice / iteration / alternative
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

    // ② map / select / comparison / boolean aggregation
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

    // ④ reduce / foreach / variable binding
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

    // ⑥ type lineage + is* selectors
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

    // ⑦ strings / regex (regex-lite)
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

    // ⑧ formatting @* (aho-corasick / base64 / urlencoding)
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

    // ⑨ tojson/fromjson roundtrip (hifijson: big integers stay strings, key order preserved)
    for f in [
        ".nums as $n | ($n | tojson | fromjson) == $n",
        "\"123456789012345678901234567890\" | fromjson",
        "\"[3, 1, 4]\" | fromjson | sort",
        "\"2.5e3\" | fromjson",
        ".unicode | tojson | fromjson",
    ] {
        run(&doc, f);
    }

    // ⑩ math / float bit patterns (libm exact bit patterns + nan/inf)
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

    // ⑪ time (fixed epoch, chrono pure UTC date arithmetic)
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

    // ⑫ updates / recursion / generators / try-catch
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

    // ⑬ error paths: parse / compile / runtime
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

    // ⑭ identity roundtrip: Val <-> serde_json::Value equality invariant
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
