#!/usr/bin/env mirvm
---
[dependencies]
# jsonschema pinned to the exact patch, "locked" meaning =0.48.0.
# default-features = false: 0.48's defaults are resolve-http + resolve-file +
# tls-aws-lc-rs, which pull reqwest/hyper/rustls and the cmake-built C library
# aws-lc-sys (198 crates in the graph). That breaks the pure-Rust target and no
# case here fetches a remote $ref anyway, so trimming leaves a 100-crate closure.
# serde_json is a direct dependency (json! builds the cases) and is pinned too.
jsonschema = { version = "=0.48.0", default-features = false }
serde_json = "=1.0.150"
---
// jsonschema JSON Schema validation differential, compared byte-for-byte with
// native. Twelve cases per draft, half valid and half invalid:
// - draft7: object properties with type/minimum/required, array items, a JSON
//   pointer segment holding a unicode property name, pattern (the ECMA-262 to
//   Rust regex translation path) and the anyOf first-error text.
// - draft 2019-09: dependentRequired, contains+minContains, recursive $defs+$ref,
//   unevaluatedProperties, if/then/else, and multipleOf 0.1 through the exact
//   fraction path rather than f64 approximation.
// - draft 2020-12: prefixItems+items:false, sibling keywords next to $ref,
//   $dynamicRef/$dynamicAnchor, a \p{Lu} unicode property regex, and a 40-level
//   child chain (valid plus a deep failure with a long pointer).
// - format assertions (should_validate_formats, 6 cases): hostname (idna plus the
//   unicode-general-category table), uuid (uuid-simd runtime SIMD detection) and
//   email.
// - meta-schema self-check: all 16 driver schemas through meta::is_valid (the
//   crate's embedded LazyLock<Validator>); explicit $schema in the draft7 /
//   2019-09 / 2020-12 cases triggers the matching meta validator, and two bad
//   schemas have a fixed first-error pointer.
// - stable compile diagnostics: an unknown type name ("flump") and a missing
//   $ref pointer, each printing instance_path plus the full Display.
// Each case prints valid plus, on failure, the first error's instance_path (JSON
// pointer, empty at the root), schema_path and Display. Inputs are fixed, so the
// text is fixed; properties iterate through serde_json::Map (BTreeMap order); no
// time, RNG or environment enters. Valid counts are pinned with assert_eq!.
//
//
// Three-way rerun:
//   A: target/release/mirvm run tests/data/programs/c_jsonschema.rs
//   B: cd $(grep -l 'name = "c_jsonschema"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) \
//        && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//           "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_jsonschema.rs
use jsonschema::Validator;
use serde_json::{json, Value};

/// One case: is_valid plus the first error's instance_path/schema_path/Display.
fn probe(v: &Validator, inst: &Value) -> (bool, String, String, String) {
    if v.is_valid(inst) {
        (true, String::new(), String::new(), String::new())
    } else {
        let e = v.validate(inst).unwrap_err();
        let ptr = e.instance_path().to_string();
        let sptr = e.schema_path().to_string();
        let msg = e.to_string();
        (false, ptr, sptr, msg)
    }
}

fn line(tag: &str, r: (bool, String, String, String)) -> bool {
    let (ok, ptr, sptr, msg) = r;
    if ok {
        println!("{tag} valid=true");
    } else {
        println!("{tag} valid=false ptr=[{ptr}] sptr=[{sptr}] err={msg}");
    }
    ok
}

fn main() {
    let mut meta_ok = 0usize;
    let mut meta_n = 0usize;
    let mut check_meta = |s: &Value| {
        meta_n += 1;
        if jsonschema::meta::is_valid(s) {
            meta_ok += 1;
        }
    };

    // ============ draft7 ============
    let s1 = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "age": {"type": "integer", "minimum": 0}
        },
        "required": ["name"]
    });
    check_meta(&s1);
    let v1 = jsonschema::draft7::new(&s1).unwrap();
    let mut d7 = 0usize;
    d7 += line("d7.obj.ok", probe(&v1, &json!({"name": "mirvm", "age": 3}))) as usize;
    d7 += line("d7.obj.type", probe(&v1, &json!({"name": 1}))) as usize;
    d7 += line("d7.obj.min", probe(&v1, &json!({"name": "m", "age": -2}))) as usize;
    d7 += line("d7.obj.req", probe(&v1, &json!({"age": 5}))) as usize;

    let s2 = json!({"type": "array", "items": {"type": "integer"}, "minItems": 2});
    check_meta(&s2);
    let v2 = jsonschema::draft7::new(&s2).unwrap();
    d7 += line("d7.arr.ok", probe(&v2, &json!([1, 2, 3]))) as usize;
    d7 += line("d7.arr.item", probe(&v2, &json!([1, "x"]))) as usize;

    // JSON pointer segment for a unicode property name
    let s3 = json!({"type": "object", "properties": {"汉字": {"type": "string"}}, "required": ["汉字"]});
    check_meta(&s3);
    let v3 = jsonschema::draft7::new(&s3).unwrap();
    d7 += line("d7.uni.ok", probe(&v3, &json!({"汉字": "值"}))) as usize;
    d7 += line("d7.uni.ptr", probe(&v3, &json!({"汉字": 7}))) as usize;

    // pattern: the ECMA-262 -> Rust regex translation path
    let s4 = json!({"pattern": "^[a-z]+[0-9]$"});
    check_meta(&s4);
    let v4 = jsonschema::draft7::new(&s4).unwrap();
    d7 += line("d7.pat.ok", probe(&v4, &json!("abc9"))) as usize;
    d7 += line("d7.pat.no", probe(&v4, &json!("ABC"))) as usize;

    let s5 = json!({"anyOf": [{"type": "string"}, {"type": "integer", "minimum": 3}]});
    check_meta(&s5);
    let v5 = jsonschema::draft7::new(&s5).unwrap();
    d7 += line("d7.any.ok", probe(&v5, &json!(5))) as usize;
    d7 += line("d7.any.no", probe(&v5, &json!(2))) as usize;
    println!("d7 valid_count={d7}/12");
    assert_eq!(d7, 5);

    // ============ draft 2019-09 ============
    let s6 = json!({"type": "object", "dependentRequired": {"credit_card": ["billing_address"]}});
    check_meta(&s6);
    let v6 = jsonschema::draft201909::new(&s6).unwrap();
    let mut d19 = 0usize;
    d19 += line("d19.dep.ok", probe(&v6, &json!({"credit_card": "4111", "billing_address": "x"}))) as usize;
    d19 += line("d19.dep.no", probe(&v6, &json!({"credit_card": "4111"}))) as usize;

    let s7 = json!({"contains": {"type": "integer", "minimum": 2}, "minContains": 2});
    check_meta(&s7);
    let v7 = jsonschema::draft201909::new(&s7).unwrap();
    d19 += line("d19.cont.ok", probe(&v7, &json!([1, 2, 3]))) as usize;
    d19 += line("d19.cont.no", probe(&v7, &json!([1]))) as usize;

    // recursive $defs + $ref
    let s8 = json!({
        "type": "object",
        "properties": {"child": {"$ref": "#/$defs/node"}},
        "$defs": {"node": {"type": "object", "properties": {"v": {"type": "integer"}}, "required": ["v"]}}
    });
    check_meta(&s8);
    let v8 = jsonschema::draft201909::new(&s8).unwrap();
    d19 += line("d19.ref.ok", probe(&v8, &json!({"child": {"v": 1}}))) as usize;
    d19 += line("d19.ref.no", probe(&v8, &json!({"child": {"v": "x"}}))) as usize;

    let s9 = json!({
        "type": "object",
        "properties": {"a": {"type": "integer"}},
        "unevaluatedProperties": false
    });
    check_meta(&s9);
    let v9 = jsonschema::draft201909::new(&s9).unwrap();
    d19 += line("d19.unev.ok", probe(&v9, &json!({"a": 1}))) as usize;
    d19 += line("d19.unev.no", probe(&v9, &json!({"a": 1, "b": 2}))) as usize;

    let s10 = json!({
        "if": {"properties": {"t": {"const": "a"}}},
        "then": {"required": ["x"]},
        "else": {"required": ["y"]}
    });
    check_meta(&s10);
    let v10 = jsonschema::draft201909::new(&s10).unwrap();
    d19 += line("d19.ifthen.ok", probe(&v10, &json!({"t": "a", "x": 1}))) as usize;
    d19 += line("d19.ifthen.no", probe(&v10, &json!({"t": "b"}))) as usize;

    // multipleOf through exact fractions (0.1's binary approximation is unused)
    let s11 = json!({"type": "number", "multipleOf": 0.1});
    check_meta(&s11);
    let v11 = jsonschema::draft201909::new(&s11).unwrap();
    d19 += line("d19.mult.ok", probe(&v11, &json!(0.3))) as usize;
    d19 += line("d19.mult.no", probe(&v11, &json!(0.35))) as usize;
    println!("d19 valid_count={d19}/12");
    assert_eq!(d19, 6);

    // ============ draft 2020-12 ============
    let s12 = json!({"prefixItems": [{"type": "boolean"}, {"type": "string"}], "items": false});
    check_meta(&s12);
    let v12 = jsonschema::draft202012::new(&s12).unwrap();
    let mut d20 = 0usize;
    d20 += line("d20.prefix.ok", probe(&v12, &json!([true, "x"]))) as usize;
    d20 += line("d20.prefix.extra", probe(&v12, &json!([true, "x", 9]))) as usize;
    d20 += line("d20.prefix.type", probe(&v12, &json!([true, 7]))) as usize;

    // 2020-12: sibling keywords next to $ref take effect
    let s13 = json!({
        "$defs": {"pos": {"type": "integer", "exclusiveMinimum": 0}},
        "$ref": "#/$defs/pos",
        "maximum": 10
    });
    check_meta(&s13);
    let v13 = jsonschema::draft202012::new(&s13).unwrap();
    d20 += line("d20.refsib.ok", probe(&v13, &json!(5))) as usize;
    d20 += line("d20.refsib.low", probe(&v13, &json!(-1))) as usize;
    d20 += line("d20.refsib.high", probe(&v13, &json!(20))) as usize;

    // $dynamicRef / $dynamicAnchor
    let s14 = json!({
        "$defs": {
            "itemType": {"$dynamicAnchor": "itemType", "type": "string"},
            "list": {"type": "object", "properties": {"items": {"type": "array", "items": {"$dynamicRef": "#itemType"}}}}
        },
        "$ref": "#/$defs/list"
    });
    check_meta(&s14);
    let v14 = jsonschema::draft202012::new(&s14).unwrap();
    d20 += line("d20.dyn.ok", probe(&v14, &json!({"items": ["a", "b"]}))) as usize;
    d20 += line("d20.dyn.no", probe(&v14, &json!({"items": ["a", 2]}))) as usize;

    // unicode property regex class \p{Lu}
    let s15 = json!({"pattern": "\\p{Lu}"});
    check_meta(&s15);
    let v15 = jsonschema::draft202012::new(&s15).unwrap();
    d20 += line("d20.unip.ok", probe(&v15, &json!("abcDEF"))) as usize;
    d20 += line("d20.unip.no", probe(&v15, &json!("abc"))) as usize;

    // deep recursion: a 40-level child chain
    let s16 = json!({
        "type": "object",
        "required": ["v"],
        "properties": {"v": {"type": "integer"}, "child": {"$ref": "#"}}
    });
    check_meta(&s16);
    let v16 = jsonschema::draft202012::new(&s16).unwrap();
    let build_deep = |bad: bool| -> Value {
        let mut inner = json!({"v": if bad { Value::from("x") } else { Value::from(1) }});
        for _ in 0..40 {
            inner = json!({"v": 1, "child": inner});
        }
        inner
    };
    d20 += line("d20.deep.ok", probe(&v16, &build_deep(false))) as usize;
    d20 += line("d20.deep.no", probe(&v16, &build_deep(true))) as usize;
    println!("d20 valid_count={d20}/12");
    assert_eq!(d20, 5);

    // ============ format assertions (should_validate_formats) ============
    let s17 = json!({"format": "hostname"});
    let f1 = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&s17)
        .unwrap();
    let mut fmt = 0usize;
    fmt += line("fmt.host.ok", probe(&f1, &json!("mirvm-host.example.com"))) as usize;
    fmt += line("fmt.host.no", probe(&f1, &json!("-bad-.x"))) as usize;

    let s18 = json!({"format": "uuid"});
    let f2 = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&s18)
        .unwrap();
    fmt += line("fmt.uuid.ok", probe(&f2, &json!("550e8400-e29b-41d4-a716-446655440000"))) as usize;
    fmt += line("fmt.uuid.no", probe(&f2, &json!("not-a-uuid"))) as usize;

    let s19 = json!({"format": "email"});
    let f3 = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&s19)
        .unwrap();
    fmt += line("fmt.mail.ok", probe(&f3, &json!("a@b.co"))) as usize;
    fmt += line("fmt.mail.no", probe(&f3, &json!("@no-local"))) as usize;
    println!("fmt valid_count={fmt}/6");
    assert_eq!(fmt, 3);

    // ============ meta-schema self-check ============
    println!("meta core_valid={meta_ok}/{meta_n}");
    assert_eq!(meta_ok, meta_n);
    let m7 = json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "string"});
    let m19 = json!({"$schema": "https://json-schema.org/draft/2019-09/schema", "type": "integer"});
    let m20 = json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "boolean"});
    println!(
        "meta drafts d7={} d19={} d20={}",
        jsonschema::meta::is_valid(&m7),
        jsonschema::meta::is_valid(&m19),
        jsonschema::meta::is_valid(&m20)
    );
    assert!(jsonschema::meta::is_valid(&m7));
    assert!(jsonschema::meta::is_valid(&m19));
    assert!(jsonschema::meta::is_valid(&m20));

    let bad_meta = json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": 42});
    print!("meta bad type42");
    match jsonschema::meta::validate(&bad_meta) {
        Ok(()) => println!(" valid-unexpected"),
        Err(e) => println!(" ptr=[{}] err={e}", e.instance_path()),
    }
    let bad_meta2 = json!({"properties": {"a": {"type": "nope"}}});
    print!("meta bad typename");
    match jsonschema::meta::validate(&bad_meta2) {
        Ok(()) => println!(" valid-unexpected"),
        Err(e) => println!(" ptr=[{}] err={e}", e.instance_path()),
    }

    // ============ stable compile diagnostics ============
    match jsonschema::draft202012::new(&json!({"type": "flump"})) {
        Ok(_) => println!("compile flump unexpected-ok"),
        Err(e) => println!("compile flump ptr=[{}] err={e}", e.instance_path()),
    }
    match jsonschema::draft202012::new(&json!({"$ref": "#/definitions/nope"})) {
        Ok(_) => println!("compile ref unexpected-ok"),
        Err(e) => println!("compile ref ptr=[{}] err={e}", e.instance_path()),
    }
    println!("done");
}
