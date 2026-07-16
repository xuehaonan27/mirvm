#!/usr/bin/env mirvm
---
[dependencies]
# 钉 0.14.3（0.14 系最新）：显式 SIMD JSON——stage-1 结构位分类走 AVX2/SSE4.2
# 运行时 CPUID 派发（is_x86_feature_detected!，mirvm asm-stub 真机 cpuid 回真特性：
# guest/host 选同一实现），pshuf.b/pmadd 等 llvm.x86.* 热路径全在已内建族内。
simd-json = "=0.14.3"
---
// simd-json 0.14.3：显式 SIMD intrinsic 面压力+探测。runtime-detection（默认
// feature）下 stage-1 选 AVX2 实现（_mm256_shuffle_epi8=llvm.x86.avx2.pshuf.b、
// maddubs/madd 已内建；movemask/cmpeq 为通用 simd 降级无形体外符号），UTF-8 校验走
// simdutf8 ChunkedUtf8Validator（同套 AVX2 表查算法）。halfbrown 对象表默认
// FxHasher（BuildHasherDefault，无随机种子）→ 对象迭代/stringify 键序为纯计算
// 函数，native/mirvm 可逐字节对拍。
// 覆盖：guest cpuid 特性探测锚定（is_x86_feature_detected! 五特性，native/guest
// 同选 AVX2 即公平对拍）/ 标量根 / 嵌套索引 / as_* 转换与缺失键 / to_owned_value
// vs from_slice::<OwnedValue> 两入口 / to_string+to_vec+to_writer+pretty 四路序列化
// roundtrip / 数字谱系（i64::MIN、u64::MAX+1、-0.0、subnormal、下溢 1e-999、
// 超 f53 精度整数）/ 全 escape+代理对+CJK / 40 键对象（超 halfbrown 32 上限→
// hashbrown probing+FxHasher，原生侧 sse2 Group）与 256 整数大数组（SWAR 数字
// 解析）/ 多个 ≥64B 字符串（stage-1 满块分类+utf8 满块校验）/ borrowed 零拷贝
// 入口 / Buffers 复用 / 十类错误路径（烂语法、未终结字符串、非法 UTF-8、
// Depth 超限、空输入、trail 垃圾、尾逗号）。确定性：定种 xorshift64* 生成大文档；
// f64 全 to_bits 打印；聚合走整数与 fnv；不打印地址/时间/路径。
use simd_json::prelude::*;
use simd_json::{Buffers, OwnedValue, ValueType};

/// 定种 xorshift64*（native/mirvm 同序列）。
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

/// 标量节点锚定打印：类型 + 位级表示（f64 to_bits）。
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

/// 递归聚合：节点计数 / i64+u64 缠绕和 / f64 位异或 / 串字节 fnv——全整数确定。
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
            agg.nodes += 1; // 键也是确定计算产物
            agg.fxor ^= fnv1a(k.as_bytes());
            walk(e, agg);
        }
    }
}

fn main() {
    // ⓪ 派发前提锚定：guest cpuid（asm-stub 真机）应回 host 真特性——native/guest
    // 同选 AVX2 实现才是公平对拍；任一侧回退 portable 即此行 DIFF。
    println!(
        "detect avx2={} sse4.2={} ssse3={} pclmulqdq={} bmi2={}",
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("sse4.2"),
        std::is_x86_feature_detected!("ssse3"),
        std::is_x86_feature_detected!("pclmulqdq"),
        std::is_x86_feature_detected!("bmi2")
    );

    // ① 标量根文档（SIMD 短尾路径 <64B）
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

    // ② 固定嵌套文档：索引链 + as_* + 缺失键 + 克隆内部值
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

    // ③ 四路序列化 + roundtrip（键序 = halfbrown FxHasher 纯计算序，两侧可比）
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
    // serde 反序列化入口（Deserializer→Visitor 路径）
    let mut d2 = doc.to_vec();
    let t: OwnedValue = simd_json::from_slice(&mut d2).unwrap();
    println!("from_slice eq={}", t == v);
    // mutate（as_object_mut insert）→ 再 roundtrip
    let mut m = v.clone();
    m.as_object_mut().unwrap().insert("extra".to_string(), "added".into());
    let ms = simd_json::to_string(&m).unwrap();
    let mut msd = ms.into_bytes();
    let m2: OwnedValue = simd_json::to_owned_value(&mut msd).unwrap();
    println!("mutated keys={} rt-eq={}", m.as_object().unwrap().len(), m == m2);

    // ④ 数字谱系（SWAR/下溢/溢出/精度全边界）
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

    // ⑤ escape 全覆盖 + 代理对 + 多字节（stringparse SIMD 跳引号 + 解转义缓冲）
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

    // ⑥ 生成式大文档：40 键对象（超 halfbrown 32 上限→hashbrown 表）、256 整数
    // 数组（SWAR 数字解析）、≥64B ASCII/CJK 长串（stage-1 满块 + utf8 满块校验）。
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
                // ≥64B 转义串（stringparse 解转义 + stage-1 连续块）
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
    // 键迭代原序（FxHasher+probing 纯计算序——native/mirvm 语义一致才逐字节同）
    let keys: Vec<&str> = bdoc["obj"].as_object().unwrap().keys().map(String::as_str).collect();
    println!("big objkeys {}", keys.join(","));
    let bser = simd_json::to_string(&bdoc).unwrap();
    let mut bsd = bser.clone().into_bytes();
    let bdoc2: OwnedValue = simd_json::to_owned_value(&mut bsd).unwrap();
    println!("big ser len={} fnv={:016x} eq={}", bser.len(), fnv1a(bser.as_bytes()), bdoc == bdoc2);

    // ⑦ borrowed 零拷贝入口（无 escape 串借源切片，含 escape 用解转义缓冲）
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

    // ⑧ Buffers 复用入口：同一 buffer 连续解析三篇
    let mut bufs = Buffers::new(4096);
    for (label, src) in [("b1", "[1,2,3]"), ("b2", "{\"x\":null}"), ("b3", "\"s\"")] {
        let mut d = src.as_bytes().to_vec();
        let v = simd_json::to_owned_value_with_buffers(&mut d, &mut bufs).unwrap();
        scalar_line(label, &v);
    }

    // ⑨ 错误路径十连（文本为库内固定格式：ErrorType 名 + 字符索引）
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
        ("e-iovf", b"18446744073709551616".to_vec()), // u64 溢出：simd-json 报 InvalidNumber
        ("e-lead0", b"01".to_vec()),
    ];
    for (label, mut d) in cases {
        let e = simd_json::to_owned_value(&mut d).unwrap_err();
        println!("{label} {e}");
    }
    // 深层嵌套正路径（tape 栈堆上动态增长，无硬 Depth 限制）
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
