#!/usr/bin/env mirvm
---
[dependencies]
polars = { version = "0.44", default-features = false, features = ["lazy", "dtype-date", "dtype-datetime", "fmt_no_tty"] }
---
// polars 0.44.2 differential: the full Arrow2 columnar DataFrame chain (eager
// construction/filter/group-by aggregation + lazy expression chains/inner join +
// statistical summary + the null lineage). Feature set = lazy (query engine) +
// dtype-date (Date logical type, pulling temporal -> chrono) + dtype-datetime +
// fmt_no_tty (DataFrame Display, bypassing tty width probing for a fixed fallback
// width, so pipelines are deterministic).
//
// FRONTIER: mirvm's two dimensions cannot run yet; the native driver is verified green.
// polars' dependency graph necessarily contains psm (non-optional psm <- stacker <-
// polars-utils <- polars; no feature/version/API combination removes it). psm 0.1.31
// compiles x86_64 assembly into libpsm.a, visibly exporting rust_psm_on_stack and
// three more symbols in dynsym -- while the mirvm process's RTLD_DEFAULT already defines
// the same name: librustc_driver-*.so (mirvm links rustc as a library) exports the
// stacker/psm used by rustc's query-system recursion guard (nm -D). native_archive
// therefore refuses by design (a dynsym collision cannot reproduce the native linker
// order from dlsym priority; only hidden collisions have a symtab fallback). Archive
// materialization happens before the whole module is lowered, independent of the driver.
// The diagnostic (a panic, exit 101) reads:
//   thread 'rustc' panicked at src/lower/mod.rs:1646:38: Static native library load failed:
//   static archive .../native-archives/38daa748a74132cfe2c177878f86f965.so exports symbol
//   `rust_psm_on_stack`, but RTLD_DEFAULT already defines it; mirvm's dlsym priority cannot
//   unambiguously reproduce the native linker order
//
// Route notes:
// - Upstream feature hole: polars-io 0.44.2's csv/write datetime serializer references
//   chrono unconditionally, and chrono is only attached by dtype-datetime, while lazy ->
//   polars-plan turns on polars-io/csv unconditionally, so default-features=false + lazy +
//   dtype-date fails with E0433; adding dtype-datetime pulls chrono back in.
// - Hand-written describe: DataFrame::describe() only landed in 0.45 (0.44 has an empty
//   "describe" feature stub), so the summary comes from the Series reduction kernels
//   (mean/std/median/min/max + null_count), with f64 locked by to_bits().
// - Determinism: group-by output order depends on hash-table layout (polars does not
//   guarantee it) -> aggregation results sort on the unique key before printing and join
//   rows on the unique id; mean runs only on Int64 columns, so partition order is moot.
// - Zero randomness/environment: all data is inline and fixed; no now()/random sampling.
// Coverage: Series construction for the five dtypes i64/f64/str/bool/date (including
// Option columns), schema iteration, BooleanChunked mask eager filter (including the
// length-mismatch error path), lazy group_by sum/mean/count (multi-key dtypes), lazy
// sort, with_columns expression chains (arithmetic multiply / scalar mixing / comparison
// / alias x three chains) + select, inner join (one row dropped per side), the to_string
// FNV anchor, AnyValue printing, and the null lineage (null_count/is_null, validity
// bitmap after rechunk, drop_nulls length, is_null mask filtering).
use polars::prelude::*;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn true_cnt(ca: &BooleanChunked) -> usize {
    ca.iter().filter(|b| matches!(b, Some(true))).count()
}

fn bits(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{:#018x}", x.to_bits()),
        None => "∅".to_string(),
    }
}

fn main() -> PolarsResult<()> {
    // ① build the frame with five dtypes (date via an Int32 physical cast, with one null)
    let df = DataFrame::new(vec![
        Column::from(Series::new("id".into(), [1i64, 2, 3, 4, 5, 6])),
        Column::from(Series::new(
            "city".into(),
            ["sz", "bj", "sz", "sh", "bj", "hz"],
        )),
        Column::from(Series::new("qty".into(), [10i64, 3, 8, 12, 5, 8])),
        Column::from(Series::new(
            "price".into(),
            [2.5f64, 9.75, 2.5, 4.25, 9.75, 2.5],
        )),
        Column::from(Series::new(
            "active".into(),
            [true, false, true, true, false, true],
        )),
        Column::from(
            Series::new(
                "day".into(),
                [
                    Some(19358i32), // 2023-01-01
                    Some(19327),    // 2022-12-01
                    Some(19358),
                    Some(19418), // 2023-03-02
                    Some(19418),
                    None,
                ],
            )
            .cast(&DataType::Date)?,
        ),
    ])?;

    // ② schema (IndexMap insertion order, dtype Display)
    for (name, dtype) in df.schema().iter() {
        println!("schema {name}: {dtype}");
    }

    // ③ full-frame Display (comfy-table fixed fallback width) + FNV byte anchor
    let tbl = df.to_string();
    println!("{tbl}");
    println!(
        "frame bytes len={} fnv={:#018x}",
        tbl.len(),
        fnv1a(tbl.as_bytes())
    );

    // ④ eager filter: boolean mask + the length-mismatch error path
    let mask = df.column("qty")?.i64()?.gt_eq(8);
    let f = df.filter(&mask)?;
    println!("{f}");
    println!("filter height={} width={}", f.height(), f.width());
    let bad = df
        .filter(&BooleanChunked::new("m".into(), [true, false]))
        .unwrap_err();
    println!("filter err: {bad}");

    // ⑤ lazy group_by x (sum/mean/count): hash order -> sort on the unique key to lock it
    let gsum = df
        .clone()
        .lazy()
        .group_by([col("city")])
        .agg([col("qty").sum(), col("price").sum()])
        .sort(["city"], Default::default())
        .collect()?;
    println!("{gsum}");
    let gmean = df
        .clone()
        .lazy()
        .group_by([col("city")])
        .agg([col("qty").mean().alias("qty_mean")])
        .sort(["city"], Default::default())
        .collect()?;
    println!("{gmean}");
    let gcnt = df
        .clone()
        .lazy()
        .group_by([col("city")])
        .agg([col("qty").count().alias("qty_count")])
        .sort(["city"], Default::default())
        .collect()?;
    println!("{gcnt}");
    // bool-key group-by (dtype coverage)
    let gbool = df
        .clone()
        .lazy()
        .group_by([col("active")])
        .agg([col("qty").sum()])
        .sort(["active"], Default::default())
        .collect()?;
    println!("{gbool}");

    // ⑥ lazy with_columns expression chain (qty*price -> amount -> taxed -> boolean flag
    //    plus scalar mixed arithmetic) closed out by select
    let df2 = df
        .clone()
        .lazy()
        .with_columns([(col("qty") * col("price")).alias("amount")])
        .with_columns([(col("amount") * lit(1.13f64)).alias("taxed")])
        .with_columns([
            col("taxed").gt(lit(20.0f64)).alias("big_deal"),
            (col("qty") + lit(1i64) * lit(2i64)).alias("qty_shift"),
        ])
        .select([
            col("id"),
            col("amount"),
            col("taxed"),
            col("big_deal"),
            col("qty_shift"),
        ])
        .collect()?;
    println!("{df2}");

    // ⑦ inner join: hz (left-only) / gz (right-only) drop a row per side; sort on the unique id
    let geo = DataFrame::new(vec![
        Column::from(Series::new("city".into(), ["sz", "bj", "sh", "gz"])),
        Column::from(Series::new(
            "region".into(),
            ["south", "north", "east", "south"],
        )),
    ])?;
    let joined = df
        .clone()
        .lazy()
        .join(
            geo.lazy(),
            [col("city")],
            [col("city")],
            JoinArgs::new(JoinType::Inner),
        )
        .sort(["id"], Default::default())
        .collect()?;
    println!("{joined}");

    // ⑧ describe summary (the hand-written 0.44 version): mean/std/median locked via to_bits
    for c in ["qty", "price"] {
        let s = df.column(c)?.as_materialized_series();
        let min = s.min_reduce()?.value().to_string();
        let max = s.max_reduce()?.value().to_string();
        println!(
            "describe {c}: n={} null={} mean={} std={} median={} min={} max={}",
            s.len() - s.null_count(),
            s.null_count(),
            bits(s.mean()),
            bits(s.std(1)),
            bits(s.median()),
            min,
            max
        );
    }

    // ⑨ null lineage: Option columns across four dtypes
    let nn = DataFrame::new(vec![
        Column::from(Series::new(
            "a".into(),
            [Some(1i64), None, Some(-3), None, Some(42)],
        )),
        Column::from(Series::new(
            "b".into(),
            [Some(1.5f64), None, None, Some(-0.0), Some(7.25)],
        )),
        Column::from(Series::new(
            "c".into(),
            [Some("α"), None, Some("ββ"), Some(""), None],
        )),
        Column::from(Series::new(
            "d".into(),
            [Some(true), None, Some(false), Some(true), None],
        )),
    ])?;
    println!("{nn}");
    for name in ["a", "b", "c", "d"] {
        let s = nn.column(name)?.as_materialized_series();
        let rc = s.rechunk();
        let has_bitmap = rc.chunks()[0].validity().is_some();
        println!(
            "null {name}: len={} null_count={} is_null_true={} not_null_true={} validity_bitmap={} drop_nulls_len={}",
            s.len(),
            s.null_count(),
            true_cnt(&s.is_null()),
            true_cnt(&s.is_not_null()),
            has_bitmap,
            s.drop_nulls().len()
        );
    }
    // is_null mask used as a filter (bool expressions consumed on the arrow side)
    let null_b = nn.filter(&nn.column("b")?.as_materialized_series().is_null())?;
    println!("null_b height={}", null_b.height());

    Ok(())
}
