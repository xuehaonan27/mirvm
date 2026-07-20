//! mirvm_* 运行期助手（自 jit_compile.rs J3-J5 整搬）：c2i 万能壳/
//! unreachable/div_zero/volatile + 128/f128/f16 宿主直算 21 件 + libm
//! 符号表。JIT 码经 import symbol 调回引擎；注册点 = compiler.rs。

use super::*;
use super::compiler::SHARED;

pub(super) extern "C-unwind" fn mirvm_c2i(func: u64, args: *const u64, n: u64, ret: *mut u64) {
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let ctx = crate::vm::engine::ctx::attach(shared);
    let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = crate::vm::engine::interp::call_guest(ctx, func as u32, a);
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// T1-b CallIndirect 助手（m5.4-design §3.2；interp runblocks CallIndirect 臂
/// 同一派发：fn_addrs 反查 → call_guest 本体；未命中 + native_sig →
/// ffi::call_addr 本体；空槽 null_ok 空操作 / 空指针与未知目标的诊断同 interp）。
pub(super) extern "C-unwind" fn mirvm_call_indirect(
    addr: u64,
    args: *const u64,
    n: u64,
    ret: *mut u64,
    null_ok: u64,
    native_sig: u64,
    caller: u64,
) {
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let ctx = crate::vm::engine::ctx::attach(shared);
    let module = &shared.module;
    if null_ok != 0 && addr == 0 {
        return; // dyn 虚 drop 空槽：空操作（interp 同）
    }
    let caller_name = &module.funcs[caller as usize].name;
    if addr == 0 {
        crate::vm::engine::interp::engine_abort(&format!(
            "间接调用空 fn 指针（调用者 {caller_name}）"
        ));
    }
    let av = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = if let Some(&fid) = module.fn_addrs.get(&addr) {
        crate::vm::engine::interp::call_guest(ctx, fid, av)
    } else if native_sig != 0 {
        // guest 持 native 真码 fn ptr（运行期 dlsym 所得）→ 按冻结签名直调；
        // Agg 返回时首槽即目的地址（libffi sret 不占参数位，剔除后直调）——interp 同
        let nsig = unsafe { &*(native_sig as *const crate::vm::engine::ir::ForeignSig) };
        let (ret_dst, arg_slice) = if matches!(nsig.ret, crate::vm::engine::ir::FfiKind::Agg(_))
        {
            (av.first().copied(), &av[1..])
        } else {
            (None, av)
        };
        (
            crate::vm::engine::ffi::call_addr(addr as usize, nsig, arg_slice, ret_dst),
            0,
        )
    } else {
        crate::vm::engine::interp::engine_abort(&format!(
            "间接调用目标 {addr:#x} 不是已知 fn 条目（调用者 {caller_name}）"
        ));
    };
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// T1-b TlsRef 助手（同本体 interp::tls_addr 的惰性物化——每线程实例块）。
pub(super) extern "C-unwind" fn mirvm_tls_ref(id: u64) -> u64 {
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let ctx = crate::vm::engine::ctx::attach(shared);
    crate::vm::engine::interp::tls_addr(ctx, id as u32)
}

/// Unreachable 终止子的诊断口径与解释器一致（不用裸 trap 的 SIGILL）。
pub(super) extern "C-unwind" fn mirvm_jit_unreachable(func: u64) -> ! {
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let name = shared
        .module
        .funcs
        .get(func as usize)
        .map(|f| &*f.name)
        .unwrap_or("?");
    eprintln!("mirvm[jit]: 到达 Unreachable（fn {name}）");
    std::process::abort();
}

// ===== M5.4b 助手（与 interp 共享实现本体，不复制逻辑）=====

/// 除零诊断退出（M5.4b-1）：与 interp engine_abort 的文案/退出码逐位一致。
pub(super) extern "C-unwind" fn mirvm_jit_div_zero(kind: u64) -> ! {
    let what = match kind {
        0 => "guest 整除以零",
        1 => "guest 取余以零",
        2 => "guest 128 位整除以零",
        _ => "guest 128 位取余以零",
    };
    eprintln!("mirvm[m4-engine]: {what}");
    std::process::exit(70);
}

/// volatile 读（M5.4b-1）：走 interp 的 opaque 字节载体 + 分块分解同一实现。
pub(super) extern "C-unwind" fn mirvm_volatile_load(addr: u64, dst: u64, size: u64) {
    crate::vm::engine::interp::mem_read_volatile(addr, dst, size as u32);
}

/// volatile 写（同上）。
pub(super) extern "C-unwind" fn mirvm_volatile_store(addr: u64, src: u64, size: u64) {
    crate::vm::engine::interp::mem_write_volatile(addr, src, size as u32);
}

// ===== M5.4b-3 助手（f16/f128/128 位族；interp 的宿主直算同一通道——
// 助手用 Rust f16/f128/i128/u128 算术，rustc 降到与 interp/native 同一批
// compiler-builtins/__*tf* 与 glibc *f128 libm 符号，同源即位同）=====

pub(super) fn lo_hi(lo: u64, hi: u64) -> u128 {
    (lo as u128) | ((hi as u128) << 64)
}
pub(super) fn hi_lo(v: u128) -> (u64, u64) {
    (v as u64, (v >> 64) as u64)
}
pub(super) fn f128_of(lo: u64, hi: u64) -> f128 {
    f128::from_bits(lo_hi(lo, hi))
}
pub(super) fn pair_of(v: f128) -> (u64, u64) {
    hi_lo(v.to_bits())
}

/// i128/u128 overflowing_add/sub/mul（Bin128 with_overflow 的 flag；写结果对到 out）
pub(super) extern "C-unwind" fn mirvm_bin128_ovf(
    op: u64,
    signed: bool,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) -> u64 {
    let (r, ovf) = if signed {
        let (x, y) = (lo_hi(alo, ahi) as i128, lo_hi(blo, bhi) as i128);
        match op {
            0 => x.overflowing_add(y),
            1 => x.overflowing_sub(y),
            _ => x.overflowing_mul(y),
        }
    } else {
        let (x, y) = (lo_hi(alo, ahi), lo_hi(blo, bhi));
        match op {
            0 => (x.overflowing_add(y).0 as i128, x.overflowing_add(y).1),
            1 => (x.overflowing_sub(y).0 as i128, x.overflowing_sub(y).1),
            _ => (x.overflowing_mul(y).0 as i128, x.overflowing_mul(y).1),
        }
    };
    let (lo, hi) = hi_lo(r as u128);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
    ovf as u64
}

/// f128 四则（op: 0=add 1=sub 2=mul 3=rem(fmodf128) 4=div）
pub(super) extern "C-unwind" fn mirvm_f128_bin(
    op: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    out: *mut u64,
) {
    let (a, b) = (f128_of(alo, ahi), f128_of(blo, bhi));
    let r = match op {
        0 => a + b,
        1 => a - b,
        2 => a * b,
        4 => a / b,
        _ => a % b,
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 比较（cc: 0=Eq 1=Ne 2=Lt 3=Le 4=Gt 5=Ge；IEEE 语义 NaN 全 false 除 Ne）
pub(super) extern "C-unwind" fn mirvm_f128_cmp(cc: u64, alo: u64, ahi: u64, blo: u64, bhi: u64) -> u64 {
    let (a, b) = (f128_of(alo, ahi), f128_of(blo, bhi));
    match cc {
        0 => (a == b) as u64,
        1 => (a != b) as u64,
        2 => (a < b) as u64,
        3 => (a <= b) as u64,
        4 => (a > b) as u64,
        _ => (a >= b) as u64,
    }
}

/// f128 单目（op: 0=neg；其余为一元数学族——glibc *f128 libm 符号）
pub(super) extern "C-unwind" fn mirvm_f128_un(op: u64, alo: u64, ahi: u64, out: *mut u64) {
    let a = f128_of(alo, ahi);
    let r = match op {
        0 => -a,
        1 => a.sqrt(),
        2 => a.sin(),
        3 => a.cos(),
        4 => a.exp(),
        5 => a.exp2(),
        6 => a.ln(),
        7 => a.log2(),
        8 => a.log10(),
        9 => a.abs(),
        10 => a.floor(),
        11 => a.ceil(),
        12 => a.trunc(),
        13 => a.round(),
        _ => a.round_ties_even(),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 数学二元（op: 0=pow 1=powi 2=copysign 3=minnum 4=maxnum 5=fma(c 在 out[2..4]）
pub(super) extern "C-unwind" fn mirvm_f128_math(
    op: u64,
    alo: u64,
    ahi: u64,
    blo: u64,
    bhi: u64,
    clo: u64,
    chi: u64,
    out: *mut u64,
) {
    let (a, b, c) = (f128_of(alo, ahi), f128_of(blo, bhi), f128_of(clo, chi));
    let r = match op {
        0 => a.powf(b),
        1 => a.powi(b as i32),
        2 => a.copysign(b),
        3 => a.min(b),
        4 => a.max(b),
        _ => a.mul_add(b, c),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// 标量 → f128（kind: 0=f16 1=f32 2=f64 3..7=int(8/16/32/64 位, signed=kind-3 偶=signed?）
/// kind: 0..2 = float 互转；3/4/5/6 = i8/u8..i64/u64 选（3=i8,4=u8,5=i16,6=u16,7=i32,8=u32,9=i64,10=u64）
pub(super) extern "C-unwind" fn mirvm_f128_from_scalar(kind: u64, v: u64, out: *mut u64) {
    let r = match kind {
        0 => f128::from(f16::from_bits(v as u16)),
        1 => f128::from(f32::from_bits(v as u32)),
        2 => f128::from(f64::from_bits(v)),
        3 => f128::from(v as i8),
        4 => f128::from(v as u8),
        5 => f128::from(v as i16),
        6 => f128::from(v as u16),
        7 => f128::from(v as i32),
        8 => f128::from(v as u32),
        9 => f128::from(v as i64),
        _ => f128::from(v),
    };
    let (lo, hi) = pair_of(r);
    unsafe {
        *out = lo;
        *out.add(1) = hi;
    }
}

/// f128 → 标量（kind 同上；float 互转位型 / int `as` 饱和语义）
pub(super) extern "C-unwind" fn mirvm_f128_to_scalar(kind: u64, alo: u64, ahi: u64) -> u64 {
    let a = f128_of(alo, ahi);
    match kind {
        0 => (a as f16).to_bits() as u64,
        1 => (a as f32).to_bits() as u64,
        2 => (a as f64).to_bits(),
        3 => (a as i8) as u8 as u64,
        4 => (a as u8) as u64,
        5 => (a as i16) as u16 as u64,
        6 => (a as u16) as u64,
        7 => (a as i32) as u32 as u64,
        8 => (a as u32) as u64,
        9 => (a as i64) as u64,
        _ => a as u64,
    }
}

/// i128/u128 ↔ f128（signed: 0=unsigned, 1=signed；方向 from: int→f128 / to: f128→int 饱和）
pub(super) extern "C-unwind" fn mirvm_f128_from_wide(signed: bool, lo: u64, hi: u64, out: *mut u64) {
    let r = if signed {
        (lo_hi(lo, hi) as i128) as f128
    } else {
        lo_hi(lo, hi) as f128
    };
    let (l, h) = pair_of(r);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}
pub(super) extern "C-unwind" fn mirvm_f128_to_wide(signed: bool, alo: u64, ahi: u64, out: *mut u64) {
    let a = f128_of(alo, ahi);
    let v: u128 = if signed {
        (a as i128) as u128
    } else {
        a as u128
    };
    let (l, h) = hi_lo(v);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}

/// float → i128/u128 饱和（Wide128ToFloat 的对侧；kind: 0=f16 1=f32 2=f64）
pub(super) extern "C-unwind" fn mirvm_float_to_wide(kind: u64, v: u64, signed: bool, out: *mut u64) {
    let r: u128 = match (kind, signed) {
        (0, true) => (f16::from_bits(v as u16) as i128) as u128,
        (0, false) => f16::from_bits(v as u16) as u128,
        (1, true) => (f32::from_bits(v as u32) as i128) as u128,
        (1, false) => f32::from_bits(v as u32) as u128,
        (2, true) => (f64::from_bits(v) as i128) as u128,
        _ => f64::from_bits(v) as u128,
    };
    let (l, h) = hi_lo(r);
    unsafe {
        *out = l;
        *out.add(1) = h;
    }
}

/// i128/u128 → f16（Wide128ToFloat 的 f16 目标；f32/f64 目标走 CLIF fcvt）
pub(super) extern "C-unwind" fn mirvm_wide_to_f16(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f16
    } else {
        lo_hi(lo, hi) as f16
    };
    v.to_bits() as u64
}

// ===== f16 助手（interp 的宿主直算通道）=====

/// f16 四则（op 同 mirvm_f128_bin；参数/返回 = f16 位型的 u64）
pub(super) extern "C-unwind" fn mirvm_f16_bin(op: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    let r = match op {
        0 => x + y,
        1 => x - y,
        2 => x * y,
        _ => x % y,
    };
    r.to_bits() as u64
}
pub(super) extern "C-unwind" fn mirvm_f16_cmp(cc: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    match cc {
        0 => (x == y) as u64,
        1 => (x != y) as u64,
        2 => (x < y) as u64,
        3 => (x <= y) as u64,
        4 => (x > y) as u64,
        _ => (x >= y) as u64,
    }
}
pub(super) extern "C-unwind" fn mirvm_f16_neg(a: u64) -> u64 {
    (-f16::from_bits(a as u16)).to_bits() as u64
}
/// f16 互转（kind: 1=→f32 2=→f64 3=f32→ 4=f64→）
pub(super) extern "C-unwind" fn mirvm_f16_cast(kind: u64, v: u64) -> u64 {
    match kind {
        1 => (f16::from_bits(v as u16) as f32).to_bits() as u64,
        2 => (f16::from_bits(v as u16) as f64).to_bits(),
        3 => (f32::from_bits(v as u32) as f16).to_bits() as u64,
        _ => (f64::from_bits(v) as f16).to_bits() as u64,
    }
}
/// f16 ↔ int（to: 0=i8 1=u8 2=i16 3=u16 4=i32 5=u32 6=i64 7=u64；from 同码）
pub(super) extern "C-unwind" fn mirvm_f16_to_int(kind: u64, a: u64) -> u64 {
    let x = f16::from_bits(a as u16);
    match kind {
        0 => (x as i8) as u8 as u64,
        1 => (x as u8) as u64,
        2 => (x as i16) as u16 as u64,
        3 => (x as u16) as u64,
        4 => (x as i32) as u32 as u64,
        5 => (x as u32) as u64,
        6 => (x as i64) as u64,
        _ => x as u64,
    }
}
pub(super) extern "C-unwind" fn mirvm_f16_from_int(kind: u64, v: u64) -> u64 {
    let r = match kind {
        0 => (v as i8) as f16,
        1 => (v as u8) as f16,
        2 => (v as i16) as f16,
        3 => (v as u16) as f16,
        4 => (v as i32) as f16,
        5 => (v as u32) as f16,
        6 => (v as i64) as f16,
        _ => v as f16,
    };
    r.to_bits() as u64
}

// M5.4b-2：powi 走 compiler-builtins（Rust 的 powi 降到同一批符号）
unsafe extern "C" {
    fn __powidf2(x: f64, n: i32) -> f64;
    fn __powisf2(x: f32, n: i32) -> f32;
}

/// M5.4b-2 libm 符号表（注册进 JITBuilder；interp 的 libm 宿主直算同批符号。
/// libc crate 已不带数学函数绑定 → 直接 extern 声明取地址（进程本就链 libm）。
mod libm_decls {
    #![allow(dead_code)]
    unsafe extern "C" {
        pub fn sqrtf();
        pub fn sqrt();
        pub fn sinf();
        pub fn sin();
        pub fn cosf();
        pub fn cos();
        pub fn expf();
        pub fn exp();
        pub fn exp2f();
        pub fn exp2();
        pub fn logf();
        pub fn log();
        pub fn log2f();
        pub fn log2();
        pub fn log10f();
        pub fn log10();
        pub fn fabsf();
        pub fn fabs();
        pub fn floorf();
        pub fn floor();
        pub fn ceilf();
        pub fn ceil();
        pub fn truncf();
        pub fn trunc();
        pub fn roundf();
        pub fn round();
        pub fn rintf();
        pub fn rint();
        pub fn powf();
        pub fn pow();
        pub fn copysignf();
        pub fn copysign();
        pub fn fminf();
        pub fn fmin();
        pub fn fmaxf();
        pub fn fmax();
        pub fn fmodf();
        pub fn fmod();
    }
}
pub(super) fn libm_syms() -> Vec<(&'static str, usize)> {
    vec![
        ("sqrtf", libm_decls::sqrtf as *const () as usize),
        ("sqrt", libm_decls::sqrt as *const () as usize),
        ("sinf", libm_decls::sinf as *const () as usize),
        ("sin", libm_decls::sin as *const () as usize),
        ("cosf", libm_decls::cosf as *const () as usize),
        ("cos", libm_decls::cos as *const () as usize),
        ("expf", libm_decls::expf as *const () as usize),
        ("exp", libm_decls::exp as *const () as usize),
        ("exp2f", libm_decls::exp2f as *const () as usize),
        ("exp2", libm_decls::exp2 as *const () as usize),
        ("logf", libm_decls::logf as *const () as usize),
        ("log", libm_decls::log as *const () as usize),
        ("log2f", libm_decls::log2f as *const () as usize),
        ("log2", libm_decls::log2 as *const () as usize),
        ("log10f", libm_decls::log10f as *const () as usize),
        ("log10", libm_decls::log10 as *const () as usize),
        ("fabsf", libm_decls::fabsf as *const () as usize),
        ("fabs", libm_decls::fabs as *const () as usize),
        ("floorf", libm_decls::floorf as *const () as usize),
        ("floor", libm_decls::floor as *const () as usize),
        ("ceilf", libm_decls::ceilf as *const () as usize),
        ("ceil", libm_decls::ceil as *const () as usize),
        ("truncf", libm_decls::truncf as *const () as usize),
        ("trunc", libm_decls::trunc as *const () as usize),
        ("roundf", libm_decls::roundf as *const () as usize),
        ("round", libm_decls::round as *const () as usize),
        ("rintf", libm_decls::rintf as *const () as usize),
        ("rint", libm_decls::rint as *const () as usize),
        ("powf", libm_decls::powf as *const () as usize),
        ("pow", libm_decls::pow as *const () as usize),
        ("copysignf", libm_decls::copysignf as *const () as usize),
        ("copysign", libm_decls::copysign as *const () as usize),
        ("fminf", libm_decls::fminf as *const () as usize),
        ("fmin", libm_decls::fmin as *const () as usize),
        ("fmaxf", libm_decls::fmaxf as *const () as usize),
        ("fmax", libm_decls::fmax as *const () as usize),
        ("fmodf", libm_decls::fmodf as *const () as usize),
        ("fmod", libm_decls::fmod as *const () as usize),
    ]
}

// ===== 准入（M5.3 v1 标量子集 + M5.4a 内存操作数；拒绝 = 永久维持解释）=====

