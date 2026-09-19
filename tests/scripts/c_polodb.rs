#!/usr/bin/env mirvm
---
[dependencies]
# polodb_core is the embedded document database of the PoloDB family (`polodb`
# itself is the MongoDB-compatible server shell). Pinned to =3.5.2, the newest 3.x:
# 5.x depends on polodb-librocksdb-sys, a bundled C++ rocksdb build that needs
# clang/cmake and is not pure Rust; 4.x is outside the 3.x line.
# The 3.5.2 Linux closure is entirely pure Rust: bson 2, byteorder, crc64fast,
# getrandom 0.2, hashbrown, lru, num_enum, serde, uuid 1 (web-sys/winapi/js-sys are
# cfg-gated and not compiled on Linux).
# WAL is the built-in journal: <name>.db.journal sits beside the db path and merges
# into the main file once it reaches 1000 bytes; unix locking is libc
# flock(LOCK_EX|LOCK_NB), and reopening runs journal recovery.
polodb_core = "=3.5.2"
# polodb_core 3.5.2 requests uuid's "getrandom" feature, which was removed in
# uuid 1.14+ (renamed to rng-getrandom in 1.13). Resolving uuid to 1.24.0 fails
# with "requested a feature that does not exist"; plain cargo resolves the same way, so this is not a
# mirvm fork. Pin uuid = 1.6.1: that is where the getrandom feature is still
# present.
uuid = "=1.6.1"
---
// polodb_core 3.5.2 embedded document database differential: the crate ships its own
// query bytecode VM, btree pages, journal WAL and optimistic session transactions.
//
// Coverage:
//   ① Create the db in a temp dir (.db + .db.journal cleared at the start);
//   ② Bulk insert in a transaction: 40 user docs covering BSON Int32/Int64/Double
//      (from_bits table)/Boolean/String/Binary (variable length)/nested
//      Document/Array/Null/fixed DateTime/fixed-byte ObjectId; blobs gets 2 docs
//      (16KB binary across a 4-page large-ticket overflow chain; 2KB string);
//   ③ The primary-key B-tree is the only unique index -- secondary create_index is
//      unreachable in 3.5.2: Collection::create_index is private and
//      Database::create_index is pub(super), landing on internal_create_index =
//      unimplemented!(), so no public path can create an index. Index coverage
//      therefore uses the `_id` B-tree: point lookup (pkey fast path), non-key
//      equality full scan, [$gte,$lt) range, $or, $in, full read in pkey order;
//   ④ Update + delete transaction (ClientSession optimistic: writes accumulate in
//      page_map and the global journal is locked only at commit): tx2 does
//      $set/$inc/$mul/$max/$unset/delete_many + commit; tx3 inserts 5 rows, sees 41
//      inside the transaction, aborts, and the rows are gone;
//   ⑤ Error paths: $set on _id illegal / duplicate _id DataExist / find, count and
//      update on a missing collection (both match arms print, Err and Ok are both
//      deterministic, native is the oracle); commit/rollback with no transaction;
//   ⑥ Close/reopen persistence: pre-close full ids + byte-for-byte BSON FNV anchor
//      -> drop -> reopen (FileBackend::drop checkpoint + journal recovery) -> same
//      full re-read checked with assert_eq + spot re-read of update/delete traces;
//
// Upstream 3.5.2 behaviours the driver depends on:
//   * update_one/update_many on the base session (the auto-commit path with no
//     session) leak a global journal Write transaction (DbAuto refcount imbalance):
//     any later ClientSession commit (which needs the global journal lock) reports
//     StartTransactionInAnotherTransaction, and the leaked, pre-close-visible update
//     is discarded by journal recovery on drop and reverts after reopen (printed as
//     a deterministic data point in [8][9]); every session commit section ([1][2]) is
//     therefore ordered before any base update ([8]);
//   * find_many on a missing collection returns Ok(empty) and count returns Ok(0);
//     the `_id` primary-key B-tree standing in for index fields is the only deviation.
//
// Determinism discipline:
//   - `_id` is always explicit i64: fix_doc auto-generates a random ObjectId::new()
//     for a document without _id, so the driver never omits it;
//   - engine-internal random sources (collection uuid = Uuid::now_v1, session
//     ObjectId, journal salt) are never printed; inserted_ids is a HashMap, only its
//     len is printed; names are re-sorted; f64 only via to_bits();
//     digests are bson::to_vec then FNV (BSON order = insertion order, stable
//     across processes); seeded xorshift64*; no path/time/address; stderr empty.
//
// Differential oracle: the same fixture is run three ways and the three outputs are
// compared.
//   A: the release mirvm binary interpreted (`mirvm run tests/scripts/c_polodb.rs`);
//   B: the cached cargo project for this script, built and run with the real nightly
//      rustc (native), against the same pinned Cargo.lock;
//   C: the mirvm run with MIRVM_JIT_THRESHOLD=1, compiling every guest fn.
//
// The oracle requires all three runs to agree byte-for-byte with exit 0 and empty
// stderr. FRONTIER: none.
//   * session optimistic transactions (start/commit/abort, in-transaction reads,
//     aborted rows gone): identical across all three runs;
//   * journal WAL persistence: tx1/tx2 commits survive drop+reopen with an equal full
//     FNV, FileBackend::drop checkpoint + reopen recovery without divergence;
//   * upstream 3.5.2 base-update leak ([8] visible pre-close, [9] reverted after
//     reopen): reproduces in the native run as crate-internal logic, not a bug;
//   * CRC64 frames (crc64fast), flock, large-ticket page overflow and all BSON type
//     codecs: no divergence between JIT-threshold-1 and the interpreter.
use polodb_core::bson::{doc, oid::ObjectId, spec::BinarySubtype, Binary, Bson, DateTime, Document};
use polodb_core::Database;

/// Seeded xorshift64* (same sequence in native, mirvm and JIT).
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

/// Byte-for-byte document anchor: FNV over the BSON encoding (field order = insertion order).
fn doc_fnv(d: &Document) -> u64 {
    fnv1a(&polodb_core::bson::to_vec(d).unwrap())
}

/// Fixed Double bit patterns (0.0/-0.0/1.5/-2.25/π/MAX/smallest subnormal/1e-8).
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

/// User document `i` (every BSON type, all field values fixed).
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

/// Full re-read: id sequence + per-document BSON FNV fold (in `_id` primary-key order).
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
    let _ = std::fs::remove_file(&path); // clear at start so repeated runs do not accumulate
    let _ = std::fs::remove_file(&journal);

    // ---- [0] open the database ----
    let db = Database::open_file(&path).unwrap();
    println!("[0] opened version={}", Database::get_version());

    // ---- [1] tx1 (session optimistic transaction): all 40 users + 2 blobs, commit ----
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
        let big = Rng(0xB16B_0001).bytes(16 * 1024); // 16KB across a 4-page overflow chain
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

    // ---- [2] tx2: update ($set/$inc/$mul/$max/$unset) + delete, commit ----
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

    // ---- [3] query surface (base reads, auto-commit): point lookup, equality, range ----
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
        // re-read the tx2 update traces
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
        // equality full scan on a non-key field
        let hits = col.find_many(doc! { "group": 2i32 }).unwrap();
        let ids: Vec<String> = hits.iter().map(|d| get_i64(d, "_id").to_string()).collect();
        let mut acc: u64 = 0xcbf29ce484222325;
        for d in &hits {
            acc ^= doc_fnv(d);
            acc = acc.wrapping_mul(0x100000001b3);
        }
        println!("[3] group=2 n={} ids={} fnv={:016x}", hits.len(), ids.join(","), acc);
        // primary-key range (spans the tx2 delete hole)
        let hits = col
            .find_many(doc! { "_id": { "$gte": 10i64, "$lt": 20i64 } })
            .unwrap();
        let ids: Vec<String> = hits.iter().map(|d| get_i64(d, "_id").to_string()).collect();
        println!("[3] range [10,20) n={} ids={}", hits.len(), ids.join(","));
        // compound predicates
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

    // ---- [4] tx3 abort: visible in-transaction, gone after rollback; no-transaction errors ----
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

    // ---- [5] error paths (both match arms print) ----
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

    // ---- [6] blobs large-object read-back (large-ticket overflow chain) ----
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

    // ---- [7] pre-close full anchor (only anchor-preserving operations follow) ----
    let (pre_n, pre_ids, pre_fnv) = {
        let col = db.collection::<Document>("users");
        scan_all(&col)
    };
    println!("[7] pre-close n={pre_n} ids={pre_ids}");
    println!("[7] pre-close fnv={pre_fnv:016x} blob_fnv={blob_fnv:016x}");

    // ---- [8] base session update (upstream 3.5.2: leaks a journal write transaction,
    //      visible pre-close, dropped on drop, reverted after reopen) ----
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

    // ---- [9] close and reopen: committed data persists, leaked base update reverts ----
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

    // ---- [10] clean up the temp database files ----
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&journal);
    println!("[10] cleaned={}", !path.exists() && !journal.exists());
    println!("polodb ok");
}
