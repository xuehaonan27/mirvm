#!/usr/bin/env mirvm
---
[dependencies]
ark-ff = "0.4"
ark-ec = "0.4"
ark-bls12-381 = "0.4"
---
// arkworks 0.4 三件套差分：ark-ff（Fp/Fp2/Fp6/Fp12 域塔 + Montgomery 256/384 limb
// 算术，重 u128 进位路径）+ ark-ec（短 Weierstrass 群算术 + 双线性配对框架）+
// ark-bls12-381（BLS12-381 固定参数：Fq 381-bit = 6×u64、Fr 255-bit = 4×u64、G1/G2）。
// 内容：
//   ① BigInteger256 底层：wrapping 加减乘、add_nocarry/sub_noborrow 进位位、
//     mul2/div2、num_bits/get_bit、bits/bytes roundtrip、全序比较。
//   ② Fr ↔ BigInteger256 互转：from_bigint（含 =MODULUS 的 None 边界）、
//     into_bigint roundtrip、-1 = MODULUS-1 谱系、四则与除法 roundtrip。
//   ③ Fq：固定元素四则 / square / double / neg / inverse（零元 None 边界）/
//     pow（0/1/7/48bit 指数）/ frobenius_map / sqrt 往返 / sum_of_products /
//     除法 roundtrip / 借位回绕——全部打印 into_bigint（Montgomery 约化后）的
//     limb hex 谱系。
//   ④ Fq2/Fq6/Fq12：固定元 new 构造，加/乘/平方/逆（含 Fq6 frobenius 与 pow、
//     Fq12 pow 小指数与 frobenius_map(1)）、Fq2 sqrt 往返。
//   ⑤ G1：generator 坐标、double 与 g+g 一致、(a+b)+c 结合律、neg 归零、
//     标量乘（小标量 5 / 大标量 4×u64 limbs）、4 点 MSM == 手工和、zero/零标量边界。
//   ⑥ G2：同类操作（Fq2 坐标谱系）。
//   ⑦ Bls12-381 配对一次：pairing(g1, g2) → Fq12 的 72 个 limb 字节流 FNV-1a 指纹。
// 确定性：全部元素从固定整数构造（无 rng/时间/地址/HashMap 序）；只打印 limb
// hex / 布尔 / 整数；stderr 必空。
use ark_bls12_381::{
    Bls12_381, Fq, Fq12, Fq2, Fq6, Fr, G1Affine, G1Projective, G2Affine, G2Projective,
};
use ark_ec::pairing::Pairing;
use ark_ec::{AffineRepr, CurveGroup, Group, VariableBaseMSM};
use ark_ff::biginteger::{BigInt, BigInteger};
use ark_ff::{Field, One, PrimeField, Zero};

/// limb 谱系：little-endian limb 序，逐个 {:016x}。
fn h<const N: usize>(b: &BigInt<N>) -> String {
    let mut s = String::from("[");
    for (i, l) in b.0.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{l:016x}"));
    }
    s.push(']');
    s
}

fn hq(x: &Fq) -> String {
    h(&x.into_bigint())
}

fn hq2(x: &Fq2) -> String {
    format!("{}|{}", hq(&x.c0), hq(&x.c1))
}

fn hq6(x: &Fq6) -> String {
    format!("{}|{}|{}", hq2(&x.c0), hq2(&x.c1), hq2(&x.c2))
}

fn hq12(x: &Fq12) -> String {
    format!("{}|{}", hq6(&x.c0), hq6(&x.c1))
}

/// Fq12 的 72 个 canonical limb（c0.c0.c0 … 固定遍历序）。
fn fq12_limbs(w: &Fq12) -> Vec<u64> {
    let mut v = Vec::with_capacity(72);
    for f6 in [w.c0, w.c1] {
        for f2 in [f6.c0, f6.c1, f6.c2] {
            for f in [f2.c0, f2.c1] {
                v.extend_from_slice(&f.into_bigint().0);
            }
        }
    }
    v
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn main() {
    // ---- ① BigInteger256 limb 底层算术 ----
    let x = BigInt::<4>::new([
        0xfedcba9876543210,
        0x0123456789abcdef,
        0xdeadbeefcafebabe,
        0x0badf00d5eedface,
    ]);
    let y = BigInt::<4>::new([
        0x1111111111111111,
        0x2222222222222222,
        0x3333333333333333,
        0x0444444444444444,
    ]);
    println!("b256.x = {}", h(&x));
    println!("b256.y = {}", h(&y));
    let mut c = x;
    let carry = c.add_with_carry(&y);
    println!("b256.add = {} carry={}", h(&c), carry);
    let mut c = x;
    let borrow = c.sub_with_borrow(&y);
    println!("b256.sub = {} borrow={}", h(&c), borrow);
    let mut c = y;
    let borrow2 = c.sub_with_borrow(&x);
    println!("b256.sub.rev = {} borrow={}", h(&c), borrow2);
    let mut c = x;
    let c_out = c.mul2();
    println!("b256.mul2 = {} carry_out={}", h(&c), c_out);
    let mut c = x;
    c.muln(67);
    println!("b256.muln67 = {}", h(&c));
    let mut c = x;
    c.div2();
    println!("b256.div2 = {}", h(&c));
    let mut c = x;
    c.divn(13);
    println!("b256.divn13 = {}", h(&c));
    println!(
        "b256.x bits num={} b0={} b63={} b64={} b128={} b255={}",
        x.num_bits(),
        x.get_bit(0),
        x.get_bit(63),
        x.get_bit(64),
        x.get_bit(128),
        x.get_bit(255)
    );
    let bits_le = x.to_bits_le();
    println!(
        "b256.bits_le roundtrip = {} len={}",
        BigInt::<4>::from_bits_le(&bits_le) == x,
        bits_le.len()
    );
    let bytes_be = x.to_bytes_be();
    let bytes_le = x.to_bytes_le();
    println!(
        "b256.bytes len={}/{} be.fnv={:016x} le.fnv={:016x}",
        bytes_be.len(),
        bytes_le.len(),
        fnv1a(&bytes_be),
        fnv1a(&bytes_le)
    );
    println!("b256.cmp x>y={} y>x={} x==x={}", x > y, y > x, x == x);
    println!("b256.even/odd x={}/{} y={}/{}", x.is_even(), x.is_odd(), y.is_even(), y.is_odd());

    // ---- ② Fr ↔ BigInteger256 互转 ----
    println!("fr.modulus = {}", h(&Fr::MODULUS));
    let a = Fr::from(0x0123456789abcdefu64);
    let b = Fr::from(0xfedcba9876543210u64);
    println!("fr.a = {}", h(&a.into_bigint()));
    println!("fr.b = {}", h(&b.into_bigint()));
    println!(
        "fr.roundtrip a = {}",
        Fr::from_bigint(a.into_bigint()).unwrap() == a
    );
    println!(
        "fr.from_bigint(modulus) none = {}",
        Fr::from_bigint(Fr::MODULUS).is_none()
    );
    let m1 = Fr::zero() - Fr::one(); // = MODULUS - 1
    println!("fr.-1 = {}", h(&m1.into_bigint()));
    println!(
        "fr.-1 roundtrip = {}",
        Fr::from_bigint(m1.into_bigint()).unwrap() == m1
    );
    println!("fr.add = {}", h(&(a + b).into_bigint()));
    println!("fr.sub = {}", h(&(a - b).into_bigint()));
    println!("fr.mul = {}", h(&(a * b).into_bigint()));
    println!("fr.div = {}", h(&(a / b).into_bigint()));
    println!("fr.(a/b)*b==a = {}", (a / b) * b == a);
    println!("fr.inv(a)*a==1 = {}", a.inverse().unwrap() * a == Fr::one());
    println!("fr.pow0==1 = {}", a.pow([0u64]) == Fr::one());
    println!("fr.zero.inv none = {}", Fr::zero().inverse().is_none());

    // ---- ③ Fq Montgomery limb 谱系 ----
    println!("fq.modulus = {}", h(&Fq::MODULUS));
    let f1 = Fq::from(0x1234567890abcdefu64);
    let f2 = Fq::from(0x0fedcba987654321u64);
    // 6 个固定 limb，最高 limb 0x0123… < q 最高 limb 0x1a01… ⇒ 保证 < q。
    let f3 = Fq::from_bigint(BigInt::<6>::new([
        0xdeadbeefcafebabe,
        0x0123456789abcdef,
        0xfedcba9876543210,
        0x1111111111111111,
        0x2222222222222222,
        0x0123456789abcdef,
    ]))
    .unwrap();
    println!("fq.f1 = {}", hq(&f1));
    println!("fq.f3 = {}", hq(&f3));
    println!("fq.add = {}", hq(&(f1 + f2)));
    println!("fq.sub = {}", hq(&(f1 - f2)));
    println!("fq.sub.wrap = {}", hq(&(Fq::zero() - f2)));
    println!("fq.mul = {}", hq(&(f1 * f2)));
    println!("fq.f3.mul = {}", hq(&(f3 * f2)));
    println!("fq.f3.square = {}", hq(&f3.square()));
    println!("fq.f3.double = {}", hq(&f3.double()));
    println!("fq.f3.neg = {}", hq(&(-f3)));
    println!("fq.f3.inv = {}", hq(&f3.inverse().unwrap()));
    println!("fq.inv*self==1 = {}", f3.inverse().unwrap() * f3 == Fq::one());
    println!("fq.zero.inv none = {}", Fq::zero().inverse().is_none());
    println!("fq.pow0 = {}", hq(&f3.pow([0u64])));
    println!("fq.pow1 = {}", hq(&f3.pow([1u64])));
    println!("fq.pow7 = {}", hq(&f3.pow([7u64])));
    println!("fq.pow48bit = {}", hq(&f3.pow([0x0000deadbeefcafeu64])));
    println!("fq.frob1 = {}", hq(&f3.frobenius_map(1)));
    println!("fq.frob3 = {}", hq(&f3.frobenius_map(3)));
    let sq = f3.square();
    let r = sq.sqrt().unwrap();
    println!("fq.sqrt r*r==sq = {}", r * r == sq);
    println!("fq.sqrt(0)==0 = {}", Fq::zero().sqrt().unwrap() == Fq::zero());
    let sop = Fq::sum_of_products(&[f1, f2], &[f3, f1]);
    println!("fq.sop = {}", hq(&sop));
    println!("fq.sop==a*c+b*d = {}", sop == f1 * f3 + f2 * f1);
    println!("fq.div = {}", hq(&(f3 / f2)));
    println!("fq.(a/b)*b==a = {}", (f3 / f2) * f2 == f3);
    println!(
        "fq.from_bigint(modulus) none = {}",
        Fq::from_bigint(Fq::MODULUS).is_none()
    );

    // ---- ④ Fq2 / Fq6 / Fq12 域塔 ----
    let e21 = Fq2::new(f1, f2);
    let e22 = Fq2::new(f2, f3);
    println!("fq2.e22 = {}", hq2(&e22));
    println!("fq2.add = {}", hq2(&(e21 + e22)));
    println!("fq2.mul = {}", hq2(&(e21 * e22)));
    println!("fq2.square = {}", hq2(&e22.square()));
    println!("fq2.inv = {}", hq2(&e22.inverse().unwrap()));
    println!(
        "fq2.inv*self==1 = {}",
        e22.inverse().unwrap() * e22 == Fq2::one()
    );
    println!("fq2.zero.inv none = {}", Fq2::zero().inverse().is_none());
    println!("fq2.frob1 = {}", hq2(&e22.frobenius_map(1)));
    let sq2 = e22.square();
    let r2 = sq2.sqrt().unwrap();
    println!("fq2.sqrt r*r==sq = {}", r2 * r2 == sq2);

    let f61 = Fq6::new(e21, e22, Fq2::new(f3, f1));
    let f62 = Fq6::new(e22, Fq2::new(f2, f2), e21);
    println!("fq6.mul = {}", hq6(&(f61 * f62)));
    println!("fq6.square = {}", hq6(&f62.square()));
    println!(
        "fq6.inv*self==1 = {}",
        f62.inverse().unwrap() * f62 == Fq6::one()
    );
    println!("fq6.frob2 = {}", hq6(&f62.frobenius_map(2)));
    println!("fq6.pow3 = {}", hq6(&f62.pow([3u64])));

    let w1 = Fq12::new(f61, f62);
    let w2 = Fq12::new(f62, f61);
    println!("fq12.mul = {}", hq12(&(w1 * w2)));
    println!(
        "fq12.inv*self==1 = {}",
        w1.inverse().unwrap() * w1 == Fq12::one()
    );
    println!("fq12.pow5 = {}", hq12(&w1.pow([5u64])));
    println!("fq12.frob1 = {}", hq12(&w1.frobenius_map(1)));

    // ---- ⑤ G1 群算术 ----
    let g = G1Affine::generator();
    println!("g1.gen.x = {}", hq(&g.x));
    println!("g1.gen.y = {}", hq(&g.y));
    println!("g1.gen.infinity = {}", g.infinity);
    println!(
        "g1.oncurve = {} subgroup = {}",
        g.is_on_curve(),
        g.is_in_correct_subgroup_assuming_on_curve()
    );
    let p = g.into_group();
    let d = p.double();
    println!("g1.double==g+g = {}", d == (p + p));
    let g5 = p.mul_bigint([5u64]);
    println!("g1.x5==2d+g = {}", g5 == (d + d + p));
    let g5a = g5.into_affine();
    println!("g1.x5.x = {}", hq(&g5a.x));
    println!("g1.x5.y = {}", hq(&g5a.y));
    let bigk = [
        0xdeadbeefcafebabeu64,
        0x0123456789abcdef,
        0xfedcba9876543210,
        0x0011223344556677,
    ];
    let gk = p.mul_bigint(bigk);
    let gka = gk.into_affine();
    println!("g1.kg.x = {}", hq(&gka.x));
    println!("g1.kg.y = {}", hq(&gka.y));
    println!("g1.affine roundtrip = {}", gka.into_group() == gk);
    let q1 = p + g5;
    let q2 = g5 + gk;
    println!("g1.assoc = {}", (p + q1) + q2 == p + (q1 + q2));
    println!("g1.addneg==0 = {}", (p + (-p)).is_zero());
    println!("g1.zero+kg==kg = {}", (G1Projective::zero() + gk) == gk);
    println!("g1.mul0==0 = {}", p.mul_bigint([0u64]).is_zero());
    println!("g1.mul1==self = {}", p.mul_bigint([1u64]) == p);
    println!("g1.zero.infinity = {}", G1Affine::zero().infinity);
    // 4 点 MSM（Pippenger）与手工和交叉校验
    let bases = [g, g5a, gka, (p + g5).into_affine()];
    let scalars = [
        Fr::from(7u64),
        Fr::from(9u64),
        Fr::from(0x123456789abcdefu64),
        m1,
    ];
    let msm = G1Projective::msm(&bases, &scalars).unwrap();
    let manual = p * scalars[0] + g5 * scalars[1] + gk * scalars[2] + (p + g5) * scalars[3];
    println!("g1.msm==manual = {}", msm == manual);
    let msma = msm.into_affine();
    println!("g1.msm.x = {}", hq(&msma.x));
    println!("g1.msm.y = {}", hq(&msma.y));

    // ---- ⑥ G2 群算术（Fq2 坐标）----
    let h2 = G2Affine::generator();
    println!("g2.gen.x = {}", hq2(&h2.x));
    println!("g2.gen.y = {}", hq2(&h2.y));
    println!(
        "g2.oncurve = {} subgroup = {}",
        h2.is_on_curve(),
        h2.is_in_correct_subgroup_assuming_on_curve()
    );
    let rpt = h2.into_group();
    println!("g2.double==g+g = {}", rpt.double() == (rpt + rpt));
    let r5 = rpt.mul_bigint([5u64]);
    println!("g2.x5.x = {}", hq2(&r5.into_affine().x));
    let rk = rpt.mul_bigint(bigk);
    let rka = rk.into_affine();
    println!("g2.kg.x = {}", hq2(&rka.x));
    println!("g2.kg.y = {}", hq2(&rka.y));
    println!("g2.addneg==0 = {}", (rpt + (-rpt)).is_zero());
    println!("g2.mul0==0 = {}", rpt.mul_bigint([0u64]).is_zero());
    println!(
        "g2.zero+kg==kg = {}",
        (G2Projective::zero() + rk) == rk
    );

    // ---- ⑦ BLS12-381 配对一次 → 72 limb 字节 FNV ----
    let e = Bls12_381::pairing(g, h2);
    let limbs = fq12_limbs(&e.0);
    let mut buf = Vec::with_capacity(limbs.len() * 8);
    for l in &limbs {
        buf.extend_from_slice(&l.to_le_bytes());
    }
    println!("pair.limbs = {} bytes = {}", limbs.len(), buf.len());
    println!("pair.fnv = {:016x}", fnv1a(&buf));
    println!("pair.is_one = {}", e.0 == Fq12::one());
    println!(
        "pair.inv*self==1 = {}",
        e.0.inverse().unwrap() * e.0 == Fq12::one()
    );
    let fe = e.0.frobenius_map(2);
    let fe_limbs = fq12_limbs(&fe);
    let mut fbuf = Vec::with_capacity(fe_limbs.len() * 8);
    for l in &fe_limbs {
        fbuf.extend_from_slice(&l.to_le_bytes());
    }
    println!("pair.frob2.fnv = {:016x}", fnv1a(&fbuf));
}
