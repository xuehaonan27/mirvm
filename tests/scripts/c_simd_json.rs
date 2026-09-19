#!/usr/bin/env mirvm
---
[dependencies]
# Pinned 0.14.3 (latest 0.14.x): explicit SIMD JSON. stage-1 structural-bit
# classification dispatches AVX2/SSE4.2 on runtime CPUID (is_x86_feature_detected!;
# mirvm's asm-stub cpuid returns real host features, so guest/host agree).
simd-json = "=0.14.3"
---
// simd-json 0.14.3: explicit SIMD intrinsic surface stress and probe. Under the
// default runtime-detection feature stage-1 selects AVX2 (_mm256_shuffle_epi8 =
// llvm.x86.avx2.pshuf.b; maddubs/madd built in; movemask/cmpeq generic SIMD, no
// vector symbol). UTF-8 validation uses simdutf8's ChunkedUtf8Validator.
// halfbrown tables default to FxHasher (BuildHasherDefault, no seed), so object
// iteration/stringify key order is pure computation: byte-for-byte native/mirvm.
// Covers: guest cpuid feature-probe anchor (both sides must pick AVX2), scalar
// root, nested indexing, as_* conversions and missing keys, to_owned_value vs
// from_slice, four serialization paths, the number spectrum (i64::MIN,
// u64::MAX+1, -0.0, subnormal, 1e-999, past-f53 integers), escapes + surrogate
// pairs + CJK, a 40-key object (past halfbrown's 32 limit -> hashbrown probing +
// FxHasher, sse2 Group natively) and a 256-int array, strings >= 64B (stage-1
// full-block classify + utf8 full-block check), borrowed zero-copy, Buffers
// reuse, and ten error paths (bad syntax, unterminated string, invalid UTF-8,
// depth, empty input, trailing garbage, trailing comma). Determinism: seeded
// xorshift64*, f64 via to_bits, integer/fnv aggregation, no addresses/time/paths.
use simd_json::prelude::*;
use simd_json::{Buffers, OwnedValue, ValueType};

/// Seeded xorshift64* (same sequence for native and mirvm).
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

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Anchor-print a scalar node: type plus bit-level representation (f64 via to_bits).
fn scalar_line(label: &str, v: &OwnedValue) {
    let ty = match v.value_type() {
        ValueType::Null => "null",
        ValueType::Bool => "bool",
        ValueType::I64 => "i64",
        ValueType::U64 => "u64",
        ValueType::F64 => "f64",
        ValueType::String => "string",
        ValueType::Array => "array",
        ValueType::Object => "object",
        _ => "other",
    };
    let repr = if let Some(b) = v.as_bool() {
        format!("{b}")
    } else if let Some(i) = v.as_i64() {
        format!("{i}")
    } else if let Some(u) = v.as_u64() {
        format!("{u}")
    } else if let Some(f) = v.as_f64() {
        format!("{:016x}", f.to_bits())
    } else if let Some(s) = v.as_str() {
        format!("len={} fnv={:016x}", s.len(), fnv1a(s.as_bytes()))
    } else {
        "-".to_string()
    };
    println!("{label} {ty} {repr}");
}

/// Recursive aggregate: node count, wrapping i64+u64 sum, f64 bit xor, string fnv.
struct Agg {
    nodes: u64,
    isum: i64,
    usum: u64,
    fxor: u64,
    strfnv: u64,
}

fn walk(v: &OwnedValue, agg: &mut Agg) {
    agg.nodes += 1;
    if let Some(i) = v.as_i64() {
        agg.isum = agg.isum.wrapping_add(i);
    } else if let Some(u) = v.as_u64() {
        agg.usum = agg.usum.wrapping_add(u);
    } else if let Some(f) = v.as_f64() {
        agg.fxor ^= f.to_bits();
    } else if let Some(s) = v.as_str() {
        agg.strfnv = fnv1a(s.as_bytes());
    } else if let Some(a) = v.as_array() {
        for e in a {
            walk(e, agg);
        }
    } else if let Some(o) = v.as_object() {
        for (k, e) in o.iter() {
            agg.nodes += 1; // keys are deterministic computation products too
            agg.fxor ^= fnv1a(k.as_bytes());
            walk(e, agg);
        }
    }
}

fn main() {
    // ⓪ Dispatch precondition anchor: guest cpuid (asm-stub, real machine) must
    // report the host features, so both sides pick AVX2; a portable fallback DIFFs.
    println!(
        "detect avx2={} sse4.2={} ssse3={} pclmulqdq={} bmi2={}",
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("sse4.2"),
        std::is_x86_feature_detected!("ssse3"),
        std::is_x86_feature_detected!("pclmulqdq"),
        std::is_x86_feature_detected!("bmi2")
    );

    // ① Scalar root documents (SIMD short-tail path, < 64B)
    for (label, src) in [
        ("r-int", "42"),
        ("r-neg", "-17"),
        ("r-f64", "3.5"),
        ("r-true", "true"),
        ("r-false", "false"),
        ("r-null", "null"),
        ("r-str", "\"hello world\""),
    ] {
        let mut d = src.as_bytes().to_vec();
        let v: OwnedValue = simd_json::to_owned_value(&mut d).unwrap();
        scalar_line(label, &v);
    }

    // ② Fixed nested document: index chains + as_* + missing keys + cloning an inner value
    let doc = br#"{"a":{"b":[1,2,{"c":"hi","d":[1,-2]}]},"n":-3,"f":1.25,"ok":true,"z":null}"#;
    let mut d = doc.to_vec();
    let v: OwnedValue = simd_json::to_owned_value(&mut d).unwrap();
    scalar_line("nest c", &v["a"]["b"][2]["c"]);
    scalar_line("nest n", &v["n"]);
    scalar_line("nest f", &v["f"]);
    scalar_line("nest ok", &v["ok"]);
    scalar_line("nest z", &v["z"]);
    println!("nest blen {} doff {}", v["a"]["b"].as_array().unwrap().len(), v["a"]["b"][2]["d"][1].as_i64().unwrap());
    println!("nest keys {} absent {}", v.as_object().unwrap().keys().count(), v["a"].as_object().unwrap().get("absent").is_none());
    let fixed: OwnedValue = v["a"]["b"][2]["d"].clone();
    println!("nest d-clone {}", simd_json::to_string(&fixed).unwrap());

    // ③ Four serialization paths + roundtrip (key order = halfbrown FxHasher computation order)
    let s = simd_json::to_string(&v).unwrap();
    let mut sd = s.clone().into_bytes();
    let v2: OwnedValue = simd_json::to_owned_value(&mut sd).unwrap();
    println!("to_string len={} fnv={:016x} eq={}", s.len(), fnv1a(s.as_bytes()), v == v2);
    let bv = simd_json::to_vec(&v).unwrap();
    let mut bvd = bv.clone();
    let v3: OwnedValue = simd_json::to_owned_value(&mut bvd).unwrap();
    println!("to_vec len={} fnv={:016x} eq={}", bv.len(), fnv1a(&bv), v == v3);
    let pretty = simd_json::to_string_pretty(&v).unwrap();
    println!("pretty len={} fnv={:016x}", pretty.len(), fnv1a(pretty.as_bytes()));
    let mut wbuf: Vec<u8> = Vec::new();
    simd_json::to_writer(&mut wbuf, &v).unwrap();
    println!("to_writer len={} fnv={:016x}", wbuf.len(), fnv1a(&wbuf));
    // serde deserialization entry (Deserializer -> Visitor path)
    let mut d2 = doc.to_vec();
    let t: OwnedValue = simd_json::from_slice(&mut d2).unwrap();
    println!("from_slice eq={}", t == v);
    // mutate (as_object_mut insert) -> roundtrip again
    let mut m = v.clone();
    m.as_object_mut().unwrap().insert("extra".to_string(), "added".into());
    let ms = simd_json::to_string(&m).unwrap();
    let mut msd = ms.into_bytes();
    let m2: OwnedValue = simd_json::to_owned_value(&mut msd).unwrap();
    println!("mutated keys={} rt-eq={}", m.as_object().unwrap().len(), m == m2);

    // ④ Number spectrum (SWAR / underflow / overflow / precision boundaries)
    for (label, src) in [
        ("n-zero", "0"),
        ("n-negzero", "-0.0"),
        ("n-1e2", "1e2"),
        ("n-half", "2.5"),
        ("n-1em7", "1e-7"),
        ("n-f53p1", "9007199254740993"),
        ("n-u64max", "18446744073709551615"),
        ("n-i64min", "-9223372036854775808"),
        ("n-1e308", "1e308"),
        ("n-subnorm", "5e-324"),
        ("n-under", "1e-999"),
        ("n-max", "1.7976931348623157e308"),
    ] {
        let mut d = src.as_bytes().to_vec();
        let v: OwnedValue = simd_json::to_owned_value(&mut d).unwrap();
        scalar_line(label, &v);
    }

    // ⑤ Full escapes + surrogate pairs + multibyte (stringparse SIMD quote skip + unescape buffer)
    let esc = r#"{"esc":"q\" bs\\ sl\/ b\b f\f n\n r\r t\t upA lowé hi\u20ac pair𝄞","han":"汉字界叟","emo":"🦀🎉"}"#;
    let mut ed = esc.as_bytes().to_vec();
    let ev: OwnedValue = simd_json::to_owned_value(&mut ed).unwrap();
    let es = ev["esc"].as_str().unwrap();
    println!("esc len={} fnv={:016x}", es.len(), fnv1a(es.as_bytes()));
    let eser = simd_json::to_string(&ev).unwrap();
    let mut eserd = eser.clone().into_bytes();
    let ev2: OwnedValue = simd_json::to_owned_value(&mut eserd).unwrap();
    println!("esc rt-len={} rt-fnv={:016x} eq={}", eser.len(), fnv1a(eser.as_bytes()), ev == ev2);
    scalar_line("han", &ev["han"]);
    scalar_line("emo", &ev["emo"]);

    // ⑥ Generated large document: 40-key object (past halfbrown's 32 limit ->
    // hashbrown table), 256-int array, >= 64B ASCII/CJK strings (full-block stages).
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut big = String::from("{\"ints\":[");
    for i in 0..256u64 {
        if i > 0 {
            big.push(',');
        }
        let n = rng.next();
        big.push_str(&match i % 5 {
            0 => (n % 10).to_string(),
            1 => ((n % 1_000_000) as i64).to_string(),
            2 => (-((n % 1_000_000_000) as i64)).to_string(),
            3 => (n % 1_000_000_000_000).to_string(),
            _ => (n as i64).to_string(),
        });
    }
    big.push_str("],\"obj\":{");
    for i in 0..40u64 {
        if i > 0 {
            big.push(',');
        }
        big.push_str(&format!("\"k{i:02}\":"));
        match i % 4 {
            0 => big.push_str(&format!("{}", rng.next() % 1000)),
            1 => {
                // >= 64B escaped string (stringparse unescape + stage-1 contiguous block)
                big.push('"');
                for _ in 0..9 {
                    big.push_str(r"tab\ttrail ");
                }
                big.push('"');
            }
            2 => big.push_str(if rng.below(2) == 0 { "true" } else { "false" }),
            _ => {
                big.push('"');
                for _ in 0..8 {
                    big.push_str("汉字飘过边界");
                }
                big.push('"');
            }
        }
    }
    big.push_str("},\"tail\":\"");
    for _ in 0..11 {
        big.push_str("pad-tail-0123456789");
    }
    big.push_str("\"}");
    println!("big bytes {}", big.len());

    let mut bd = big.clone().into_bytes();
    let bdoc: OwnedValue = simd_json::to_owned_value(&mut bd).unwrap();
    let mut agg = Agg { nodes: 0, isum: 0, usum: 0, fxor: 0, strfnv: 0xcbf29ce484222325 };
    walk(&bdoc, &mut agg);
    println!(
        "big agg nodes={} isum={} usum={} fxor={:016x} strfnv={:016x}",
        agg.nodes, agg.isum, agg.usum, agg.fxor, agg.strfnv
    );
    // Key iteration order (FxHasher + probing is pure computation, so native/mirvm agree byte-for-byte)
    let keys: Vec<&str> = bdoc["obj"].as_object().unwrap().keys().map(String::as_str).collect();
    println!("big objkeys {}", keys.join(","));
    let bser = simd_json::to_string(&bdoc).unwrap();
    let mut bsd = bser.clone().into_bytes();
    let bdoc2: OwnedValue = simd_json::to_owned_value(&mut bsd).unwrap();
    println!("big ser len={} fnv={:016x} eq={}", bser.len(), fnv1a(bser.as_bytes()), bdoc == bdoc2);

    // ⑦ Borrowed zero-copy entry (escape-free strings borrow the source slice; escaped ones use the buffer)
    let bsrc = String::from(r#"{"plain":"borrowed slice","uni":"雪花slice","arr":[true,null,7]}"#);
    let mut bytes = bsrc.into_bytes();
    let bv: simd_json::BorrowedValue = simd_json::to_borrowed_value(&mut bytes).unwrap();
    println!(
        "borrowed plain={} uni-fnv={:016x} arr0={} arrlen={}",
        bv["plain"].as_str().unwrap(),
        fnv1a(bv["uni"].as_str().unwrap().as_bytes()),
        bv["arr"][0].as_bool().unwrap(),
        bv["arr"].as_array().unwrap().len()
    );

    // ⑧ Buffers reuse: parse three documents in a row with one buffer
    let mut bufs = Buffers::new(4096);
    for (label, src) in [("b1", "[1,2,3]"), ("b2", "{\"x\":null}"), ("b3", "\"s\"")] {
        let mut d = src.as_bytes().to_vec();
        let v = simd_json::to_owned_value_with_buffers(&mut d, &mut bufs).unwrap();
        scalar_line(label, &v);
    }

    // ⑨ Ten error paths (library-fixed text: ErrorType name + character index)
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("e-tape", br#"{"a": [1, 2, }"#.to_vec()),
        ("e-open", b"[1, 2".to_vec()),
        ("e-unterm", b"\"unterminated".to_vec()),
        ("e-bare", b"not json".to_vec()),
        ("e-nul", b"nul".to_vec()),
        ("e-utf8", vec![b'"', 0xFF, b'"']),
        ("e-empty", Vec::new()),
        ("e-trail", b"1 2".to_vec()),
        ("e-comma", b"[1,]".to_vec()),
        ("e-iovf", b"18446744073709551616".to_vec()), // u64 overflow: simd-json reports InvalidNumber
        ("e-lead0", b"01".to_vec()),
    ];
    for (label, mut d) in cases {
        let e = simd_json::to_owned_value(&mut d).unwrap_err();
        println!("{label} {e}");
    }
    // Deep nesting positive path (the tape stack grows on the heap; no hard depth limit)
    let deep = format!("{}0{}", "[".repeat(1100), "]".repeat(1100));
    let mut dd = deep.into_bytes();
    let deepv: OwnedValue = simd_json::to_owned_value(&mut dd).unwrap();
    let mut levels = 0u32;
    let mut cur = &deepv;
    while let Some(a) = cur.as_array() {
        levels += 1;
        cur = &a[0];
    }
    println!("deep ok levels={} leaf={}", levels, match cur.as_u64() {
        Some(u) => format!("{u}"),
        None => "none".to_string(),
    });
}
