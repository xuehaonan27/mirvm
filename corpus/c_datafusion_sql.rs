#!/usr/bin/env mirvm
---
[dependencies]
datafusion = { version = "=54.0.0", default-features = false, features = ["sql"] }
# Upstream semver hole: datafusion 54.0.0 uses ^54.0.0 for its internal crates,
# but physical-plan 54.1.0 changed RecursiveQueryExec::try_new from 4 args to 5,
# so the 54.0.0 lib fails to compile (E0061; cargo's own fresh resolution picks
# 54.1.0 too -- not a mirvm fork). The internal family chains itself with ^54.x
# (catalog 54.1.0 -> physical-plan ^54.1.0), so pinning one hits the next: all 28
# internal crates are pinned to =54.0.0 as a self-consistent set.
datafusion-catalog = "=54.0.0"
datafusion-catalog-listing = "=54.0.0"
datafusion-common = "=54.0.0"
datafusion-common-runtime = "=54.0.0"
datafusion-datasource = "=54.0.0"
datafusion-datasource-arrow = "=54.0.0"
datafusion-datasource-csv = "=54.0.0"
datafusion-datasource-json = "=54.0.0"
datafusion-doc = "=54.0.0"
datafusion-execution = "=54.0.0"
datafusion-expr = "=54.0.0"
datafusion-expr-common = "=54.0.0"
datafusion-functions = "=54.0.0"
datafusion-functions-aggregate = "=54.0.0"
datafusion-functions-aggregate-common = "=54.0.0"
datafusion-functions-nested = "=54.0.0"
datafusion-functions-table = "=54.0.0"
datafusion-functions-window = "=54.0.0"
datafusion-functions-window-common = "=54.0.0"
datafusion-macros = "=54.0.0"
datafusion-optimizer = "=54.0.0"
datafusion-physical-expr = "=54.0.0"
datafusion-physical-expr-adapter = "=54.0.0"
datafusion-physical-expr-common = "=54.0.0"
datafusion-physical-optimizer = "=54.0.0"
datafusion-physical-plan = "=54.0.0"
datafusion-pruning = "=54.0.0"
datafusion-session = "=54.0.0"
datafusion-sql = "=54.0.0"
tokio = { version = "1", default-features = false, features = ["rt"] }
---
// c_datafusion_sql -- Apache DataFusion on the current stable line: SessionContext
// + three in-memory RecordBatch tables + a full SQL query sequence, three-way diff.
//
// mirvm runs all three dimensions: the build completes (including the zstd-sys C
// family), SessionContext initializes, all three tables register, SQL parses, the
// logical plans build, and 95 stdout lines (Q1-Q7 plus the Q2/Q5 plan fingerprints)
// match native (empty stderr, exit 0).
//
// Dynamic-dispatch path exercised: TableScan physicalization cannot skip the
// trait-object upcast that `downcast_ref` performs; the chain is
//   datafusion-54.0.0/src/physical_planner.rs:666  TableScan physicalization calls
//     source_as_provider(source)
//   -> datafusion-catalog-54.0.0/src/default_table_source.rs:94
//     source.as_ref().downcast_ref::<DefaultTableSource>()
//   -> datafusion-expr-54.0.0/src/table_source.rs:138  (self as &dyn Any)
//     -- the dyn upcast coercion itself, lowered by the PC::Unsize arm of
//   -> src/lower/func/cast.rs  (target vtable = *(source vtable +
//     supertrait_vtable_slot x 8), the same criterion as cg_ssa unsized_info).
//
// Minimal repro (no cargo-script dependency, measured in /tmp):
//   use std::any::Any;
//   trait Table: Any { fn rows(&self) -> i64; }
//   struct Mem { n: i64 }
//   impl Table for Mem { fn rows(&self) -> i64 { self.n } }
//   fn inspect(t: &dyn Table) -> String {
//       let any: &dyn Any = t;                    // <- the upcast coercion
//       match any.downcast_ref::<Mem>() {
//           Some(m) => format!("downcast={}", m.rows()),
//           None => "downcast=none".to_string(),
//       }
//   }
//   fn main() {
//       let t: &dyn Table = &Mem { n: 7 };
//       println!("up={} {}", t.rows(), inspect(t));
//   }
//   native and mirvm both print up=7 downcast=7 and exit 0.
// The chase covers both fat-pointer forms this driver reaches: `&dyn` and `Arc<dyn>`
// are both scalar pairs (an Arc's metadata lives in the Arc pointer, not the allocation).
// A shape the arm cannot derive a vtable for is rejected with an English diagnostic
// rather than lowered to a wrong vtable, e.g.
//   "dyn upcast target is not a pair (...)",
//   "dyn upcast source is not a pair (...; nested tail-pair wrapper not handled)",
//   "dyn upcast source is not a fat pointer (...; constant fat-pointer upcast not handled)",
//   "dyn upcast source meta is not a slot (...; non-constant form not handled)".
// No red_pattern applies: the shape this driver hits takes the chase path, so stderr
// stays empty and exit is 0; a rejected shape would carry one of those messages and
// exit 70.
// The path is unavoidable: collect/create_physical_plan must go through that downcast,
// so the SessionContext+SQL+collect mainline cannot skip physicalization; the cast is a
// real MIR `PointerCoercion(Unsize)`, not something a driver-side switch can avoid.
// Build plus run takes about 6m14s (well within the 400 s registered timeout); the JIT
// dimension (MIRVM_JIT_THRESHOLD=1, warm cache) takes about 14 s and reproduces the
// interpreted stdout, stderr and exit code exactly.
//
//
// Coverage:
//   1) Tables: emps (8 rows: id/name/dept/salary/city_id, salary with one NULL,
//      city_id with one NULL key and one dangling reference 9) / depts (4 rows,
//      research is right-side-only) / cities (4 rows, guangzhou left-side-only)
//      -- the full join-key null spectrum.
//   2) Q1 projection + arithmetic + WHERE filter (salary>=7000, NULL row filtered).
//   3) Q2 GROUP BY with five aggregates sum/count/min/max/avg: the hr group has a
//      NULL salary, COUNT(*) counts the row while SUM/AVG skip NULL -- anchoring
//      null aggregate semantics.
//   4) Q3 two-table INNER JOIN: right-only research is not visible (result depts
//      are eng/hr/ops only).
//   5) Q4a/Q4b two-table LEFT JOIN both ways: emps LEFT cities yields a NULL city
//      (NULL key id=5, dangling id=7); cities LEFT emps yields NULL name/id.
//   6) Q5 window functions: ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary
//      DESC, id ASC) (total-order tiebreak) and RANK() OVER (PARTITION BY dept
//      ORDER BY salary DESC) (the ops 6200.25 tie shares a rank).
//   7) Q6 scalar subquery: salary > (SELECT AVG(salary) FROM emps).
//   8) Q7 ORDER BY salary DESC NULLS LAST, id ASC LIMIT 3.
//   9) Plan fingerprints: Q2 (aggregate) and Q5 (window) each print the logical
//      plan (display_indent) and physical plan (displayable().indent(false)) in
//      full indented text plus len and an FNV-1a anchor.
//  10) Result printing: after DataFrame::collect, batches/rows/cells are printed
//      by hand -- f64 as to_bits() hex, Utf8 with escaping and quotes, NULL as
//      "NULL", and a schema line with column names + DataType Debug.
//
// Determinism: SessionConfig::with_target_partitions(1) removes parallel-partition
//   ordering (single-partition MemTable scans/aggregates, so accumulation order
//   equals insertion order); every query has an explicit ORDER BY with a total-order
//   key (a unique id breaks ties, and rank ties share a value); row_number's ORDER
//   BY includes an id tiebreak; tokio's current_thread runtime; stderr is empty.
//
// Version pins:
//   datafusion =54.0.0 (pinned exactly; folds in arrow ^58.3.0 / sqlparser 0.62.0 / tokio ^1.52).
//   default-features=false with only "sql" (a legitimate official feature cut):
//   the spec covers core SQL semantics only, while the default surface's parquet/
//   compression/regex/unicode/crypto/datetime/nested_expressions are unrelated and
//   inflate the build budget; window and aggregate functions are non-optional
//   dependencies in v54 (datafusion-functions-window / -aggregate), so they stay.
//   recursive_protection is off with the default surface; these queries are shallow.
//   tokio = 1 (default-features=false + rt; resolves 1.53.0).
//
use std::sync::Arc;

use datafusion::arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::dataframe::DataFrame;
use datafusion::error::Result;
use datafusion::execution::config::SessionConfig;
use datafusion::execution::context::SessionContext;
use datafusion::physical_plan::displayable;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn cell(arr: &ArrayRef, row: usize) -> String {
    if arr.is_null(row) {
        return "NULL".to_string();
    }
    match arr.data_type() {
        DataType::Int64 => arr
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::UInt64 => arr
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .value(row)
            .to_string(),
        DataType::Float64 => format!(
            "{:#018x}",
            arr.as_any()
                .downcast_ref::<Float64Array>()
                .unwrap()
                .value(row)
                .to_bits()
        ),
        DataType::Utf8 => esc(
            arr.as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(row),
        ),
        t => panic!("unhandled cell type {t:?}"),
    }
}

fn print_batches(tag: &str, batches: &[RecordBatch]) {
    if batches.is_empty() {
        println!("{tag} rows=0 (no batches)");
        return;
    }
    let schema = batches[0].schema();
    let cols: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| format!("{}:{:?}", f.name(), f.data_type()))
        .collect();
    println!("{tag} schema: {}", cols.join(", "));
    let mut n = 0usize;
    for b in batches {
        for r in 0..b.num_rows() {
            let cells: Vec<String> = b.columns().iter().map(|c| cell(c, r)).collect();
            println!("{tag} r{n}: {}", cells.join(" | "));
            n += 1;
        }
    }
    println!("{tag} rows={n}");
}

async fn plan_fingerprint(df: &DataFrame, tag: &str) -> Result<()> {
    let lp = df.logical_plan().display_indent().to_string();
    println!("{tag} logical_plan:");
    print!("{lp}");
    if !lp.ends_with('\n') {
        println!();
    }
    println!(
        "{tag} logical_plan len={} fnv={:#018x}",
        lp.len(),
        fnv1a(lp.as_bytes())
    );
    let pp = df.create_physical_plan().await?;
    let ps = displayable(pp.as_ref()).indent(false).to_string();
    println!("{tag} physical_plan:");
    print!("{ps}");
    if !ps.ends_with('\n') {
        println!();
    }
    println!(
        "{tag} physical_plan len={} fnv={:#018x}",
        ps.len(),
        fnv1a(ps.as_bytes())
    );
    Ok(())
}

async fn run() -> Result<()> {
    let config = SessionConfig::new().with_target_partitions(1);
    let ctx = SessionContext::new_with_config(config);

    // Three tables: emps (salary has one NULL, city_id has one NULL key and one dangling 9),
    // depts (research is right-side-only), cities (guangzhou is left-side-only)
    let emps = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("dept", DataType::Utf8, false),
            Field::new("salary", DataType::Float64, true),
            Field::new("city_id", DataType::Int64, true),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1i64, 2, 3, 4, 5, 6, 7, 8])) as ArrayRef,
            Arc::new(StringArray::from(vec![
                "alice", "bob", "carol", "dave", "erin", "frank", "grace", "heidi",
            ])) as ArrayRef,
            Arc::new(StringArray::from(vec![
                "eng", "eng", "ops", "ops", "eng", "ops", "hr", "hr",
            ])) as ArrayRef,
            Arc::new(Float64Array::from(vec![
                Some(9000.0f64),
                Some(7500.5),
                Some(6200.25),
                Some(6200.25),
                Some(8100.0),
                Some(5800.0),
                None,
                Some(7000.0),
            ])) as ArrayRef,
            Arc::new(Int64Array::from(vec![
                Some(1i64),
                Some(2),
                Some(1),
                Some(3),
                None,
                Some(2),
                Some(9),
                Some(1),
            ])) as ArrayRef,
        ],
    )?;
    ctx.register_batch("emps", emps)?;

    let depts = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("dept", DataType::Utf8, false),
            Field::new("budget", DataType::Int64, false),
        ])),
        vec![
            Arc::new(StringArray::from(vec!["eng", "ops", "hr", "research"])) as ArrayRef,
            Arc::new(Int64Array::from(vec![1500000i64, 900000, 600000, 2000000])) as ArrayRef,
        ],
    )?;
    ctx.register_batch("depts", depts)?;

    let cities = RecordBatch::try_new(
        Arc::new(Schema::new(vec![
            Field::new("city_id", DataType::Int64, false),
            Field::new("city", DataType::Utf8, false),
        ])),
        vec![
            Arc::new(Int64Array::from(vec![1i64, 2, 3, 4])) as ArrayRef,
            Arc::new(StringArray::from(vec!["shenzhen", "beijing", "shanghai", "guangzhou"]))
                as ArrayRef,
        ],
    )?;
    ctx.register_batch("cities", cities)?;

    // Q1 projection + arithmetic + WHERE (the NULL salary row is filtered out)
    let q1 = "SELECT id, name, salary * 2.0 AS dbl FROM emps WHERE salary >= 7000.0 ORDER BY id";
    println!("Q1 sql: {q1}");
    print_batches("Q1", &ctx.sql(q1).await?.collect().await?);

    // Q2 GROUP BY five aggregates (hr has a NULL salary: COUNT(*)=2 while SUM/AVG see only 7000)
    let q2 = "SELECT dept, COUNT(*) AS n, SUM(salary) AS total, MIN(salary) AS lo, \
              MAX(salary) AS hi, AVG(salary) AS av FROM emps GROUP BY dept ORDER BY dept";
    println!("Q2 sql: {q2}");
    let df2 = ctx.sql(q2).await?;
    plan_fingerprint(&df2, "Q2").await?;
    print_batches("Q2", &df2.collect().await?);

    // Q3 INNER JOIN: the right-side-only research dept is not visible
    let q3 = "SELECT e.id, e.name, d.dept, d.budget FROM emps e INNER JOIN depts d \
              ON e.dept = d.dept ORDER BY e.id";
    println!("Q3 sql: {q3}");
    print_batches("Q3", &ctx.sql(q3).await?.collect().await?);

    // Q4a LEFT JOIN: the NULL key (id=5) and the dangling key (id=7 -> 9) yield a NULL city
    let q4a = "SELECT e.id, e.name, c.city FROM emps e LEFT JOIN cities c \
               ON e.city_id = c.city_id ORDER BY e.id";
    println!("Q4a sql: {q4a}");
    print_batches("Q4a", &ctx.sql(q4a).await?.collect().await?);

    // Q4b reverse LEFT JOIN: guangzhou yields NULL name/id
    let q4b = "SELECT c.city, e.name, e.id FROM cities c LEFT JOIN emps e \
               ON c.city_id = e.city_id ORDER BY c.city_id, e.id NULLS FIRST";
    println!("Q4b sql: {q4b}");
    print_batches("Q4b", &ctx.sql(q4b).await?.collect().await?);

    // Q5 windows: row_number total-order tiebreak + rank ties sharing a rank (ops 6200.25
    // ties twice; the hr NULL salary gets a rank)
    let q5 = "SELECT id, dept, salary, \
              ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC, id ASC) AS rn, \
              RANK() OVER (PARTITION BY dept ORDER BY salary DESC) AS rk \
              FROM emps ORDER BY dept, rk, id";
    println!("Q5 sql: {q5}");
    let df5 = ctx.sql(q5).await?;
    plan_fingerprint(&df5, "Q5").await?;
    print_batches("Q5", &df5.collect().await?);

    // Q6 scalar subquery
    let q6 = "SELECT id, name, salary FROM emps \
              WHERE salary > (SELECT AVG(salary) FROM emps) ORDER BY id";
    println!("Q6 sql: {q6}");
    print_batches("Q6", &ctx.sql(q6).await?.collect().await?);

    // Q7 ORDER BY ... LIMIT (explicit NULLS LAST + id total-order tiebreak)
    let q7 = "SELECT name, salary FROM emps ORDER BY salary DESC NULLS LAST, id ASC LIMIT 3";
    println!("Q7 sql: {q7}");
    print_batches("Q7", &ctx.sql(q7).await?.collect().await?);

    Ok(())
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(run()).unwrap();
}
