#!/usr/bin/env mirvm
---
[dependencies]
# phonenumber pinned to =0.3.10, the latest 0.3.x; its version string is actually
# 0.3.10+9.0.33 and the build metadata is ignored, so "=0.3.10" locks that
# release. 0.3.10 declares no features (features={}), so the default set is the
# minimal and only closure. Pure Rust: the crate itself uses nom 7 at
# runtime (natural/RFC3966 parsers), regex+regex-cache (metadata patterns),
# postcard+serde (build.rs serializes the libphonenumber XML metadata into
# database.bin, read back by include_bytes! into a once_cell Lazy<Database>),
# fnv (a deterministic hasher). Roughly 60 crates, all pure Rust, no C/SIMD/asm.
phonenumber = "=0.3.10"
---
// rust-phonenumber 0.3.10 (libphonenumber port, metadata snapshot 9.0.33)
// differential, compared byte-for-byte with native.
// (1) Six regions (US/GB/DE/FR/CN/JP): each number is parsed, the country is
//     inferred, then is_valid, number_type and the three format modes
//     (E164/International/National) are printed per case. The US/GB/FR inputs
//     are the crate's own test anchors; DE is the official doc example "301/23456"
//     with a region hint, which goes through the nom natural parser's tolerance
//     for separators and slashes; the CN/JP inputs are built from the runtime
//     fixed_line examples ("+{cc} {example}", legal by definition and fixed
//     because 0.3.10 is pinned) and additionally exercise country-code inference
//     without a hint. Values anchored by the crate's tests are asserted with
//     assert_eq! (US (650) 253-0000 in all three modes, the GB/FR anchors and the
//     DE national "030 123456"); the rest are compared with native.
// (2) Error types: three unparsable inputs hit distinct thiserror Debug/Display
//     values -- "" -> NoNumber("not a number"), "+999 12345" ->
//     InvalidCountryCode("invalid country code"), and "+1" plus 18 digits ->
//     TooLong("the number is too long", with MAX_LENGTH_FOR_NSN=17).
// (3) Metadata queries: six country codes (1/44/49/33/86/81) map to a sorted
//     region list, the primary region id and a count of types that have an
//     example number (all 17 Type slots probed individually), plus whole-database
//     aggregates (entry count, regions with a general example, total examples).
// Everything is embedded constants plus the pinned metadata snapshot: no time,
// randomness, environment or addresses are printed, the output is about 25 ASCII
// lines and stderr stays empty. No known frontier issues.
//
// All of the above is what the differential oracle covers; the fixture is fully
// deterministic.
//
use phonenumber::metadata::{Database, DATABASE};
use phonenumber::{country, Mode, Type};

/// The 17 Type slots (the full Descriptors::get branch set, Unknown -> general).
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

/// Number of type slots that have an example number.
fn example_type_count(db: &Database, region_id: &str) -> usize {
    let meta = db.by_id(region_id).unwrap();
    TYPES
        .iter()
        .filter(|t| meta.descriptors().get(**t).and_then(|d| d.example()).is_some())
        .count()
}

fn main() {
    let db: &Database = &DATABASE;

    // ---- (1) parse/validate/format across six regions ----
    // (label, hint, input); the CN/JP inputs are appended from metadata examples.
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

        // Expected values anchored by the crate's tests/doc (version-independent API behavior).
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

    // ---- (2) error types (three distinct variants) ----
    for input in ["", "+999 12345", "+1 650253000012345678"] {
        match phonenumber::parse(None, input) {
            Ok(n) => println!("err-case {input:?}: unexpected ok {n}"),
            Err(e) => println!("err-case {input:?}: {:?} = {:?}", e, e.to_string()),
        }
    }

    // ---- (3) metadata queries: country code -> regions/main region/example counts ----
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

    // Whole-database aggregates (counts only, independent of hash order).
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
