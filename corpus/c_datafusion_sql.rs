#!/usr/bin/env mirvm
---
[dependencies]
datafusion = { version = "=54.0.0", default-features = false, features = ["sql"] }
tokio = { version = "1", default-features = false, features = ["rt"] }
---
// c_datafusion_sql —— Apache DataFusion 现行稳定大物：SessionContext + 内存
// RecordBatch 三表 + SQL 全序列的三维差分驱动（批10 波1，arrow 已通的接棒）。
//
// 【FRONTIER · 锁定 expected-red：mirvm A/C 两维当前不可跑；B 维 native 已
// 验证为绿（cargo run 两跑 stdout 95 行逐字节一致、stderr 真空、exit 0，
// Q1–Q7 结果与 Q2/Q5 plan 指纹 fnv 全部产出）】
//
// 红因③（引擎语义缺口）：Rust trait upcasting coercion（dyn SubTrait → dyn
// SuperTrait，1.86 稳定化）的 vtable 上溯变换未实现。触发链（任一注册表
// TableScan 物理化的必经之路，无官方开关可绕）：
//   datafusion-54.0.0/src/physical_planner.rs:666  TableScan 物理化调
//     source_as_provider(source)
//   → datafusion-catalog-54.0.0/src/default_table_source.rs:94
//     source.as_ref().downcast_ref::<DefaultTableSource>()
//   → datafusion-expr-54.0.0/src/table_source.rs:138  (self as &dyn Any)
//     —— dyn 上溯 coercion 本体
//   → mirvm src/lower/func.rs:1888  lowering 拒处理（源码自标 M4.2+ 里程碑）
// A 维现场：构建全程通过（含 zstd-sys C 族），执行过 SessionContext 初始化、
// 三表 register_batch、SQL 解析与逻辑计划，在 Q1 collect 的物理计划处：
//   stdout 止于首行 "Q1 sql: SELECT ..."；stderr 单行 TRAP；exit=70；
//   构建+跑到陷阱 real 6m13.8s（预算内）。
// C 维现场（MIRVM_JIT_THRESHOLD=1，缓存热）：real 13.8s，stdout/stderr/exit
// 与 A 维逐字节一致（同一 lowering 缺口，两维同点同文）。
// 最小复现（无依赖 cargo-script，/tmp 现场实测闭环）：
//   use std::any::Any;
//   trait Table: Any { fn rows(&self) -> i64; }
//   struct Mem { n: i64 }
//   impl Table for Mem { fn rows(&self) -> i64 { self.n } }
//   fn inspect(t: &dyn Table) -> String {
//       let any: &dyn Any = t;                    // ← 上溯 coercion
//       match any.downcast_ref::<Mem>() {
//           Some(m) => format!("downcast={}", m.rows()),
//           None => "downcast=none".to_string(),
//       }
//   }
//   fn main() {
//       let t: &dyn Table = &Mem { n: 7 };
//       println!("up={} {}", t.rows(), inspect(t));
//   }
//   native 实际：up=7 downcast=7，exit 0（期望一致）；
//   mirvm 实际：stderr "mirvm[m4-engine]: TRAP: dyn 上溯 vtable 变换
//   （dyn Table → dyn std::any::Any，M4.2+）"，exit 70。
// 接线建议：在 src/lower/func.rs:1888 的 Unsize 分支实现 dyn→dyn 上溯
// vtable 变换（与 cg_ssa unsized_info 同判据；按 impl 生成/选取超 trait
// vtable 即可解锁 datafusion 全系 TableScan 物理化）。
//   red_code=70（mirvm 诊断 TRAP exit 码）
//   red_pattern=「mirvm[m4-engine]: TRAP: dyn 上溯 vtable 变换（dyn 」
//   （稳定特征前缀，空格收尾；本 driver 实例全串：`dyn datafusion::
//   logical_expr::TableSource → dyn std::any::Any，M4.2+`）
// 不可绕行论证：collect/create_physical_plan 必经上述 downcast（规格主干
// SessionContext+SQL+collect 无法绕开物理化）；缺口在 lowering 无条件
// Err，无后端开关/force-soft env 可切；降级 datafusion 旧版躲引擎缺口
// 不属于「钉版本避上游破洞」，规格即现行稳定，故不钉旧。
//
// 覆盖清单：
//   1) 建表：emps(8 行: id/name/dept/salary/city_id，salary 含一个 NULL，
//      city_id 含一个 NULL 键与一个悬挂引用 9) / depts(4 行，research 右表
//      独有) / cities(4 行，guangzhou 左表独有)——join 键 null 谱系全。
//   2) Q1 投影 + 算术 + WHERE 过滤（salary>=7000，NULL 行被滤除）。
//   3) Q2 GROUP BY 五聚合 sum/count/min/max/avg：hr 组内 salary 有 NULL，
//      COUNT(*) 计入行而 SUM/AVG 跳过 NULL，聚合 null 语义锚定。
//   4) Q3 两表 INNER JOIN：depts 右表独有 research 不可见（结果 dept 集合
//      只 eng/hr/ops）。
//   5) Q4a/Q4b 两表 LEFT JOIN 双向：emps LEFT cities 出 NULL city（NULL 键
//      id=5 与悬挂 id=7）；cities LEFT emps 出 NULL name/id（guangzhou）。
//   6) Q5 窗口函数：ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary
//      DESC, id ASC)（全序 tiebreak 锁死）与 RANK() OVER (PARTITION BY dept
//      ORDER BY salary DESC)（ops 组 6200.25 双平局同秩、hr 组 NULL 参秩）。
//   7) Q6 标量子查询：salary > (SELECT AVG(salary) FROM emps)。
//   8) Q7 ORDER BY salary DESC NULLS LAST, id ASC LIMIT 3。
//   9) plan 指纹：Q2（聚合）与 Q5（窗口）各打 logical plan（display_indent）
//      与 physical plan（displayable().indent(false)）缩排全文 + len +
//      FNV-1a 锚。
//  10) 结果打印：DataFrame::collect 后逐 batch 逐行逐格自写打印——f64 一律
//      to_bits() 十六进制，Utf8 自写转义加引号，NULL 打 "NULL"，schema 行
//      带列名+DataType Debug。
//
// 确定性：SessionConfig::with_target_partitions(1) 灭并行分区序（MemTable
//   单分区扫描+单分区聚合，累加序=插入序）；每条查询显式 ORDER BY 且排序键
//   全序（唯一 id 收尾 / rank 平局同值不依赖组内序）；窗口 row_number 的
//   ORDER BY 含 id tiebreak 成全序；无真随机/壁钟/HashMap 迭代序/裸地址/
//   浮点 to_string；tokio current_thread 单线程运行时（c_tokio 先例）；
//   stderr 面为空。
//
// 钉版本与绕行记录：
//   datafusion =54.0.0（2026-07-18 快拍 crates.io max_stable=54.0.0，精确
//   钉死；其自身锁定 arrow ^58.3.0 / sqlparser 0.62.0 / tokio ^1.52）。
//   default-features=false 只开 "sql"（官方 feature 开关合法裁剪）：
//   规格只测核心 SQL 语义面，默认面的 parquet/compression/regex/unicode/
//   crypto/datetime/nested_expressions 与本规格无关且显著拉长构建预算；
//   窗口/聚合函数在 v54 为非可选依赖（datafusion-functions-window /
//   -aggregate 直挂），裁剪后窗口与五聚合照常在位。recursive_protection
//   亦随默认面关闭，本驱动查询浅、无深递归表达式，语义面无差。
//   tokio = 1（default-features=false + rt；registry 实解 1.53.0，
//   c_tokio/c_sqlx_sqlite 先例）。未使用任何引擎绕行开关/force-soft env。

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

    // 三表：emps（salary 一 NULL、city_id 一 NULL 键一悬挂 9）、
    // depts（research 右表独有）、cities（guangzhou 左表独有）
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

    // Q1 投影 + 算术 + WHERE（NULL salary 被滤除）
    let q1 = "SELECT id, name, salary * 2.0 AS dbl FROM emps WHERE salary >= 7000.0 ORDER BY id";
    println!("Q1 sql: {q1}");
    print_batches("Q1", &ctx.sql(q1).await?.collect().await?);

    // Q2 GROUP BY 五聚合（hr 组带 NULL salary：COUNT(*)=2 而 SUM/AVG 只看 7000）
    let q2 = "SELECT dept, COUNT(*) AS n, SUM(salary) AS total, MIN(salary) AS lo, \
              MAX(salary) AS hi, AVG(salary) AS av FROM emps GROUP BY dept ORDER BY dept";
    println!("Q2 sql: {q2}");
    let df2 = ctx.sql(q2).await?;
    plan_fingerprint(&df2, "Q2").await?;
    print_batches("Q2", &df2.collect().await?);

    // Q3 INNER JOIN：depts 右表独有 research 不可见
    let q3 = "SELECT e.id, e.name, d.dept, d.budget FROM emps e INNER JOIN depts d \
              ON e.dept = d.dept ORDER BY e.id";
    println!("Q3 sql: {q3}");
    print_batches("Q3", &ctx.sql(q3).await?.collect().await?);

    // Q4a LEFT JOIN：NULL 键(id=5)与悬挂键(id=7→9)出 NULL city
    let q4a = "SELECT e.id, e.name, c.city FROM emps e LEFT JOIN cities c \
               ON e.city_id = c.city_id ORDER BY e.id";
    println!("Q4a sql: {q4a}");
    print_batches("Q4a", &ctx.sql(q4a).await?.collect().await?);

    // Q4b 反向 LEFT JOIN：guangzhou 出 NULL name/id
    let q4b = "SELECT c.city, e.name, e.id FROM cities c LEFT JOIN emps e \
               ON c.city_id = e.city_id ORDER BY c.city_id, e.id NULLS FIRST";
    println!("Q4b sql: {q4b}");
    print_batches("Q4b", &ctx.sql(q4b).await?.collect().await?);

    // Q5 窗口：row_number 全序 tiebreak + rank 平局同秩（ops 6200.25 双平局、
    // hr 组 NULL salary 参秩）
    let q5 = "SELECT id, dept, salary, \
              ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC, id ASC) AS rn, \
              RANK() OVER (PARTITION BY dept ORDER BY salary DESC) AS rk \
              FROM emps ORDER BY dept, rk, id";
    println!("Q5 sql: {q5}");
    let df5 = ctx.sql(q5).await?;
    plan_fingerprint(&df5, "Q5").await?;
    print_batches("Q5", &df5.collect().await?);

    // Q6 标量子查询
    let q6 = "SELECT id, name, salary FROM emps \
              WHERE salary > (SELECT AVG(salary) FROM emps) ORDER BY id";
    println!("Q6 sql: {q6}");
    print_batches("Q6", &ctx.sql(q6).await?.collect().await?);

    // Q7 ORDER BY ... LIMIT（显式 NULLS LAST + id 全序 tiebreak）
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
