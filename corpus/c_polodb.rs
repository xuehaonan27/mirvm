#!/usr/bin/env mirvm
---
[dependencies]
# 分工条目标名「polodb-lite」：crates.io 实勘该名从未发布。PoloDB 家族的嵌入式
# 库 = polodb_core（polodb 是 MongoDB 兼容 server 壳）。按条目「最新 3.x、纯 Rust、
# temp file + WAL」三约束对齐：polodb_core 钉 =3.5.2（3.x 最新）。其它大代排除理由：
#   5.x：依赖 polodb-librocksdb-sys（C++ rocksdb 捆绑构建，需 clang/cmake——clang
#        本机缺席且并非「纯 Rust」）；
#   4.x：纯 Rust 但超出「3.x」钉选口径。
# 3.5.2 Linux 生效闭包全纯 Rust：bson 2/byteorder/crc64fast/getrandom 0.2/hashbrown/
# lru/num_enum/serde/uuid 1（web-sys/winapi/js-sys 为 cfg 平台门控，Linux 不编译）。
# WAL = 内建 journal：db 路径旁落 <name>.db.journal，默认满 1000B 与主库 merge；
# unix 文件锁走 libc flock(LOCK_EX|LOCK_NB)，重开走 journal recovery。
polodb_core = "=3.5.2"
# 上游语义破洞实锤（2026-07-27）：polodb_core 3.5.2 请求 uuid 的
# "getrandom" feature——该 feature 在 uuid 1.14+ 被移除（1.13 起改名
# rng-getrandom），max 解到 1.24.0 即"feature 不存在"（cargo 自家 fresh
# 解析同撞，非 mirvm 分叉）。钉 uuid = 1.6.1（driver 验收时代的历史 lock
# 同版，getrandom feature 在场）。
uuid = "=1.6.1"
---
// polodb_core 3.5.2 嵌入式文档库差分（批8 波2：VM/语言机——polodb 自带查询
// 字节码 VM，btree 页存 + journal WAL + 乐观会话事务，VM-in-VM 压力面）。
//
// 测试面清单（条目共六项全覆盖；其中「索引字段」按实勘降级，见下注）：
//   ① temp 目录建库（.db + .db.journal 起点清空，版本行锚定 3.5.2）；
//   ② 事务段批量插入：40 user docs，BSON 型别覆盖 Int32/Int64/Double(from_bits
//      定值表)/Boolean/String/Binary(变长定种)/嵌套 Document/Array/Null/固定
//      DateTime/固定字节 ObjectId；blobs 集合 2 docs（16KB 大二值——跨 4 页
//      large-ticket 溢出链；2KB 字符串）；
//   ③ 主键 B-tree 即唯一索引——二级 create_index 不可用的既定降级：
//      3.5.2 里 Collection::create_index 为私有、Database::create_index 为
//      pub(super)，落到 internal_create_index = unimplemented!()（源码实勘），
//      任何公开路径都到不了索引创建。故「索引字段」以 `_id` 主键 B-tree 面
//      覆盖：点查（pkey 快路）/非键字段等值全扫/[$gte,$lt) 范围/$or/$in，
//      全量回读即主键序；
//   ④ 更新+删除事务段（ClientSession 乐观事务：写入攒 page_map、commit 时
//      才对全局 journal 加锁落盘——3.5.2 会话模型实勘）：tx2 内 $set/$inc/
//      $mul/$max/$unset/delete_many+commit；tx3 插入 5 行事务内计数 41 后
//      abort 回滚不见；
//   ⑤ 错误路径：$set _id 非法 / 重复 _id DataExist / 不存在集合 find/count/
//      update（match 双分支打印，Err/Ok 皆定，native 定 oracle）；无事务
//      commit/rollback 文案；
//   ⑥ 关闭重开持久性：pre-close 全量 ids + BSON 逐字节 FNV 锚 → drop →
//      open_file 重开（FileBackend::drop checkpoint + journal recovery）→
//      同口径全量回读 assert_eq 对撞 + 更新/删除痕迹抽样复读 + 清理文件。
//
//   上游 3.5.2 行为实勘（双维同文，driver 结构据此安排）：
//     * base session（无 session 的自动提交路径）的 update_one/update_many
//       会泄漏全局 journal Write 事务（DbAuto 计数失衡）：此后任何
//       ClientSession commit（需全局 journal 锁）报
//       StartTransactionInAnotherTransaction，且泄漏的预-close 可见更新
//       在 drop 时被 journal 恢复丢弃、重开后回退（native 实测复现，见
//       [8][9] 段——作为确定性数据点打印断言，两维必须同文）。故全部
//       会话 commit 段（[1][2]）排在任何 base update（[8]）之前；
//     * 不存在集合的 find_many 返回 Ok(空)、count 返回 Ok(0)（[3] 实勘行）。
//   「索引字段」降级为 `_id` 主键 B-tree 面为唯一条目外偏差，其余全覆盖。
//
// 确定性纪律：
//   - `_id` 一律显式赋 i64——fix_doc 对缺 _id 文档自动生成 ObjectId::new()
//     （进程随机，源码实勘），本 driver 从不缺省；
//   - 引擎内部随机源（集合 uuid = Uuid::now_v1、会话 ObjectId、journal salt）
//     一律不打印；InsertManyResult.inserted_ids 是 HashMap，只打印 len；
//   - list_collection_names 客户端再排序；f64 只打 to_bits()；文档摘要一律
//     bson::to_vec 后 FNV（BSON 序即插入序，跨进程稳定）；无路径/时间/地址输出；
//   - 数据全由定种 xorshift64* 生成；stderr 真空。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_polodb.rs
//   B: cd "$(grep -l 'name = "c_polodb"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_polodb.rs
//
// 构建预算备注：闭包 ~40 crate 全 Rust 无 C，轻量（A 热 4.4s、B 增量 0.1s、
// C 热 4.0s，wall）。FRONTIER 绕行：无。
// 2026-07-18 首跑三维逐字节一致、exit 全 0、stderr 全空（各维复跑两次锁定）：
//   ① 会话乐观事务全通道（start/commit/abort + 事务内可读、回滚不见）三维同文；
//   ② journal WAL 持久性闭环——tx1/tx2 commit 数据跨 drop+reopen 全量 FNV 对撞
//      相等，FileBackend::drop checkpoint + 重开 recovery 无分歧；
//   ③ 上游 3.5.2 的 base-update 泄漏语义（[8] 预-close 可见、[9] 重开回退）
//      与 native 逐字节同文复现——属 crate 内部逻辑，非引擎分歧；
//   ④ CRC64 帧校验（crc64fast）、flock、页溢出 large-ticket、bson 全型别
//      编解码经 JIT 阈 1 与解释器无分歧。引擎疑似问题：未发现。
use polodb_core::bson::{doc, oid::ObjectId, spec::BinarySubtype, Binary, Bson, DateTime, Document};
use polodb_core::Database;

/// 定种 xorshift64*（native/mirvm/JIT 三维同序列）。
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

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(n);
        out
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

/// 文档逐字节锚：BSON 编码 FNV（字段序 = 插入序，两侧一致）。
fn doc_fnv(d: &Document) -> u64 {
    fnv1a(&polodb_core::bson::to_vec(d).unwrap())
}

/// 定值 Double 位模式表（0.0/-0.0/1.5/-2.25/π/MAX/最小次正规/1e-8）。
const F64_BITS: [u64; 8] = [
    0x0000000000000000,
    0x8000000000000000,
    0x3ff8000000000000,
    0xc002000000000000,
    0x400921fb54442d18,
    0x7fefffffffffffff,
    0x0000000000000001,
    0x3e45798ee2308c3a,
];

/// 第 i 个 user 文档（全型别覆盖，字段全定值）。
fn user_doc(i: i64) -> Document {
    let blen = 8 + ((i * 7) % 24) as usize;
    let mut oid_bytes = [0u8; 12];
    oid_bytes.copy_from_slice(&Rng(0x01D0_0000 + i as u64).bytes(12));
    doc! {
        "_id": i,
        "i32v": ((i * 37) % 251 - 125) as i32,
        "i64v": Rng(0xCAFE_0000 + i as u64).next() as i64,
        "f64v": f64::from_bits(F64_BITS[(i % 8) as usize]),
        "boolv": i % 3 == 0,
        "s": format!("user:{i:03}"),
        "group": (i % 5) as i32,
        "bytes": Binary {
            subtype: BinarySubtype::Generic,
            bytes: Rng(0xB17E_0000 + i as u64).bytes(blen),
        },
        "nested": { "a": i * 100, "b": [i as i32, (i + 1) as i32, (i + 2) as i32], "c": i % 2 == 0 },
        "arr": [i, i * i, -i],
        "nil": Bson::Null,
        "ts": DateTime::from_millis(1_700_000_000_000 + i * 1000),
        "oid": ObjectId::from_bytes(oid_bytes),
    }
}

fn get_i32(d: &Document, k: &str) -> i32 {
    match d.get(k) {
        Some(Bson::Int32(v)) => *v,
        other => panic!("{k} not i32: {other:?}"),
    }
}

fn get_i64(d: &Document, k: &str) -> i64 {
    match d.get(k) {
        Some(Bson::Int64(v)) => *v,
        other => panic!("{k} not i64: {other:?}"),
    }
}

fn get_str<'a>(d: &'a Document, k: &str) -> &'a str {
    match d.get(k) {
        Some(Bson::String(v)) => v.as_str(),
        other => panic!("{k} not str: {other:?}"),
    }
}

/// 全量回读：ids 序列 + 逐文档 BSON FNV 折叠（_id 主键 B-tree 序）。
fn scan_all(col: &polodb_core::Collection<Document>) -> (usize, String, u64) {
    let all = col.find_many(None).unwrap();
    let mut ids = Vec::with_capacity(all.len());
    let mut acc: u64 = 0xcbf29ce484222325;
    for d in &all {
        ids.push(get_i64(d, "_id").to_string());
        acc ^= doc_fnv(d);
        acc = acc.wrapping_mul(0x100000001b3);
    }
    (all.len(), ids.join(","), acc)
}

fn main() {
    let path = std::env::temp_dir().join("mirvm_c_polodb.db");
    let journal = std::env::temp_dir().join("mirvm_c_polodb.db.journal");
    let _ = std::fs::remove_file(&path); // 起点清空，多跑不累加
    let _ = std::fs::remove_file(&journal);

    // ---- [0] 建库 ----
    let db = Database::open_file(&path).unwrap();
    println!("[0] opened version={}", Database::get_version());

    // ---- [1] tx1（会话乐观事务）：全部 40 users + 2 blobs，commit ----
    let (u_ins, b_ins) = {
        let col = db.collection::<Document>("users");
        let blobs = db.collection::<Document>("blobs");
        let mut s = db.start_session().unwrap();
        s.start_transaction(None).unwrap();
        let mut u_ins = 0u64;
        for i in 0..40i64 {
            col.insert_one_with_session(user_doc(i), &mut s).unwrap();
            u_ins += 1;
        }
        let big = Rng(0xB16B_0001).bytes(16 * 1024); // 16KB 跨 4 页溢出链
        let texts: String = (0..2048u32).map(|k| (b'a' + (k % 26) as u8) as char).collect();
        blobs
            .insert_one_with_session(
                doc! { "_id": 0i64, "payload": Binary { subtype: BinarySubtype::Generic, bytes: big } },
                &mut s,
            )
            .unwrap();
        blobs
            .insert_one_with_session(doc! { "_id": 1i64, "text": texts }, &mut s)
            .unwrap();
        s.commit_transaction().unwrap();
        (u_ins, 2)
    };
    let (n_users, n_blobs, names) = {
        let col = db.collection::<Document>("users");
        let blobs = db.collection::<Document>("blobs");
        let mut names = db.list_collection_names().unwrap();
        names.sort();
        (
            col.count_documents().unwrap(),
            blobs.count_documents().unwrap(),
            names,
        )
    };
    println!("[1] tx1_users={u_ins} tx1_blobs={b_ins} count users={n_users} blobs={n_blobs} cols={}", names.join("|"));

    // ---- [2] tx2：更新($set/$inc/$mul/$max/$unset) + 删除，commit ----
    let (m1, mm, m6, mu, md) = {
        let col = db.collection::<Document>("users");
        let mut s = db.start_session().unwrap();
        s.start_transaction(None).unwrap();
        let m1 = col
            .update_one_with_session(
                doc! { "_id": 3i64 },
                doc! { "$set": { "s": "renamed-03" }, "$inc": { "i32v": 10i32, "i64v": 1000000i64 } },
                &mut s,
            )
            .unwrap()
            .modified_count;
        let mm = col
            .update_many_with_session(
                doc! { "_id": { "$gte": 12i64, "$lt": 16i64 } },
                doc! { "$mul": { "i32v": 2i32 }, "$max": { "group": 9i32 } },
                &mut s,
            )
            .unwrap()
            .modified_count;
        let m6 = col
            .update_one_with_session(doc! { "_id": 6i64 }, doc! { "$set": { "group": 7i32 } }, &mut s)
            .unwrap()
            .modified_count;
        let mu = col
            .update_one_with_session(doc! { "_id": 4i64 }, doc! { "$unset": { "nil": "" } }, &mut s)
            .unwrap()
            .modified_count;
        let md = col
            .delete_many_with_session(doc! { "_id": { "$gte": 8i64, "$lt": 12i64 } }, &mut s)
            .unwrap()
            .deleted_count;
        s.commit_transaction().unwrap();
        (m1, mm, m6, mu, md)
    };
    let n_after_tx2 = db.collection::<Document>("users").count_documents().unwrap();
    println!("[2] tx2 set1={m1} mul_max={mm} m6={m6} unset={mu} deleted={md} count={n_after_tx2}");

    // ---- [3] 查询面（base 读，自动提交路径）：点查全型别 / 等值 / 范围 / 复合 ----
    {
        let col = db.collection::<Document>("users");
        let d = col.find_one(doc! { "_id": 7i64 }).unwrap().unwrap();
        let (fbits, boolv, blen, bfnv) = match d.get("f64v") {
            Some(Bson::Double(f)) => {
                let (bl, bf) = match d.get("bytes") {
                    Some(Bson::Binary(b)) => (b.bytes.len(), fnv1a(&b.bytes)),
                    _ => panic!("bytes missing"),
                };
                (
                    f.to_bits(),
                    match d.get("boolv") {
                        Some(Bson::Boolean(b)) => *b,
                        _ => panic!("boolv missing"),
                    },
                    bl,
                    bf,
                )
            }
            _ => panic!("f64v missing"),
        };
        let nested_a = match d.get("nested") {
            Some(Bson::Document(nd)) => get_i64(nd, "a"),
            _ => panic!("nested missing"),
        };
        let arr_join = match d.get("arr") {
            Some(Bson::Array(a)) => a
                .iter()
                .map(|v| match v {
                    Bson::Int64(x) => x.to_string(),
                    _ => panic!("arr not i64"),
                })
                .collect::<Vec<_>>()
                .join(","),
            _ => panic!("arr missing"),
        };
        let (nil_null, ts_ms, oid_hex) = (
            matches!(d.get("nil"), Some(Bson::Null)),
            match d.get("ts") {
                Some(Bson::DateTime(dt)) => dt.timestamp_millis(),
                _ => panic!("ts missing"),
            },
            match d.get("oid") {
                Some(Bson::ObjectId(o)) => o.to_hex(),
                _ => panic!("oid missing"),
            },
        );
        println!(
            "[3] pk=7 i32={} i64={} f64_bits={:016x} bool={} s={} group={}",
            get_i32(&d, "i32v"),
            get_i64(&d, "i64v"),
            fbits,
            boolv,
            get_str(&d, "s"),
            get_i32(&d, "group")
        );
        println!(
            "[3] pk=7 bytes len={} fnv={:016x} nested.a={} arr=[{}] nil={} ts={} oid={}",
            blen, bfnv, nested_a, arr_join, nil_null, ts_ms, oid_hex
        );
        println!("[3] miss none={}", col.find_one(doc! { "_id": 999i64 }).unwrap().is_none());
        // tx2 更新痕迹复读
        let d3 = col.find_one(doc! { "_id": 3i64 }).unwrap().unwrap();
        let d4 = col.find_one(doc! { "_id": 4i64 }).unwrap().unwrap();
        let mut pieces = Vec::new();
        for id in 12..16i64 {
            let dd = col.find_one(doc! { "_id": id }).unwrap().unwrap();
            pieces.push(format!("{}:{}:{}", id, get_i32(&dd, "i32v"), get_i32(&dd, "group")));
        }
        println!(
            "[3] pk=3 s={} i32={} i64={} pk4nil={} mulmax=[{}]",
            get_str(&d3, "s"),
            get_i32(&d3, "i32v"),
            get_i64(&d3, "i64v"),
            d4.get("nil").is_some(),
            pieces.join(",")
        );
        // 非键字段等值全扫
        let hits = col.find_many(doc! { "group": 2i32 }).unwrap();
        let ids: Vec<String> = hits.iter().map(|d| get_i64(d, "_id").to_string()).collect();
        let mut acc: u64 = 0xcbf29ce484222325;
        for d in &hits {
            acc ^= doc_fnv(d);
            acc = acc.wrapping_mul(0x100000001b3);
        }
        println!("[3] group=2 n={} ids={} fnv={:016x}", hits.len(), ids.join(","), acc);
        // 主键范围（贯通 tx2 删除洞）
        let hits = col
            .find_many(doc! { "_id": { "$gte": 10i64, "$lt": 20i64 } })
            .unwrap();
        let ids: Vec<String> = hits.iter().map(|d| get_i64(d, "_id").to_string()).collect();
        println!("[3] range [10,20) n={} ids={}", hits.len(), ids.join(","));
        // 复合谓词
        let or_hits = col
            .find_many(doc! { "$or": [ { "_id": 3i64 }, { "group": 4i32 } ] })
            .unwrap();
        let or_ids: Vec<String> = or_hits.iter().map(|d| get_i64(d, "_id").to_string()).collect();
        let in_hits = col
            .find_many(doc! { "_id": { "$in": [1i64, 5i64, 999i64] } })
            .unwrap();
        let in_ids: Vec<String> = in_hits.iter().map(|d| get_i64(d, "_id").to_string()).collect();
        println!("[3] or n={} ids={}", or_hits.len(), or_ids.join(","));
        println!("[3] in n={} ids={}", in_hits.len(), in_ids.join(","));
    }

    // ---- [4] tx3 abort：事务内可见、回滚不见；无事务报错文案 ----
    {
        let col = db.collection::<Document>("users");
        let mut s = db.start_session().unwrap();
        s.start_transaction(None).unwrap();
        for i in 100..105i64 {
            col.insert_one_with_session(doc! { "_id": i, "s": format!("tmp:{i}") }, &mut s).unwrap();
        }
        let in_tx = col.count_documents_with_session(&mut s).unwrap();
        s.abort_transaction().unwrap();
        let after = col.count_documents().unwrap();
        println!("[4] abort_tx in_tx={in_tx} after={after}");
        let mut s2 = db.start_session().unwrap();
        match s2.commit_transaction() {
            Ok(()) => println!("[4] commit_none ok"),
            Err(e) => println!("[4] commit_none err: {e}"),
        }
        match s2.abort_transaction() {
            Ok(()) => println!("[4] abort_none ok"),
            Err(e) => println!("[4] abort_none err: {e}"),
        }
    }

    // ---- [5] 错误路径（match 双分支打印）----
    {
        let col = db.collection::<Document>("users");
        match col.update_one(doc! { "_id": 5i64 }, doc! { "$set": { "_id": 55i64 } }) {
            Ok(r) => println!("[5] update_pkey unexpected ok modified={}", r.modified_count),
            Err(e) => println!("[5] update_pkey err: {e}"),
        }
        match col.insert_one(doc! { "_id": 7i64, "s": "dup" }) {
            Ok(_) => println!("[5] dup unexpected ok"),
            Err(e) => println!("[5] dup err: {e}"),
        }
        let nope = db.collection::<Document>("nope");
        match nope.find_many(None) {
            Ok(v) => println!("[5] missing find: ok n={}", v.len()),
            Err(e) => println!("[5] missing find: err: {e}"),
        }
        println!("[5] missing count={}", nope.count_documents().unwrap());
        match nope.update_one(doc! { "_id": 0i64 }, doc! { "$set": { "s": "x" } }) {
            Ok(r) => println!("[5] missing update: ok modified={}", r.modified_count),
            Err(e) => println!("[5] missing update: err: {e}"),
        }
    }

    // ---- [6] blobs 大对象读回（large-ticket 溢出链）----
    let blob_fnv = {
        let blobs = db.collection::<Document>("blobs");
        let d0 = blobs.find_one(doc! { "_id": 0i64 }).unwrap().unwrap();
        let expect = Rng(0xB16B_0001).bytes(16 * 1024);
        let (len, fnv, eq) = match d0.get("payload") {
            Some(Bson::Binary(b)) => (b.bytes.len(), fnv1a(&b.bytes), b.bytes == expect),
            _ => panic!("payload missing"),
        };
        let d1 = blobs.find_one(doc! { "_id": 1i64 }).unwrap().unwrap();
        let tlen = get_str(&d1, "text").len();
        println!("[6] blob0 len={len} fnv={fnv:016x} eq={eq} blob1 text_len={tlen}");
        fnv
    };

    // ---- [7] pre-close 全量锚（此后只做不破坏锚的操作）----
    let (pre_n, pre_ids, pre_fnv) = {
        let col = db.collection::<Document>("users");
        scan_all(&col)
    };
    println!("[7] pre-close n={pre_n} ids={pre_ids}");
    println!("[7] pre-close fnv={pre_fnv:016x} blob_fnv={blob_fnv:016x}");

    // ---- [8] base session 更新（3.5.2 上游语义：泄漏 journal 写事务，预-close
    //      可见、drop 时被丢弃、重开回退——确定性数据点，两维同文才准）----
    {
        let col = db.collection::<Document>("users");
        let up = col
            .update_one(
                doc! { "_id": 5i64 },
                doc! { "$set": { "s": "leak-demo" }, "$inc": { "i32v": 1i32 } },
            )
            .unwrap();
        let d5 = col.find_one(doc! { "_id": 5i64 }).unwrap().unwrap();
        println!(
            "[8] base_update modified={} id5 s={} i32={}",
            up.modified_count,
            get_str(&d5, "s"),
            get_i32(&d5, "i32v")
        );
    }

    // ---- [9] 关闭重开：commit 数据持久 + base 泄漏更新回退（上游语义对撞）----
    drop(db);
    let db = Database::open_file(&path).unwrap();
    let (post_n, post_ids, post_fnv, post_blob_fnv, id5_reverted, spot) = {
        let col = db.collection::<Document>("users");
        let (n, ids, f) = scan_all(&col);
        let blobs = db.collection::<Document>("blobs");
        let bf = match blobs.find_one(doc! { "_id": 0i64 }).unwrap().unwrap().get("payload") {
            Some(Bson::Binary(b)) => fnv1a(&b.bytes),
            _ => panic!("payload missing post"),
        };
        let d3 = col.find_one(doc! { "_id": 3i64 }).unwrap().unwrap();
        let d5 = col.find_one(doc! { "_id": 5i64 }).unwrap().unwrap();
        let d6 = col.find_one(doc! { "_id": 6i64 }).unwrap().unwrap();
        let d4 = col.find_one(doc! { "_id": 4i64 }).unwrap().unwrap();
        let id5_reverted = get_str(&d5, "s") == "user:005";
        let spot = format!(
            "id3.s={} id6.group={} id4.has_nil={} gone8={} id5.s={}",
            get_str(&d3, "s"),
            get_i32(&d6, "group"),
            d4.get("nil").is_some(),
            col.find_one(doc! { "_id": 8i64 }).unwrap().is_none(),
            get_str(&d5, "s")
        );
        (n, ids, f, bf, id5_reverted, spot)
    };
    println!("[9] reopened n={post_n} fnv={post_fnv:016x} blob_fnv={post_blob_fnv:016x}");
    println!("[9] persist match n={} ids={} fnv={}", post_n == pre_n, post_ids == pre_ids, post_fnv == pre_fnv);
    println!("[9] leak_reverted={id5_reverted} spot: {spot}");
    assert_eq!(post_n, pre_n);
    assert_eq!(post_ids, pre_ids);
    assert_eq!(post_fnv, pre_fnv);
    assert_eq!(post_blob_fnv, blob_fnv);
    assert!(id5_reverted);
    drop(db);

    // ---- [10] 清理临时库文件 ----
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&journal);
    println!("[10] cleaned={}", !path.exists() && !journal.exists());
    println!("polodb ok");
}
