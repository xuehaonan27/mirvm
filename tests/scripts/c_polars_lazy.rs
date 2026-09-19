#!/usr/bin/env mirvm
---
[dependencies]
polars = { version = "=0.44.2", default-features = false, features = ["lazy", "dtype-date", "dtype-datetime", "fmt_no_tty", "cse", "semi_anti_join", "rank", "moment", "cum_agg", "rolling_window", "is_in", "is_between", "is_unique", "is_first_distinct", "unique_counts", "diff", "pct_change", "round_series", "sign", "abs", "coalesce"] }
---
// c_polars_lazy -- broad plan-side surface for polars 0.44.2 lazy frames: query optimizer (plan text /
// pushdown switches / optimized-vs-unoptimized cross-check / CSE) + wide expression battery + lazy plan
// execution (group_by with moment, over windows, inner/semi/anti join, union, limit, error
// paths), all on in-memory frames with zero IO.
//
// Version pinning (evidence for a compatible combination):
//   * polars pinned to =0.44.2, the same version and base as the eager fixtures: that
//     version's eager pipeline (including small lazy group_by/join surfaces) is byte-identical
//     across the three runs and the psm/stacker dynsym collision is fixed in the dependency
//     tree (polars-core/plan/lazy/ops/mem-engine + arrow2 + psm + chrono), so no version gamble.
//   * feature set = the proven four-feature base (lazy + dtype-date + dtype-datetime +
//     fmt_no_tty) plus 17 plan-side features, all mapping onto polars-lazy/polars-plan/
//     polars-ops internal crates (verified against 0.44.2's Cargo.toml, no new heavy
//     external dependency). dtype-datetime keeps the upstream feature-hole workaround:
//     polars-io 0.44.2's csv/write datetime serializer references chrono unconditionally,
//     and chrono is only attached by dtype-datetime, while lazy -> polars-plan turns on
//     polars-io/csv unconditionally; without dtype-datetime the combo fails with E0433.
//
// Determinism:
//   * all data is inline and fixed (orders 12 rows + quota 4 rows): zero randomness,
//     zero now(), zero env vars, zero IO; POLARS_VERBOSE is unset, so stderr stays empty.
//   * every sort is locked to a total order: each sort's by list ends with the unique key
//     id (or region), compensating for SortMultipleOptions' default maintain_order=false
//     unstable sort; group_by/join/union output is sorted by that key before printing.
//   * no floating-point divergence: the qty/price reductions (mean/skew/kurtosis/
//     rolling_mean/pct_change) are ordered IEEE ops on small single-chunk data, and
//     Display uses std fmt's shortest round-trip (NaN always "NaN"), so bits agree.
//   * plan text (explain(false/true)) is a pure string tree: no addresses, no hash order;
//     the CSE CACHE node id comes from an in-process counter, so identical code, same order.
//   * the CSE trigger shapes are pinned down by native experiment (0.44.2 semantics,
//     not a mirvm fork): elim_cmn_subplans deduplicates only "identical subplans" -- a
//     self-join sharing a with_columns base or a union of identical branches yields CACHE
//     (caches=2); a union of complementary/differently filtered branches and a self-join
//     with a different filter on each side are not deduplicated (caches=0, because to_alp
//     writes the pushable filter into DataFrameScan.filter, making the scans mismatch and
//     closing the has_duplicate_scans gate). S7 takes the upstream test_cse_self_joins shape.
//
// Failure triage:
//   * three-way differential rerun:
//     A: target/release/mirvm run tests/scripts/c_polars_lazy.rs
//     B: d=$(grep -l 'name = "c_polars_lazy"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && cargo +nightly-2026-07-02 run -q
//     C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_polars_lazy.rs
//   * if A/C fail: read the first stderr line -- a mirvm TRAP ("mirvm[m4-engine]: TRAP:")
//     points at an engine gap; a panic loading the native archive (psm family) is the known
//     psm dynsym collision; if the runs diverge in numbers or plan text, localize with the
//     S1/S7 explain text, then bisect expressions column by column with the S3 battery.
//
// Oracle: the native release run, the cargo build run and the JIT run must all agree byte-for-byte on
// stdout, and every anchor line must pass: S1 has_selection/projected/differ all
// true, S2 opt_consistent=true, S7 caches=2/differ/cse_consistent all true and
// S8 has_sort_by=true; stderr must be empty and the exit status 0, and a repeated native run must
// match the first one byte-for-byte, so the fixture is deterministic across processes. The anchors
// hash and length the printed data (S0), plan text (S1), optimization on/off consistency (S2),
// the expression battery (S3), group_by aggregates (S4), over windows (S5), joins (S6), CSE (S7),
// sort/limit (S8) and the error path (S9), so any divergence shows up as a hash mismatch. No FRONTIER needed.
//
// Coverage list:
//   S0  data anchors: full-frame Display + FNV-1a/64 for orders/quota.
//   S1  query optimizer plan text: the unoptimized explain(false) text of the
//       filter -> with_columns -> select -> sort chain, the optimized explain(true)
//       text (asserting SELECTION = predicate pushdown and PROJECT = projection pushdown
//       or column pruning), then the plan with all three pushdowns disabled -> differ=true.
//   S2  optimized vs unoptimized result cross-check: the same query with OptFlags::default() all on vs
//       without_optimizations() (leaving only TYPE_COERCION) must be byte-identical.
//   S3  expression battery over 4 frames: arithmetic/when-then-otherwise/coalesce/rolling_mean/
//       cum_sum/shift/diff/pct_change/rank(Dense)/abs/sign/round/is_between/
//       is_in(Series literal)/is_unique/is_first_distinct + unique_counts
//       (in first-occurrence order, an independent single-column frame).
//   S4  group_by aggregation over 2 frames: sum/mean/count/null_count + skew/kurtosis
//       (the moment surface, including the all-equal-group NaN spectrum).
//   S5  over windows: sum/rank/mean over(partition by region), then sort by id.
//   S6  three join shapes: inner (a row dropped on each side) / semi / anti (the semi_anti_join surface).
//   S7  CSE: a self-join sharing a with_columns base (38-row n² fan-out, sort
//       [id,id_right] total order): the explain(true) text with CSE on by default (asserting
//       caches=2) vs CSE off (caches=0, differ=true), and the collect results of the two states
//       must be byte-identical.
//   S8  slice/limit: an optimized-plan anchor for a descending sort with tiebreak + limit(4) (SORT BY
//       assertion; limit is not shown separately in the 0.44.2 optimized plan text) plus the top-4 result.
//   S9  error path: the ColumnNotFound error text for selecting a missing column.
use polars::prelude::*;
use polars::series::ops::NullBehavior;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn show(tag: &str, df: &DataFrame) {
    let s = df.to_string();
    println!("{s}");
    println!("anchor {tag}: len={} fnv={:#018x}", s.len(), fnv1a(s.as_bytes()));
}

fn orders() -> PolarsResult<DataFrame> {
    DataFrame::new(vec![
        Column::from(Series::new(
            "id".into(),
            [1i64, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        )),
        Column::from(Series::new(
            "region".into(),
            [
                "north", "south", "north", "east", "west", "south", "north", "east", "west",
                "south", "north", "east",
            ],
        )),
        Column::from(Series::new(
            "product".into(),
            ["a", "b", "a", "c", "a", "b", "c", "a", "b", "c", "a", "b"],
        )),
        Column::from(Series::new(
            "qty".into(),
            [
                Some(10i64),
                Some(3),
                None,
                Some(12),
                Some(5),
                Some(8),
                Some(10),
                None,
                Some(7),
                Some(8),
                Some(3),
                Some(12),
            ],
        )),
        Column::from(Series::new(
            "price".into(),
            [
                Some(2.5f64),
                Some(9.75),
                Some(2.5),
                Some(4.25),
                Some(9.75),
                Some(2.5),
                Some(3.0),
                None,
                Some(1.5),
                Some(2.5),
                Some(8.25),
                Some(4.25),
            ],
        )),
        Column::from(
            Series::new(
                "day".into(),
                [
                    Some(19358i32), // 2023-01-01
                    Some(19372),    // 2023-01-15
                    Some(19358),
                    Some(19389), // 2023-02-01
                    Some(19418), // 2023-03-02
                    Some(19372),
                    Some(19389),
                    Some(19418),
                    None,
                    Some(19372),
                    Some(19358),
                    Some(19389),
                ],
            )
            .cast(&DataType::Date)?,
        ),
        Column::from(Series::new(
            "promo".into(),
            [
                true, false, true, false, true, false, true, false, true, false, true, false,
            ],
        )),
    ])
}

fn quota() -> PolarsResult<DataFrame> {
    DataFrame::new(vec![
        Column::from(Series::new(
            "region".into(),
            ["north", "south", "east", "central"],
        )),
        Column::from(Series::new("quota".into(), [60i64, 19, 24, 33])),
    ])
}

fn main() -> PolarsResult<()> {
    let orders = orders()?;
    let quota = quota()?;

    // S0 data anchors
    show("S0.orders", &orders);
    show("S0.quota", &quota);

    // S1 optimizer plan text: filter -> with_columns -> select -> sort
    let q1 = orders
        .clone()
        .lazy()
        .filter(col("qty").gt(lit(5i64)))
        .with_columns([
            (col("qty") * col("price")).alias("amount"),
            (col("qty") + lit(1i64)).alias("qty_dropped"),
        ])
        .select([col("id"), col("region"), col("amount")])
        .sort(["id"], Default::default());
    let plan_unopt = q1.explain(false)?;
    println!("S1 plan_unoptimized:\n{plan_unopt}");
    println!(
        "anchor S1.plan_unopt: len={} fnv={:#018x}",
        plan_unopt.len(),
        fnv1a(plan_unopt.as_bytes())
    );
    let plan_opt = q1.clone().explain(true)?;
    println!("S1 plan_optimized:\n{plan_opt}");
    println!(
        "anchor S1.plan_opt: len={} fnv={:#018x} has_selection={} projected={} differ_unopt={}",
        plan_opt.len(),
        fnv1a(plan_opt.as_bytes()),
        plan_opt.contains("SELECTION"),
        plan_opt.contains("PROJECT"),
        plan_opt != plan_unopt
    );
    let plan_nopush = q1
        .clone()
        .with_predicate_pushdown(false)
        .with_projection_pushdown(false)
        .with_slice_pushdown(false)
        .explain(true)?;
    println!(
        "anchor S1.plan_nopush: len={} fnv={:#018x} differ_opt={}",
        plan_nopush.len(),
        fnv1a(plan_nopush.as_bytes()),
        plan_nopush != plan_opt
    );

    // S2 optimized on/off result cross-check (both collect_with_optimizations states)
    let s2_on = q1.clone().collect()?;
    let s2_off = q1.without_optimizations().collect()?;
    let on_s = s2_on.to_string();
    let off_s = s2_off.to_string();
    println!("{on_s}");
    println!(
        "anchor S2: opt_consistent={} len={} fnv={:#018x}",
        on_s == off_s,
        on_s.len(),
        fnv1a(on_s.as_bytes())
    );

    // S3 expression battery (lock the id order first, then print in three frames)
    let roll3 = RollingOptionsFixedWindow {
        window_size: 3,
        min_periods: 1,
        ..Default::default()
    };
    let dense = RankOptions {
        method: RankMethod::Dense,
        descending: false,
    };
    let battery = orders
        .clone()
        .lazy()
        .sort(["id"], Default::default())
        .with_columns([
            (col("qty") * lit(2i64)).alias("qty_dbl"),
            when(col("qty").is_null())
                .then(lit(-1i64))
                .otherwise(col("qty"))
                .alias("qty_filled"),
            coalesce(&[col("qty"), lit(0i64)]).alias("qty_coal"),
            col("qty")
                .cast(DataType::Float64)
                .rolling_mean(roll3)
                .alias("qty_rm3"),
            col("qty").cum_sum(false).alias("qty_cum"),
            col("qty").shift(lit(1i64)).alias("qty_s1"),
            col("qty").diff(1, NullBehavior::Ignore).alias("qty_d1"),
            col("price").pct_change(lit(1i64)).alias("price_pc"),
            col("qty").rank(dense, None).alias("qty_rank"),
            (col("qty") - lit(6i64)).abs().alias("qty_abs6"),
            (col("qty") - lit(6i64)).sign().alias("qty_sign6"),
            col("price").round(1).alias("price_r1"),
            col("id")
                .is_between(lit(3i64), lit(7i64), ClosedInterval::Both)
                .alias("id_mid"),
            col("region")
                .is_in(lit(Series::new("".into(), ["north", "east"])))
                .alias("reg_ne"),
            col("qty").is_unique().alias("qty_uniq"),
            col("qty").is_first_distinct().alias("qty_first"),
        ]);
    let s3a = battery.clone().select([
        col("id"),
        col("qty"),
        col("qty_filled"),
        col("qty_coal"),
        col("qty_rm3"),
        col("qty_cum"),
        col("qty_s1"),
    ]);
    show("S3a", &s3a.collect()?);
    let s3b = battery.clone().select([
        col("id"),
        col("price"),
        col("price_pc"),
        col("qty_d1"),
        col("qty_rank"),
        col("qty_sign6"),
        col("id_mid"),
    ]);
    show("S3b", &s3b.collect()?);
    let s3c = battery.select([
        col("id"),
        col("region"),
        col("reg_ne"),
        col("qty_uniq"),
        col("qty_first"),
        col("price_r1"),
    ]);
    show("S3c", &s3c.collect()?);
    // unique_counts emits the count of each distinct value in first-occurrence order
    // (length = distinct count != frame height, so with_columns raises ShapeMismatch); anchor in a select.
    let s3d = orders
        .clone()
        .lazy()
        .select([col("region").unique_counts().alias("reg_ucnt")]);
    show("S3d", &s3d.collect()?);

    // S4 group_by aggregation (moment surface included; sort by the unique key region to lock order)
    let s4a = orders
        .clone()
        .lazy()
        .group_by([col("region")])
        .agg([
            col("qty").sum().alias("qty_sum"),
            col("qty").mean().alias("qty_mean"),
            col("qty").count().alias("qty_count"),
            col("qty").null_count().alias("qty_nulls"),
        ])
        .sort(["region"], Default::default());
    show("S4a", &s4a.collect()?);
    let s4b = orders
        .clone()
        .lazy()
        .group_by([col("region")])
        .agg([
            col("price").skew(true).alias("price_skew"),
            col("price").kurtosis(true, true).alias("price_kurt"),
            col("price").mean().alias("price_mean"),
        ])
        .sort(["region"], Default::default());
    show("S4b", &s4b.collect()?);

    // S5 over windows (partition by region; sort by id after the window join to lock order)
    let s5 = orders
        .clone()
        .lazy()
        .with_columns([
            col("qty").sum().over([col("region")]).alias("region_qty_sum"),
            col("qty")
                .rank(dense, None)
                .over([col("region")])
                .alias("qty_rank_reg"),
            col("price")
                .mean()
                .over([col("region")])
                .alias("region_price_mean"),
        ])
        .sort(["id"], Default::default())
        .select([
            col("id"),
            col("region"),
            col("qty"),
            col("region_qty_sum"),
            col("qty_rank_reg"),
            col("region_price_mean"),
        ]);
    show("S5", &s5.collect()?);

    // S6 three join shapes (west is left-only / central is right-only; semi/anti go through semi_anti_join)
    let s6_inner = orders
        .clone()
        .lazy()
        .join(
            quota.clone().lazy(),
            [col("region")],
            [col("region")],
            JoinArgs::new(JoinType::Inner),
        )
        .sort(["id"], Default::default())
        .select([col("id"), col("region"), col("qty"), col("quota")]);
    show("S6.inner", &s6_inner.collect()?);
    let s6_semi = orders
        .clone()
        .lazy()
        .join(
            quota.clone().lazy(),
            [col("region")],
            [col("region")],
            JoinArgs::new(JoinType::Semi),
        )
        .sort(["id"], Default::default())
        .select([col("id"), col("region"), col("qty")]);
    show("S6.semi", &s6_semi.collect()?);
    let s6_anti = orders
        .clone()
        .lazy()
        .join(
            quota.lazy(),
            [col("region")],
            [col("region")],
            JoinArgs::new(JoinType::Anti),
        )
        .sort(["id"], Default::default())
        .select([col("id"), col("region"), col("qty")]);
    show("S6.anti", &s6_anti.collect()?);

    // S7 CSE: a self-join sharing a with_columns base (the shape observed with polars
    // 0.44.2 -- CSE dedups only identical subplans; a union of complementary filter
    // branches is not, see the header "Determinism" note). CSE on by default vs off;
    // the join key region fans out to n² rows per group (38), sorted totally on [id, id_right].
    let lf7 = orders
        .clone()
        .lazy()
        .with_columns([(col("qty") * col("price")).alias("amount")]);
    let mk7 = |on: bool| {
        lf7.clone()
            .join(
                lf7.clone(),
                [col("region")],
                [col("region")],
                JoinArgs::new(JoinType::Inner),
            )
            .with_comm_subplan_elim(on)
    };
    let plan_cse_on = mk7(true).explain(true)?;
    println!("S7 plan_cse_on:\n{plan_cse_on}");
    let plan_cse_off = mk7(false).explain(true)?;
    println!(
        "anchor S7.plan: on_len={} on_fnv={:#018x} caches={} off_len={} off_fnv={:#018x} differ={}",
        plan_cse_on.len(),
        fnv1a(plan_cse_on.as_bytes()),
        plan_cse_on.matches("CACHE").count(),
        plan_cse_off.len(),
        fnv1a(plan_cse_off.as_bytes()),
        plan_cse_on != plan_cse_off
    );
    let s7sel = [
        col("id"),
        col("region"),
        col("amount"),
        col("id_right"),
        col("amount_right"),
    ];
    let u_on = mk7(true)
        .sort(["id", "id_right"], Default::default())
        .select(s7sel.clone())
        .collect()?
        .to_string();
    let u_off = mk7(false)
        .sort(["id", "id_right"], Default::default())
        .select(s7sel)
        .collect()?
        .to_string();
    println!("{u_on}");
    println!(
        "anchor S7.result: cse_consistent={} len={} fnv={:#018x}",
        u_on == u_off,
        u_on.len(),
        fnv1a(u_on.as_bytes())
    );

    // S8 slice/limit: descending order + id tiebreak total order + limit(4)
    let s8 = orders
        .clone()
        .lazy()
        .sort(
            ["qty", "id"],
            SortMultipleOptions::new().with_order_descending_multi([true, false]),
        )
        .limit(4);
    let plan_s8 = s8.clone().explain(true)?;
    println!(
        "anchor S8.plan: len={} fnv={:#018x} has_sort_by={}",
        plan_s8.len(),
        fnv1a(plan_s8.as_bytes()),
        plan_s8.contains("SORT BY")
    );
    show("S8.top4", &s8.collect()?);

    // S9 error path: missing column
    let err = orders
        .lazy()
        .select([col("nope")])
        .collect()
        .unwrap_err();
    println!("S9 err: {err}");

    Ok(())
}
