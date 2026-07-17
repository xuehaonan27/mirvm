#!/usr/bin/env mirvm
---
[dependencies]
# phonenumber 钉 =0.3.10（最新 0.3.x，实际版本串 0.3.10+9.0.33，2026-07-02 发布；
# 版本需求里的 build metadata 被忽略，"=0.3.10" 即锁定该版本）。0.3.10 无
# feature 定义（crate 声明 features={}），default 即最小且唯一闭包。纯 Rust
# crate：运行期依赖 nom 7（natural/RFC3966 双语法 parser）、regex+regex-cache
# （元数据模式）、postcard+serde（编译期把 libphonenumber XML 元数据 build.rs
# 序列化成 database.bin，运行期 include_bytes! 后 postcard 反序列化进
# once_cell Lazy<Database>）、fnv（确定性 hasher）。闭包约 60 crate，全纯 Rust，
# 无 C/SIMD/asm 面；regex/nom/serde 族在本 corpus 均已验证。
phonenumber = "=0.3.10"
---
// rust-phonenumber 0.3.10（libphonenumber 移植，元数据快照 9.0.33）三维差分。
// 测试面：
//  ① 跨 6 地区（US/GB/DE/FR/CN/JP）号码 parse → country 推断 → is_valid →
//     number_type → format 三形态（E164/International/National）逐条打印。
//     US/GB/FR 输入取 crate 自带测试锚定串，DE 取官方 doc 例（national 形态
//     "301/23456" 配地区 hint，走 nom natural parser 的装饰符/斜杠容忍路径），
//     CN/JP 输入由运行期元数据 fixed_line 示例构造（"+{cc} {example}"——示例
//     号码按定义合法，且 0.3.10 钉死后内容固定）；两例另走无 hint 的国家码
//     推断路径。crate 测试锚定的输出值用 assert_eq! 硬断言（US (650) 253-0000
//     三形态、GB/FR 锚、DE national "030 123456"），其余靠三维对拍。
//  ② 错误类型打印：3 个不可分析输入分别命中 thiserror Debug/Display——
//     ""→NoNumber("not a number")、"+999 12345"→InvalidCountryCode
//     ("invalid country code")、"+1"+18 位→TooLong("the number is too long"，
//     MAX_LENGTH_FOR_NSN=17)。
//  ③ 元数据查询：6 个国家码（1/44/49/33/86/81）→ region 列表（BTree 序）/
//     主区域 id/有示例号码的类型计数（17 个 Type 槽位逐一查 example()）；
//     附全库聚合：条目数/含通用示例区域数/全库示例总数（纯计数，与 FNV 哈希
//     表迭代序无关）。
// 确定性：全内嵌常量 + 钉死版本的内置元数据快照；无时间/随机/环境/地址打印；
// 输出 ~25 行全 ASCII。stderr 真空。
// FRONTIER 记录：无（预期直通）。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_phonenumber.rs
//   B: sd=$(grep -rl 'name = "c_phonenumber"' ~/.cache/mirvm/scripts/*/Cargo.toml -m1 | xargs dirname)
//      && cd "$sd" && RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//         "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_phonenumber.rs
use phonenumber::metadata::{Database, DATABASE};
use phonenumber::{country, Mode, Type};

/// 17 个 Type 槽位（Descriptors::get 的完整分支集，含 Unknown→general）。
const TYPES: [Type; 17] = [
    Type::FixedLine,
    Type::Mobile,
    Type::FixedLineOrMobile,
    Type::TollFree,
    Type::PremiumRate,
    Type::SharedCost,
    Type::PersonalNumber,
    Type::Voip,
    Type::Pager,
    Type::Uan,
    Type::Emergency,
    Type::Voicemail,
    Type::ShortCode,
    Type::StandardRate,
    Type::Carrier,
    Type::NoInternational,
    Type::Unknown,
];

/// 有示例号码的类型槽位数。
fn example_type_count(db: &Database, region_id: &str) -> usize {
    let meta = db.by_id(region_id).unwrap();
    TYPES
        .iter()
        .filter(|t| meta.descriptors().get(**t).and_then(|d| d.example()).is_some())
        .count()
}

fn main() {
    let db: &Database = &DATABASE;

    // ---- ① 跨 6 地区 parse/validate/format ----
    // (label, hint, input)；CN/JP 输入由元数据示例构造，后补。
    let mut cases: Vec<(&'static str, Option<country::Id>, String)> = vec![
        ("US", Some(country::US), "+1 (650) 253-0000".to_string()),
        ("GB", Some(country::GB), "+44 20 7031 3000".to_string()),
        ("DE", Some(country::DE), "301/23456".to_string()),
        ("FR", Some(country::FR), "+33 6 31 96 65 43".to_string()),
    ];
    for (label, id, cc) in [("CN", country::CN, 86u16), ("JP", country::JP, 81)] {
        let example = db
            .by_id(id.as_ref())
            .unwrap()
            .descriptors()
            .fixed_line()
            .unwrap()
            .example()
            .unwrap();
        cases.push((label, None, format!("+{cc} {example}")));
    }

    for (label, hint, input) in &cases {
        let n = phonenumber::parse(*hint, input).unwrap();
        let region = n
            .country()
            .id()
            .map(|i| i.as_ref().to_owned())
            .unwrap_or_else(|| "-".to_string());
        let ty = n.number_type(db);
        let e164 = n.format().mode(Mode::E164).to_string();
        let intl = n.format().mode(Mode::International).to_string();
        let natl = n.format().mode(Mode::National).to_string();
        println!(
            "case {label} input={input:?} region={region} code={:?} valid={} type={ty:?}",
            n.country().code(),
            n.is_valid()
        );
        println!("  e164={e164} intl={intl:?} natl={natl:?}");

        // crate 自带测试/doc 锚定的期望值（与版本无关的 API 行为）。
        match *label {
            "US" => {
                assert_eq!(e164, "+16502530000");
                assert_eq!(intl, "+1 650-253-0000");
                assert_eq!(natl, "(650) 253-0000");
                assert!(n.is_valid());
            }
            "GB" => {
                assert_eq!(e164, "+442070313000");
                assert_eq!(intl, "+44 20 7031 3000");
                assert_eq!(natl, "020 7031 3000");
            }
            "DE" => {
                assert_eq!(e164, "+4930123456");
                assert_eq!(natl, "030 123456");
            }
            "FR" => {
                assert_eq!(e164, "+33631966543");
                assert_eq!(intl, "+33 6 31 96 65 43");
                assert_eq!(natl, "06 31 96 65 43");
            }
            _ => {}
        }
    }

    // ---- ② 错误类型打印（3 个 distinct 变体）----
    for input in ["", "+999 12345", "+1 650253000012345678"] {
        match phonenumber::parse(None, input) {
            Ok(n) => println!("err-case {input:?}: unexpected ok {n}"),
            Err(e) => println!("err-case {input:?}: {:?} = {:?}", e, e.to_string()),
        }
    }

    // ---- ③ 元数据查询：国家码 → 区域/主区域/示例类型计数 ----
    for cc in [1u16, 44, 49, 33, 86, 81] {
        let mut regions: Vec<&str> = db.region(&cc).unwrap_or_default();
        regions.sort();
        let metas = db.by_code(&cc).unwrap();
        let main = metas[0];
        println!(
            "code={cc} regions={regions:?} main={} example_types={}",
            main.id(),
            example_type_count(db, main.id())
        );
    }

    // 全库聚合（纯计数，与哈希表迭代序无关）。
    let entries = db.iter().count();
    let general_examples = db
        .iter()
        .filter(|m| m.descriptors().general().example().is_some())
        .count();
    let total_examples: usize = db
        .iter()
        .map(|m| example_type_count(db, m.id()))
        .sum();
    println!(
        "db entries={entries} general_examples={general_examples} total_examples={total_examples}"
    );
}
