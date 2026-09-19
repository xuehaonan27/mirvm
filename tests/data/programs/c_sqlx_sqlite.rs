#!/usr/bin/env mirvm
---
[dependencies]
# sqlx is pinned to =0.8.6, the newest 0.8 release on crates.io (0.9.0 exists but is outside
# the newest-0.8.x scope required here). With default features off, only runtime-tokio (the local
# sqlite library needs no TLS, so no tls feature) and sqlite are enabled; sqlite goes through
# sqlx-sqlite -> libsqlite3-sys bundled, which cc-compiles C sqlite3.c in place, the same
# family as c_rusqlite_db. That family once hit a missing -lm in the native-archive closure,
# now fixed by always appending system libraries via LINK_SUFFIX. tokio is pinned to 1 with
# only rt+time, the smallest current_thread runtime surface inside main (as in c_tokio).
sqlx = { version = "=0.8.6", default-features = false, features = ["runtime-tokio", "sqlite"] }
tokio = { version = "1", default-features = false, features = ["rt", "time"] }
---
// sqlx 0.8.6 plus bundled C sqlite: a three-way stress of the async executor surface and the
// C FFI channel.
//
// Pressure points:
//   ① sqlx-sqlite's worker-thread channel: every connection spawns a std thread, commands go
//      in over std::sync::mpsc and results come back over a futures_channel oneshot, so guest
//      thread park/unpark all goes through the simulated futex; the current_thread runtime's
//      block_on and the worker feed each other, stressing cooperative scheduling, the async
//      stackless state machine and deterministic SC interleaving.
//   ② The full C sqlite FFI channel: extern fns such as prepare/step/column_*/errmsg land in
//      the .a -> .so closure artifact; arguments and returns are all scalar marshalling, with
//      no by-value aggregates.
//   ③ A connection pool plus an explicit transaction state machine (begin/commit/rollback).
//
// Test surface:
//   a tokio current_thread runtime built inside main, driven by block_on;
//   an in-memory sqlite database (sqlite::memory:, no file IO) opened with a sqlite_version
//     anchor;
//   CREATE TABLE with PRIMARY KEY / UNIQUE / CHECK / REAL / nullable TEXT;
//   a 1000-row batch insert in a single transaction (the commit path), cross-checking the
//     client-side running sum_val and name fnv against server-side COUNT and SUM;
//   aggregate queries: COUNT/SUM/AVG (f64 to_bits) plus GROUP BY into 16 buckets with a fixed
//     ORDER BY;
//   the explicit rollback path (1003 rows visible inside the transaction, 1000 after rollback);
//   error paths with fixed strings: UNIQUE violation, CHECK violation, no such table, and a
//     fetch_optional miss (None);
//   the NULL read path (server COUNT(note IS NULL) cross-checked against the client count, and
//     one NULL column really read into an Option<String>);
//   type-info reflection: the six columns print Column::name and TypeInfo::name and are read
//     by name;
//   the full table re-read in ORDER BY id order, its fnv compared with the client sequence.
//
// Determinism: a seeded xorshift64*; no file IO, time or randomness; no HashMap-ordered
// output; f64 values printed as to_bits; error strings are sqlite's own fixed strings; rows
// use assert_eq! but every key figure is also pinned by a println; stderr is empty (the driver
// has zero warnings). The 1000-row batch prints only its aggregate counts; the output is
// about 40 lines.
//
// Three-way rerun:
//   A: target/release/mirvm run tests/data/programs/c_sqlx_sqlite.rs
//   B: cd "$(grep -l 'name = "c_sqlx_sqlite"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_sqlx_sqlite.rs
//
// Build budget: the whole sqlx+tokio+futures graph plus bundled sqlite3.c compiled by cc in
// place; see the three-way rerun above.
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Column, Row, TypeInfo};

/// Seeded xorshift64*, the same sequence on native, mirvm and JIT.
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

/// Inline FNV-1a, anchoring binary content without printing raw bytes.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(run());
}

async fn run() {
    // Single-connection in-memory pool: max_connections(1) keeps transactions and later queries
    // on one connection (sqlite::memory: opens an empty database; more would be invisible to each other).
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();

    let (ver,): (String,) = sqlx::query_as("SELECT sqlite_version()")
        .fetch_one(&pool)
        .await
        .unwrap();
    println!("sqlite version = {ver}");

    sqlx::query(
        "CREATE TABLE items (
             id    INTEGER PRIMARY KEY,
             grp   INTEGER NOT NULL,
             name  TEXT NOT NULL UNIQUE,
             val   INTEGER NOT NULL CHECK (val >= -2000000),
             score REAL NOT NULL,
             note  TEXT
         )",
    )
    .execute(&pool)
    .await
    .unwrap();
    println!("ddl done");

    // ① Single-transaction batch insert of 1000 rows (the commit path); client-side running checksum anchor.
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut sum_val: i64 = 0;
    let mut name_concat: Vec<u8> = Vec::new();
    let mut null_notes: u64 = 0;
    let mut tx = pool.begin().await.unwrap();
    for i in 0..1000u64 {
        let grp = (i % 16) as i64;
        let name = format!("item-{i:04}-{:05x}", rng.below(0x100000));
        let val = rng.below(3_000_000) as i64 - 1_500_000;
        let score = rng.below(100_000) as f64 / 100.0;
        let note = if rng.below(4) == 0 {
            null_notes += 1;
            None
        } else {
            Some(format!("note-{:05x}", rng.below(0x100000)))
        };
        sqlx::query("INSERT INTO items(grp, name, val, score, note) VALUES (?1, ?2, ?3, ?4, ?5)")
            .bind(grp)
            .bind(&name)
            .bind(val)
            .bind(score)
            .bind(&note)
            .execute(&mut *tx)
            .await
            .unwrap();
        sum_val += val;
        name_concat.extend_from_slice(name.as_bytes());
    }
    tx.commit().await.unwrap();
    let client_fnv = fnv1a(&name_concat);
    println!("batch commit rows=1000 sum_val={sum_val} null_notes={null_notes} fnv={client_fnv:016x}");

    // ② Server-side aggregate cross-check (COUNT/SUM plus AVG f64 bits).
    let (n, s): (i64, i64) = sqlx::query_as("SELECT COUNT(*), SUM(val) FROM items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1000);
    assert_eq!(s, sum_val);
    println!("count={n} sum_val={s} client_match={}", n == 1000 && s == sum_val);
    let (avg,): (f64,) = sqlx::query_as("SELECT AVG(val) FROM items")
        .fetch_one(&pool)
        .await
        .unwrap();
    println!("avg_val_bits={:016x}", avg.to_bits());

    // ③ GROUP BY into 16 buckets, printed in fixed order.
    let groups: Vec<(i64, i64, i64)> =
        sqlx::query_as("SELECT grp, COUNT(*), SUM(val) FROM items GROUP BY grp ORDER BY grp")
            .fetch_all(&pool)
            .await
            .unwrap();
    println!("groups={}", groups.len());
    for (g, c, s) in &groups {
        println!("grp {g:02} n={c} sum={s}");
    }

    // Primary-key point query (the later error path reuses first_name to trigger UNIQUE).
    let (first_name, first_val): (String, i64) =
        sqlx::query_as("SELECT name, val FROM items WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    println!("first id=1 name={first_name:?} val={first_val}");

    // ④ Explicit rollback path: 1003 rows are visible inside the transaction, always 1000 after.
    {
        let mut tx = pool.begin().await.unwrap();
        for i in 0..3u64 {
            sqlx::query(
                "INSERT INTO items(grp, name, val, score, note) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(0)
            .bind(format!("rb-{i}"))
            .bind(1)
            .bind(0.0)
            .bind(None::<String>)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        let (inside,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM items")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let (after,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM items")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(inside, 1003);
        assert_eq!(after, 1000);
        println!("rollback inside={inside} after={after}");
    }

    // ⑤ Error paths: UNIQUE / CHECK / missing table / fetch_optional miss.
    match sqlx::query("INSERT INTO items(grp, name, val, score, note) VALUES (?1, ?2, ?3, ?4, ?5)")
        .bind(1)
        .bind(&first_name)
        .bind(1)
        .bind(0.0)
        .bind(None::<String>)
        .execute(&pool)
        .await
    {
        Ok(_) => println!("unique: unexpected ok"),
        Err(e) => println!("unique err: {e}"),
    }
    match sqlx::query("INSERT INTO items(grp, name, val, score, note) VALUES (?1, ?2, ?3, ?4, ?5)")
        .bind(1)
        .bind("bad-val")
        .bind(-3_000_000)
        .bind(0.0)
        .bind(None::<String>)
        .execute(&pool)
        .await
    {
        Ok(_) => println!("check: unexpected ok"),
        Err(e) => println!("check err: {e}"),
    }
    match sqlx::query("SELECT nope FROM missing_table").execute(&pool).await {
        Ok(_) => println!("prepare: unexpected ok"),
        Err(e) => println!("prepare err: {e}"),
    }
    let miss = sqlx::query("SELECT name FROM items WHERE id = 9999")
        .fetch_optional(&pool)
        .await
        .unwrap();
    println!("miss none = {}", miss.is_none());

    // NULL read path: server-side NULL count cross-checked with the client, plus one real NULL column read.
    let (null_cnt,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM items WHERE note IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    println!("null notes server={null_cnt} client_match={}", null_cnt as u64 == null_notes);
    let null_row = sqlx::query("SELECT note FROM items WHERE note IS NULL ORDER BY id LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let null_note: Option<String> = null_row.get(0);
    println!("null note read none = {}", null_note.is_none());

    // ⑥ Type-info reflection: all six columns print name/type, then are read by name.
    let row = sqlx::query("SELECT id, grp, name, val, score, note FROM items WHERE id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    for (i, c) in row.columns().iter().enumerate() {
        println!("col {i} name={} type={}", c.name(), c.type_info().name());
    }
    let id: i64 = row.get("id");
    let name: String = row.get("name");
    let score: f64 = row.get("score");
    let note: Option<String> = row.get("note");
    assert_eq!(id, 1);
    assert_eq!(name, first_name);
    println!(
        "row1 id={id} name={name:?} score_bits={:016x} note_none={}",
        score.to_bits(),
        note.is_none()
    );

    // ⑦ Full-table re-read (ORDER BY id, 1000 rows) with its fnv compared against the client sequence.
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM items ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1000);
    let mut joined: Vec<u8> = Vec::new();
    for (rname,) in &rows {
        joined.extend_from_slice(rname.as_bytes());
    }
    let rescan_fnv = fnv1a(&joined);
    println!(
        "rescan rows={} fnv={rescan_fnv:016x} match={}",
        rows.len(),
        rescan_fnv == client_fnv
    );
}
