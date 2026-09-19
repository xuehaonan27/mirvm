#!/usr/bin/env mirvm
---
[dependencies]
candle-core = "0.9"
---
// candle-core CPU mini MLP over tensor bit patterns compared with native. The pin
// candle-core = "0.9" resolves to 0.9.2, the latest stable release of that line.
// (1) from_bits-constructed f32 weights (W1:4x8 / b1:8 / W2:8x2 / b2:2), no rand;
// (2) Tensor::from_vec plus dtype/shape echoes for inputs and weights;
// (3) a 2-layer MLP forward pass: matmul through the gemm backend, (1,4)x(4,8)
//     and (1,8)x(8,2), with a broadcast_add bias per layer;
// (4) a hard-wired relu, implemented as clamp(0, f64::MAX); h1_pre mixes signs so
//     both the clamped-to-zero and the kept branch are hit;
// (5) softmax: max_keepdim(1) -> broadcast_sub -> exp -> sum_keepdim(1) ->
//     broadcast_div, printing max/exp/sum and the probabilities as to_bits();
// (6) a 2-batch forward pass (X2:(2,4)x(4,8)) reusing the same pipeline, which
//     covers the other gemm m case;
// (7) each tensor segment's trailing FNV-1a summary line, a bit-level fingerprint.
// Weights and inputs are all f32::from_bits constants: no RNG, no wall clock, no
// HashMap iteration order, no raw addresses. Floats print only as to_bits() in
// row-major order; stderr stays empty. The CPU forward pass is an elementwise
// deterministic loop and the softmax reduction order follows the tensor layout.
// Dependency hazard: the pinned candle-core = "0.9" has default = [] and
// gemm = "^0.19" is non-optional, so CPU matmul goes through gemm::gemm ->
// gemm-f32 0.19.0 -> pulp 0.22.3. gemm-common asks for pulp without
// default-features/std, so pulp uses only its own cpuid probe and the LD_ST
// global_asm addressing TRAP is not reachable. If a future pulp release
// switches to a std-based probe, pin pulp/gemm without std.
//
//
//
//
//
//
//
//
//
//
//
//
//
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

// One layer: pre = x.matmul(w).broadcast_add(b), optionally followed by relu.
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

    // softmax: max -> sub -> exp -> sum -> div (all keepdim + broadcast)
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

    // Weights: from_bits constants (exact binary fractions, plus nearest 0.1 values)
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

    // First single-sample forward pass: relu hits the clamp-to-zero and keep branch
    let x1 = from_bits2("X1", &[&[0x3f000000, 0xbf800000, 0x3fc00000, 0x3e800000]], &dev);
    forward("S1", &x1, &w1, &b1, &w2, &b2);

    // Second single sample: a different sign combination.
    let x2 = from_bits2("X2", &[&[0xbf400000, 0x40000000, 0xbf000000, 0x3f800000]], &dev);
    forward("S2", &x2, &w1, &b1, &w2, &b2);

    // 2-batch forward pass: (2,4)x(4,8), the other gemm m case
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
