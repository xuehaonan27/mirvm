#!/usr/bin/env mirvm
---
[dependencies]
# smartcore 0.4.10（0.4.x 最新；钉 =patch 防漂移），default features——
# 0.4.10 的 default 本身为空集（datasets/serde/std_rand 全关）：不开
# std_rand 时 KMeans RNG 走 crate 经 rand 0.8 vendored 的 SmallRng，
# 纯算法、无 OS 熵源，配 seed: Some(42) 完全确定。依赖闭包 14 个
# crate（num 系 + rand 0.8 + approx + ordered-float），纯 Rust 数值码。
smartcore = "=0.4.10"
---
// smartcore 0.4.10（纯 Rust ML）三维差分三件套：
// ① LinearRegression 双 solver（SVD 默认 + QR）fit 6 样本 2 特征内嵌小数据集，
//    intercept/coefficients/predict 全 to_bits 锚定 + 行列不匹配错误文案；
// ② KMeans k=3、seed=Some(42) 定种拟合 9 个 well-separated 点，
//    labels 序列 + 簇 size + 派生质心 to_bits 锚定 + k=1 错误文案——
//    注：0.4.10 的 KMeans 字段全私有、无 centroids() 访问器（公开面只有
//    fit/predict），质心按 predict 的 labels 求各簇均值派生（公式确定、
//    两侧同码），labels 本身即模型输出锚点；
// ③ metrics：r2 与 mean_squared_error 自由函数对两个 solver 的预测打分，
//    to_bits 锚定（两 solver 末位 ulp 有别，恰好能多压一层数值分歧面）。
//
// 确定性：数据全内嵌字面量；KMeans 初值由 SmallRng(seed=42) 唯一决定；
// 无 IO/时间/线程/哈希序；浮点一律 to_bits；stderr 真空（driver 零 warning）。
//
// 三维复跑命令（仓库根）：
//   A: target/release/mirvm run corpus/c_smartcore.rs
//   B: cd $(grep -l 'name = "c_smartcore"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_smartcore.rs
//
// FRONTIER：无（期待全绿）。
use smartcore::cluster::kmeans::{KMeans, KMeansParameters};
use smartcore::linalg::basic::arrays::Array;
use smartcore::linalg::basic::matrix::DenseMatrix;
use smartcore::linear::linear_regression::{
    LinearRegression, LinearRegressionParameters, LinearRegressionSolverName,
};
use smartcore::metrics;

/// f64 → 位型 hex（确定性锚点）。
fn b(x: f64) -> String {
    format!("{:016x}", x.to_bits())
}

/// LinearRegression：fit + 系数 + 逐点 predict + r2/mse，全 bits 锚定。
fn lr_case(tag: &str, x: &DenseMatrix<f64>, y: &Vec<f64>, solver: LinearRegressionSolverName) {
    let lr = LinearRegression::fit(x, y, LinearRegressionParameters::default().with_solver(solver))
        .unwrap();
    println!("{tag} intercept={}", b(*lr.intercept()));
    let coef = lr.coefficients();
    let (rows, _) = coef.shape();
    for i in 0..rows {
        println!("{tag} coef[{i}]={}", b(*coef.get((i, 0))));
    }
    let y_hat = lr.predict(x).unwrap();
    for (i, p) in y_hat.iter().enumerate() {
        println!("{tag} pred[{i}]={}", b(*p));
    }
    println!("{tag} r2={}", b(metrics::r2(y, &y_hat)));
    println!("{tag} mse={}", b(metrics::mean_squared_error(y, &y_hat)));
}

fn main() {
    // ---- ① LinearRegression 小数据集（2 特征 × 6 样本）----
    let x = DenseMatrix::from_2d_array(&[
        &[1.0, 2.0],
        &[2.0, 1.0],
        &[3.0, 5.0],
        &[4.0, 2.0],
        &[5.0, 6.0],
        &[6.0, 4.0],
    ])
    .unwrap();
    let y: Vec<f64> = vec![3.0, 4.0, 8.0, 6.0, 11.0, 10.0];

    println!("lr dataset n=6 d=2");
    lr_case("lr-svd", &x, &y, LinearRegressionSolverName::SVD);
    lr_case("lr-qr", &x, &y, LinearRegressionSolverName::QR);

    // 行列不匹配 → 确定性错误文案。
    let y_short: Vec<f64> = vec![1.0, 2.0];
    match LinearRegression::<f64, f64, DenseMatrix<f64>, Vec<f64>>::fit(
        &x,
        &y_short,
        Default::default(),
    ) {
        Ok(_) => println!("lr mismatch unexpected ok"),
        Err(e) => println!("lr mismatch err={e}"),
    }

    // ---- ② KMeans 定种拟合（k=3，3 簇 × 3 点，well-separated）----
    let data = DenseMatrix::from_2d_array(&[
        &[0.0, 0.5],
        &[0.5, 0.0],
        &[0.1, 0.2],
        &[5.0, 5.0],
        &[5.5, 5.2],
        &[4.9, 5.1],
        &[10.0, 0.0],
        &[10.2, 0.3],
        &[9.8, 0.1],
    ])
    .unwrap();
    let km = KMeans::<f64, usize, DenseMatrix<f64>, Vec<usize>>::fit(
        &data,
        KMeansParameters {
            k: 3,
            seed: Some(42),
            ..Default::default()
        },
    )
    .unwrap();
    let labels = km.predict(&data).unwrap();
    let seq: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    println!("km labels={}", seq.join(","));

    // 质心 = 各簇均值（labels 派生，见头注）。
    let mut sums = vec![[0f64; 2]; 3];
    let mut size = vec![0usize; 3];
    for (i, &l) in labels.iter().enumerate() {
        sums[l][0] += *data.get((i, 0));
        sums[l][1] += *data.get((i, 1));
        size[l] += 1;
    }
    println!("km sizes={},{},{}", size[0], size[1], size[2]);
    for c in 0..3 {
        for j in 0..2 {
            println!("km centroid[{c}][{j}]={}", b(sums[c][j] / size[c] as f64));
        }
    }

    // k=1 → 确定性错误文案。
    match KMeans::<f64, usize, DenseMatrix<f64>, Vec<usize>>::fit(
        &data,
        KMeansParameters {
            k: 1,
            seed: Some(42),
            ..Default::default()
        },
    ) {
        Ok(_) => println!("km k=1 unexpected ok"),
        Err(e) => println!("km k=1 err={e}"),
    }
}
