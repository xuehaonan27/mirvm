#!/usr/bin/env mirvm
---
[dependencies]
rkyv = "0.8"
---
// rkyv 0.8 零拷贝序列化差分（unsafe/unaligned 偏门）。derive(Archive/
// Serialize/Deserialize) 的嵌套结构：String（inline/out-of-line 两种 repr）
// / Vec / HashMap→ArchivedHashMap（swiss table，FxHasher64）/ BTreeMap→
// ArchivedBTreeMap / enum（unit/tuple/struct 三变体）/ Option / Option<Box>
// 递归，外加 [bool;3]、u8、u16、奇长 Vec<u8> 等尺寸非 8 倍数的对齐边界成员。
// 覆盖：to_bytes（尺寸+FNV 锚定）→ access 经 bytecheck 全量校验后零拷贝逐
// 字段读（不解构）并与原值交叉断言 → deserialize / from_bytes /
// from_bytes_unchecked(unsafe) 三路 roundtrip 布尔 → access_unchecked
// (unsafe) 旁路读 → 三条校验错误路径（截断 / UTF-8 篡改 0xFF / 非对齐根
// 指针）。
//
// 确定性：源 HashMap 一律用 BuildHasherDefault<DefaultHasher>——RandomState
// 每跑随机换种，其迭代序决定 archived swiss table 的物理槽位布局，会让
// 序列化字节逐 run 发散（native/mirvm 对拍必炸）；DefaultHasher::new() 固定
// key，同插入序同布局。ArchivedHashMap 迭代（FxHasher64 槽序）收集后排序再
// 打印。只打印值/计数/排序条目/FNV/布尔，不打印地址与原始字节。
use rkyv::rancor::Error;
use rkyv::util::AlignedVec;
use rkyv::{
    access, access_unchecked, deserialize, from_bytes, from_bytes_unchecked,
    to_bytes, Archive, Deserialize, Serialize,
};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::BuildHasherDefault;

/// 固定 key 的 SipHash——同代码同插入序则迭代序确定（见文件头注）。
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

/// 校验错误的确定归类（rkyv validator 文案含裸地址，不能原文打印）。
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
    flags: [bool; 3], // 尺寸非 8 倍数成员
    level: u8,        // 对齐边界：单字节成员
}

#[derive(Archive, Serialize, Deserialize, Debug, PartialEq)]
struct Team {
    name: String,
    members: Vec<Profile>,
    lead: Option<Box<Profile>>,
    labels: BTreeMap<String, String>,
    index: DetMap<String, u32>,
    blob: Vec<u8>, // 奇长度 13
    marker: u16,   // 对齐边界：两字节成员
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
        tags: vec!["admin".to_string(), "汉".to_string(), String::new()], // 空串边界
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
        // > INLINE_CAPACITY → out-of-line repr；内嵌唯一 marker 供篡改定位
        nick: "bob mrk3.14159-zzz padding-abcdefghijklmnopqrstuvwxyz0123456789"
            .to_string(),
        tags: Vec::new(),             // 空 Vec
        scores: scores2,
        quotas: BTreeMap::new(),      // 空 BTreeMap
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
        tags: vec!["x".repeat(40)],   // 长 tag
        scores: det_map(),            // 空 HashMap
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

    // ---- ① 序列化：尺寸 + FNV 锚定 ----
    let bytes = to_bytes::<Error>(&team).unwrap();
    println!(
        "archive len={} len%8={} fnv={:016x} aligned16={}",
        bytes.len(),
        bytes.len() % 8,
        fnv1a(&bytes),
        bytes.as_ptr().align_offset(16) == 0
    );

    // ---- ② checked access：bytecheck 全量校验 + 零拷贝逐字段读 ----
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
        // ArchivedHashMap 槽序迭代 → 收集排序后打印（见文件头注）
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
    // BTreeMap → 有序迭代，无需再排
    let labels: Vec<(String, String)> = archived
        .labels
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.as_str().to_string()))
        .collect();
    println!("labels={labels:?}");
    // ArchivedHashMap::get 命中 / 未命中
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

    // 零拷贝读与原值交叉断言（不经 deserialize）
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

    // ---- ③ 三路 roundtrip 布尔 ----
    let back = deserialize::<Team, Error>(archived).unwrap();
    println!("deserialize roundtrip eq={}", back == team);
    let back2 = from_bytes::<Team, Error>(&bytes).unwrap();
    println!("from_bytes roundtrip eq={}", back2 == team);
    let back3 = unsafe { from_bytes_unchecked::<Team, Error>(&bytes).unwrap() };
    println!("from_bytes_unchecked roundtrip eq={}", back3 == team);

    // ---- ④ access_unchecked（unsafe 零校验旁路读）----
    let uarch = unsafe { access_unchecked::<ArchivedTeam>(&bytes) };
    println!(
        "unchecked name eq={} blob eq={}",
        uarch.name.as_str() == archived.name.as_str(),
        fnv1a(uarch.blob.as_slice()) == fnv1a(archived.blob.as_slice())
    );

    // ---- ⑤ bytecheck 错误路径：截断 / UTF-8 篡改 / 非对齐根 ----
    // rkyv ArchiveValidator 的错误文案内嵌裸指针地址（ASLR 随机，native/mirvm
    // 必不同）——截断与非对齐两条只打印归类标签；"invalid UTF-8" 文案无地址，
    // 原文打印。
    match access::<ArchivedTeam, Error>(&bytes[..bytes.len() - 8]) {
        Ok(_) => println!("truncated: unexpected ok"),
        Err(e) => println!("truncated err kind={}", err_kind(&e)),
    }
    // 定位唯一 marker 串，写入恒非法 UTF-8 字节 0xFF
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
    // 前缀 1 字节使根指针非对齐（基址+1 必为奇地址 → 必不满足对齐要求）
    let mut raw = Vec::with_capacity(bytes.len() + 1);
    raw.push(0u8);
    raw.extend_from_slice(&bytes);
    match access::<ArchivedTeam, Error>(&raw[1..]) {
        Ok(_) => println!("unaligned root: unexpected ok"),
        Err(e) => println!("unaligned root err kind={}", err_kind(&e)),
    }
}
