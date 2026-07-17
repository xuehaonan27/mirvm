#!/usr/bin/env mirvm
---
[dependencies]
candle-core = "0.9"
---
// candle-core 0.9 CPU mini MLP 差分。选版：candle-core = "0.9" → 解析 0.9.2
// （0.9 线最新稳定版；批9 规格钦定 0.9 大版本）。
//
// 覆盖清单（批9 规格）：
//   ① from_bits 定值 f32 权重（W1:4x8 / b1:8 / W2:8x2 / b2:2），全程无 rand；
//   ② Tensor::from_vec 建张量 + dtype/shape 文本锚定（输入回显 + 权重回显）；
//   ③ 2 层 MLP 前向：matmul（gemm 后端，(1,4)x(4,8) 与 (1,8)x(8,2)）
//      + 每层 broadcast_add 偏置（(1,8)+(8,) 与 (1,2)+(2,) 秩广播面）；
//   ④ 激活函数写死 relu（clamp(0, f64::MAX) 实现，h1_pre 含正负混合确保
//      钳零/保持两分支都命中）；
//   ⑤ softmax：max_keepdim(1) → broadcast_sub → exp → sum_keepdim(1) →
//      broadcast_div，中间量（max/exp/sum）与概率全部打印 to_bits()；
//   ⑥ 2-batch 前向（X2:(2,4)x(4,8)）复用同一管线，覆盖 gemm 另一 m 档；
//   ⑦ 每段张量尾随 FNV-1a 汇总行（位型级指纹）。
//
// 确定性说明：权重/输入全部 f32::from_bits 定值；无随机源、无壁钟、无
// HashMap 迭代序、无裸地址；浮点只打印 to_bits()（{:08x}），排序固定按行主
// 序遍历；stderr 保持真空。candle CPU 前向为逐元素确定循环（gemm 块分解只
// 依赖形状/线程数，三维同机同配置），softmax 折减序由张量布局确定。
//
// 绕行/钉版本记录：无任何绕行。钉 candle-core = "0.9"（批9 规格钦定大版本）
// → Cargo.lock 解析 0.9.2（0.9 线最新稳定版，crates.io 2026-07 时点）。
// 依赖链风险评估与实证：
//   candle-core 0.9.x 的 default = []（0.9 起特性面重排：avx/f16/mkl/rayon
//   开关全部移除，0.8 及以前那些自愿降档点不存在，无可调）；gemm = "^0.19"
//   为非可选普通依赖，CPU matmul（mkl/accelerate 均未开）无条件走
//   gemm::gemm（src/cpu_backend/mod.rs Map2 for MatMul）→ gemm-f32 0.19.0
//   → pulp 0.22.3（gemm-common 对 pulp default-features=false 且不带 std
//   → pulp/std 未被任何边启用，faer 批6 头注所述 LD_ST global_asm 取址
//   TRAP 面不触发；pulp 此行只能走 cpuid 自检测，两侧同一逻辑档位一致）。
//   实测三维全绿，无红出无绕行必要（若未来 pulp 版本换了 std 检测面导致
//   复红，先例绕行法 = 钉 pulp/gemm 到无 std 检测版本）。
// 三维验收（2026-07-18，Linux x86_64 8 核）：
//   A mirvm 默认 29s（含首次依赖构建）/ B native cargo run 71s（含全量
//   debug 构建）/ C JIT=1 0.4s；stdout 各 229 行，stderr 三维全空，
//   exit 全 0；A/B/C stdout+stderr 逐字节一致（baeb4df9… 相同 sha256）。
use candle_core::{Device, Tensor};

fn fnv_mix(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn dump(tag: &str, t: &Tensor) {
    let rows = t.to_vec2::<f32>().expect("to_vec2");
    let mut h = 0xcbf29ce484222325u64;
    for (i, row) in rows.iter().enumerate() {
        for (j, &v) in row.iter().enumerate() {
            let bits = v.to_bits();
            println!("{tag}[{i},{j}]={bits:08x}");
            fnv_mix(&mut h, &bits.to_le_bytes());
        }
    }
    let d = t.dims();
    println!("{tag} fnv={h:016x} shape={}x{} dtype={:?}", d[0], d[1], t.dtype());
}

// 一层：pre = x.matmul(w).broadcast_add(b)，可选 relu
fn layer(pre_tag: &str, act_tag: &str, x: &Tensor, w: &Tensor, b: &Tensor, relu: bool) -> Tensor {
    let pre = x.matmul(w).expect("matmul").broadcast_add(b).expect("broadcast_add");
    dump(pre_tag, &pre);
    let post = if relu {
        pre.clamp(0.0f64, f64::MAX).expect("clamp(relu)")
    } else {
        pre.clone()
    };
    dump(act_tag, &post);
    post
}

fn forward(tag: &str, x: &Tensor, w1: &Tensor, b1: &Tensor, w2: &Tensor, b2: &Tensor) {
    dump(&format!("{tag}.x"), x);
    let h1 = layer(&format!("{tag}.h1_pre"), &format!("{tag}.h1_relu"), x, w1, b1, true);
    let logits = layer(&format!("{tag}.logits_pre"), &format!("{tag}.logits"), &h1, w2, b2, false);

    // softmax：max → sub → exp → sum → div（全部 keepdim + broadcast 面）
    let m = logits.max_keepdim(1usize).expect("max_keepdim");
    dump(&format!("{tag}.sm_max"), &m);
    let e = logits.broadcast_sub(&m).expect("broadcast_sub").exp().expect("exp");
    dump(&format!("{tag}.sm_exp"), &e);
    let s = e.sum_keepdim(1usize).expect("sum_keepdim");
    dump(&format!("{tag}.sm_sum"), &s);
    let p = e.broadcast_div(&s).expect("broadcast_div");
    dump(&format!("{tag}.sm_probs"), &p);
}

fn from_bits2(tag: &str, rows: &[&[u32]], dev: &Device) -> Tensor {
    let data: Vec<f32> = rows.iter().flat_map(|r| r.iter().map(|&b| f32::from_bits(b))).collect();
    let t = Tensor::from_vec(data, (rows.len(), rows[0].len()), dev).expect("from_vec");
    dump(tag, &t);
    t
}

fn from_bits1(tag: &str, bits: &[u32], dev: &Device) -> Tensor {
    let data: Vec<f32> = bits.iter().map(|&b| f32::from_bits(b)).collect();
    let t = Tensor::from_vec(data, (1usize, bits.len()), dev).expect("from_vec");
    dump(tag, &t);
    let v: Vec<f32> = t.to_vec2::<f32>().expect("echo").into_iter().next().unwrap();
    Tensor::from_vec(v, (bits.len(),), dev).expect("bias rank-1")
}

fn main() {
    println!("== candle-core 0.9 cpu mini mlp ==");
    let dev = Device::Cpu;

    // 权重：全部为 from_bits 定值（0.5 档精确二进制分数 + 0.1 档就近值）
    let w1 = from_bits2(
        "W1",
        &[
            &[0x3f000000, 0xbf000000, 0x3f800000, 0xbf800000, 0x3e800000, 0xbe800000, 0x40000000, 0xbf400000],
            &[0xbfc00000, 0x3f000000, 0xbf000000, 0x3fa00000, 0x3f800000, 0x3f400000, 0xbf800000, 0x3f000000],
            &[0x3e800000, 0x3f800000, 0xbfa00000, 0xbf000000, 0x3f000000, 0xbfc00000, 0xbf000000, 0x3f800000],
            &[0xc0000000, 0xbe800000, 0x3f400000, 0x3f000000, 0xbf800000, 0x3fc00000, 0x3e800000, 0xbfa00000],
        ],
        &dev,
    );
    let b1 = from_bits1(
        "b1",
        &[0x3dcccccd, 0xbe4ccccd, 0x3e99999a, 0xbecccccd, 0x3f000000, 0xbf19999a, 0x3f333333, 0xbf4ccccd],
        &dev,
    );
    let w2 = from_bits2(
        "W2",
        &[
            &[0x3f000000, 0xbe800000],
            &[0xbf800000, 0x3f400000],
            &[0x3e800000, 0xbfc00000],
            &[0xbf000000, 0x3f800000],
            &[0x3fa00000, 0xbf400000],
            &[0x3f400000, 0xbfa00000],
            &[0xbfa00000, 0x3f000000],
            &[0x3fc00000, 0xbf000000],
        ],
        &dev,
    );
    let b2 = from_bits1("b2", &[0x3d4ccccd, 0xbd4ccccd], &dev);

    // 单侧本前向：relu 钳零/保持两分支都命中
    let x1 = from_bits2("X1", &[&[0x3f000000, 0xbf800000, 0x3fc00000, 0x3e800000]], &dev);
    forward("S1", &x1, &w1, &b1, &w2, &b2);

    // 第二单样本：另一组符号组合
    let x2 = from_bits2("X2", &[&[0xbf400000, 0x40000000, 0xbf000000, 0x3f800000]], &dev);
    forward("S2", &x2, &w1, &b1, &w2, &b2);

    // 2-batch 前向：(2,4)x(4,8)，gemm 的另一 m 档 + broadcast_add (2,8)+(8,)
    let xb = from_bits2(
        "XB",
        &[
            &[0x3f000000, 0xbf800000, 0x3fc00000, 0x3e800000],
            &[0xbf400000, 0x40000000, 0xbf000000, 0x3f800000],
        ],
        &dev,
    );
    forward("B2", &xb, &w1, &b1, &w2, &b2);
}
