#!/usr/bin/env mirvm
---
[dependencies]
# ndarray 0.16.1（当时 0.16.x 最新）。0.16 起 matrixmultiply 0.3 是非可选
# 内置依赖（2D dot/gemm 必经其运行期 cpuid 派发的 x86 microkernel），无法
# feature 关闭——本 driver 直探该风险面：若 microkernel 撞未内建 intrinsic，
# 只能退到纯 op 路径（1D inner dot / mat-vec 是 ndarray 自带纯 Rust gemv，
# 不走 matrixmultiply）并在头注记 FRONTIER。
# ndarray-stats 0.6.0（支持 ndarray 0.16 的最新一版）：SummaryStatisticsExt
# （mean/central_moments/skewness/kurtosis/harmonic/geometric）+ QuantileExt
# （argmax/argmin/min/max）。其依赖 rand 仅服务 quickselect pivot，本 driver
# 不用分位数 API，运行期零随机。
ndarray = "=0.16.1"
ndarray-stats = "=0.6.0"
---
// ndarray 0.16 + ndarray-stats 0.6 三维差分：全固定字面量矩阵（无随机源）。
//
// 覆盖测试面：
//   构造    ：arr2! / from_shape_fn / zeros；shape/strides/len 断言
//   切片    ：双轴负步反转 s![..;-1, ..;-2]、子块 s![..2, 1..3]、
//             非连续 column 视图（is_standard_layout=false 的物化考贝）
//   广播    ：3x4 + Array1(4) 广播加、标量乘
//   reshape ：into_shape_with_order((2,6))（Owned C 序免拷贝）+ t() 视图
//   dot     ：1D inner（v·w）、2D mat·vec（ndarray 内部纯 Rust gemv）、
//             2D gemm 3x4·4x2（matrixmultiply 运行期 cpuid 微内核）、
//             8x8 满 tile 见证 gemm（非平凡小数，锚 kernel 选择+累加序：
//             fma 单次舍入与 mul+add 两次舍入在 bits 上可分辨）
//   zip     ：Zip::from(...).and(...).map_collect
//   轴折叠  ：sum_axis(Axis(0))
//   统计    ：mean / central_moments(0..=4) / skewness / kurtosis /
//             harmonic_mean / geometric_mean / argmax / argmin / min / max
//   整数面  ：i32 arr2 sum + iter().product
//
// 确定性：输出只由字面量与 IEEE 754 运算决定；浮点一律 to_bits() 锁位型，
// 汇总走过 FNV-1a；无 HashMap/时间/地址/线程。stderr 真空。
//
// 三维复跑命令（仓库根）：
//   A: target/release/mirvm run corpus/c_ndarray.rs
//   B: cd $(grep -l 'name = "c_ndarray"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_ndarray.rs
//
// FRONTIER：无。matrixmultiply x86 微内核（运行期 cpuid 派发）在解释器/JIT
// 两维均未撞未内建 intrinsic，三维逐字节全绿。
use ndarray::{arr1, arr2, s, Array1, Array2, ArrayView2, Axis, Zip};
use ndarray_stats::{QuantileExt, SummaryStatisticsExt};

/// f64 → 位型 hex。
fn b64(x: f64) -> String {
    format!("{:016x}", x.to_bits())
}

fn fnv_mix(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn fnv2(m: &ArrayView2<f64>) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for (_, &x) in m.indexed_iter() {
        fnv_mix(&mut h, &x.to_bits().to_le_bytes());
    }
    h
}

/// 1D 全元素 bits + FNV 汇总。
fn dump1(tag: &str, v: &Array1<f64>) {
    let mut h = 0xcbf29ce484222325u64;
    for (i, &x) in v.iter().enumerate() {
        println!("{tag}[{i}]={:016x}", x.to_bits());
        fnv_mix(&mut h, &x.to_bits().to_le_bytes());
    }
    println!("{tag} fnv={h:016x}");
}

/// 2D 全元素 bits（indexed_iter 逻辑序）+ FNV 汇总。
fn dump2(tag: &str, m: &ArrayView2<f64>) {
    let mut h = 0xcbf29ce484222325u64;
    for ((i, j), &x) in m.indexed_iter() {
        println!("{tag}[{i},{j}]={:016x}", x.to_bits());
        fnv_mix(&mut h, &x.to_bits().to_le_bytes());
    }
    println!("{tag} fnv={h:016x}");
}

fn main() {
    println!("== ndarray matrix/stats battery ==");

    // ---------- 数据（全字面量） ----------
    let m = arr2(&[
        [1.5f64, -2.0, 3.25, 0.5],
        [-1.0, 4.0, 2.75, -3.5],
        [2.0, 0.25, -0.5, 5.0],
    ]);
    let b = arr2(&[[2.0f64, -1.0], [0.5, 3.0], [-1.5, 1.0], [1.0, 2.5]]);
    let v = arr1(&[1.0f64, 2.5, -3.0, 4.25, 0.5, -1.75]);
    let w = arr1(&[0.5f64, -1.0, 2.0, 1.25, -0.75, 3.0]);
    let row = arr1(&[0.5f64, 1.0, -0.25, 2.0]);
    let p = arr1(&[1.0f64, 2.0, 3.0, 4.0, 5.0, 6.0]);
    let col4 = arr1(&[1.0f64, -2.0, 0.5, 3.0]);

    // ---------- 构造 ----------
    let g: Array2<f64> = Array2::from_shape_fn((2, 3), |(i, j)| (i * 3 + j) as f64 * 0.5 - 1.0);
    let z: Array2<f64> = Array2::zeros((2, 2));
    assert_eq!(m.shape(), &[3, 4]);
    assert_eq!(m.len(), 12);
    assert_eq!(g.shape(), &[2, 3]);
    assert!(z.iter().all(|&x| x == 0.0));
    println!("m shape={:?} strides={:?}", m.shape(), m.strides());
    println!("g.sum={}", b64(g.sum()));

    // ---------- 切片 ----------
    let rev = m.slice(s![..;-1, ..;-2]);
    assert_eq!(rev.shape(), &[3, 2]);
    println!(
        "rev[0]={} rev[last]={} fnv={:016x}",
        b64(rev[(0, 0)]),
        b64(rev[(2, 1)]),
        fnv2(&rev)
    );
    dump2("m.sub", &m.slice(s![..2, 1..3]));
    let colv = m.column(1);
    assert!(!colv.is_standard_layout());
    let col1: Array1<f64> = colv.to_owned();
    dump1("m.col1", &col1);

    // ---------- 广播 ----------
    let badd = &m + &row;
    dump2("m+row", &badd.view());
    let scaled = &m * 1.5f64;
    println!("m*1.5 fnv={:016x}", fnv2(&scaled.view()));

    // ---------- reshape / transpose ----------
    let r = m.clone().into_shape_with_order((2, 6)).unwrap();
    assert_eq!(r.shape(), &[2, 6]);
    println!("r[1,5]={} r fnv={:016x}", b64(r[(1, 5)]), fnv2(&r.view()));
    let rt = r.t();
    println!(
        "rt shape={:?} strides={:?} fnv={:016x}",
        rt.shape(),
        rt.strides(),
        fnv2(&rt)
    );

    // ---------- dot 家族 ----------
    println!("v.dot.w={}", b64(v.dot(&w)));
    let mv = m.dot(&col4);
    dump1("m.dot(col)", &mv);
    let gemm = m.dot(&b);
    dump2("gemm", &gemm.view());

    // ---------- gemm 微内核见证：8x8 满 tile、非平凡小数 ----------
    // （fma 单次舍入 vs mul+add 两次舍入在 bits 上可分辨；锚定 kernel 选择与累加序）
    let wa: Array2<f64> = Array2::from_shape_fn((8, 8), |(i, j)| {
        ((i * 8 + j) as f64 + 1.0) * 1.234_567_890_123_456_7
    });
    let wb: Array2<f64> = Array2::from_shape_fn((8, 8), |(i, j)| {
        (((j * 7 + i) % 11) as f64 - 5.0) * 9.876_543_210_987_654
    });
    let wg = wa.dot(&wb);
    for j in 0..8 {
        println!("wgem[0,{j}]={:016x}", wg[(0, j)].to_bits());
    }
    println!("wgem fnv={:016x}", fnv2(&wg.view()));

    // ---------- zip ----------
    let zz = Zip::from(&m).and(&badd).map_collect(|&x, &y| x * y + 0.25);
    println!("zip fnv={:016x}", fnv2(&zz.view()));

    // ---------- 轴折叠 ----------
    let saxis = m.sum_axis(Axis(0));
    dump1("sum_ax0", &saxis);

    // ---------- 统计（ndarray-stats） ----------
    println!("m.mean={}", b64(m.mean().unwrap()));
    let moms = m.central_moments(4).unwrap();
    assert_eq!(moms.len(), 5); // 返回阶 0..=4（μ0 恒为 1.0）
    for (k, &mu) in moms.iter().enumerate() {
        println!("m.moment{k}={}", b64(mu));
    }
    println!("m.skew={}", b64(m.skewness().unwrap()));
    println!("m.kurt={}", b64(m.kurtosis().unwrap()));
    println!("p.hmean={}", b64(p.harmonic_mean().unwrap()));
    println!("p.gmean={}", b64(p.geometric_mean().unwrap()));
    println!("m.argmax={:?}", m.argmax().unwrap());
    println!("m.argmin={:?}", m.argmin().unwrap());
    println!("v.argmax={:?}", v.argmax().unwrap());
    println!("m.min={}", b64(*m.min().unwrap()));
    println!("m.max={}", b64(*m.max().unwrap()));

    // ---------- 整数面 ----------
    let im = arr2(&[[1i32, 2, 3], [4, 5, 6]]);
    assert_eq!(im.sum(), 21);
    println!("im.sum={} im.prod={}", im.sum(), im.iter().product::<i32>());
}
