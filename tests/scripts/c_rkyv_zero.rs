#!/usr/bin/env mirvm
---
[dependencies]
rkyv = "0.8"
---
// rkyv 0.8 zero-copy serialization differential (unsafe/unaligned corner cases).
// derive(Archive/Serialize/Deserialize) tree: String (inline/out-of-line repr),
// Vec, HashMap -> ArchivedHashMap (swiss table, FxHasher64), BTreeMap ->
// ArchivedBTreeMap, enum (unit/tuple/struct), Option, Option<Box> recursion,
// plus non-8-multiple alignment members: [bool;3], u8, u16, odd-length Vec<u8>.
// Covers: to_bytes (size + FNV anchor) -> access (bytecheck full validation,
// zero-copy field-by-field read without destructuring, cross-assert vs the
// original) -> deserialize / from_bytes / from_bytes_unchecked(unsafe) three-
// way roundtrip booleans -> access_unchecked(unsafe) bypass read -> three
// validation error paths: truncated / UTF-8 tamper 0xFF / unaligned root.
// Determinism: every source HashMap uses BuildHasherDefault<DefaultHasher>;
// RandomState reseeds per run and its iteration order fixes the archived swiss
// table layout, so the bytes would diverge per run and break the native/mirvm
// differential. DefaultHasher::new() pins the key: same insertion order ->
// same layout. ArchivedHashMap iteration (FxHasher64 slot order) is sorted
// before printing; only values, counts, entries, FNV and booleans appear.
use rkyv::rancor::Error;
use rkyv::util::AlignedVec;
use rkyv::{
    access, access_unchecked, deserialize, from_bytes, from_bytes_unchecked,
    to_bytes, Archive, Deserialize, Serialize,
};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasherDefault;

/// SipHash with a fixed key: same insertion order -> deterministic iteration order.
type DetMap<K, V> = HashMap<K, V, BuildHasherDefault<DefaultHasher>>;

fn det_map<K, V>() -> DetMap<K, V> {
    HashMap::with_hasher(BuildHasherDefault::new())
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Maps validation errors to stable labels: rkyv validator text embeds a raw address.
fn err_kind(e: &Error) -> &'static str {
    let msg = e.to_string();
    if msg.contains("unaligned pointer") {
        "unaligned-pointer"
    } else if msg.contains("overran range") {
        "subtree-overran-range"
    } else if msg.contains("invalid UTF-8") {
        "invalid-utf8"
    } else {
        "other"
    }
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq)]
enum Status {
    Active,
    Suspended(u32),
    Banned { reason: String, until: u64 },
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq)]
struct Profile {
    id: u64,
    nick: String,
    tags: Vec<String>,
    scores: DetMap<String, u32>,
    quotas: BTreeMap<String, u64>,
    status: Status,
    friend: Option<u64>,
    flags: [bool; 3], // member sized not a multiple of 8
    level: u8,        // one-byte member at the alignment boundary
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq)]
struct Team {
    name: String,
    members: Vec<Profile>,
    lead: Option<Box<Profile>>,
    labels: BTreeMap<String, String>,
    index: DetMap<String, u32>,
    blob: Vec<u8>, // odd length 13
    marker: u16,   // two-byte member at the alignment boundary
}

fn build_team() -> Team {
    let mut scores1 = det_map();
    scores1.insert("math".to_string(), 97u32);
    scores1.insert("rust".to_string(), 88);
    scores1.insert("go".to_string(), 75);
    scores1.insert("c".to_string(), 60);
    let mut quotas1 = BTreeMap::new();
    quotas1.insert("cpu".to_string(), 4u64);
    quotas1.insert("mem".to_string(), 1u64 << 20);
    let p1 = Profile {
        id: 42,
        nick: "alice 汉字🎉".to_string(),
        tags: vec!["admin".to_string(), "汉".to_string(), String::new()], // empty-string boundary
        scores: scores1,
        quotas: quotas1,
        status: Status::Active,
        friend: Some(7),
        flags: [true, false, true],
        level: 9,
    };

    let mut scores2 = det_map();
    scores2.insert("math".to_string(), 55);
    let p2 = Profile {
        id: u64::MAX,
        // > INLINE_CAPACITY -> out-of-line repr; unique embedded marker for tamper location
        nick: "bob mrk3.14159-zzz padding-abcdefghijklmnopqrstuvwxyz0123456789"
            .to_string(),
        tags: Vec::new(),             // empty Vec
        scores: scores2,
        quotas: BTreeMap::new(),      // empty BTreeMap
        status: Status::Suspended(30),
        friend: None,
        flags: [false; 3],
        level: 0,
    };

    let mut quotas3 = BTreeMap::new();
    quotas3.insert("disk".to_string(), u64::MAX);
    let p3 = Profile {
        id: 1_000_000_007,
        nick: "carol".to_string(),
        tags: vec!["x".repeat(40)],   // long tag
        scores: det_map(),            // empty HashMap
        quotas: quotas3,
        status: Status::Banned {
            reason: "spam 垃圾".to_string(),
            until: 1_700_000_000,
        },
        friend: Some(u64::MAX),
        flags: [true, true, false],
        level: 255,
    };

    let lead = Profile {
        id: 7,
        nick: "lead 队长".to_string(),
        tags: vec!["oncall".to_string()],
        scores: det_map(),
        quotas: BTreeMap::new(),
        status: Status::Active,
        friend: None,
        flags: [true, false, false],
        level: 10,
    };

    let mut index = det_map();
    for (i, k) in ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta"]
        .iter()
        .enumerate()
    {
        index.insert(k.to_string(), (i as u32 + 1) * 11);
    }
    let mut labels = BTreeMap::new();
    labels.insert("env".to_string(), "prod-生产".to_string());
    labels.insert("region".to_string(), "cn-hangzhou".to_string());
    labels.insert("z-last".to_string(), "Ω".to_string());

    Team {
        name: "mirvm-rkyv 差分队 🚀".to_string(),
        members: vec![p1, p2, p3],
        lead: Some(Box::new(lead)),
        labels,
        index,
        blob: (0u8..13).map(|i| i.wrapping_mul(31).wrapping_add(7)).collect(),
        marker: 0xBEEF,
    }
}

fn main() {
    let team = build_team();

    // ---- ① serialize: size + FNV anchor ----
    let bytes = to_bytes::<Error>(&team).unwrap();
    println!(
        "archive len={} len%8={} fnv={:016x} aligned16={}",
        bytes.len(),
        bytes.len() % 8,
        fnv1a(&bytes),
        bytes.as_ptr().align_offset(16) == 0
    );

    // ---- ② checked access: bytecheck full validation + zero-copy field reads ----
    let archived = access::<ArchivedTeam, Error>(&bytes).unwrap();
    println!(
        "name={} members={} marker={}",
        archived.name.as_str(),
        archived.members.len(),
        u16::from(archived.marker)
    );
    for (i, p) in archived.members.iter().enumerate() {
        let tags: Vec<&str> = p.tags.iter().map(|t| t.as_str()).collect();
        println!(
            "m{i} id={} nick={} level={} flags={:?} tags={:?}",
            u64::from(p.id),
            p.nick.as_str(),
            p.level,
            p.flags,
            tags
        );
        // ArchivedHashMap slot-order iteration -> collect, sort, print (see header)
        let mut sc: Vec<(String, u32)> = p
            .scores
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), u32::from(*v)))
            .collect();
        sc.sort();
        println!("m{i} scores={sc:?}");
        let quotas: Vec<(String, u64)> = p
            .quotas
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), u64::from(*v)))
            .collect();
        println!("m{i} quotas={quotas:?}");
        match &p.status {
            ArchivedStatus::Active => println!("m{i} status=active"),
            ArchivedStatus::Suspended(n) => {
                println!("m{i} status=suspended({})", u32::from(*n))
            }
            ArchivedStatus::Banned { reason, until } => println!(
                "m{i} status=banned({},{})",
                reason.as_str(),
                u64::from(*until)
            ),
        }
        match p.friend.as_ref() {
            Some(f) => println!("m{i} friend={}", u64::from(*f)),
            None => println!("m{i} friend=none"),
        }
    }
    // BTreeMap iterates in order; no re-sort needed
    let labels: Vec<(String, String)> = archived
        .labels
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_str().to_string()))
        .collect();
    println!("labels={labels:?}");
    // ArchivedHashMap::get hit / miss
    for key in ["alpha", "theta", "ghost"] {
        match archived.index.get(key) {
            Some(v) => println!("index {key}={}", u32::from(*v)),
            None => println!("index {key}=none"),
        }
    }
    let mut idx: Vec<(String, u32)> = archived
        .index
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), u32::from(*v)))
        .collect();
    idx.sort();
    println!("index len={} all={idx:?}", archived.index.len());
    match archived.lead.as_ref() {
        Some(b) => println!("lead={} id={}", b.nick.as_str(), u64::from(b.id)),
        None => println!("lead=none"),
    }
    println!(
        "blob len={} fnv={:016x}",
        archived.blob.len(),
        fnv1a(archived.blob.as_slice())
    );

    // Zero-copy reads cross-asserted against the original (without deserialize)
    println!(
        "zc name eq={} members len eq={}",
        archived.name.as_str() == team.name,
        archived.members.len() == team.members.len()
    );
    let zc_scores = archived.members[1].scores.get("math").map(|v| u32::from(*v))
        == team.members[1].scores.get("math").copied();
    println!("zc scores eq={zc_scores}");
    let zc_label = archived.labels.get("env").map(|v| v.as_str())
        == team.labels.get("env").map(|s| s.as_str());
    println!("zc labels eq={zc_label}");
    let zc_status = match (&archived.members[2].status, &team.members[2].status) {
        (ArchivedStatus::Banned { until, .. }, Status::Banned { until: u2, .. }) => {
            u64::from(*until) == *u2
        }
        _ => false,
    };
    println!("zc status eq={zc_status}");

    // ---- ③ three-way roundtrip booleans ----
    let back = deserialize::<Team, Error>(archived).unwrap();
    println!("deserialize roundtrip eq={}", back == team);
    let back2 = from_bytes::<Team, Error>(&bytes).unwrap();
    println!("from_bytes roundtrip eq={}", back2 == team);
    let back3 = unsafe { from_bytes_unchecked::<Team, Error>(&bytes).unwrap() };
    println!("from_bytes_unchecked roundtrip eq={}", back3 == team);

    // ---- ④ access_unchecked (unsafe, no validation, bypass read) ----
    let uarch = unsafe { access_unchecked::<ArchivedTeam>(&bytes) };
    println!(
        "unchecked name eq={} blob eq={}",
        uarch.name.as_str() == archived.name.as_str(),
        fnv1a(uarch.blob.as_slice()) == fnv1a(archived.blob.as_slice())
    );

    // ---- ⑤ bytecheck error paths: truncation / UTF-8 tamper / unaligned root ----
    // rkyv ArchiveValidator text embeds a raw pointer address (randomized by ASLR
    // and always different across native/mirvm) -- truncation and unaligned root
    // print only the label; "invalid UTF-8" carries no address and prints verbatim.
    match access::<ArchivedTeam, Error>(&bytes[..bytes.len() - 8]) {
        Ok(_) => println!("truncated: unexpected ok"),
        Err(e) => println!("truncated err kind={}", err_kind(&e)),
    }
    // Locate the unique marker string and write the always-invalid UTF-8 byte 0xFF
    let mark = b"mrk3.14159-zzz";
    let pos = bytes
        .windows(mark.len())
        .position(|w| w == mark)
        .expect("marker must exist in archive");
    println!("marker pos={pos}");
    let mut bad = AlignedVec::<16>::new();
    bad.extend_from_slice(&bytes);
    bad[pos] = 0xFF;
    match access::<ArchivedTeam, Error>(&bad) {
        Ok(_) => println!("utf8-tamper: unexpected ok"),
        Err(e) => println!("utf8-tamper err: {e}"),
    }
    // One prefix byte unaligns the root pointer (base+1 is odd -> alignment never holds)
    let mut raw = Vec::with_capacity(bytes.len() + 1);
    raw.push(0u8);
    raw.extend_from_slice(&bytes);
    match access::<ArchivedTeam, Error>(&raw[1..]) {
        Ok(_) => println!("unaligned root: unexpected ok"),
        Err(e) => println!("unaligned root err kind={}", err_kind(&e)),
    }
}
