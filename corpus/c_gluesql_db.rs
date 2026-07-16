#!/usr/bin/env mirvm
---
[dependencies]
# wrapper 0.16 的 default features 拖 sled/parquet/redis/… 整簇重存储；只需要
# 纯 Rust memory storage（gluesql-core 0.16.3：sqlparser 0.46 + 纯 Rust 执行层，
# 行存 BTreeMap<Key, DataRow>，扫描序 = 键序，天然确定）。
gluesql = { version = "0.16", default-features = false, features = ["gluesql_memory_storage"] }
futures = "0.3"
# gluesql-core 0.16.3 的 data/literal.rs 里 `*r.as_ref() == 0.into()` 靠"当时只有
# 一个满足的 PartialEq 实现"做类型推断；bigdecimal 0.4.6+ 给 BigDecimal 加了成批
# PartialEq<int> impl 后推断变歧义（E0283，上游 semver 破洞，c_jieba_cut 同款）。
# 上游 gluesql 0.16.0 自带 Cargo.lock 钉的就是 0.4.5——对齐钉死。
bigdecimal = "=0.4.5"
---
// gluesql 0.16（memory storage）SQL 引擎差分。
// 覆盖：
//  ① DDL：users（INT PRIMARY KEY / TEXT NOT NULL / UNIQUE / DEFAULT / BOOLEAN /
//     FLOAT64 / DECIMAL / DATE / UUID / 可空 TEXT）、orders（INT PK / INT NOT NULL /
//     DECIMAL / TIMESTAMP / BOOLEAN）、typezoo（INT8~INT128 / UINT8~UINT128 /
//     FLOAT32 / TIMESTAMP / TIME / INTERVAL / BYTEA / INET，min/max/±0.0 边界）；
//     SHOW COLUMNS 回读列定义。
//  ② 定种批量插入：内联 xorshift64* 生成 40 用户（含省略 age 走 DEFAULT 的批）
//     + 80 订单（故意 1..=45 的 user_id，留出 INNER/LEFT JOIN 差异）+ 12 行
//     全类型边界。
//  ③ SELECT：WHERE+ORDER BY+LIMIT；表达式投影（UPPER/CONCAT/LEFT/LPAD/ROUND/
//     IFNULL 纯标量函数）；全局聚合 COUNT/SUM/AVG/VARIANCE/STDEV/MIN/MAX；
//     GROUP BY+HAVING+ORDER BY+LIMIT；DISTINCT；IN 子查询。
//  ④ JOIN：INNER JOIN（悬空 user_id 行被滤）+ LEFT JOIN+GROUP 计数。
//  ⑤ UPDATE（表达式 SET+谓词）/DELETE（复合谓词），改前改后聚合校验。
//  ⑥ 事务：START TRANSACTION → MemoryStorage 报不支持（确定性错误串）；
//     ROLLBACK/COMMIT 在无事务上下文下仍 Ok（原样打印引擎真实行为）。
//  ⑦ 错误路径：语法错（sqlparser）、类型错（'yes'→BOOLEAN）、表不存在、
//     UNIQUE 冲突、PK 冲突、NOT NULL 违约、I8 算术溢出。
//  ⑧ DROP TABLE 后复读同表 → 表不存在。
// 确定性：所有 SELECT 带总序 ORDER BY；F32/F64 打印 to_bits()；聚合求和序 =
// BTreeMap 键序（两侧相同）；无时间/随机源/HashMap 迭代序外泄（GROUP BY 内部
// 哈希一律经 ORDER BY 收口）。
use futures::executor::block_on;
use gluesql::core::data::Value;
use gluesql::core::executor::Payload;
use gluesql::prelude::{Glue, MemoryStorage};

// ---- 定种 RNG（xorshift64*，与 c_zip_arch 同款）----
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

// ---- Value 确定性文本化：浮点只打 bits，NULL/空串显式标记 ----
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

    // ===== ② 定种批量插入 =====
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    // users：i%5==0 的行省略 age 列走 DEFAULT 18
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

    // orders：80 行单语句；user_id ∈ 1..=45（41..=45 悬空）
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

    // typezoo：全类型边界（min/max、±0.0、空 bytea、v4/v6 inet、跨年 interval）。
    // INET 'x' typed-string 语法 sqlparser 0.46 不吃（自定义类型只认 DATE/TIME/…
    // 标准前缀），INET 一律走 CAST('x' AS INET)。
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

    // ===== ④ UPDATE / DELETE（前后聚合校验）=====
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

    // ===== ⑤ 事务（MemoryStorage 不支持 → 确定性错误）=====
    run(&mut g, "tx start", "START TRANSACTION");
    run(&mut g, "tx rollback", "ROLLBACK");
    run(&mut g, "tx commit", "COMMIT");

    // ===== ⑥ 错误路径 =====
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

    // ===== ⑦ DROP + 复读 =====
    run(&mut g, "drop typezoo", "DROP TABLE typezoo");
    run(
        &mut g,
        "select after drop",
        "SELECT id FROM typezoo ORDER BY id",
    );

    // 收尾计数（DROP 不影响 users/orders）
    run(
        &mut g,
        "final counts",
        "SELECT COUNT(*) AS c FROM users",
    );
}
