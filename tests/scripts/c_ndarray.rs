#!/usr/bin/env mirvm
---
[dependencies]
# ndarray 0.16.1 (newest 0.16.x at the time). Since 0.16, matrixmultiply 0.3 is a
# non-optional built-in dep: 2D dot/gemm pass through its runtime cpuid-
# dispatched x86 microkernel and it cannot be feature-disabled. This driver
# probes that risk: a missing intrinsic forces pure op paths (1D inner dot /
# mat-vec use ndarray's pure-Rust gemv, not matrixmultiply) -> header FRONTIER.
# ndarray-stats 0.6.0 (newest for ndarray 0.16): SummaryStatisticsExt (mean /
# central_moments / skewness / kurtosis / harmonic / geometric) + QuantileExt
# (argmax / argmin / min / max). Its rand dep only serves quickselect pivots;
# the quantile API is unused, so the runtime has zero randomness.
ndarray = "=0.16.1"
ndarray-stats = "=0.6.0"
---
// ndarray 0.16 + ndarray-stats 0.6 three-way differential: fixed literal matrices (no RNG).
//
// Coverage:
//   construction: arr2! / from_shape_fn / zeros; shape/strides/len assertions
//   slicing    : two-axis negative-step reversal s![..;-1, ..;-2], sub-block s![..2,
//                1..3], non-contiguous column view (is_standard_layout=false copy)
//   broadcast  : 3x4 + Array1(4) broadcast add, scalar multiply
//   reshape    : into_shape_with_order((2,6)) (owned C order, no copy) + t() view
//   dot        : 1D inner (v·w), 2D mat·vec (ndarray's internal pure-Rust gemv),
//                2D gemm 3x4·4x2 (matrixmultiply runtime cpuid microkernel),
//                8x8 full-tile witness gemm (non-trivial decimals, anchoring kernel
//                choice + accumulation order: fma's 1 rounding vs mul+add's 2 in bits)
//   zip        : Zip::from(...).and(...).map_collect
//   axis fold  : sum_axis(Axis(0))
//   statistics : mean / central_moments(0..=4) / skewness / kurtosis /
//                harmonic_mean / geometric_mean / argmax / argmin / min / max
//   integers   : i32 arr2 sum + iter().product
//
// determinism: output depends only on literals and IEEE 754 arithmetic; floats are
// to_bits()-locked, rollups use FNV-1a; no HashMap/time/address/thread; stderr empty.
//
// three-way rerun commands (repo root):
//   A: target/release/mirvm run tests/scripts/c_ndarray.rs
//   B: cd $(grep -l 'name = "c_ndarray"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_ndarray.rs
//
// FRONTIER: none. The matrixmultiply x86 microkernel (runtime cpuid dispatch) hit
// no missing intrinsic in the interpreter or JIT; all three runs are byte-identical.
use ndarray::{arr1, arr2, s, Array1, Array2, ArrayView2, Axis, Zip};
use ndarray_stats::{QuantileExt, SummaryStatisticsExt};

/// f64 -> bit-pattern hex.
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

/// All 1D element bits + FNV rollup.
fn dump1(tag: &str, v: &Array1<f64>) {
    let mut h = 0xcbf29ce484222325u64;
    for (i, &x) in v.iter().enumerate() {
        println!("{tag}[{i}]={:016x}", x.to_bits());
        fnv_mix(&mut h, &x.to_bits().to_le_bytes());
    }
    println!("{tag} fnv={h:016x}");
}

/// All 2D element bits (indexed_iter logical order) + FNV rollup.
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

    // ---------- data (all literals) ----------
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

    // ---------- construction ----------
    let g: Array2<f64> = Array2::from_shape_fn((2, 3), |(i, j)| (i * 3 + j) as f64 * 0.5 - 1.0);
    let z: Array2<f64> = Array2::zeros((2, 2));
    assert_eq!(m.shape(), &[3, 4]);
    assert_eq!(m.len(), 12);
    assert_eq!(g.shape(), &[2, 3]);
    assert!(z.iter().all(|&x| x == 0.0));
    println!("m shape={:?} strides={:?}", m.shape(), m.strides());
    println!("g.sum={}", b64(g.sum()));

    // ---------- slicing ----------
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

    // ---------- broadcast ----------
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

    // ---------- dot family ----------
    println!("v.dot.w={}", b64(v.dot(&w)));
    let mv = m.dot(&col4);
    dump1("m.dot(col)", &mv);
    let gemm = m.dot(&b);
    dump2("gemm", &gemm.view());

    // ---------- gemm microkernel witness: 8x8 full tile, non-trivial decimals ----------
    // (fma's 1 rounding vs mul+add's 2 differ in the bits; anchors kernel choice + order)
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

    // ---------- axis fold ----------
    let saxis = m.sum_axis(Axis(0));
    dump1("sum_ax0", &saxis);

    // ---------- statistics (ndarray-stats) ----------
    println!("m.mean={}", b64(m.mean().unwrap()));
    let moms = m.central_moments(4).unwrap();
    assert_eq!(moms.len(), 5); // returns orders 0..=4 (mu0 is always 1.0)
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

    // ---------- integers ----------
    let im = arr2(&[[1i32, 2, 3], [4, 5, 6]]);
    assert_eq!(im.sum(), 21);
    println!("im.sum={} im.prod={}", im.sum(), im.iter().product::<i32>());
}
