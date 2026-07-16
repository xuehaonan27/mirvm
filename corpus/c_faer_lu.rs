#!/usr/bin/env mirvm
---
[dependencies]
faer = { version = "0.21", default-features = false }
---
// faer 0.21 线性代数差分（pulp SIMD 运行时分发探测）。固定字面量/闭包矩阵，
// 无随机源；set_global_parallelism(Par::Seq) 锁单线程使并行归约序不泄露线程数。
//
// 特性抉择（FRONTIER 绕行，实测三维全绿前提）：
//   默认特性（std/rayon/…）开启 pulp/std → pulp V3(AVX2) 运行期检测命中
//   → 任何 lu/qr/det 因子化的 mask_between 路径读 static LD_ST[544]（pulp
//   build.rs 生成的 global_asm 例程 `libpulp_v0_21_5_{ld,st}_b32s_<mask>`，
//   该表取全部 544 个 extern fn 地址）→ 依赖 crate global_asm 符号不在 bin
//   mono 流、rlib 因 -Zno-codegen 无本机对象，mirvm 响亮 TRAP：
//     TRAP: extern fn `libpulp_v0_21_5_ld_b32s_0000000000000000` 被当作值取址，
//           但符号未命中（归档兜底表 / dlsym 全域均无）
//   default-features=false 时 pulp 无 std 检测面 → V3::try_new()=false（native
//   与 mirvm 两侧同此逻辑，档位一致）→ 落到标量内核，不触 LD_ST。该绕行语义
//   上同时关掉 rayon/npy/rand/sparse-linalg——本驱动均不使用，API 面（linalg
//   求解器族）完整保留。
//
// 覆盖：Mat 构造（mat! 宏 / from_fn 闭包 / identity）/ partial_piv_lu
// （L、U、P/inv 置换、solve、inverse、reconstruct）/ full_piv_lu（P、Q 双置换）/
// qr（compute_Q、R、方阵 solve）+ 矩形 least-squares / determinant /
// 精确奇异 rank-1 矩阵（det=0、inverse 出 inf/nan bits）/ 三角解残差 /
// Mat Mul（gemm 路径）/ transpose-mul / norm_max / ColRef 视图。
// 全部 f64 打印 to_bits() 锁位型，尾随每矩阵 FNV-1a 汇总行。
// 确定性：输出只由字面量与 IEEE 754 基本运算决定；无 HashMap/时间/地址/线程数。
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
            let bits = m[(i, j)].to_bits();
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
    // partial-piv LU：置换 + L/U + solve + inverse
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

    // 残差 r = A*x - b（走 gemm matmul 路径）
    let r = a.as_ref() * x.as_ref() - b.as_ref();
    println!("{tag}.lu.resid_norm_max={:016x}", r.as_ref().norm_max().to_bits());

    // full-piv LU：双置换面
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

    // ① 4x4：首列主元 0.125 非最大 → 强制行交换；均为二进制精确分数
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

    // 双右端 solve（Mat 4x2 rhs）
    let b42 = mat![[1.0, -1.0], [2.0, -2.0], [3.0, -3.0], [4.0, -4.0]];
    let lu42 = a4.as_ref().partial_piv_lu();
    dump("A4.lu.x2", lu42.solve(b42.as_ref()).as_ref());

    // ② 8x8：from_fn 闭包构对角占优矩阵，元素仍为精确二进制分数
    let a8 = Mat::from_fn(8usize, 8usize, |i, j| {
        let off = ((i * 7 + j * 13 + 3) % 19) as f64;
        (off - 9.0) / 4.0 + if i == j { 8.0 } else { 0.0 }
    });
    let b8 = Mat::from_fn(8usize, 1usize, |i, _| (i * 3 + 1) as f64 * 0.5);
    lu_battery("A8", a8.clone(), b8.clone());
    qr_battery("A8", a8.clone(), b8.clone());
    println!("A8.det={:016x}", a8.as_ref().determinant().to_bits());

    // ③ 矩形 5x3 QR least-squares（thin Q/R + solve_lstsq_in_place）
    let a53 = Mat::from_fn(5usize, 3usize, |i, j| ((i * 5 + j * 7 + 1) % 11) as f64 / 2.0 - 2.0);
    let b5 = mat![[1.0], [-2.0], [0.5], [3.0], [-0.25]];
    let qr53 = a53.as_ref().qr();
    dump("A53.qr.thinQ", qr53.compute_thin_Q().as_ref());
    dump("A53.qr.thinR", qr53.thin_R());
    let mut lstsq_rhs = b5.clone();
    qr53.solve_lstsq_in_place_with_conj(Conj::No, lstsq_rhs.as_mut());
    // m×n (m≥n)：in-place 解覆写 rhs 前 n 行
    dump("A53.qr.lstsq_x", lstsq_rhs.as_ref().subrows(0usize, 3usize));
    // 残差范数：|A*x - b|（thin Q^T b 面也隐式对已覆盖）
    let x3 = lstsq_rhs.as_ref().subrows(0usize, 3usize);
    let r53 = a53.as_ref() * x3 - b5.as_ref();
    println!("A53.qr.lstsq_resid_norm_max={:016x}", r53.as_ref().norm_max().to_bits());

    // ④ 精确奇异 rank-1：det=±0；inverse 出 nan（确定性 IEEE 位型）
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

    // ⑤ Mat 基础代数面：Mul（gemm）、transpose×self、identity、norm_max、ColRef
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

    // ⑥ f64 边界值谱：±0、非规范小数、大数 —— solve 位型对拍
    let edge = mat![
        [1.0e300, 0.0],
        [-0.0, 1.0e-300],
    ];
    let be = mat![[2.0e300], [3.0e-300]];
    let elu = edge.as_ref().partial_piv_lu();
    dump("E.lu.x", elu.solve(be.as_ref()).as_ref());
    println!("E.det={:016x}", edge.as_ref().determinant().to_bits());
}
