#!/usr/bin/env mirvm
---
[dependencies]
polars = { version = "0.44", default-features = false, features = ["lazy", "dtype-date", "dtype-datetime", "fmt_no_tty"] }
---
// polars 0.44.2 差分：Arrow2 列式 DataFrame 全链路（eager 构造/过滤/group-by
// 聚合 + lazy 表达式链/内连接 + 统计摘要 + null 谱系）。feature 面 = lazy
// （query 引擎）+ dtype-date（Date 逻辑型，拉 temporal→chrono）+ dtype-datetime
// + fmt_no_tty（DataFrame Display，绕开 tty 宽度探测走固定 fallback 宽度，
// 管线下确定）。
//
// 【FRONTIER · mirvm 两维当前不可跑，driver native 已验证为绿】
// polars 依赖图必含 psm（非可选边 psm ← stacker ← polars-utils ← polars；
// feature/版本/API 都无法摘除）。psm 0.1.31 编译 x86_64 汇编成 libpsm.a，
// dynsym 可见导出 rust_psm_on_stack 等四符号；而 mirvm 进程的 RTLD_DEFAULT
// 里已有同名定义——librustc_driver-*.so（mirvm 以 rustc as library 链接）
// 导出自 rustc 查询系统递归保护所用的 stacker/psm（nm -D 实证）。native_archive 因此按设计拒绝
// （dynsym 碰撞无法从 dlsym 优先级复现 native linker 顺序；hidden 碰撞才有
// symtab 兜底）。归档物化发生在整个模块降低前，与 driver 调用路径无关。
// 诊断原文（panic，exit 101）：
//   thread 'rustc' panicked at src/lower/mod.rs:1646:38:
//   Static native library 装载失败: 静态归档
//   `/home/xuehaonan/.cache/mirvm/native-archives/38daa748a74132cfe2c177878f86f965.so`
//   导出符号 `rust_psm_on_stack`，但 RTLD_DEFAULT 已有同名定义；mirvm 的
//   dlsym 优先级无法无歧义复现 native linker 顺序
// native 侧（原生链接只含 crate 自己的 psm）一切正常。
//
// 路线说明：
// - 【上游 feature 破洞绕行】polars-io 0.44.2 的 csv/write datetime serializer
//   无条件引用 chrono，而 chrono 依赖只由 dtype-datetime 挂上；lazy → polars-
//   plan 又会无条件开 polars-io/csv。default-features=false + lazy + dtype-date
//   组合因此直接 E0433 编不过（上游默认 feature 组合里 chrono 恰好在场没暴露）。
//   加 dtype-datetime 把 chrono 拉回依赖图，语义覆盖不变。
// - 【手写 describe】DataFrame::describe() 方法 0.45 才落地，0.44 只有空的
//   "describe" feature 桩（polars-core/Cargo.toml 里 describe = []，源码无
//   对应方法）——本 driver 用 Series 归约内核（mean/std/median/min/max +
//   null_count）等效实现 describe 摘要，f64 结果一律 to_bits() 锁位。
// - 【确定性】group-by 输出序依赖哈希表布局（polars 明确不保证次序）→ 三个
//   聚合结果统一 sort 唯一键后打印；join 输出行序同理 sort 唯一 id。mean 只对
//   Int64 列做（整数部分和精确，除法位确定），不向并行分区顺序让位。
// - 【零随机/零环境】数据全内联固定；不碰 now()/随机采样/环境变量。
// 覆盖：i64/f64/str/bool/date 五 dtype 的 Series 构造（含 Option 列）、schema
// 迭代、BooleanChunked 掩码 eager filter（含长度失配错误路径）、lazy group_by
// sum/mean/count（多键 dtype）、lazy sort、with_columns 表达式链（算术乘/
// 标量混合/比较/alias × 三链）+ select、inner join（左右各丢一行）、to_string
// FNV 锚、AnyValue 打印、null 谱系（null_count/is_null 计数/rechunk 后
// validity 位图存在性/drop_nulls 长度/is_null 掩码过滤）。
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
    // ① 五 dtype 建帧（date 经 Int32 物理型 cast，含一个 null）
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

    // ② schema（IndexMap 插入序、dtype Display）
    for (name, dtype) in df.schema().iter() {
        println!("schema {name}: {dtype}");
    }

    // ③ 全帧 Display（comfy-table 固定 fallback 宽度）+ FNV 字节锚
    let tbl = df.to_string();
    println!("{tbl}");
    println!(
        "frame bytes len={} fnv={:#018x}",
        tbl.len(),
        fnv1a(tbl.as_bytes())
    );

    // ④ eager filter：布尔掩码 + 长度失配错误路径
    let mask = df.column("qty")?.i64()?.gt_eq(8);
    let f = df.filter(&mask)?;
    println!("{f}");
    println!("filter height={} width={}", f.height(), f.width());
    let bad = df
        .filter(&BooleanChunked::new("m".into(), [true, false]))
        .unwrap_err();
    println!("filter err: {bad}");

    // ⑤ lazy group_by × (sum/mean/count)：哈希序 → sort 唯一键锁序
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
    // bool 键 group-by（dtype 覆盖）
    let gbool = df
        .clone()
        .lazy()
        .group_by([col("active")])
        .agg([col("qty").sum()])
        .sort(["active"], Default::default())
        .collect()?;
    println!("{gbool}");

    // ⑥ lazy with_columns 表达式链（qty*price → amount → taxed → 布尔标记
    //    + 标量混合算术）+ select 收口
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

    // ⑦ inner join：hz（左独有）/gz（右独有）双方丢行；sort 唯一 id 锁序
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

    // ⑧ describe 摘要（0.44 手写版）：均值/标准差/中位数 to_bits 锁位
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

    // ⑨ null 谱系：四种 dtype 的 Option 列
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
    // is_null 掩码作过滤（bool 表达式的 arrow 侧消费）
    let null_b = nn.filter(&nn.column("b")?.as_materialized_series().is_null())?;
    println!("null_b height={}", null_b.height());

    Ok(())
}
