#!/usr/bin/env mirvm
---
[dependencies]
rusqlite = { version = "0.32", features = ["bundled"] }
---
// rusqlite 0.32 + bundled sqlite3（libsqlite3-sys 0.30.1 编译 C 静态归档）。
//
// ★ FRONTIER（2026-07-15，新类别：静态原生归档闭包）：mirvm 无法装载，
//   native 全绿。诊断原文（lower 期 panic，exit 101）：
//     Static native library 装载失败: 静态原生归档 `…/libsqlite3.a` 无法安全
//     转换为共享库（要求 ELF PIC、依赖在本归档内闭合）: /usr/bin/ld:
//     …sqlite3.c:235126: undefined reference to `log'
//   根因：src/native_archive.rs 的 .a→.so 转换以
//   `cc -shared -Wl,-z,defs --whole-archive` 独立链接每个归档，符号必须在
//   归档 + cc 默认库（libc/libgcc）内闭合；libsqlite3-sys build.rs 硬编码
//   -DSQLITE_ENABLE_FTS5（无特性可关），fts5Bm25GetData 引用 libm 的 `log`
//   ——全归档唯一闭包外符号（已用同一链接行手工复现确认）。native 不受影响
//   （rustc linux-gnu 最终链接自带 -lm）；运行期本也无碍（ldd 实证 mirvm
//   进程自身 DT_NEEDED 含 libm.so.6，无 -z defs 时 dlopen 可经全局域解析）。
//   绕行穷举：rusqlite 0.32 / libsqlite3-sys 0.30.x 全 feature 无
//   SQLITE_OMIT_FTS5 开关；版本钉死（任务给定）；CFLAGS/env 不属于单文件
//   driver（验收命令固定，不可复现）；手工预填 native-archives 缓存等于
//   篡改 harness 状态，拒绝采用。→ 按纪律报 FRONTIER，driver 保留为
//   native 验证过的解锁探针（mirvm 侧放行归档的 libm 依赖后即应转绿）。
// 覆盖：文件库 open / PRAGMA / DDL 四表（PK、FK ON DELETE CASCADE、UNIQUE、
// CHECK、两个二级索引）/ 命名参数 + 位置参数 prepared statement 批量插入
// （INTEGER / REAL / TEXT / BLOB / NULL 全类型）/ 事务提交与回滚各一 /
// INSERT...RETURNING / JOIN + 聚合（GROUP BY / HAVING / 确定 ORDER BY）/
// last_insert_rowid / changes / query_row + OptionalExtension + 按名列取 /
// 列元数据 / UNIQUE、FK、NOT NULL、CHECK、坏表名、列型错六条错误路径 /
// FK 级联删除 / BLOB fnv 锚定 + roundtrip 布尔 / 结尾清理临时库文件。
// 确定性：定种 xorshift64* 生成数据；REAL 一律 to_bits() 打印；不打印路径 /
// 时间 / 地址 / HashMap 序；sqlite 错误文本为库内固定字符串；stderr 为空
// （driver 零 warning）。
use rusqlite::{named_params, params, Connection, OptionalExtension};

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

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(n);
        out
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

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn main() {
    // ---- ⓪ 固定名字的临时库（多跑不累加：先清残留，结尾再清） ----
    let db = std::env::temp_dir().join("mirvm_corpus_rusqlite_db.sqlite3");
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }

    println!("sqlite version {}", rusqlite::version());
    let mut conn = Connection::open(&db).unwrap();

    // ---- ① PRAGMA + DDL：四表两索引 ----
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    let fk_on: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    println!("pragma foreign_keys = {fk_on}");
    conn.execute_batch(
        "CREATE TABLE authors (
             id   INTEGER PRIMARY KEY,
             name TEXT NOT NULL UNIQUE,
             born INTEGER NOT NULL CHECK (born BETWEEN 0 AND 2100)
         );
         CREATE TABLE books (
             id        INTEGER PRIMARY KEY,
             author_id INTEGER NOT NULL REFERENCES authors(id) ON DELETE CASCADE,
             title     TEXT NOT NULL,
             price     REAL NOT NULL CHECK (price >= 0.0),
             note      TEXT,
             digest    BLOB,
             UNIQUE (author_id, title)
         );
         CREATE TABLE tags (
             id    INTEGER PRIMARY KEY,
             label TEXT NOT NULL UNIQUE
         );
         CREATE TABLE book_tags (
             book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
             tag_id  INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
             PRIMARY KEY (book_id, tag_id)
         );
         CREATE INDEX idx_books_author ON books(author_id);
         CREATE INDEX idx_book_tags_tag ON book_tags(tag_id);",
    )
    .unwrap();
    println!("ddl done");

    // ---- ② authors：命名参数 prepared statement + last_insert_rowid ----
    let authors = [
        ("Ada", 1815i64),
        ("Grace", 1906),
        ("Edsger", 1930),
        ("Donald", 1938),
        ("Barbara", 1939),
        ("Kernighan", 1942),
        ("Solo", 1950), // 无书作者：聚合 LEFT JOIN 的 NULL 路径
    ];
    {
        let mut st = conn
            .prepare("INSERT INTO authors(name, born) VALUES (:name, :born)")
            .unwrap();
        for (name, born) in authors {
            let n = st
                .execute(named_params! { ":name": name, ":born": born })
                .unwrap();
            println!(
                "author {name} born={born} changes={n} rowid={}",
                conn.last_insert_rowid()
            );
        }
    }

    // ---- ③ tags：INSERT ... RETURNING ----
    for label in ["rust", "db", "ffi", "math"] {
        let id: i64 = conn
            .query_row(
                "INSERT INTO tags(label) VALUES (?1) RETURNING id",
                params![label],
                |r| r.get(0),
            )
            .unwrap();
        println!("tag {label} id={id}");
    }

    // ---- ④ 事务提交：books + book_tags 批量插入（定种数据，全类型） ----
    let words = [
        "alpha", "beta", "gamma", "delta", "omega", "sigma", "theta", "zeta",
    ];
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut expected: Vec<(i64, Vec<u8>)> = Vec::new();
    let tx = conn.transaction().unwrap();
    {
        let mut ins_book = tx
            .prepare(
                "INSERT INTO books(author_id, title, price, note, digest) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .unwrap();
        let mut ins_bt = tx
            .prepare("INSERT INTO book_tags(book_id, tag_id) VALUES (?1, ?2)")
            .unwrap();
        for i in 0..24u64 {
            let author_id = 1 + rng.below(6) as i64;
            let title = format!("vol-{i:02}-{}", words[rng.below(8) as usize]);
            let cents = 199 + rng.below(9800) as i64;
            let price = cents as f64 / 100.0;
            let note = if i % 3 == 0 {
                None
            } else {
                Some(format!("note-{i:02}-{}", words[rng.below(8) as usize]))
            };
            let digest = rng.bytes(12 + (i % 4) as usize * 4);
            let n = ins_book
                .execute(params![author_id, title, price, note, digest])
                .unwrap();
            let book_id = tx.last_insert_rowid();
            println!("book {i:02} changes={n} rowid={book_id}");
            expected.push((book_id, digest));
            let tag_a = 1 + rng.below(4) as i64;
            ins_bt.execute(params![book_id, tag_a]).unwrap();
            if rng.below(2) == 1 {
                let tag_b = 1 + rng.below(4) as i64;
                if tag_b != tag_a {
                    ins_bt.execute(params![book_id, tag_b]).unwrap();
                }
            }
        }
    }
    tx.commit().unwrap();
    println!("commit books = {}", count(&conn, "SELECT COUNT(*) FROM books"));

    // ---- ⑤ BLOB roundtrip：读回逐条比对 + 总 fnv 锚 ----
    let mut st = conn.prepare("SELECT id, digest FROM books ORDER BY id").unwrap();
    let back = st
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut all = Vec::new();
    let mut ok = back.len() == expected.len();
    for (got, want) in back.iter().zip(expected.iter()) {
        ok &= got == want;
        all.extend_from_slice(&got.1);
    }
    println!(
        "blob roundtrip ok={ok} rows={} bytes={} fnv={:016x}",
        back.len(),
        all.len(),
        fnv1a(&all)
    );
    drop(st); // Statement 带 Drop，借用延至作用域尾，须显式收尾

    // ---- ⑥ 事务回滚：内见 3 行，回滚后恢复原计数 ----
    let before = count(&conn, "SELECT COUNT(*) FROM books");
    let tx = conn.transaction().unwrap();
    for i in 0..3 {
        tx.execute(
            "INSERT INTO books(author_id, title, price) VALUES (1, ?1, 9.99)",
            params![format!("rb-{i}")],
        )
        .unwrap();
    }
    let inside = count(&tx, "SELECT COUNT(*) FROM books");
    tx.rollback().unwrap();
    let after = count(&conn, "SELECT COUNT(*) FROM books");
    println!("rollback before={before} inside={inside} after={after}");

    // ---- ⑦ changes：UPDATE / 空 DELETE / 真 DELETE ----
    let n = conn
        .execute("UPDATE books SET price = price * 2.0 WHERE author_id = 1", [])
        .unwrap();
    println!("update changes={n} conn.changes={}", conn.changes());
    let n = conn.execute("DELETE FROM books WHERE id < 0", []).unwrap();
    println!("delete-empty changes={n}");
    let n = conn
        .execute("DELETE FROM book_tags WHERE tag_id = 4", [])
        .unwrap();
    println!("delete-tag4 changes={n}");

    // ---- ⑧ JOIN（确定序）打印结果集 ----
    let mut st = conn
        .prepare(
            "SELECT a.name, b.title, b.price FROM books b \
             JOIN authors a ON a.id = b.author_id \
             ORDER BY a.name ASC, b.title ASC",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })
        .unwrap();
    let mut n = 0;
    for row in rows {
        let (a, t, p) = row.unwrap();
        println!("join {a} | {t} | price_bits={:016x}", p.to_bits());
        n += 1;
    }
    println!("join rows={n}");
    drop(st);

    // ---- ⑨ FK 级联：删 author 6 → books/book_tags 级联消失 ----
    let n = conn.execute("DELETE FROM authors WHERE id = 6", []).unwrap();
    println!(
        "cascade authors changes={n} books6={} links={}",
        count(&conn, "SELECT COUNT(*) FROM books WHERE author_id = 6"),
        count(&conn, "SELECT COUNT(*) FROM book_tags")
    );

    // ---- ⑩ 聚合：GROUP BY / HAVING / 整数计数确定序（含 LEFT JOIN NULL 路径） ----
    let mut st = conn
        .prepare(
            "SELECT a.name, COUNT(b.id), SUM(b.price), MIN(b.price), MAX(b.price) \
             FROM authors a LEFT JOIN books b ON b.author_id = a.id \
             GROUP BY a.id HAVING COUNT(b.id) >= 0 \
             ORDER BY COUNT(b.id) DESC, a.name ASC",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<f64>>(4)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (name, cnt, sum, min, max) = row.unwrap();
        let f = |v: Option<f64>| {
            v.map(|x| format!("{:016x}", x.to_bits()))
                .unwrap_or_else(|| "null".to_string())
        };
        println!("agg {name} n={cnt} sum={} min={} max={}", f(sum), f(min), f(max));
    }
    drop(st);
    let (total, noted, avg): (i64, i64, f64) = conn
        .query_row("SELECT COUNT(*), COUNT(note), AVG(price) FROM books", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    println!("books total={total} noted={noted} avg_bits={:016x}", avg.to_bits());

    // ---- ⑪ query_row / OptionalExtension / 按名列取 / 列元数据 ----
    let one: String = conn
        .query_row("SELECT title FROM books WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    let miss: Option<String> = conn
        .query_row("SELECT title FROM books WHERE id = 9999", [], |r| r.get(0))
        .optional()
        .unwrap();
    println!("row1={one:?} miss={miss:?}");
    let (t3, a3): (String, i64) = conn
        .query_row("SELECT author_id, title FROM books WHERE id = 3", [], |r| {
            Ok((r.get("title")?, r.get("author_id")?))
        })
        .unwrap();
    println!("by-name title={t3:?} author_id={a3}");
    println!(
        "books cols = {:?}",
        conn.prepare("SELECT * FROM books").unwrap().column_names()
    );

    // ---- ⑫ 错误路径 ×6：UNIQUE / FK / NOT NULL / CHECK / 坏表名 / 列型错 ----
    let (dup_a, dup_t): (i64, String) = conn
        .query_row("SELECT author_id, title FROM books WHERE id = 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    match conn.execute(
        "INSERT INTO books(author_id, title, price) VALUES (?1, ?2, 1.0)",
        params![dup_a, dup_t],
    ) {
        Ok(_) => println!("unique: unexpected ok"),
        Err(e) => println!("unique err: {e}"),
    }
    match conn.execute(
        "INSERT INTO books(author_id, title, price) VALUES (999, 'orphan', 1.0)",
        [],
    ) {
        Ok(_) => println!("fk: unexpected ok"),
        Err(e) => println!("fk err: {e}"),
    }
    match conn.execute("INSERT INTO books(author_id, title, price) VALUES (1, NULL, 1.0)", []) {
        Ok(_) => println!("notnull: unexpected ok"),
        Err(e) => println!("notnull err: {e}"),
    }
    match conn.execute("INSERT INTO authors(name, born) VALUES ('Bad', 5000)", []) {
        Ok(_) => println!("check: unexpected ok"),
        Err(e) => println!("check err: {e}"),
    }
    match conn.prepare("SELECT nope FROM missing_table") {
        Ok(_) => println!("prepare: unexpected ok"),
        Err(e) => println!("prepare err: {e}"),
    }
    let ty = conn.query_row("SELECT title FROM books WHERE id = 1", [], |r| {
        r.get::<_, i64>(0)
    });
    match ty {
        Ok(_) => println!("type: unexpected ok"),
        Err(e) => println!("type err: {e}"),
    }

    // ---- ⑬ 清理临时库文件 ----
    drop(conn);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    println!("cleanup exists={}", db.exists());
}
