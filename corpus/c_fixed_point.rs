#!/usr/bin/env mirvm
---
[dependencies]
fixed = "1"
---
// fixed 1.31（二进制定点算术：8/16/32/64/128 位存储 × 可变小数位）差分。
// 溢出谱系即 128 位压力：I64F64/U64F64 的 mul/div/解析缩放/hypot 在 crate
// 内部走 int256 双字中间量（src/int256.rs 的 U256/I256），外加
// wide_mul(I32F32→FixedI128) 的 64→128 位扩展乘法。
//
// ★ DIFF 留痕（历史 miscompile 候选，已消解）：mirvm 曾对宽中间量除法谱系
//   输入相关错值——128 位 sqrt 非极值输入产 0（`sqrt(i64f64 1e18)` native
//   bits=0x3b9aca000000000000000000 vs mirvm 0；`sqrt(u64f64 2)` native
//   bits=0x16a09e667f3bcc908 vs mirvm 0；U64F64::MAX 两侧一致）、i64 存储
//   hypot(3,4) native 0x500000000 vs mirvm 0。解释器/JIT(THRESHOLD=1) 同错。
//   2026-07 复核：§3 全部行与 native 逐字节一致（三维全绿），该错已随
//   128 位族修复消解，行原样保留作回归哨兵。
//
// ★ 128 位族恢复记录（2026-07 批5 三洞修好，谱系已复原）：
//   1. 定点→f64 方向：fixed 的 to_num::<f64>()/From 全家族经 helpers.rs
//      的 Widest::Unsigned(u128) 统一中转，直 cast 本已通，真 trap 点是
//      float_helper 里 u128 常量的 `!`（Not on 128）——修后 §6 双方向
//      完整谱系（0.1 三类型回转 bits、往返无损布尔、-0.0、1e19/MAX/MIN）。
//   2. 带符号 128 位的 Neg/Not（unary `-x`、`abs()`）已修：
//      §2 的 I64F64 链恢复 neg/abs 行。
//   3. u128 的 `saturating_add/sub`（core::num 内建路径）已修：
//      §5 无符号饱和谱系恢复 U64F64(u128) 呈现，与 U32F32(u64) 双侧对照。
//   另注 crate 自身语义（非 mirvm 问题）：fixed 的 sat/wrap/ovf 除法对
//   零除数与负输入 sqrt、NaN/±Inf 的 from_num 一律 panic，只走
//   checked_*=None 的错误路径纳入本 driver（§4）。
//
// 超越函数说明（任务给定 sin/cos/atan2/powi）：fixed 的 lib.rs 明确声明
// “No trigonometric functions … no pow … no log or exp”（代数/三角/超越
// 函数一律不提供，建议交给 cordic crate）。本 crate 实际提供的最近邻是
// sqrt（含 64/128 位 isqrt 路径）/ hypot（int256 平方和开方）/ recip，
// 超越节以该三件套顶替并打印 bits，忠于 crate 真实 API 面。
//
// 覆盖：①解析谱系（十进制/二进制/八进制/十六进制/指数/各种错误文本，
// 含 >27 位有效数字的 int256 缩放路径与解析舍入）；②四则运算链与小数
// 精度传播（÷3÷10 回乘、mul_add 融合、mean、lerp）；③sqrt/hypot/recip
// 谱系（含 U64F64::MAX 的 128 位开方）；④checked_* 溢出路径（MAX+DELTA、
// 除零、MIN.checked_neg、checked_from_num 超程/NaN、f64 边界 2^31）；
// ⑤saturating/wrapping/overflowing 三模式对照（add/sub/mul/neg/div 溢出/
// from_num 双方向）；⑥f64↔定点双方向舍入（0.1 三类型截断与回转 bits、
// 往返无损、ties-to-even 半 LSB 谱系、双源一致性、±0、1e19/MAX/MIN 回转）；
// ⑦wide_mul 128 位扩展乘法。
// 确定性：定点一律打印 Display+to_bits() 锁位型；无随机/时间/地址/
// HashMap 序；错误文本为 crate 内固定字符串；无 IO。
use fixed::types::{I0F16, I16F16, I32F32, I4F12, I64F64, U32F32, U64F64};

/// 打印定点值：十进制展开 + 位型。
macro_rules! pv {
    ($label:expr, $v:expr) => {
        println!("{} = {} bits={:#x}", $label, $v, $v.to_bits())
    };
}

/// 打印 Option<定点>。
macro_rules! popt {
    ($label:expr, $v:expr) => {
        match $v {
            Some(x) => println!("{} = {} bits={:#x}", $label, x, x.to_bits()),
            None => println!("{} = none", $label),
        }
    };
}

/// 打印解析结果（Ok 值 / Err 文本）。
macro_rules! pparse {
    ($label:expr, $v:expr) => {
        match $v {
            Ok(x) => println!("{} ok {} bits={:#x}", $label, x, x.to_bits()),
            Err(e) => println!("{} err {}", $label, e),
        }
    };
}

/// 打印 overflowing_* 的 (值, 溢出位)。
macro_rules! povf {
    ($label:expr, $v:expr) => {{
        let (x, o) = $v;
        println!("{} = {} bits={:#x} overflow={}", $label, x, x.to_bits(), o);
    }};
}

// ① 解析谱系
fn parse_spectrum() {
    println!("== 1 parse ==");
    let dec = [
        "3.14159265358979", "-2.5", "+7", "-0", "0.1", "1.5e-3", "6.25E+3",
        "123456789012345678901234567890.5", // >27 位有效数字 → int256 缩放
        "1e999", "", "abc", "12.34.56", "0x10", "1e2e3",
    ];
    for s in dec {
        pparse!(format!("I32F32::{s:?}"), I32F32::from_str(s));
    }
    // 边界溢出：I16F16 整数位 15（含符号外 16 位存储）
    for s in ["32767.5", "32768", "1e10", "-32768", "-32768.5"] {
        pparse!(format!("I16F16::{s:?}"), I16F16::from_str(s));
    }
    // 无符号大数边界 + 负数拒绝
    for s in ["18446744073709551615.5", "18446744073709551616", "-1"] {
        pparse!(format!("U64F64::{s:?}"), U64F64::from_str(s));
    }
    // 其它进制
    for s in ["1101.1011", "-10.1", "1.11e3", "101"] {
        pparse!(format!("I32F32::bin {s:?}"), I32F32::from_str_binary(s));
    }
    pparse!("I32F32::oct \"17.704\"", I32F32::from_str_octal("17.704"));
    pparse!("I32F32::hex \"ff.8\"", I32F32::from_str_hex("ff.8"));
    pparse!("I32F32::hex \"1.fp4\"", I32F32::from_str_hex("1.fp4"));
    pparse!("I32F32::hex \"dead.beef\"", I32F32::from_str_hex("dead.beef"));
    // 解析舍入 ties-to-even：I4F12 的 LSB=2^-12，半 LSB=2^-13
    for s in ["1.00048828125", "1.0009765625", "1.00146484375"] {
        pparse!(format!("I4F12::{s:?}"), I4F12::from_str(s));
    }
}

// ② 四则运算链 + 小数精度传播
fn arith_chains() {
    println!("== 2 arith ==");
    let a = I32F32::from_num(7.25);
    let b = I32F32::from_num(-2.5);
    let s1 = a + b;
    pv!("i32f32 7.25+(-2.5)", s1);
    let s2 = s1 * b;
    pv!("(..)*(-2.5)", s2);
    let s3 = s2 / 3;
    pv!("(..)/3", s3);
    let s4 = s3 - a;
    pv!("(..)-7.25", s4);
    let s5 = -s4;
    pv!("neg", s5);
    let s6 = s5.abs();
    pv!("abs", s6);

    let one = I32F32::from_num(1);
    let third = one / 3;
    pv!("i32f32 1/3", third);
    let back3 = third * 3;
    pv!("(1/3)*3", back3);
    println!("(1/3)*3 == 1 : {}", back3 == one);
    let tenth = one / 10;
    pv!("1/10", tenth);
    pv!("(1/10)*10", tenth * 10);

    // 融合乘加（全精度中间量再舍入一次）vs 分步
    let c = I32F32::from_num(0.375);
    pv!("mul_add 7.25*(-2.5)+0.375", a.mul_add(b, c));
    let step = a * b + c;
    pv!("a*b+c 分步", step);
    println!("mul_add == 分步 : {}", a.mul_add(b, c) == step);
    pv!("mean(1/3, 1.5)", third.mean(I32F32::from_num(1.5)));

    // lerp：t.lerp(start, end)
    let t = I32F32::from_num(0.625);
    pv!(
        "lerp t=0.625 [2, 9.5]",
        t.lerp(I32F32::from_num(2), I32F32::from_num(9.5))
    );
    let t2 = I32F32::from_num(2);
    pv!(
        "lerp t=2 [-1.5, 7]",
        t2.lerp(I32F32::from_num(-1.5), I32F32::from_num(7))
    );

    // 128 位存储的链：U64F64 只走非负
    let u = U64F64::from_num(9.75);
    let v = U64F64::from_num(0.5);
    let u1 = u - v;
    pv!("u64f64 9.75-0.5", u1);
    let u2 = u1 * 4;
    pv!("(..)*4", u2);
    let u3 = u2 / 7;
    pv!("(..)/7", u3);
    let u4 = u3 + U64F64::from_num(1024);
    pv!("(..)+1024", u4);
    let uu = U64F64::from_num(1);
    let uthird = uu / 3;
    pv!("u64f64 1/3", uthird);
    pv!("u64f64 (1/3)*3", uthird * 3);

    // I64F64（带符号 128 位）：混合链 + unary neg/abs（128 位 Neg/Not 已修，
    // 与 32 位链同形态呈现）
    let w = I64F64::from_num(-123.125);
    let w1 = w * I64F64::from_num(0.0078125); // ×2^-7
    pv!("i64f64 -123.125*0.0078125", w1);
    let w2 = w1 / 11;
    pv!("(..)/11", w2);
    let w3 = w2 + I64F64::DELTA;
    pv!("(..)+DELTA", w3);
    let w4 = w3 * I64F64::from_num(-0.5);
    pv!("(..)*(-0.5)", w4);
    let w5 = -w4;
    pv!("i64f64 neg", w5);
    let w6 = w5.abs();
    pv!("i64f64 abs(neg)", w6);
    let w7 = w.abs();
    pv!("i64f64 abs(-123.125)", w7);
}

// ③ 根号/倒数谱系（crate 无三角/幂/对数，见文件头说明）
fn sqrt_spectrum() {
    println!("== 3 sqrt/hypot/recip ==");
    for v in [0.0, 1.0, 2.0, 3.14159265358979, 65536.0] {
        pv!(format!("sqrt({v})"), I32F32::from_num(v).sqrt());
    }
    pv!("sqrt(u64f64 MAX)", U64F64::MAX.sqrt());
    pv!("sqrt(u64f64 2)", U64F64::from_num(2).sqrt());
    pv!("sqrt(i64f64 1e18)", I64F64::from_num(1_000_000_000_000_000_000u64).sqrt());
    popt!("checked_sqrt(-2.5)", I32F32::from_num(-2.5).checked_sqrt());
    // 负数下 saturating/wrapping_sqrt 与 sqrt 一样 panic——该路径由
    // popt!/checked 覆盖；这里用纯小数型 I0F16 的结果溢出触发两模式：
    // sqrt(0.25)=0.5 超出 I0F16 正值域 → sat=MAX / wrap=负值。
    let q = I0F16::from_num(0.25);
    pv!("i0f16 saturating_sqrt(0.25)", q.saturating_sqrt());
    pv!("i0f16 wrapping_sqrt(0.25)", q.wrapping_sqrt());
    pv!(
        "hypot(3,4)",
        I32F32::from_num(3).hypot(I32F32::from_num(4))
    );
    pv!(
        "hypot(5e-9,1.2e-8)",
        I64F64::from_num(5e-9).hypot(I64F64::from_num(1.2e-8))
    );
    popt!(
        "checked_hypot(MAX,MAX)",
        I32F32::MAX.checked_hypot(I32F32::MAX)
    );
    pv!(
        "saturating_hypot(MAX,MAX)",
        I32F32::MAX.saturating_hypot(I32F32::MAX)
    );
    pv!("recip(7)", I32F32::from_num(7).recip());
    pv!("recip(-0.0625)", I32F32::from_num(-0.0625).recip());
    pv!("saturating_recip(DELTA)", I32F32::DELTA.saturating_recip());
    pv!("wrapping_recip(DELTA)", I32F32::DELTA.wrapping_recip());
}

// ④ checked_* 溢出路径
fn checked_paths() {
    println!("== 4 checked ==");
    popt!("MAX.checked_add(DELTA)", I16F16::MAX.checked_add(I16F16::DELTA));
    popt!("MAX.checked_add(-1)", I16F16::MAX.checked_add(I16F16::from_num(-1)));
    popt!("MIN.checked_sub(DELTA)", I16F16::MIN.checked_sub(I16F16::DELTA));
    popt!(
        "100.checked_mul(1000)",
        I16F16::from_num(100).checked_mul(I16F16::from_num(1000))
    );
    popt!(
        "3.checked_div(0)",
        I16F16::from_num(3).checked_div(I16F16::ZERO)
    );
    popt!("MIN.checked_neg()", I16F16::MIN.checked_neg());
    popt!("checked_from_num(1e300)", I16F16::checked_from_num(1e300f64));
    popt!("checked_from_num(NaN)", I16F16::checked_from_num(f64::NAN));
    // f64 整型边界：I32F32 值域 [-2^31, 2^31-2^-32]
    popt!(
        "checked_from_num(2^31-1)",
        I32F32::checked_from_num(2_147_483_647.0f64)
    );
    popt!(
        "checked_from_num(2^31)",
        I32F32::checked_from_num(2_147_483_648.0f64)
    );
    popt!("u64 MAX.checked_add(DELTA)", U64F64::MAX.checked_add(U64F64::DELTA));
    popt!(
        "u64 0.checked_div(0)",
        U64F64::ZERO.checked_div(U64F64::ZERO)
    );
}

// ⑤ saturating / wrapping / overflowing 三模式对照
fn modes() {
    println!("== 5 modes ==");
    let one = I16F16::from_num(1);
    pv!("sat MAX+1", I16F16::MAX.saturating_add(one));
    pv!("wrap MAX+1", I16F16::MAX.wrapping_add(one));
    povf!("ovf MAX+1", I16F16::MAX.overflowing_add(one));
    pv!("sat MIN-1", I16F16::MIN.saturating_sub(one));
    pv!("wrap MIN-1", I16F16::MIN.wrapping_sub(one));
    povf!("ovf MIN-1", I16F16::MIN.overflowing_sub(one));
    pv!("sat -MIN", I16F16::MIN.saturating_neg());
    pv!("wrap -MIN", I16F16::MIN.wrapping_neg());
    povf!("ovf -MIN", I16F16::MIN.overflowing_neg());
    let h = I16F16::from_num(100);
    let k = I16F16::from_num(1000);
    pv!("sat 100*1000", h.saturating_mul(k));
    pv!("wrap 100*1000", h.wrapping_mul(k));
    povf!("ovf 100*1000", h.overflowing_mul(k));
    // 除法溢出（非零除数）：fixed 的 sat/wrap/ovf div 在除数为 0 时与
    // plain div 一样 panic（§4 已用 checked_div(0)=none 覆盖错误路径），
    // 故此处以 3/DELTA=196608 超 I16F16 上界驱动三模式。
    let three = I16F16::from_num(3);
    let mthree = I16F16::from_num(-3);
    pv!("sat 3/DELTA", three.saturating_div(I16F16::DELTA));
    pv!("sat -3/DELTA", mthree.saturating_div(I16F16::DELTA));
    pv!("wrap 3/DELTA", three.wrapping_div(I16F16::DELTA));
    povf!("ovf 3/DELTA", three.overflowing_div(I16F16::DELTA));
    // f64 超程转换的三模式
    pv!("sat_from_num(1e300)", I16F16::saturating_from_num(1e300f64));
    pv!("wrap_from_num(1e300)", I16F16::wrapping_from_num(1e300f64));
    povf!("ovf_from_num(1e300)", I16F16::overflowing_from_num(1e300f64));
    pv!("sat_from_num(-1e300)", I16F16::saturating_from_num(-1e300f64));
    // 无符号负源
    pv!("u sat_from_num(-5)", U32F32::saturating_from_num(-5.0f64));
    pv!("u wrap_from_num(-1.5)", U32F32::wrapping_from_num(-1.5f64));
    povf!("u ovf_from_num(-1.5)", U32F32::overflowing_from_num(-1.5f64));
    // 无符号饱和谱系：u64 存储 + u128 存储双侧呈现（u128 的
    // saturating_add/sub 已修，与 wrap/ovf 两模式同段对照）
    pv!("u sat MAX+1", U32F32::MAX.saturating_add(U32F32::from_num(1)));
    pv!("u sat 0-1", U32F32::ZERO.saturating_sub(U32F32::from_num(1)));
    pv!("u64 sat MAX+1", U64F64::MAX.saturating_add(U64F64::from_num(1)));
    pv!("u64 sat 0-1", U64F64::ZERO.saturating_sub(U64F64::from_num(1)));
    pv!("u64 wrap MAX+1", U64F64::MAX.wrapping_add(U64F64::from_num(1)));
    povf!("u64 ovf MAX+1", U64F64::MAX.overflowing_add(U64F64::from_num(1)));
}

// ⑥ 与 f64 互转舍入（双方向：定点→f64 经 Widest(u128) 位操作通道，已修）
fn f64_rounding() {
    println!("== 6 f64 rounding ==");
    // f64 → 定点：0.1 的二进制截断
    let d1 = I32F32::from_num(0.1f64);
    pv!("i32f32 from 0.1", d1);
    let d2 = U64F64::from_num(0.1f64);
    pv!("u64f64 from 0.1", d2);
    let d3 = I64F64::from_num(0.1f64);
    pv!("i64f64 from 0.1", d3);
    // 定点 → f64：三存储宽回转（bits 锚定；to_num 与 From 同族）
    let r1: f64 = d1.to_num();
    println!("i32f32 0.1 -> f64 bits={:#x}", r1.to_bits());
    let r2: f64 = d2.to_num();
    println!("u64f64 0.1 -> f64 bits={:#x}", r2.to_bits());
    let r3: f64 = d3.to_num();
    println!("i64f64 0.1 -> f64 bits={:#x}", r3.to_bits());
    let r4: f64 = I32F32::from_num(2.5).to_num();
    println!("i32f32 2.5 -> f64 bits={:#x}", r4.to_bits());
    // 往返一致性：值域内 定点→f64→定点 逐位无损
    println!("0.1 rt i32f32== : {}", I32F32::from_num(r1) == d1);
    println!("0.1 rt u64f64== : {}", U64F64::from_num(r2) == d2);
    println!("0.1 rt i64f64== : {}", I64F64::from_num(r3) == d3);
    // 半 LSB ties-to-even（I4F12，半 LSB = 2^-13）
    let half = 1.0f64 / 8192.0;
    for k in [1.0f64, 3.0, 5.0, 7.0, 9.0] {
        pv!(format!("i4f12 from {k}*2^-13"), I4F12::from_num(k * half));
    }
    pv!("i4f12 from 0.1", I4F12::from_num(0.1f64));
    // 双源一致性：from_num(f64 二进制源) 与 from_str(十进制解析源)
    let ds = I32F32::from_str("0.1").unwrap();
    println!("0.1 from_num==from_str : {}", d1 == ds);
    let ds2 = I32F32::from_str("2.5").unwrap();
    println!("2.5 from_num==from_str : {}", I32F32::from_num(2.5f64) == ds2);
    let nz = I32F32::from_num(-0.0f64);
    pv!("i32f32 from -0.0", nz);
    println!("-0.0 bits==0 : {}", nz.to_bits() == 0);
    let rnz: f64 = nz.to_num();
    println!("-0.0 -> f64 bits={:#x}", rnz.to_bits());
    // NaN/±Inf：fixed 的 sat/wrap/ovf_from_num 对非有限源一律 panic
    //（"NaN"/"infinite"），唯一安全入口是 §4 已覆盖的 checked_from_num→none。
    // 128 位 f64↔定点（from_num 走 int256 软路径）：
    pv!("u64f64 from 1e19", U64F64::from_num(1e19f64));
    let rb: f64 = U64F64::from_num(1e19f64).to_num();
    println!("u64f64 1e19 -> f64 bits={:#x}", rb.to_bits());
    let rmax: f64 = U64F64::MAX.to_num();
    println!("u64f64 MAX -> f64 bits={:#x}", rmax.to_bits());
    let rmin: f64 = I64F64::MIN.to_num();
    println!("i64f64 MIN -> f64 bits={:#x}", rmin.to_bits());
}

// ⑦ wide_mul：64 → 128 位扩展乘法
fn wide_mul_section() {
    println!("== 7 wide_mul ==");
    let a = I32F32::MAX;
    let b = I32F32::from_num(3.5);
    let w = a.wide_mul(b); // FixedI128<U64>
    pv!("i32f32 MAX wide*3.5 -> i64f64", w);
    let c = I32F32::from_num(-2.25);
    let d = I32F32::from_num(-7.75);
    pv!("(-2.25) wide*(-7.75)", c.wide_mul(d));
    let e = U32F32::MAX;
    let f = U32F32::from_num(1.5);
    pv!("u32f32 MAX wide*1.5 -> u64f64", e.wide_mul(f));
    let g = I16F16::DELTA;
    pv!("i16f16 DELTA wide*DELTA -> i32f32", g.wide_mul(g));
}

fn main() {
    parse_spectrum();
    arith_chains();
    sqrt_spectrum();
    checked_paths();
    modes();
    f64_rounding();
    wide_mul_section();
}
