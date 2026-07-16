#!/usr/bin/env mirvm
---
[dependencies]
nalgebra = "0.33"
---
// nalgebra 0.33 线性代数浮点重差分。固定字面量矩阵（无随机源）：
//   spd3  = 3x3 对称正定（对角占优）
//   indef = 3x3 对称不定（cholesky 失败路径）
//   psd3  = 3x3 rank-1 半正定（精确奇异，try_inverse/solve 失败路径）
//   gen4  = 4x4 一般（非对称，复特征值路径）
//   hilb4 = 4x4 Hilbert 病态/近奇异（A[i][j]=1/(i+j+1)）
//   spd3f = 3x3 f32 对称正定（f32 路径）
// 覆盖：cholesky(L bits 与不定 None) / LU(solve b 固定 + is_invertible) /
// QR(Q、R 全元素 bits) / SVD(singular_values bits + rank) /
// SymmetricEigen(eigenvalues bits + eigenvectors bits) / 非对称 Complex
// eigenvalues / determinant / try_inverse(Some/None) / 矩阵-向量乘 /
// 范数族(norm/norm_squared/lp1/lp3/normalize/dot/metric_distance/frobenius/
// trace/amax)。全部 f64 打印 to_bits() 锁位型，外加每矩阵 FNV-1a 汇总行。
// 确定性：输出只由字面量与 IEEE 754 基本运算决定；无 HashMap/时间/地址。
use nalgebra::{Matrix3, Matrix4, SMatrix, SVector, SymmetricEigen, Vector3, Vector4};

fn fnv_mix(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn b64(x: f64) -> String {
    format!("{:016x}", x.to_bits())
}

fn b32(x: f32) -> String {
    format!("{:08x}", x.to_bits())
}

fn dumpm<const R: usize, const C: usize>(tag: &str, m: &SMatrix<f64, R, C>) {
    let mut h = 0xcbf29ce484222325u64;
    for i in 0..R {
        for j in 0..C {
            let bits = m[(i, j)].to_bits();
            println!("{tag}[{i},{j}]={bits:016x}");
            fnv_mix(&mut h, &bits.to_le_bytes());
        }
    }
    println!("{tag} fnv={h:016x}");
}

fn dumpmf32<const R: usize, const C: usize>(tag: &str, m: &SMatrix<f32, R, C>) {
    let mut h = 0xcbf29ce484222325u64;
    for i in 0..R {
        for j in 0..C {
            let bits = m[(i, j)].to_bits();
            println!("{tag}[{i},{j}]={bits:08x}");
            fnv_mix(&mut h, &bits.to_le_bytes());
        }
    }
    println!("{tag} fnv={h:016x}");
}

fn dumpv<const N: usize>(tag: &str, v: &SVector<f64, N>) {
    let mut h = 0xcbf29ce484222325u64;
    for i in 0..N {
        let bits = v[i].to_bits();
        println!("{tag}[{i}]={bits:016x}");
        fnv_mix(&mut h, &bits.to_le_bytes());
    }
    println!("{tag} fnv={h:016x}");
}

fn dumpvf32<const N: usize>(tag: &str, v: &SVector<f32, N>) {
    let mut h = 0xcbf29ce484222325u64;
    for i in 0..N {
        let bits = v[i].to_bits();
        println!("{tag}[{i}]={bits:08x}");
        fnv_mix(&mut h, &bits.to_le_bytes());
    }
    println!("{tag} fnv={h:016x}");
}

fn main() {
    println!("== nalgebra_la f64/f32 decomposition battery ==");

    let spd3 = Matrix3::new(4.0, 1.5, -0.5, 1.5, 3.0, 0.25, -0.5, 0.25, 2.0);
    let indef = Matrix3::new(2.0, -3.0, 1.0, -3.0, 0.5, 2.5, 1.0, 2.5, -1.0);
    let psd3 = Matrix3::new(1.0, 2.0, 3.0, 2.0, 4.0, 6.0, 3.0, 6.0, 9.0);
    let gen4 = Matrix4::new(
        2.0, -1.0, 0.5, 3.0, //
        0.0, 1.5, 2.0, -1.0, //
        -3.0, 0.25, 4.0, 0.75, //
        1.0, 2.0, -2.0, 1.5,
    );
    let mut hilb4 = Matrix4::zeros();
    for i in 0..4 {
        for j in 0..4 {
            hilb4[(i, j)] = 1.0 / ((i + j + 1) as f64);
        }
    }
    let b3 = Vector3::new(1.0, -2.0, 3.5);
    let b4 = Vector4::new(2.0, 0.5, -1.0, 4.0);

    // —— cholesky：SPD 出 L；不定/psd-rank1 失败路径 ——
    match spd3.cholesky() {
        Some(ch) => dumpm("spd3.chol.L", &ch.unpack()),
        None => println!("spd3.chol none"),
    }
    println!("indef.chol.is_none={}", indef.cholesky().is_none());
    println!("psd3.chol.is_none={}", psd3.cholesky().is_none());

    // —— LU：solve 固定 b；精确奇异 → None ——
    let lu = spd3.lu();
    println!("spd3.lu.invertible={}", lu.is_invertible());
    match lu.solve(&b3) {
        Some(x) => dumpv("spd3.lu.x", &x),
        None => println!("spd3.lu.x none"),
    }
    let lu4 = gen4.lu();
    println!("gen4.lu.invertible={}", lu4.is_invertible());
    match lu4.solve(&b4) {
        Some(x) => dumpv("gen4.lu.x", &x),
        None => println!("gen4.lu.x none"),
    }
    match hilb4.lu().solve(&b4) {
        Some(x) => dumpv("hilb4.lu.x", &x),
        None => println!("hilb4.lu.x none"),
    }
    println!(
        "psd3.lu.invertible={} solve_none={}",
        psd3.lu().is_invertible(),
        psd3.lu().solve(&b3).is_none()
    );

    // —— 行列式（含病态 ~1.65e-7 与精确 0）——
    println!("spd3.det={}", b64(spd3.determinant()));
    println!("gen4.det={}", b64(gen4.determinant()));
    println!("hilb4.det={}", b64(hilb4.determinant()));
    println!("psd3.det={}", b64(psd3.determinant()));

    // —— 逆：Some/None 双路径 ——
    match spd3.try_inverse() {
        Some(m) => dumpm("spd3.inv", &m),
        None => println!("spd3.inv none"),
    }
    match gen4.try_inverse() {
        Some(m) => dumpm("gen4.inv", &m),
        None => println!("gen4.inv none"),
    }
    match hilb4.try_inverse() {
        Some(m) => dumpm("hilb4.inv", &m),
        None => println!("hilb4.inv none"),
    }
    println!("psd3.inv.is_none={}", psd3.try_inverse().is_none());

    // —— QR：Q、R 全元素 bits 谱（一般矩阵 + 病态矩阵）——
    let (q, r) = gen4.qr().unpack();
    dumpm("gen4.qr.Q", &q);
    dumpm("gen4.qr.R", &r);
    let (qh, rh) = hilb4.qr().unpack();
    dumpm("hilb4.qr.Q", &qh);
    dumpm("hilb4.qr.R", &rh);

    // —— SVD：奇异值 bits + rank（病态 σ min 与 rank-1 σ 谱）——
    let svd4 = gen4.svd(true, true);
    dumpv("gen4.svd.s", &svd4.singular_values);
    println!("gen4.svd.rank@1e-10={}", svd4.rank(1e-10));
    let svdh = hilb4.svd(true, true);
    dumpv("hilb4.svd.s", &svdh.singular_values);
    println!("hilb4.svd.rank@1e-10={}", svdh.rank(1e-10));
    let svdp = psd3.svd(true, true);
    dumpv("psd3.svd.s", &svdp.singular_values);
    println!("psd3.svd.rank@1e-10={}", svdp.rank(1e-10));

    // —— SymmetricEigen：SPD / 不定 / rank-1 半正定 ——
    let se = SymmetricEigen::new(spd3);
    dumpv("spd3.eig.vals", &se.eigenvalues);
    dumpm("spd3.eig.vecs", &se.eigenvectors);
    let sei = SymmetricEigen::new(indef);
    dumpv("indef.eig.vals", &sei.eigenvalues);
    let sep = SymmetricEigen::new(psd3);
    dumpv("psd3.eig.vals", &sep.eigenvalues);

    // —— 实 Schur 分解的复特征值（含 2x2 块 → 共轭对路径）——
    let ces = gen4.schur().complex_eigenvalues();
    for (i, e) in ces.iter().enumerate() {
        println!(
            "gen4.ceig[{i}] re={:016x} im={:016x}",
            e.re.to_bits(),
            e.im.to_bits()
        );
    }

    // —— 矩阵-向量乘 ——
    dumpv("gen4*vb", &(gen4 * b4));
    dumpv("spd3*v3", &(spd3 * b3));

    // —— 范数族 ——
    let v = Vector3::new(3.0, -4.0, 1.5);
    let w = Vector3::new(-1.0, 2.0, 0.5);
    println!("v.norm={}", b64(v.norm()));
    println!("v.norm2={}", b64(v.norm_squared()));
    println!("v.lp1={}", b64(v.lp_norm(1)));
    println!("v.lp3={}", b64(v.lp_norm(3)));
    dumpv("v.unit", &v.normalize());
    println!("v.dot.w={}", b64(v.dot(&w)));
    println!("v.dist.w={}", b64(v.metric_distance(&w)));
    println!("gen4.fro={}", b64(gen4.norm()));
    println!("gen4.trace={}", b64(gen4.trace()));
    println!("gen4.amax={}", b64(gen4.amax()));

    // —— f32 路径：3x3 SPD 全链条 ——
    let spd3f = Matrix3::<f32>::new(3.0, 0.5, 0.25, 0.5, 2.0, 1.0, 0.25, 1.0, 4.0);
    let b3f = Vector3::<f32>::new(1.0, 2.0, 3.0);
    println!("spd3f.det={}", b32(spd3f.determinant()));
    match spd3f.lu().solve(&b3f) {
        Some(x) => dumpvf32("spd3f.lu.x", &x),
        None => println!("spd3f.lu.x none"),
    }
    match spd3f.cholesky() {
        Some(ch) => dumpmf32("spd3f.chol.L", &ch.unpack()),
        None => println!("spd3f.chol none"),
    }
    match spd3f.try_inverse() {
        Some(m) => dumpmf32("spd3f.inv", &m),
        None => println!("spd3f.inv none"),
    }
    let sef = SymmetricEigen::new(spd3f);
    dumpvf32("spd3f.eig.vals", &sef.eigenvalues);
}
