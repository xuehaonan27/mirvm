#!/usr/bin/env mirvm
---
[dependencies]
# sqlx 钉 =0.8.6（0.8 系最新，crates.io 实测 2026-07-17；0.9.0 已发布，超出本槽位
# 「最新 0.8.x」授权）。default-features=false 全关后只开 runtime-tokio（sqlite 本地
# 库无 TLS 需求，不开任何 tls）+ sqlite（经 sqlx-sqlite → libsqlite3-sys bundled 现场
# cc 编 C sqlite3.c——与 c_rusqlite_db 同一家族；该家族曾撞 native-archive 闭包缺
# -lm 缺口，已由 LINK_SUFFIX 恒带系统库修复翻绿，本 driver 求同通道复核 + 压异步
# 执行器通道）。tokio 钉 1（rt+time：current_thread runtime inside main 的当前最小
# 面，c_tokio 先例；sqlx-core 自身 runtime-tokio 也会并特性并集）。
sqlx = { version = "=0.8.6", default-features = false, features = ["runtime-tokio", "sqlite"] }
tokio = { version = "1", default-features = false, features = ["rt", "time"] }
---
// sqlx 0.8.6 + bundled C sqlite：异步执行器面与 C FFI 通道的三维压测（批8 波1）。
//
// 压力点（本批「重 FFI/C」定位）：
//   ① sqlx-sqlite 的 worker 线程通道：每个连接 spawn 一条 std 线程，命令经
//      std::sync::mpsc 送入、结果经 futures_channel oneshot 取回——guest 线程间
//      park/unpark 全走模拟 futex，current_thread 运行时 block_on 与 worker 互喂，
//      压协作调度 + async 无栈状态机 + SC 交错确定性（c_crossbeam/c_tokio 先例的
//      sqlx 形态）。
//   ② C sqlite FFI 全通道：prepare/step/column_* /errmsg 等 extern fn 直落
//      .a→.so 闭包产物；参数/返回值全标量封送（无按值聚合——debt-map §9 既定
//      边界不涉及）。
//   ③ pool + 显式事务状态机（begin/commit/rollback）。
//
// 测试面清单：
//   tokio current_thread runtime inside main 手建并 block_on；
//   memory sqlite（sqlite::memory:，纯内存零落盘）open + sqlite_version 锚；
//   CREATE TABLE（PK / UNIQUE / CHECK / REAL / 可空 TEXT）；
//   1000 行单事务批量插入（commit 路径），客户端同序累计 sum_val/name fnv 与
//     服务端 COUNT/SUM 口径核对；
//   聚合查询：COUNT/SUM/AVG(f64 to_bits) + GROUP BY 16 桶 ORDER BY 定序；
//   显式事务 rollback 路径（tx 内 1003 行、回滚后 1000 行断言）；
//   error 路径锁定错误串：UNIQUE 违反 / CHECK 违反 / no such table / fetch_optional
//     miss(None)；NULL 读路径（COUNT(note IS NULL) 服务端/客户端口径核对 + 真读
//     一个 NULL 列进 Option<String>）；
//   type info 反射断言：六列 Column::name + TypeInfo::name 打印，按名列取；
//   全表 1000 行 ORDER BY id 回读 fnv 锚与客户端序列比对。
//
// 确定性：定种 xorshift64*；零文件 IO/零时间/零随机源；不用 HashMap 序输出；
// f64 一律 to_bits；错误串为 sqlite 库内定串；行内断言用 assert_eq! 但关键口径
// 全部 println 锁定；stderr 真空（driver 零 warning）。1000 行批量仅打印聚合计，
// 输出 ~40 行。
//
// 三维复跑：
//   A: target/release/mirvm run corpus/c_sqlx_sqlite.rs
//   B: cd "$(grep -l 'name = "c_sqlx_sqlite"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname)" && \
//        RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//        "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_sqlx_sqlite.rs
//
// 构建预算备注：sqlx+tokio+futures 全图 + bundled sqlite3.c 现场 cc 编制。实测
// 未触发放宽（A 冷 26.9s 含全图物化、A 热 7.4s、B 增量原生构建+跑 17s、C 1.2s，
// wall；机器另有同波 c_aws_lc 并发构建）。
// FRONTIER 绕行：无。2026-07-17 首跑三维逐字节一致、exit 全 0、stderr 全空：
// ① worker 线程通道（std::sync::mpsc 命令 + futures oneshot 回包）在协作调度下
// 与 native 行为逐字节一致；② bundled C sqlite 全 FFI 通道（prepare/step/
// column_*/errmsg 与错误码 2067/275/1）三维同文；③ JIT 阈 1 与解释器无分歧。
// 引擎疑似问题：未发现。
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::{Column, Row, TypeInfo};

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

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// 内联 FNV-1a（二进制内容锚定，不打印原始字节）。
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
    // 内存库单连接池：max_connections(1) 保证事务与后续查询恒走同一连接
    // （sqlite::memory: 的开库即空库，多连接会是多张互不相见的库）。
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

    // ① 单事务批量插入 1000 行 = commit 路径；客户端同序累计校验锚。
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

    // ② 服务端聚合口径核对（COUNT/SUM + AVG f64 bits）。
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

    // ③ GROUP BY 16 桶、定序输出。
    let groups: Vec<(i64, i64, i64)> =
        sqlx::query_as("SELECT grp, COUNT(*), SUM(val) FROM items GROUP BY grp ORDER BY grp")
            .fetch_all(&pool)
            .await
            .unwrap();
    println!("groups={}", groups.len());
    for (g, c, s) in &groups {
        println!("grp {g:02} n={c} sum={s}");
    }

    // 主键点查（后段错误路径要复用 first_name 做 UNIQUE 触发）。
    let (first_name, first_val): (String, i64) =
        sqlx::query_as("SELECT name, val FROM items WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    println!("first id=1 name={first_name:?} val={first_val}");

    // ④ 显式事务 rollback 路径：tx 内见 1003 行，回滚后恒 1000。
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

    // ⑤ error 路径：UNIQUE / CHECK / 缺表 / fetch_optional miss。
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

    // NULL 读路径：服务端 NULL 计数与客户端口径核对 + 真读一个 NULL 列。
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

    // ⑥ type info 反射：六列的 name/type 全打印 + 按名列取。
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

    // ⑦ 全表回读（ORDER BY id 定序扫描 1000 行）与客户端序列 fnv 比对。
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
