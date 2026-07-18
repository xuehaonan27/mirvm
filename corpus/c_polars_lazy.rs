#!/usr/bin/env mirvm
---
[dependencies]
polars = { version = "=0.44.2", default-features = false, features = ["lazy", "dtype-date", "dtype-datetime", "fmt_no_tty", "cse", "semi_anti_join", "rank", "moment", "cum_agg", "rolling_window", "is_in", "is_between", "is_unique", "is_first_distinct", "unique_counts", "diff", "pct_change", "round_series", "sign", "abs", "coalesce"] }
---
// c_polars_lazy —— polars 0.44.2 lazy frame 计划侧大面：查询优化器（计划文本/
// pushdown 开关/优化前后结果对拍/CSE）+ 宽表达式电池 + lazy plan 执行
// （group_by 含 moment、over 窗口、inner/semi/anti join、union、limit、错误
// 路径），全内存帧零 IO。批6 c_polars_frame（eager 已三维绿）的计划侧接棒。
//
// 版本钉（相容组合证据）：
//   * polars 钉 =0.44.2——与批6 c_polars_frame 完全同版同基座：该版 eager 全
//     链路（含 lazy group_by/join 小面）已三维逐字节绿，psm/stacker dynsym
//     撞车已于 fb0b204 修复入册，依赖树（polars-core/plan/lazy/ops/mem-engine
//     + arrow2 + psm + chrono）在场实证可构建可跑，无版本线赌博。
//   * feature 面 = 批6 实证基座四件（lazy + dtype-date + dtype-datetime +
//     fmt_no_tty）之上叠加计划侧 17 件，全部映射到 polars-lazy/polars-plan/
//     polars-ops 内部 crate（0.44.2 Cargo.toml 逐件核实，不引入新外部重依赖）。
//     dtype-datetime 保留批6 记档的上游 feature 破洞绕行：polars-io 0.44.2 的
//     csv/write datetime serializer 无条件引用 chrono，而 chrono 只由
//     dtype-datetime 挂上，lazy→polars-plan 又无条件开 polars-io/csv——不开
//     dtype-datetime 则 default-features=false 组合直接 E0433。
//
// 确定性说明：
//   * 数据全内联定值（orders 12 行 + quota 4 行），零随机、零 now()、零环境
//     变量、零 IO；POLARS_VERBOSE 未设，优化器 eprintln 全静默，stderr 真空。
//   * 排序全序锁死：每个 sort 的 by 列表末位必带唯一键 id（或唯一键 region），
//     弥补 SortMultipleOptions 默认 maintain_order=false 的不稳定排序；group_by
//     /join/union 输出序（哈希布局相关）一律先 sort 唯一键再打印。
//   * 浮点无分歧：qty/price 列归约（mean/skew/kurtosis/rolling_mean/
//     pct_change）均为单 chunk 小数据定序 IEEE 运算，Display 走 std fmt 最短
//     往返格式（NaN 一律 "NaN"），跨实现位级一致以批6 mean 绿为实证先例。
//   * 计划文本（explain(false/true)）为纯字符串树打印，无地址无哈希序；
//     CSE 的 CACHE 节点 id 由进程内顺序计数器分配，双维同码同序。
//   * CSE 触发形态经 native 实验锁定（0.44.2 语义，非 mirvm 分叉）：
//     elim_cmn_subplans 只对「恒等子计划」去重——共享 with_columns 基座的
//     self-join / 恒等分支 union 得 CACHE（caches=2）；互补/不同 filter 的
//     分支 union 与两侧各挂不同 filter 的 self-join 均不去重（caches=0，
//     to_alp 把可下推 filter 直接写进 DataFrameScan.filter 使两 scan 失配，
//     has_duplicate_scans 闸门关闭）。本 driver S7 取官方 test_cse_self_joins
//     同族的内存帧 self-join 形态。
//
// 复红定因参照：
//   * 三维复跑：
//     A: target/release/mirvm run corpus/c_polars_lazy.rs
//     B: d=$(grep -l 'name = "c_polars_lazy"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && cargo +nightly-2026-07-02 run -q
//     C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_polars_lazy.rs
//   * 若 A/C 红：先看 stderr 首行——mirvm TRAP 按前缀「mirvm[m4-engine]: TRAP:」
//     定位引擎缺口；若 panic 于 native archive 装载（psm 系）对照批6 记档
//     fb0b204；若三维数值/计划文本分叉，先用 S1/S7 的 explain 文本定位优化器
//     层，再用 S3 电池逐列二分表达式。
//
// 三维实测（2026-07-18，全绿）：A/B/C 三进程 stdout 逐字节一致（274 行
// 18927 字节，20 条 anchor 行全过：S1 has_selection/projected/differ 全
// true、S2 opt_consistent=true、S7 caches=2/differ/cse_consistent 全 true、
// S8 has_sort_by=true），stderr 全真空（0 字节）、exit 全 0；A2 复跑与 A
// 逐字节一致（跨进程确定）。时长：A 首轮含 351 crate 依赖闭包首次构建
// ≤90s（构建侧未超预算）；热缓存 A 4.9s；B（cargo 全量构建+运行）59.7s；
// C（JIT=1）4.2s。依赖闭包 351 crate（polars-* 系 22 件 + psm/stacker +
// chrono/chrono-tz）。无 FRONTIER、无引擎 bug 信号。
//
// 覆盖清单：
//   S0  数据锚：orders/quota 全帧 Display + FNV-1a/64。
//   S1  查询优化器计划文本：filter→with_columns→select→sort 链的未优化
//       explain(false) 全文、优化后 explain(true) 全文（断言含 SELECTION=
//       predicate pushdown、PROJECT 列裁剪=projection pushdown），再与三
//       pushdown 全关的优化计划比对 differ=true。
//   S2  优化前后结果对拍：同查询 OptFlags::default() 全开 vs
//       without_optimizations()（仅留 TYPE_COERCION）逐字节一致。
//   S3  表达式电池 ×4 帧：算术/when-then-otherwise/coalesce/rolling_mean/
//       cum_sum/shift/diff/pct_change/rank(Dense)/abs/sign/round/is_between/
//       is_in(Series 字面量)/is_unique/is_first_distinct + unique_counts
//       （按首次出现序，单列独立帧）。
//   S4  group_by 聚合 ×2 帧：sum/mean/count/null_count + skew/kurtosis
//       （moment 面，含同值组 NaN 谱系）。
//   S5  over 窗口：sum/rank/mean over(partition by region) 后 sort id。
//   S6  join 三面：inner（左右各丢行）/ semi / anti（semi_anti_join 面）。
//   S7  CSE：共享 with_columns 基座的 self-join（38 行 n² 扇出，sort
//       [id,id_right] 全序）：默认开 CSE 的 explain(true) 全文（断言
//       caches=2）vs 关 CSE（caches=0、differ=true），两态 collect 结果
//       逐字节一致。
//   S8  slice/limit：sort 降序带 tiebreak + limit(4) 的优化计划锚（SORT BY
//       断言；limit 在 0.44.2 优化计划文本中不单独显式）与 top4 结果。
//   S9  错误路径：select 不存在列的 ColumnNotFound 错误文本。
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

    // S0 数据锚
    show("S0.orders", &orders);
    show("S0.quota", &quota);

    // S1 优化器计划文本：filter→with_columns→select→sort
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

    // S2 优化开/关结果对拍（collect_with_optimizations 双态）
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

    // S3 表达式电池（先锁 id 序，再分三帧打印）
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
    // unique_counts 语义为「按首次出现序输出每个相异值的计数」（长度=相异值
    // 个数≠帧高，进 with_columns 会 ShapeMismatch），单列 select 独立锚。
    let s3d = orders
        .clone()
        .lazy()
        .select([col("region").unique_counts().alias("reg_ucnt")]);
    show("S3d", &s3d.collect()?);

    // S4 group_by 聚合（含 moment 面；sort 唯一键 region 锁序）
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

    // S5 over 窗口（partition by region；窗口 join 后 sort id 锁序）
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

    // S6 join 三面（west 左独有 / central 右独有；semi/anti 走 semi_anti_join）
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

    // S7 CSE：共享 with_columns 基座的 self-join（polars 0.44.2 实证形态——
    // CSE 只对恒等子计划去重；互补 filter 分支的 union 不去重，native 实验
    // 记录见头注「确定性说明」），默认开 CSE vs 显式关，计划与结果双锚。
    // join 键 region 组内 n² 行（38 行），sort [id, id_right] 全序锁死。
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

    // S8 slice/limit：降序 + id tiebreak 全序 + limit(4)
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

    // S9 错误路径：不存在列
    let err = orders
        .lazy()
        .select([col("nope")])
        .collect()
        .unwrap_err();
    println!("S9 err: {err}");

    Ok(())
}
