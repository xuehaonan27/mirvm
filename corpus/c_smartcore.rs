#!/usr/bin/env mirvm
---
[dependencies]
# smartcore pinned to =0.4.10 (the latest 0.4.x) with default features, so patch
# drift cannot move it. Those defaults are empty (datasets/serde/std_rand off):
# without std_rand the KMeans RNG is the crate's vendored rand 0.8 SmallRng, a
# pure algorithm with no OS entropy source, so seed: Some(42) is fully
# deterministic. The closure is 14 crates (num, rand 0.8, approx, ordered-float).
smartcore = "=0.4.10"
---
// The "smartcore" crate is exercised in three parts, all compared with native:
// (1) LinearRegression with both solvers (SVD by default, plus QR) fitted on a
//     small embedded set of 6 samples x 2 features: intercept, coefficients and
//     per-point predictions are anchored as to_bits, and a shape mismatch hits a
//     deterministic error message.
// (2) KMeans with k=3 and seed: Some(42) on 9 well-separated points: the label
//     sequence, cluster sizes and derived centroids are anchored as to_bits, as
//     is the k=1 error message. Note that in 0.4.10 the KMeans fields are private
//     and there is no centroids() accessor (the public surface is fit/predict), so
//     the centroid is derived by averaging each cluster's points per the predicted
//     labels; the labels themselves are the model-output anchor.
// (3) metrics: the r2 and mean_squared_error free functions score both solvers'
//     predictions, anchored as to_bits (the two solvers differ in the last ulp,
//     which puts one more layer of numeric divergence under test).
// Deterministic: all data is embedded literals, the KMeans initialization follows
// uniquely from SmallRng(seed=42), and there is no IO, time, thread or hash-order
// input; floats are always to_bits and stderr stays empty. No frontier issues.
//
//
//
//
//
//
use smartcore::cluster::kmeans::{KMeans, KMeansParameters};
use smartcore::linalg::basic::arrays::Array;
use smartcore::linalg::basic::matrix::DenseMatrix;
use smartcore::linear::linear_regression::{
    LinearRegression, LinearRegressionParameters, LinearRegressionSolverName,
};
use smartcore::metrics;

/// f64 -> bit-pattern hex (the deterministic anchor).
fn b(x: f64) -> String {
    format!("{:016x}", x.to_bits())
}

/// LinearRegression: fit, coefficients, per-point predict and r2/mse, all as bits.
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
    // ---- (1) LinearRegression on a small set (2 features x 6 samples) ----
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

    // A shape mismatch produces a deterministic error message.
    let y_short: Vec<f64> = vec![1.0, 2.0];
    match LinearRegression::<f64, f64, DenseMatrix<f64>, Vec<f64>>::fit(
        &x,
        &y_short,
        Default::default(),
    ) {
        Ok(_) => println!("lr mismatch unexpected ok"),
        Err(e) => println!("lr mismatch err={e}"),
    }

    // ---- (2) seeded KMeans (k=3, 3 clusters x 3 well-separated points) ----
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

    // Centroids are the per-cluster means derived from the labels (see the header).
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

    // k=1 produces a deterministic error message.
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
