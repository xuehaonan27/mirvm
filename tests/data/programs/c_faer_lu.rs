#!/usr/bin/env mirvm
---
[dependencies]
faer = "0.21"
---
// faer 0.21 linear-algebra differential (pulp SIMD runtime dispatch probe). Fixed
// literal/closure matrices, no randomness; Par::Seq pins one thread so reduction order is fixed.
//
// Hazard: faer is used with its default features. Defaults enable pulp/std, whose
//   runtime dispatch picks the V3 path and mask_between reads static LD_ST[544]
//   (pulp build.rs global_asm routine `libpulp_v0_21_5_{ld,st}_b32s_<mask>`); a
//   dependency-crate global_asm symbol has no static definition to resolve, so
//   mirvm TRAPs unless it materializes one. mirvm extracts dependency-crate
//   global_asm at compile time from HIR into a manifest (`.mirasm.s`); the bin
//   materializes it on the same load channel, so the default features work and
//   pulp really takes the LD_ST assembly path. Newer pulp (0.22+) also detects CPU
//   features in no-std, so the old no-std claim must not be generalized.
//
// Coverage: Mat construction (mat! / from_fn / identity) / partial_piv_lu (L, U,
// P/inv permutations, solve, inverse, reconstruct) / full_piv_lu (P and Q) / qr
// (compute_Q, R, square solve) + rectangular least-squares / determinant / exactly
// singular rank-1 (det=0, inverse yields inf/nan bits) / triangular solve residuals /
// Mat Mul (gemm path) / transpose-mul / norm_max / ColRef views. Every f64 prints
// to_bits() to pin the bit pattern, with a trailing per-matrix FNV-1a summary line.
// Deterministic: literals and IEEE 754 arithmetic only; no HashMap/time/address/threads.
use faer::linalg::solvers::{DenseSolveCore, Solve, SolveLstsqCore};
use faer::{Conj, Mat, Par, mat};

fn fnv_mix(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn dump(tag: &str, m: faer::MatRef<'_, f64>) {
    let mut h = 0xcbf29ce484222325u64;
    for i in 0..m.nrows() {
        for j in 0..m.ncols() {
            let v = m[(i, j)];
            let mut bits = v.to_bits();
            // Mask the NaN sign bit: a NaN from NaN arithmetic has an
            // implementation-defined sign (IEEE permits either). For one faer singular
            // inverse: native O0 = 7ff8 (matches mirvm), O3 = fff8, rustc const-eval = +nan.
            // Masking makes them byte-identical; payload and inf/-inf distinction survive,
            // finite bits unchanged. The oracle does not certify LLVM's choice of NaN sign.
            if v.is_nan() {
                bits &= 0x7fff_ffff_ffff_ffff;
            }
            println!("{tag}[{i},{j}]={bits:016x}");
            fnv_mix(&mut h, &bits.to_le_bytes());
        }
    }
    println!(
        "{tag} fnv={h:016x} shape={}x{}",
        m.nrows(),
        m.ncols()
    );
}

fn lu_battery(tag: &str, a: Mat<f64>, b: Mat<f64>) {
    // partial-piv LU: permutation + L/U + solve + inverse
    let lu = a.as_ref().partial_piv_lu();
    {
        let (fwd, inv) = lu.P().arrays();
        println!("{tag}.lu.P_fwd={fwd:?} P_inv={inv:?}");
    }
    dump(&format!("{tag}.lu.L"), lu.L());
    dump(&format!("{tag}.lu.U"), lu.U());
    let x = lu.solve(b.as_ref());
    dump(&format!("{tag}.lu.x"), x.as_ref());
    dump(&format!("{tag}.lu.inv"), lu.inverse().as_ref());
    dump(&format!("{tag}.lu.recon"), lu.reconstruct().as_ref());

    // Residual r = A*x - b (goes through the gemm matmul path)
    let r = a.as_ref() * x.as_ref() - b.as_ref();
    println!("{tag}.lu.resid_norm_max={:016x}", r.as_ref().norm_max().to_bits());

    // full-piv LU: both permutation faces
    let flu = a.as_ref().full_piv_lu();
    {
        let (pf, pi) = flu.P().arrays();
        let (qf, qi) = flu.Q().arrays();
        println!("{tag}.flu.P_fwd={pf:?} P_inv={pi:?} Q_fwd={qf:?} Q_inv={qi:?}");
    }
    dump(&format!("{tag}.flu.U"), flu.U());
}

fn qr_battery(tag: &str, a: Mat<f64>, b: Mat<f64>) {
    let qr = a.as_ref().qr();
    dump(&format!("{tag}.qr.Q"), qr.compute_Q().as_ref());
    dump(&format!("{tag}.qr.R"), qr.R());
    let x = qr.solve(b.as_ref());
    dump(&format!("{tag}.qr.x"), x.as_ref());
}

fn main() {
    println!("== faer 0.21 lu/qr battery ==");
    faer::set_global_parallelism(Par::Seq);
    println!("par=Seq (get={:?})", faer::get_global_parallelism());

    // (1) 4x4: first-column pivot 0.125 is not maximal -> forced row swap; exact binary fractions
    let a4 = mat![
        [0.125, 2.0, -1.0, 3.0],
        [4.0, 1.25, 0.5, -2.0],
        [1.0, -3.0, 2.0, 0.75],
        [2.5, 0.5, 1.0, 1.0],
    ];
    let b4 = mat![[1.0], [2.0], [3.0], [4.0]];
    println!("A4 nrows={} ncols={}", a4.nrows(), a4.ncols());

    lu_battery("A4", a4.clone(), b4.clone());
    qr_battery("A4", a4.clone(), b4.clone());
    println!("A4.det={:016x}", a4.as_ref().determinant().to_bits());

    // two-right-hand-side solve (4x2 Mat rhs)
    let b42 = mat![[1.0, -1.0], [2.0, -2.0], [3.0, -3.0], [4.0, -4.0]];
    let lu42 = a4.as_ref().partial_piv_lu();
    dump("A4.lu.x2", lu42.solve(b42.as_ref()).as_ref());

    // (2) 8x8: from_fn closure builds a diagonally dominant matrix, still exact binary fractions
    let a8 = Mat::from_fn(8usize, 8usize, |i, j| {
        let off = ((i * 7 + j * 13 + 3) % 19) as f64;
        (off - 9.0) / 4.0 + if i == j { 8.0 } else { 0.0 }
    });
    let b8 = Mat::from_fn(8usize, 1usize, |i, _| (i * 3 + 1) as f64 * 0.5);
    lu_battery("A8", a8.clone(), b8.clone());
    qr_battery("A8", a8.clone(), b8.clone());
    println!("A8.det={:016x}", a8.as_ref().determinant().to_bits());

    // (3) rectangular 5x3 QR least-squares (thin Q/R + solve_lstsq_in_place)
    let a53 = Mat::from_fn(5usize, 3usize, |i, j| ((i * 5 + j * 7 + 1) % 11) as f64 / 2.0 - 2.0);
    let b5 = mat![[1.0], [-2.0], [0.5], [3.0], [-0.25]];
    let qr53 = a53.as_ref().qr();
    dump("A53.qr.thinQ", qr53.compute_thin_Q().as_ref());
    dump("A53.qr.thinR", qr53.thin_R());
    let mut lstsq_rhs = b5.clone();
    qr53.solve_lstsq_in_place_with_conj(Conj::No, lstsq_rhs.as_mut());
    // m×n (m≥n): the in-place solution overwrites the first n rows of rhs
    dump("A53.qr.lstsq_x", lstsq_rhs.as_ref().subrows(0usize, 3usize));
    // residual norm: |A*x - b| (this implicitly checks the thin Q^T b side too)
    let x3 = lstsq_rhs.as_ref().subrows(0usize, 3usize);
    let r53 = a53.as_ref() * x3 - b5.as_ref();
    println!("A53.qr.lstsq_resid_norm_max={:016x}", r53.as_ref().norm_max().to_bits());

    // (4) exactly singular rank-1: det=±0; inverse yields nan (deterministic IEEE bits)
    let s4 = Mat::from_fn(4usize, 4usize, |i, j| {
        let v = [1.0f64, 2.0, 3.0, 4.0];
        let w = [0.5f64, -1.0, 2.0, 0.25];
        v[i] * w[j]
    });
    println!("S4.det={:016x}", s4.as_ref().determinant().to_bits());
    let slu = s4.as_ref().partial_piv_lu();
    {
        let (fwd, inv) = slu.P().arrays();
        println!("S4.lu.P_fwd={fwd:?} P_inv={inv:?}");
    }
    dump("S4.lu.U", slu.U());
    dump("S4.lu.inv", slu.inverse().as_ref());

    // (5) Mat basic algebra: Mul (gemm), transpose×self, identity, norm_max, ColRef
    let g = a4.as_ref() * a4.as_ref();
    dump("A4*A4", g.as_ref());
    let t = a4.transpose() * a4.as_ref();
    dump("At*A", t.as_ref());
    let i4 = Mat::<f64>::identity(4usize, 4usize);
    let lu5 = a4.as_ref().partial_piv_lu();
    let inv5 = lu5.inverse();
    let left = inv5.as_ref() * a4.as_ref() - i4.as_ref();
    println!("A4.invA-I.norm_max={:016x}", left.as_ref().norm_max().to_bits());
    let c0 = a4.as_ref().col(0usize);
    let mut csum = 0.0f64;
    for i in 0..c0.nrows() {
        csum += *c0.get(i);
    }
    println!("A4.col0_sum={:016x}", csum.to_bits());

    // (6) f64 edge-value spectrum: ±0, subnormal fraction, huge values -- solve bit comparison
    let edge = mat![
        [1.0e300, 0.0],
        [-0.0, 1.0e-300],
    ];
    let be = mat![[2.0e300], [3.0e-300]];
    let elu = edge.as_ref().partial_piv_lu();
    dump("E.lu.x", elu.solve(be.as_ref()).as_ref());
    println!("E.det={:016x}", edge.as_ref().determinant().to_bits());
}
