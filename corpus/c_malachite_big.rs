#!/usr/bin/env mirvm
---
[dependencies]
malachite = "0.4"
---
// malachite 0.4 differential (limb-based, pure-Rust bignums): Natural and Rational.
// Covers 512-bit arithmetic (mul -> 1024-bit) with div_mod/checked_sub, gcd/lcm/extended_gcd
// (Bezout identity), pow/big-pow anchors, mod_pow, shifts / bit access / bit blocks /
// and-or-xor / hamming / count_ones / low_mask / trailing_zeros / significant_bits, the
// exact-root family checked_sqrt / sqrt_rem / checked_root(exp=3) / root_rem / floor_root /
// ceiling_sqrt, u128 conversions (including try_from out-of-range None), string conversions
// in bases 2..=36 with None for bad strings, and Rational from_integers/from_naturals
// reduction plus add/sub/mul/div, floor/ceiling/sign and numerator/denominator roundtrips.
// Prime spectrum: 0.4.22 has no built-in is_prime (that arrives in 0.5+), so the driver
// hand-rolls a deterministic 12-base Miller-Rabin and a Fermat test on top of the crate's
// mod_pow, division and trailing_zeros, over small primes, Carmichael numbers
// (561/1105/1729/41041/825265/321197185), strong pseudoprimes
// (2047/1373653/25326001/3215031751), the Mersenne primes M61/M89/M107/M127, M61^2 and a
// large even number -- both engines compute the same math and their labels are compared.
// The f64 conversion family (Natural::approx_log -> sci_mantissa_and_exponent -> i128::sign)
// lowers to MIR `BinOp::Cmp(i128)` through core's three_way_compare intrinsic on this
// toolchain (nightly-2026-07-02), so mirvm must accept the three-way compare form and not
// only Cmp128/Bin128; section ⑨ pins the result with hex bits, for example
// Natural::from(10u32).pow(1000u64).approx_log().to_bits() = 0x40a1fd2b914f1517.
// Deterministic: a fixed xorshift64* seed supplies the operands; big numbers print full hex
// (≤96 chars) or a len+head+tail+fnv1a anchor; no floats, HashMap, time, addresses or
// thread ids are printed; stderr stays empty.
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

/// Seeded xorshift64* with the same sequence on native and mirvm.
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

/// Inlined FNV-1a.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Bignum print anchor: full hex when short, else len + 32 head and tail hex + fnv1a(hex).
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

/// Ordering -> i8 print mapping.
fn sgn(o: Ordering) -> i8 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// Remainder of n modulo a single-limb divisor.
fn rem_small(n: &Natural, d: u64) -> u64 {
    u64::try_from(&(n % Natural::from(d))).unwrap()
}

/// Single-base strong pseudoprime test; requires n odd and coprime to a (n > 37).
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

/// Deterministic 12-base (2..=37) Miller-Rabin, exact for n < 2^81.5; no prime in this
/// spectrum is reported composite, whatever its size.
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

    // ⓪ Operands: 512-bit A (top bit set) and B (top bit cleared -> A>B), plus odd 256-bit M
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

    // ① Conversions: u128 / limbs / string bases 2..=36
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

    // ② Arithmetic (512-bit; the 1024-bit product is anchored)
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

    // ③ gcd / lcm / extended_gcd (U=A·M, V=B·M, 768-bit)
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

    // ⑤ Bit-operation spectrum
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

    // ⑥ Exact-root spectrum (sqrt / cbrt / 5th root + remainder + ceiling)
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

    // ⑦ Prime spectrum: small primes / Carmichael / strong pseudoprimes / Mersenne / M61^2 / large even
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
    // Fermat behavior on Carmichael numbers: coprime bases pass, bases sharing a factor fail
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

    // ⑧ Rational: reduction / arithmetic / floor-ceiling-sign / numerator-denominator roundtrip
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

    // ⑨ f64 conversion family: approx_log (internally sci_mantissa_and_exponent -> i128::sign,
    // which core's three_way_compare lowers to BinOp::Cmp(i128) on this toolchain; see the
    // file header note).
    let lk = Natural::from(10u32).pow(1000u64);
    println!("approx_log 10^1000 bits={:#x}", lk.approx_log().to_bits());
    println!("approx_log A bits={:#x}", a.approx_log().to_bits());
    println!("approx_log B bits={:#x}", b.approx_log().to_bits());
    println!("approx_log 2^512 bits={:#x}", p512.approx_log().to_bits());
    println!("approx_log m127 bits={:#x}", m127.approx_log().to_bits());
    println!("approx_log one bits={:#x}", Natural::ONE.approx_log().to_bits());
}
