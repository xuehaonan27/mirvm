#!/usr/bin/env mirvm
---
[dependencies]
# wrapper 0.16's default features drag in sled/parquet/redis and the whole heavy storage
# cluster; only pure-Rust memory storage is needed (gluesql-core 0.16.3: sqlparser 0.46
# + a pure-Rust executor, rows in BTreeMap<Key, DataRow>, scan order = key order).
gluesql = { version = "0.16", default-features = false, features = ["gluesql_memory_storage"] }
futures = "0.3"
# gluesql-core 0.16.3's data/literal.rs infers the type of `*r.as_ref() == 0.into()` from
# the single satisfying "PartialEq implementation" in scope; bigdecimal 0.4.6+ added a
# batch of PartialEq<int> impls for BigDecimal, making that inference ambiguous (E0283,
# an upstream semver hole). gluesql 0.16.0's Cargo.lock already pins 0.4.5.
bigdecimal = "=0.4.5"
---
// gluesql 0.16 (memory storage) SQL engine differential.
// Coverage:
//   1) DDL: users (INT PRIMARY KEY / TEXT NOT NULL / UNIQUE / DEFAULT / BOOLEAN /
//      FLOAT64 / DECIMAL / DATE / UUID / nullable TEXT), orders (INT PK / INT NOT NULL /
//      DECIMAL / TIMESTAMP / BOOLEAN), typezoo (INT8..INT128 / UINT8..UINT128 /
//      FLOAT32 / TIMESTAMP / TIME / INTERVAL / BYTEA / INET, min/max/+-0.0 edges);
//      SHOW COLUMNS reads the column definitions back.
//   2) Seeded bulk insert: an inline xorshift64* generates 40 users (including a batch
//      that omits age so DEFAULT applies) + 80 orders (user_id in 1..=45, leaving room
//      for INNER/LEFT JOIN differences) + 12 fully-typed boundary rows.
//   3) SELECT: WHERE+ORDER BY+LIMIT; expression projections (UPPER/CONCAT/LEFT/LPAD/
//      ROUND/IFNULL); global aggregates COUNT/SUM/AVG/VARIANCE/STDEV/MIN/MAX;
//      GROUP BY+HAVING+ORDER BY+LIMIT; DISTINCT; IN subquery.
//   4) JOIN: INNER JOIN (dangling user_id rows filtered out) + LEFT JOIN with GROUP counts.
//   5) UPDATE (expression SET + predicate) / DELETE (compound predicate) with aggregate
//      checks before and after.
//   6) Transactions: START TRANSACTION -> MemoryStorage reports unsupported (a deterministic
//      error string); ROLLBACK/COMMIT still return Ok outside a transaction.
//   7) Error paths: syntax error (sqlparser), type error ('yes' -> BOOLEAN), missing
//      table, UNIQUE conflict, PK conflict, NOT NULL violation, and I8 overflow.
//   8) Re-reading a table after DROP TABLE -> table not found.
// Determinism: every SELECT has a total-order ORDER BY; F32/F64 print to_bits(); aggregate
// summation order = BTreeMap key order; no HashMap iteration order escapes (ORDER BY closes it).
use futures::executor::block_on;
use gluesql::core::data::Value;
use gluesql::core::executor::Payload;
use gluesql::prelude::{Glue, MemoryStorage};

// ---- seeded RNG (xorshift64*, same as c_zip_arch) ----
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

// ---- deterministic Value rendering: floats as bits, NULL/empty string marked explicitly ----
fn fmt_value(v: &Value) -> String {
    match v {
        Value::Bool(b) => format!("{b}"),
        Value::I8(x) => format!("{x}"),
        Value::I16(x) => format!("{x}"),
        Value::I32(x) => format!("{x}"),
        Value::I64(x) => format!("{x}"),
        Value::I128(x) => format!("{x}"),
        Value::U8(x) => format!("{x}"),
        Value::U16(x) => format!("{x}"),
        Value::U32(x) => format!("{x}"),
        Value::U64(x) => format!("{x}"),
        Value::U128(x) => format!("{x}"),
        Value::F32(x) => format!("f32:0x{:08x}", x.to_bits()),
        Value::F64(x) => format!("f64:0x{:016x}", x.to_bits()),
        Value::Decimal(d) => format!("dec:{d}"),
        Value::Str(s) => format!("{s:?}"),
        Value::Bytea(b) => format!("bytea:{}", hex(b)),
        Value::Inet(ip) => format!("inet:{ip}"),
        Value::Date(d) => format!("date:{d}"),
        Value::Timestamp(t) => format!("ts:{t}"),
        Value::Time(t) => format!("time:{t}"),
        Value::Interval(iv) => format!("iv:{iv:?}"),
        Value::Uuid(u) => format!("uuid:{u:032x}"),
        Value::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn show_payload(p: &Payload) {
    match p {
        Payload::Create => println!("ok CREATE"),
        Payload::Insert(n) => println!("ok INSERT {n}"),
        Payload::Update(n) => println!("ok UPDATE {n}"),
        Payload::Delete(n) => println!("ok DELETE {n}"),
        Payload::DropTable(n) => println!("ok DROPTABLE {n}"),
        Payload::AlterTable => println!("ok ALTERTABLE"),
        Payload::CreateIndex => println!("ok CREATEINDEX"),
        Payload::DropIndex => println!("ok DROPINDEX"),
        Payload::DropFunction => println!("ok DROPFUNCTION"),
        Payload::StartTransaction => println!("ok START-TRANSACTION"),
        Payload::Commit => println!("ok COMMIT"),
        Payload::Rollback => println!("ok ROLLBACK"),
        Payload::ShowVariable(v) => println!("ok SHOWVAR {v:?}"),
        Payload::ShowColumns(cols) => {
            println!("ok SHOWCOLUMNS n={}", cols.len());
            for (name, ty) in cols {
                println!("  col {name}: {ty:?}");
            }
        }
        Payload::Select { labels, rows } => {
            println!("ok SELECT labels=[{}] rows={}", labels.join(","), rows.len());
            for r in rows {
                let cells: Vec<String> = r.iter().map(fmt_value).collect();
                println!("  | {}", cells.join(" | "));
            }
        }
        Payload::SelectMap(rows) => println!("ok SELECTMAP rows={}", rows.len()),
    }
}

fn run(g: &mut Glue<MemoryStorage>, label: &str, sql: &str) {
    println!("== {label}");
    match block_on(g.execute(sql)) {
        Ok(ps) => {
            for p in &ps {
                show_payload(p);
            }
        }
        Err(e) => println!("err {e}"),
    }
}

const DATES: [&str; 8] = [
    "2023-01-15",
    "2023-02-28",
    "2023-03-05",
    "2023-05-21",
    "2023-07-04",
    "2023-09-30",
    "2023-11-11",
    "2024-01-01",
];

const TSS: [&str; 6] = [
    "2023-01-15 08:30:00",
    "2023-03-05 12:00:30",
    "2023-06-15 23:59:59",
    "2023-09-30 00:00:01",
    "2023-12-24 15:45:10",
    "2024-02-29 10:20:30",
];

fn main() {
    let mut g = Glue::new(MemoryStorage::default());

    // ===== ① DDL =====
    run(
        &mut g,
        "ddl users",
        "CREATE TABLE users (
            id INT PRIMARY KEY,
            name TEXT NOT NULL,
            email TEXT UNIQUE,
            age INT DEFAULT 18,
            active BOOLEAN NOT NULL,
            score FLOAT64,
            balance DECIMAL,
            joined DATE,
            uid UUID,
            note TEXT NULL
        )",
    );
    run(
        &mut g,
        "ddl orders",
        "CREATE TABLE orders (
            id INT PRIMARY KEY,
            user_id INT NOT NULL,
            qty INT NOT NULL,
            amount DECIMAL,
            placed TIMESTAMP,
            paid BOOLEAN
        )",
    );
    run(
        &mut g,
        "ddl typezoo",
        "CREATE TABLE typezoo (
            id INT PRIMARY KEY,
            i8v INT8,
            i16v INT16,
            i32v INT32,
            i128v INT128,
            u8v UINT8,
            u64v UINT64,
            u128v UINT128,
            f32v FLOAT32,
            f64v FLOAT64,
            ts TIMESTAMP,
            tm TIME,
            iv INTERVAL,
            blob BYTEA,
            ip INET,
            price DECIMAL,
            flag BOOLEAN
        )",
    );
    run(&mut g, "show columns users", "SHOW COLUMNS FROM users");

    // ===== ② seeded bulk insert =====
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // users: rows with i%5==0 omit the age column so DEFAULT 18 applies
    let mut full_cols: Vec<String> = Vec::new();
    let mut default_age: Vec<String> = Vec::new();
    for i in 0..40u64 {
        let id = i + 1;
        let name = format!("user{i:02}");
        let email = format!("u{i}@example.com");
        let age = 15 + rng.below(56); // 15..=70
        let active = i % 3 != 0;
        let cents = rng.below(100_000);
        let score = format!("{}.{:02}", cents / 100, cents % 100);
        let bcents = rng.below(1_000_000);
        let balance = format!("{}.{:02}", bcents / 100, bcents % 100);
        let joined = DATES[(i as usize) % DATES.len()];
        let uid = format!("{:08x}-1234-4abc-8def-{:012x}", i * 7 + 1, i * 97 + 1);
        let note = if i % 4 == 0 {
            "NULL".to_string()
        } else {
            format!("'note-{i}'")
        };
        if i % 5 == 0 {
            default_age.push(format!(
                "({id}, '{name}', '{email}', {active}, {score}, {balance}, DATE '{joined}', UUID '{uid}', {note})"
            ));
        } else {
            full_cols.push(format!(
                "({id}, '{name}', '{email}', {age}, {active}, {score}, {balance}, DATE '{joined}', UUID '{uid}', {note})"
            ));
        }
    }
    for (k, chunk) in full_cols.chunks(12).enumerate() {
        run(
            &mut g,
            &format!("insert users full[{k}]"),
            &format!(
                "INSERT INTO users (id, name, email, age, active, score, balance, joined, uid, note) VALUES {}",
                chunk.join(", ")
            ),
        );
    }
    run(
        &mut g,
        "insert users default-age",
        &format!(
            "INSERT INTO users (id, name, email, active, score, balance, joined, uid, note) VALUES {}",
            default_age.join(", ")
        ),
    );

    // orders: 80 rows in one statement; user_id in 1..=45 (41..=45 dangling)
    let mut order_tuples: Vec<String> = Vec::new();
    for i in 0..80u64 {
        let id = i + 1;
        let user_id = 1 + rng.below(45);
        let qty = 1 + rng.below(9);
        let acents = rng.below(500_000);
        let amount = format!("{}.{:02}", acents / 100, acents % 100);
        let placed = TSS[(i as usize) % TSS.len()];
        let paid = rng.below(4) != 0;
        order_tuples.push(format!(
            "({id}, {user_id}, {qty}, {amount}, TIMESTAMP '{placed}', {paid})"
        ));
    }
    run(
        &mut g,
        "insert orders",
        &format!(
            "INSERT INTO orders (id, user_id, qty, amount, placed, paid) VALUES {}",
            order_tuples.join(", ")
        ),
    );

    // typezoo: all-type boundaries (min/max, +-0.0, empty bytea, v4/v6 inet, multi-year interval).
    // sqlparser 0.46 does not accept the INET 'x' typed-string syntax (custom types only take the
    // DATE/TIME/... standard prefixes), so INET always goes through CAST('x' AS INET).
    run(
        &mut g,
        "insert typezoo",
        "INSERT INTO typezoo VALUES
        (1, -128, -32768, -2147483648, -170141183460469231731687303715884105728,
           0, 0, 0, -0.0, 0.0,
           TIMESTAMP '1970-01-01 00:00:00', TIME '00:00:00', INTERVAL '3' DAY,
           X'00FF10deadbeef', CAST('127.0.0.1' AS INET), 0.00, TRUE),
        (2, 127, 32767, 2147483647, 170141183460469231731687303715884105727,
           255, 18446744073709551615, 340282366920938463463374607431768211455,
           3.5, -2.75,
           TIMESTAMP '9999-12-31 23:59:59', TIME '23:59:59.999999', INTERVAL '1-2' YEAR TO MONTH,
           X'', CAST('::1' AS INET), 99999999999.99999, FALSE),
        (3, 0, -1, 1, 42, 7, 12345678901234567890, 98765432109876543210,
           1.25, 1.0e3,
           TIMESTAMP '2024-02-29 10:20:30', TIME '12:34:56', INTERVAL '45' MINUTE,
           X'ff', CAST('192.168.0.1' AS INET), -0.01, TRUE),
        (4, -1, 0, -1000000, -999999999999999999, 100, 42, 1,
           0.5, -0.125,
           TIMESTAMP '2000-02-29 00:00:00', TIME '06:00:00', INTERVAL '2' YEAR,
           X'0123456789abcdef', CAST('2001:db8::ff00:42' AS INET), 3.14, FALSE),
        (5, 12, 1000, 999983, 340282366920938463, 200, 999, 20240101,
           -3.75, 6.02214076,
           TIMESTAMP '2010-10-10 10:10:10', TIME '10:10:10', INTERVAL '3-6' YEAR TO MONTH,
           X'aabbcc', CAST('10.0.0.1' AS INET), 100.50, TRUE),
        (6, -100, -1000, -999983, -340282366920938463, 55, 18446744073709551614, 0,
           2.5, 2.0e-3,
           TIMESTAMP '1970-01-02 00:00:01', TIME '23:59:59', INTERVAL '90' SECOND,
           X'deadbeefcafe', CAST('8.8.8.8' AS INET), 0.10, FALSE)",
    );

    // ===== ③ SELECT =====
    run(
        &mut g,
        "q1 where+order+limit",
        "SELECT id, name, age, active, score, balance, joined, uid, note
         FROM users
         WHERE (age >= 30 AND active = TRUE) OR note IS NULL
         ORDER BY age DESC, id ASC
         LIMIT 9",
    );
    run(
        &mut g,
        "q2 expr projection",
        "SELECT id, UPPER(name) AS uname, CONCAT(LEFT(name, 4), '#', RIGHT(name, 2)) AS tag,
                age * 2 + 1 AS age2, ROUND(score / 7) AS adj, IFNULL(note, '-') AS n2,
                LPAD(name, 9, '*') AS pad
         FROM users
         WHERE id % 4 = 0
         ORDER BY id",
    );
    run(
        &mut g,
        "q3 global aggregates",
        "SELECT COUNT(*) AS c, COUNT(note) AS cnn, SUM(age) AS sa, AVG(score) AS avs,
                MIN(joined) AS mj, MAX(joined) AS xj, VARIANCE(age) AS va, STDEV(age) AS sd
         FROM users",
    );
    run(
        &mut g,
        "q4 group+having+order+limit",
        "SELECT age, active, COUNT(*) AS cnt, SUM(score) AS ss
         FROM users
         GROUP BY age, active
         HAVING COUNT(*) >= 2
         ORDER BY age, active
         LIMIT 12",
    );
    run(
        &mut g,
        "q5 distinct unsupported",
        "SELECT DISTINCT active FROM users ORDER BY active",
    );
    run(
        &mut g,
        "q5b group-by as distinct",
        "SELECT age FROM users GROUP BY age ORDER BY age LIMIT 6",
    );
    run(
        &mut g,
        "q6 inner join",
        "SELECT u.name, u.age, o.id AS oid, o.qty, o.amount, o.paid
         FROM users u JOIN orders o ON u.id = o.user_id
         WHERE o.paid = TRUE AND o.qty >= 3
         ORDER BY o.id
         LIMIT 15",
    );
    run(
        &mut g,
        "q7 left join + group count",
        "SELECT u.name, COUNT(o.id) AS cnt
         FROM users u LEFT JOIN orders o ON u.id = o.user_id
         GROUP BY u.name
         ORDER BY cnt DESC, u.name
         LIMIT 10",
    );
    run(
        &mut g,
        "q8 in subquery",
        "SELECT id, name FROM users
         WHERE id IN (SELECT user_id FROM orders WHERE qty >= 7 AND paid = FALSE)
         ORDER BY id",
    );
    run(
        &mut g,
        "q9 typezoo dump",
        "SELECT * FROM typezoo ORDER BY id",
   );
    run(
        &mut g,
        "q9b arithmetic cast",
        "SELECT id, f64v + 1.5, i32v / 2, i128v / 2 FROM typezoo ORDER BY id DESC LIMIT 4",
    );

    // ===== ④ UPDATE / DELETE (aggregate checks before and after) =====
    run(
        &mut g,
        "pre-update sums",
        "SELECT COUNT(*) AS c, SUM(age) AS s, AVG(score) AS a FROM users",
    );
    run(
        &mut g,
        "update users",
        "UPDATE users SET age = age + 1, score = score * 2, note = 'bumped'
         WHERE joined < DATE '2023-06-01'",
    );
    run(
        &mut g,
        "post-update sums",
        "SELECT COUNT(*) AS c, SUM(age) AS s, AVG(score) AS a FROM users",
    );
    run(
        &mut g,
        "pre-delete counts",
        "SELECT paid, COUNT(*) AS c FROM orders GROUP BY paid ORDER BY paid",
    );
    run(
        &mut g,
        "delete orders",
        "DELETE FROM orders WHERE paid = FALSE AND qty < 3",
    );
    run(
        &mut g,
        "post-delete counts",
        "SELECT paid, COUNT(*) AS c FROM orders GROUP BY paid ORDER BY paid",
    );

    // ===== ⑤ transactions (MemoryStorage does not support them -> a deterministic error) =====
    run(&mut g, "tx start", "START TRANSACTION");
    run(&mut g, "tx rollback", "ROLLBACK");
    run(&mut g, "tx commit", "COMMIT");

    // ===== ⑥ error paths =====
    run(&mut g, "err syntax", "SELEC id FROM users");
    run(
        &mut g,
        "err type",
        "INSERT INTO users (id, name, active) VALUES (901, 'badtype', 'yes')",
    );
    run(&mut g, "err no table", "SELECT id FROM ghost_table ORDER BY id");
    run(
        &mut g,
        "err unique",
        "INSERT INTO users (id, name, email, active) VALUES (950, 'dupmail', 'u3@example.com', TRUE)",
    );
    run(
        &mut g,
        "err pk",
        "INSERT INTO users (id, name, email, active) VALUES (3, 'duppk', 'fresh@example.com', TRUE)",
    );
    run(
        &mut g,
        "err notnull",
        "INSERT INTO users (id, name, active) VALUES (951, NULL, TRUE)",
    );
    run(
        &mut g,
        "err overflow",
        "SELECT i8v + 1 FROM typezoo WHERE id = 2",
    );

    // ===== ⑦ DROP + re-read =====
    run(&mut g, "drop typezoo", "DROP TABLE typezoo");
    run(
        &mut g,
        "select after drop",
        "SELECT id FROM typezoo ORDER BY id",
    );

    // closing counts (DROP does not affect users/orders)
    run(
        &mut g,
        "final counts",
        "SELECT COUNT(*) AS c FROM users",
    );
}
