#!/usr/bin/env mirvm
---
[dependencies]
malachite = "0.4"
---
// malachite 0.4（limb 重纯 Rust 大数）：Natural/Rational 差分。
// 覆盖：512-bit 四则（mul→1024-bit）与 div_mod/checked_sub、gcd/lcm/extended_gcd
// （Bezout 恒等式）、pow/big-pow 锚定、mod_pow、移位/位访问/位块/and-or-xor/
// hamming/count_ones/low_mask/trailing_zeros/significant_bits、checked_sqrt/
// sqrt_rem/checked_root(exp=3)/root_rem/floor_root/ceiling_sqrt 精确根谱系、
// u128 互转（含 try_from 越界 None）、字符串 2..=36 基互转与坏串 None、
// Rational from_integers/from_naturals 约分 + 加减乘除 + floor/ceiling/sign +
// numerator/denominator 往返。
// 素数谱系：0.4.22 尚无内置 is_prime（0.5+ 才有），故 driver 以 crate 的
// mod_pow / 除法 / trailing_zeros 手写确定版 12 基 Miller-Rabin 与 Fermat，
// 跑小素数、Carmichael 数（561/1105/1729/41041/825265/321197185）、
// 强伪素数（2047/1373653/25326001/3215031751）、Mersenne 素数 M61/M89/M107/M127、
// M61^2 与大偶数——两侧引擎算同一份 math，标签逐字节对拍。
// 128 位族恢复记录（2026-07 批5 修好，谱系已复原）：malachite 的 f64 互转族
// （Natural::approx_log → sci_mantissa_and_exponent → i128::sign）在本工具链
// nightly-2026-07-02 下经 core 新 `three_way_compare` intrinsic 落成 MIR
// `BinOp::Cmp(i128)`；mirvm lower 曾只接比较/算术（Cmp128/Bin128），三向 Cmp
// 漏接（TRAP「128 位 BinOp Cmp」），修复后 ⑨ 段恢复：approx_log 打印
// hex bits 锚定（例：Natural::from(10u32).pow(1000u64).approx_log().to_bits()
// = 0x40a1fd2b914f1517）。
// 确定性：固定 xorshift64* 种子产生操作数；大数输出全长 hex（≤96 字符）或
// len+head+tail+fnv1a 锚定；无浮点打印/HashMap/时间/地址/线程序；stderr 为空。
use malachite::num::arithmetic::traits::{
    Ceiling, CeilingSqrt, CheckedRoot, CheckedSqrt, CheckedSub, DivMod, ExtendedGcd,
    Floor, FloorRoot, Gcd, Lcm, ModPow, Pow, PowerOf2, RootRem, Sign, SqrtRem,
};
use malachite::num::basic::traits::{One, Zero};
use malachite::num::conversion::traits::{FromStringBase, ToStringBase};
use malachite::num::logic::traits::{
    BitAccess, BitBlockAccess, BitConvertible, CountOnes, HammingDistance, LowMask,
    SignificantBits,
};
use malachite::platform::Limb;
use malachite::{Integer, Natural, Rational};
use std::cmp::Ordering;

/// 定种 xorshift64*（native/mirvm 同序列）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn limbs(&mut self, n: usize) -> Vec<Limb> {
        (0..n).map(|_| self.next()).collect()
    }
}

/// 内联 FNV-1a。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 大数打印锚定：短则全长 hex，长则 len + 头尾各 32 hex + fnv1a(hex)。
fn show(n: &Natural) -> String {
    let s = n.to_string_base(16);
    if s.len() <= 96 {
        s
    } else {
        format!(
            "len={} head={} tail={} fnv={:016x}",
            s.len(),
            &s[..32],
            &s[s.len() - 32..],
            fnv1a(s.as_bytes())
        )
    }
}

fn from_hex(s: &str) -> Natural {
    Natural::from_string_base(16, s).unwrap()
}

/// Ordering → i8 打印映射。
fn sgn(o: Ordering) -> i8 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// n 对单 limb 除数的余数。
fn rem_small(n: &Natural, d: u64) -> u64 {
    u64::try_from(&(n % Natural::from(d))).unwrap()
}

/// 单基强伪素数检验：要求 n 为奇数且与 a 互素（n > 37）。
fn strong_check(n: &Natural, a: u64) -> bool {
    let nm1 = n - Natural::ONE;
    let s = nm1.clone().trailing_zeros().unwrap();
    let d = &nm1 >> s;
    let mut x = Natural::from(a).mod_pow(&d, n);
    if x == Natural::ONE || x == nm1 {
        return true;
    }
    for _ in 1..s {
        x = &x * &x % n;
        if x == nm1 {
            return true;
        }
    }
    false
}

/// 12 基（2..=37）确定版 Miller-Rabin——对 n < 2^81.5 精确；谱系内的素数无论位数
/// 均不会误报合数。
fn mr12(n: &Natural) -> bool {
    const BASES: [u64; 12] = [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37];
    if n < &Natural::from(2u32) {
        return false;
    }
    for &p in &BASES {
        if n == &Natural::from(p) {
            return true;
        }
        if rem_small(n, p) == 0 {
            return false;
        }
    }
    BASES.iter().all(|&a| strong_check(n, a))
}

fn main() {
    let mut rng = Rng(0x9E3779B97F4A7C15);

    // ⓪ 操作数：512-bit A（顶位置 1）与 B（顶位清 0 → A>B）、256-bit 奇数 M
    let mut la = rng.limbs(8);
    la[7] |= 1 << 63;
    let mut lb = rng.limbs(8);
    lb[7] &= !(1u64 << 63);
    let mut lm = rng.limbs(4);
    lm[3] |= 1 << 63;
    lm[0] |= 1;
    let a = Natural::from_limbs_asc(&la);
    let b = Natural::from_limbs_asc(&lb);
    let m = Natural::from_limbs_asc(&lm);
    println!("A bits={} limbs={} hex={}", a.significant_bits(), a.limb_count(), show(&a));
    println!("B bits={} limbs={} hex={}", b.significant_bits(), b.limb_count(), show(&b));
    println!("A>B = {}", a > b);

    // ① 互转：u128 / limbs / 字符串基 2..=36
    let x = Natural::from(u128::MAX);
    println!("u128max hex={} limbs={}", x.to_string_base(16), x.limb_count());
    println!("u128 rt={}", u128::try_from(&x) == Ok(u128::MAX));
    let over = &x + Natural::ONE; // 2^128
    println!("u128 over none={}", u128::try_from(&over).is_err());
    let small128 = Natural::from(0x0123_4567_89ab_cdefu128) * Natural::from(0xffffu32);
    println!("u128 mid={}", u128::try_from(&small128).is_ok());
    println!("limbs rt={}", Natural::from_limbs_asc(&a.to_limbs_asc()) == a);
    for base in [2u8, 8, 10, 16, 19, 36] {
        let s = a.to_string_base(base);
        let rt = Natural::from_string_base(base, &s) == Some(a.clone());
        println!("base {} len={} rt={}", base, s.len(), rt);
    }
    println!("bad16 none={}", Natural::from_string_base(16, "12xz3").is_none());
    println!("bad10 none={}", Natural::from_string_base(10, " 42").is_none());
    println!("empty none={}", Natural::from_string_base(10, "").is_none());
    println!("upper36={}", from_hex("5f3759df").to_string_base_upper(36));

    // ② 四则（512-bit；积 1024-bit 锚定）
    let sum = &a + &b;
    let diff = &a - &b;
    let prod = &a * &b;
    println!("add hex={}", show(&sum));
    println!("sub hex={}", show(&diff));
    println!("csub rev none={}", b.clone().checked_sub(a.clone()).is_none());
    println!("mul anchor={}", show(&prod));
    let (q, r) = (&a).div_mod(&b);
    println!("div q={} r={}", show(&q), show(&r));
    println!("divmod id={}", &q * &b + &r == a);
    println!("rem eq={}", &a % &b == r);
    println!("quot eq={}", &prod / &a == b);
    let mut acc = a.clone();
    acc += &prod;
    println!("addmul-assign bits={}", acc.significant_bits());

    // ③ gcd / lcm / extended_gcd（U=A·M, V=B·M，768-bit）
    let u = &a * &m;
    let v = &b * &m;
    let g = (&u).gcd(&v);
    let l = (&u).lcm(&v);
    println!("gcd bits={} anchor={}", (&g).significant_bits(), show(&g));
    println!("lcm bits={} anchor={}", (&l).significant_bits(), show(&l));
    println!("gcd*lcm id={}", &g * &l == &u * &v);
    println!("g mod m zero={}", &g % &m == Natural::ZERO);
    let (g2, ex, ey) = u.clone().extended_gcd(v.clone());
    let bezout = Integer::from(&u) * &ex + Integer::from(&v) * &ey == Integer::from(&g2);
    println!("egcd eq={} bezout={} xs={} ys={}", g2 == g, bezout, sgn(ex.sign()), sgn(ey.sign()));

    // ④ pow / mod_pow
    let p512 = Natural::power_of_2(512);
    println!("2^512 bits={} tail={}", (&p512).significant_bits(), (&p512 % &Natural::from(0xffffu32)).to_string_base(16));
    let big = Natural::from(3u32).pow(4093u64);
    println!("3^4093 bits={} anchor={}", (&big).significant_bits(), show(&big));
    let mod256 = from_hex("d3e1f5a7c9b20468d3e1f5a7c9b20468d3e1f5a7c9b20468d3e1f5a7c9b20469");
    let mp = Natural::from(7u32).mod_pow(&Natural::from(123456789u32), &mod256);
    println!("7^123456789 mod M256 hex={}", show(&mp));

    // ⑤ 位操作谱系
    let mut c = a.clone();
    c.set_bit(520);
    let bit7 = c.get_bit(7);
    c.flip_bit(7);
    let bit7_flipped = c.get_bit(7);
    c.clear_bit(0);
    println!("bitacc b7={} flipped={} hex={}", bit7, bit7_flipped, show(&c));
    println!("getbits 64..128 = {}", c.get_bits(64, 128).to_string_base(16));
    c.assign_bits(200, 264, &from_hex("fedcba9876543210"));
    println!("assignbits hex={}", show(&c));
    println!("and hex={}", show(&(&a & &b)));
    println!("or  hex={}", show(&(&a | &b)));
    println!("xor hex={}", show(&(&a ^ &b)));
    println!("hamming={}", (&a).hamming_distance(&b));
    println!("ones A={} B={}", (&a).count_ones(), (&b).count_ones());
    println!("mask200 bits={} hex-ok={}", Natural::low_mask(200).significant_bits(), Natural::low_mask(200).to_string_base(16).chars().all(|ch| ch == 'f'));
    let tz_shifted = (&a << 37u64).trailing_zeros();
    println!("tz shifted={:?} orig={:?}", tz_shifted, a.clone().trailing_zeros());
    println!("shl137 id={}", (&a << 137u64) >> 137u64 == a);
    println!("bits rt={}", Natural::from_bits_asc(a.to_bits_asc().into_iter()) == a);

    // ⑥ 精确根谱系（sqrt/cbrt/5 次根 + 余项 + ceiling）
    let r256 = from_hex("9f1c3a5e7b2d4f609f1c3a5e7b2d4f609f1c3a5e7b2d4f609f1c3a5e7b2d4f61");
    let sq = &r256 * &r256;
    println!("sqrt exact={}", sq.clone().checked_sqrt() == Some(r256.clone()));
    println!("sqrt+1 none={}", (&sq + Natural::ONE).checked_sqrt().is_none());
    let (root, rem) = (&sq + Natural::from(7u32)).sqrt_rem();
    println!("sqrtrem id={}", root == r256 && rem == Natural::from(7u32));
    println!("ceil sqrt+1={}", (&sq + Natural::ONE).ceiling_sqrt() == &r256 + Natural::ONE);
    let cb = from_hex("3a7f9c1e5b2d80463a7f9c1e5b2d80463a7f9c1e5b2");
    let cube = &cb * &cb * &cb;
    println!("cbrt exact={}", cube.clone().checked_root(3) == Some(cb.clone()));
    println!("cbrt+1 none={}", (&cube + Natural::ONE).checked_root(3).is_none());
    let (croot, crem) = (&cube + Natural::from(9u32)).root_rem(3);
    println!("rootrem id={}", croot == cb && crem == Natural::from(9u32));
    println!("5th floor 2^512 hex={}", show(&p512.clone().floor_root(5)));

    // ⑦ 素数谱系：小素数 / Carmichael / 强伪素数 / Mersenne / M61^2 / 大偶数
    let cases: [(&str, &str); 16] = [
        ("tiny2", "2"),
        ("tiny3", "3"),
        ("p97", "97"),
        ("carm561", "561"),
        ("carm1105", "1105"),
        ("carm1729", "1729"),
        ("carm41041", "41041"),
        ("carm825265", "825265"),
        ("carm321197185", "321197185"),
        ("spsp2047", "2047"),
        ("spsp1373653", "1373653"),
        ("spsp25326001", "25326001"),
        ("spsp3215031751", "3215031751"),
        ("m61", "2305843009213693951"),
        ("m89", "618970019642690137449562111"),
        ("m107", "162259276829213363391578010288127"),
    ];
    let mut prime_count = 0u32;
    for (label, dec) in cases {
        let n = Natural::from_string_base(10, dec).unwrap();
        let p = mr12(&n);
        prime_count += u32::from(p);
        println!("prime {:<18} = {}", label, p);
    }
    let m127 = Natural::from_string_base(10, "170141183460469231731687303715884105727").unwrap();
    println!("prime m127 = {}", mr12(&m127));
    prime_count += u32::from(mr12(&m127));
    let m61 = Natural::from(2305843009213693951u64);
    let m61sq = &m61 * &m61;
    println!("prime m61sq = {}", mr12(&m61sq));
    println!("prime even128 = {}", mr12(&(m127.clone() - Natural::ONE)));
    println!("prime-count = {}", prime_count);
    // Carmichael 的 Fermat 表现：互素基过、共享因子基败
    for (label, dec, b1, b2, b3) in [
        ("561", "561", 5u64, 7, 33),
        ("1105", "1105", 7, 11, 5),
        ("1729", "1729", 5, 3, 7),
        ("41041", "41041", 5, 17, 7),
    ] {
        let n = Natural::from_string_base(10, dec).unwrap();
        let nm1 = &n - Natural::ONE;
        let f = |bb: u64| Natural::from(bb).mod_pow(&nm1, &n) == Natural::ONE;
        println!("fermat {} b{}={} b{}={} b{}={}", label, b1, f(b1), b2, f(b2), b3, f(b3));
    }

    // ⑧ Rational：约分 / 四则 / floor-ceiling-sign / numerator-denominator 往返
    let q1 = Rational::from_integers(Integer::from(-6i32), Integer::from(8i32));
    println!("q1 = {}", q1);
    let q2 = Rational::from_naturals(
        Natural::from_string_base(10, "10000000000000000000000000000000000000000").unwrap(),
        Natural::power_of_2(80),
    );
    let (q2n, q2d) = q2.clone().into_numerator_and_denominator();
    println!("q2 num={} den={}", show(&q2n), show(&q2d));
    let qs = &q1 + &q2;
    let qt = &q1 - &q2;
    let qm = &q1 * &q2;
    let qd = &q1 / &q2;
    println!("q-add = {}", qs);
    println!("q-sub = {}", qt);
    println!("q-mul = {}", qm);
    println!("q-div = {}", qd);
    println!("q id1={}", &qt + Rational::from(2u32) * &q2 == qs);
    println!("q id2={}", &qd * &q2 == q1);
    let q3 = Rational::from_integers(Integer::from(-7i32), Integer::from(3i32));
    println!("q3 = {}", q3);
    println!("floor={} ceiling={} sign={}", q3.clone().floor(), q3.clone().ceiling(), sgn(q3.sign()));
    let q5 = Rational::from_integers(Integer::from(10i32), Integer::from(2i32));
    println!("q5 = {} eq-int={}", q5, q5 == Rational::from(5u32));
    let q6 = Rational::from_integers(Integer::from(4620i32), Integer::from(1980i32));
    let (q6n, q6d) = q6.clone().into_numerator_and_denominator();
    let recook = Rational::from_naturals_ref(&q6n, &q6d) == q6;
    println!("q6 {}/{} recook={}", q6n, q6d, recook);

    // ⑨ f64 互转族：approx_log（内部 sci_mantissa_and_exponent → i128::sign，
    // nightly 下 three_way_compare 落成 BinOp::Cmp(i128)——缺口 1 修复后
    // 本段恢复，见文件头记录）
    let lk = Natural::from(10u32).pow(1000u64);
    println!("approx_log 10^1000 bits={:#x}", lk.approx_log().to_bits());
    println!("approx_log A bits={:#x}", a.approx_log().to_bits());
    println!("approx_log B bits={:#x}", b.approx_log().to_bits());
    println!("approx_log 2^512 bits={:#x}", p512.approx_log().to_bits());
    println!("approx_log m127 bits={:#x}", m127.approx_log().to_bits());
    println!("approx_log one bits={:#x}", Natural::ONE.approx_log().to_bits());
}
