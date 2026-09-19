#!/usr/bin/env mirvm
---
[dependencies]
rusqlite = { version = "0.32", features = ["bundled"] }
---
// rusqlite 0.32 with bundled sqlite3 (libsqlite3-sys 0.30.1 compiles SQLite's C
// sources into a static archive).
// FRONTIER: mirvm cannot load it while native is fully green. The loader fails
// during lowering with exit 101, reporting that the static archive
// `libsqlite3.a` cannot be converted into a shared library because it needs ELF
// PIC and all its dependencies inside the archive; the linker then cannot
// resolve the symbol `log`. The cause: the .a -> .so conversion links each
// archive on its own with `cc -shared -Wl,-z,defs --whole-archive`, so every
// symbol must close inside the archive plus libc/libgcc. But libsqlite3-sys's
// build.rs hardcodes -DSQLITE_ENABLE_FTS5 with no feature to disable it, and
// fts5Bm25GetData references `log` from libm, the only symbol outside that
// closure. Native is unaffected because rustc's linux-gnu link brings -lm, and
// mirvm's own process already has libm.so.6 in DT_NEEDED, so without -z defs
// dlopen would resolve it globally.
// There is no workaround: those versions expose no SQLITE_OMIT_FTS5 feature, the
// versions are pinned, CFLAGS or env variables are not part of a single-file
// driver, and pre-filling the native-archives cache would tamper with state. So
// the driver stays a probe that turns green once mirvm lets an archive use libm.
// Coverage: open a file database; PRAGMA; DDL for four tables (PK, FK, UNIQUE,
// CHECK, two secondary indexes); batched inserts through prepared
// statements with named and positional parameters (all of INTEGER / REAL / TEXT /
// BLOB / NULL); one committed and one rolled-back transaction;
// INSERT...RETURNING; JOIN with aggregates (GROUP BY / HAVING / fixed ORDER BY);
// last_insert_rowid; changes; query_row with OptionalExtension and by-name column
// access; column metadata; six error paths (UNIQUE, FK, NOT NULL, CHECK, a bad
// table name and a column type error); FK cascade delete; BLOB fnv anchoring with
// a roundtrip boolean; and cleanup of the temporary database files.
// Deterministic: a seeded xorshift64* generates the data; REAL values always
// print through to_bits(); no path, time, address or HashMap order is printed;
use rusqlite::{named_params, params, Connection, OptionalExtension};

/// Seeded xorshift64* (the same sequence on native and mirvm).
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

/// Inline FNV-1a anchoring binary content (raw bytes are never printed).
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
    // ---- (0) fixed-name temp database (cleared before and after each run) ----
    let db = std::env::temp_dir().join("mirvm_corpus_rusqlite_db.sqlite3");
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }

    println!("sqlite version {}", rusqlite::version());
    let mut conn = Connection::open(&db).unwrap();

    // ---- (1) PRAGMA + DDL: four tables and two indexes ----
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

    // ---- (2) authors: named-parameter prepared statement + last_insert_rowid ----
    let authors = [
        ("Ada", 1815i64),
        ("Grace", 1906),
        ("Edsger", 1930),
        ("Donald", 1938),
        ("Barbara", 1939),
        ("Kernighan", 1942),
        ("Solo", 1950), // an author with no books: the LEFT JOIN NULL path
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

    // ---- (3) tags: INSERT ... RETURNING ----
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

    // ---- (4) committed transaction: batched books + book_tags (seeded, all types) ----
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

    // ---- (5) BLOB roundtrip: compare row by row and anchor the total fnv ----
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
    drop(st); // Statement implements Drop; the borrow lasts to scope end, so close it

    // ---- (6) rolled-back transaction: 3 rows visible inside, original count after ----
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

    // ---- (7) changes: UPDATE / empty DELETE / real DELETE ----
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

    // ---- (8) JOIN (fixed order) printing the result set ----
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

    // ---- (9) FK cascade: deleting author 6 removes its books/book_tags ----
    let n = conn.execute("DELETE FROM authors WHERE id = 6", []).unwrap();
    println!(
        "cascade authors changes={n} books6={} links={}",
        count(&conn, "SELECT COUNT(*) FROM books WHERE author_id = 6"),
        count(&conn, "SELECT COUNT(*) FROM book_tags")
    );

    // ---- (10) aggregates: GROUP BY / HAVING / fixed integer-count order ----
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

    // ---- (11) query_row / OptionalExtension / by-name access / column metadata ----
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

    // ---- (12) six error paths: UNIQUE / FK / NOT NULL / CHECK / bad table / bad type ----
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

    // ---- (13) clean up the temporary database files ----
    drop(conn);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        let _ = std::fs::remove_file(p);
    }
    println!("cleanup exists={}", db.exists());
}
