//! M5.3b：字节码 → Cranelift 翻译器（标量子集）+ 编译服务线程（m5.3-design §4，D3/D4/D5）。
//!
//! 输入 = 冻结的 `ir::FuncBody`（D3：JIT 吃字节码不吃 MIR；tcx 不出执行相）。
//! 语义契约 = **与解释器逐位一致**（JIT-on/off 差分是第一 oracle）：所有值保持
//! "I64 零扩到宽"的槽不变量，运算按 interp 的 int_bin/int_cmp/int_ovf 恒等式镜像，
//! 结果按宽 band 掩回。帧局部全部提升 Cranelift SSA 变量（v1 准入排除取址/内存
//! 操作数 ⇒ 无栈帧内存）；入口统一 def 0（有效 MIR 无读前未写路径，此为确定化）。
//!
//! 调用（D5 两入口 + PLT）：
//! - **fast**：纯 guest 签名（n×I64 → 0/1×I64）。编译码间经 `slots_fast[callee]`
//!   内存间接（load + call_indirect，调用点恒定形状）；未编译 callee 的槽先发
//!   **c2i 蹦床**（fast 形状，内部打包实参调 `mirvm_c2i` 回解释器）。
//! - **packed**：`extern "C-unwind" fn(*const u64, *mut u64)`——interp 的 i2c 一跳
//!   （call_guest 读 `slots[f]`）。
//!
//! 发布序 = 先 fast 后 packed（Release）；call_guest Acquire 读 ⇒ 进入编译码的
//! 线程必见其 callee 蹦床/入口（happens-before 链）。
//!
//! unwind（D6 v1 = CFI-only）：spike5 管线——create_unwind_info → gimli FrameTable
//! → 逐 FDE `__register_frame`（libgcc 语义 + CIE 判别字段）。准入已排除 cleanup 边
//! （unwind-transparent：panic 只穿透，不着陆）。
//!
//! 单 worker 线程持 JITModule（代码内存进程生命周期，cranelift-jit 无逐函数释放）。

use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, MemFlagsData, Signature, StackSlot, StackSlotData, StackSlotKind,
    TrapCode, Value, types,
};
use cranelift_codegen::isa::unwind::UnwindInfo;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId as ClifFuncId, Linkage, Module as ClifModule};

use super::ctx::Shared;
use super::ir::{
    self, IntBinOp, IntCc, Operand, OvfOp, ParamAbi, RetAbi, RetDest, ScalarPlace, Slot, Stmt,
    SwitchDiscr, Terminator, UnwindAction, Width,
};

/// c2i 壳的引擎定位（单引擎进程模型，与 TRACK_DIAGNOSTIC 全局钩同一假设面）。
static SHARED: std::sync::atomic::AtomicPtr<Shared> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

/// 启动编译服务（run_vm_engine 在 Shared 定型后调用；--jit off 时不启动）。
pub fn start(shared: &'static Shared) {
    if !shared.jit.enabled {
        return;
    }
    SHARED.store(shared as *const Shared as *mut Shared, Ordering::Release);
    let (tx, rx): (Sender<u32>, Receiver<u32>) = std::sync::mpsc::channel();
    *shared.jit.queue.lock().unwrap() = Some(tx);
    // 编译失败/线程死亡 = 静默维持解释（语义面零依赖 JIT）
    let _ = std::thread::Builder::new()
        .name("mirvm-jit".into())
        .spawn(move || worker(shared, rx));
}

fn worker(shared: &'static Shared, rx: Receiver<u32>) {
    let dbg = std::env::var_os("MIRVM_JIT_DEBUG").is_some();
    let mut c = Compiler::new(shared);
    while let Ok(func) = rx.recv() {
        if dbg {
            eprintln!(
                "mirvm-jit-debug: 收到 f{func}（{}）",
                shared.module.funcs[func as usize].name
            );
        }
        c.compile(func);
        if dbg {
            let ok = shared.jit.slots[func as usize].load(Ordering::Acquire) != 0;
            if ok {
                let addr = shared.jit.slots[func as usize].load(Ordering::Acquire);
                let fast = shared.jit.slots_fast[func as usize].load(Ordering::Acquire);
                eprintln!(
                    "mirvm-jit-debug: f{func} 发布={ok} @{addr:#x} fast@{fast:#x}（{}）",
                    shared.module.funcs[func as usize].name
                );
            } else {
                eprintln!("mirvm-jit-debug: f{func} 发布={ok}");
            }
        }
    }
}

// ===== 运行期助手（JIT 码经 import symbol 调回引擎）=====

/// c2i 万能壳：编译码调未编译 guest 函数（经蹦床打包）→ 回解释器。
/// ctx 恢复 = 边界 TLS attach（thunk 工厂同款，幂等）。
extern "C-unwind" fn mirvm_c2i(func: u64, args: *const u64, n: u64, ret: *mut u64) {
    let shared = unsafe { &*SHARED.load(Ordering::Acquire) };
    let ctx = super::ctx::attach(shared);
    let a = unsafe { std::slice::from_raw_parts(args, n as usize) };
    let (lo, hi) = super::interp::call_guest(ctx, func as u32, a);
    unsafe {
        *ret = lo;
        *ret.add(1) = hi;
    }
}

/// Unreachable 终止子的诊断口径与解释器一致（不用裸 trap 的 SIGILL）。
extern "C-unwind" fn mirvm_jit_unreachable(func: u64) -> ! {
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
extern "C-unwind" fn mirvm_jit_div_zero(kind: u64) -> ! {
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
extern "C-unwind" fn mirvm_volatile_load(addr: u64, dst: u64, size: u64) {
    super::interp::mem_read_volatile(addr, dst, size as u32);
}

/// volatile 写（同上）。
extern "C-unwind" fn mirvm_volatile_store(addr: u64, src: u64, size: u64) {
    super::interp::mem_write_volatile(addr, src, size as u32);
}

// ===== M5.4b-3 助手（f16/f128/128 位族；interp 的宿主直算同一通道——
// 助手用 Rust f16/f128/i128/u128 算术，rustc 降到与 interp/native 同一批
// compiler-builtins/__*tf* 与 glibc *f128 libm 符号，同源即位同）=====

fn lo_hi(lo: u64, hi: u64) -> u128 {
    (lo as u128) | ((hi as u128) << 64)
}
fn hi_lo(v: u128) -> (u64, u64) {
    (v as u64, (v >> 64) as u64)
}
fn f128_of(lo: u64, hi: u64) -> f128 {
    f128::from_bits(lo_hi(lo, hi))
}
fn pair_of(v: f128) -> (u64, u64) {
    hi_lo(v.to_bits())
}

/// i128/u128 overflowing_add/sub/mul（Bin128 with_overflow 的 flag；写结果对到 out）
extern "C-unwind" fn mirvm_bin128_ovf(
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
extern "C-unwind" fn mirvm_f128_bin(
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
extern "C-unwind" fn mirvm_f128_cmp(cc: u64, alo: u64, ahi: u64, blo: u64, bhi: u64) -> u64 {
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
extern "C-unwind" fn mirvm_f128_un(op: u64, alo: u64, ahi: u64, out: *mut u64) {
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
extern "C-unwind" fn mirvm_f128_math(
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
extern "C-unwind" fn mirvm_f128_from_scalar(kind: u64, v: u64, out: *mut u64) {
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
extern "C-unwind" fn mirvm_f128_to_scalar(kind: u64, alo: u64, ahi: u64) -> u64 {
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
extern "C-unwind" fn mirvm_f128_from_wide(signed: bool, lo: u64, hi: u64, out: *mut u64) {
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
extern "C-unwind" fn mirvm_f128_to_wide(signed: bool, alo: u64, ahi: u64, out: *mut u64) {
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
extern "C-unwind" fn mirvm_float_to_wide(kind: u64, v: u64, signed: bool, out: *mut u64) {
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
extern "C-unwind" fn mirvm_wide_to_f16(lo: u64, hi: u64, signed: bool) -> u64 {
    let v = if signed {
        (lo_hi(lo, hi) as i128) as f16
    } else {
        lo_hi(lo, hi) as f16
    };
    v.to_bits() as u64
}

// ===== f16 助手（interp 的宿主直算通道）=====

/// f16 四则（op 同 mirvm_f128_bin；参数/返回 = f16 位型的 u64）
extern "C-unwind" fn mirvm_f16_bin(op: u64, a: u64, b: u64) -> u64 {
    let (x, y) = (f16::from_bits(a as u16), f16::from_bits(b as u16));
    let r = match op {
        0 => x + y,
        1 => x - y,
        2 => x * y,
        _ => x % y,
    };
    r.to_bits() as u64
}
extern "C-unwind" fn mirvm_f16_cmp(cc: u64, a: u64, b: u64) -> u64 {
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
extern "C-unwind" fn mirvm_f16_neg(a: u64) -> u64 {
    (-f16::from_bits(a as u16)).to_bits() as u64
}
/// f16 互转（kind: 1=→f32 2=→f64 3=f32→ 4=f64→）
extern "C-unwind" fn mirvm_f16_cast(kind: u64, v: u64) -> u64 {
    match kind {
        1 => (f16::from_bits(v as u16) as f32).to_bits() as u64,
        2 => (f16::from_bits(v as u16) as f64).to_bits(),
        3 => (f32::from_bits(v as u32) as f16).to_bits() as u64,
        _ => (f64::from_bits(v) as f16).to_bits() as u64,
    }
}
/// f16 ↔ int（to: 0=i8 1=u8 2=i16 3=u16 4=i32 5=u32 6=i64 7=u64；from 同码）
extern "C-unwind" fn mirvm_f16_to_int(kind: u64, a: u64) -> u64 {
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
extern "C-unwind" fn mirvm_f16_from_int(kind: u64, v: u64) -> u64 {
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
fn libm_syms() -> Vec<(&'static str, usize)> {
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

fn scalar_slot(p: &ScalarPlace) -> Option<Slot> {
    match p {
        ScalarPlace::Slot(s) => Some(*s),
        ScalarPlace::Mem { .. } => None,
    }
}

fn operand_ok(op: &Operand) -> bool {
    match op {
        Operand::Slot(_) | Operand::Imm { .. } => true,
        // M5.4a：内存/地址操作数（place 求值通道全部内联）
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => place_ok(expr),
        Operand::SubImm { base, .. } => operand_ok(base),
    }
}

/// PlaceExpr 准入：base 全可（Local=帧槽/Static=绝对地址立即数）；
/// VTableAlignOffset 的 meta 需 operand_ok（interp 恒等式内联，2 幂/溢出 → trap）。
fn place_ok(pe: &ir::PlaceExpr) -> bool {
    pe.steps.iter().all(|s| match s {
        ir::PlaceStep::Deref | ir::PlaceStep::Offset(_) | ir::PlaceStep::IndexScaled { .. } => true,
        ir::PlaceStep::VTableAlignOffset { meta, .. } => operand_ok(meta),
    })
}

fn mem_place_ok(p: &ScalarPlace) -> bool {
    match p {
        ScalarPlace::Slot(_) => true,
        ScalarPlace::Mem { expr, .. } => place_ok(expr),
    }
}

fn rvalue_ok(rv: &ir::Rvalue) -> bool {
    use ir::Rvalue as R;
    match rv {
        R::Use(a) | R::NotBits(a) | R::NotBool(a) | R::Neg(a) => operand_ok(a),
        R::Cast { a, .. } => operand_ok(a),
        // M5.4b-1：Div/Rem 已接（零检 + signed MIN/-1 分支特判）
        R::IntBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::IntCmp { a, b, .. } => operand_ok(a) && operand_ok(b),
        // M5.4a 内存/地址族
        R::Ref(pe) => place_ok(pe),
        R::PtrOffset { ptr, count, .. } => operand_ok(ptr) && operand_ok(count),
        R::PtrDiff { a, b, stride } => *stride != 0 && operand_ok(a) && operand_ok(b),
        R::UMax { a, b } => operand_ok(a) && operand_ok(b),
        // M5.4b-1 标量补面
        R::IntSat { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::BitUn { a, .. } => operand_ok(a),
        R::MemCmp { a, b, n } => operand_ok(a) && operand_ok(b) && operand_ok(n),
        R::AtomicLoad { addr, .. } => operand_ok(addr),
        // M5.4b-2 浮点（f16 也收，走助手）
        R::FloatBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::FloatCmp { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::FloatNeg { a, .. } => operand_ok(a),
        R::FloatCast { a, .. } => operand_ok(a),
        R::FloatToInt { a, .. } => operand_ok(a),
        R::IntToFloat { a, .. } => operand_ok(a),
        R::MathUn { a, .. } => operand_ok(a),
        R::MathBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::MathFma { a, b, c, .. } => operand_ok(a) && operand_ok(b) && operand_ok(c),
        // M5.4b-3 f128/128 位比较（place 通道）
        R::F128Cmp { a, b, .. } => place_ok(a) && place_ok(b),
        R::Cmp128 { a, b, .. } => place_ok(a) && place_ok(b),
        _ => false,
    }
}

/// callee 的 ABI 必须全标量（蹦床/fast 签名的成立前提）。返回 (实参槽数, 有无返回值)。
fn callee_abi(body: &ir::FuncBody) -> Option<(usize, bool)> {
    if body.caller_loc_off.is_some() {
        return None; // track_caller 幻影尾参 v2
    }
    let mut n = 0usize;
    for p in &body.params {
        match p {
            ParamAbi::Scalar(_) => n += 1,
            ParamAbi::Zst => {}
            _ => return None,
        }
    }
    match body.ret {
        RetAbi::Scalar(_) => Some((n, true)),
        RetAbi::Zst => Some((n, false)),
        _ => None,
    }
}

fn admit(shared: &Shared, body: &ir::FuncBody) -> bool {
    if callee_abi(body).is_none() {
        return false;
    }
    for blk in &body.blocks {
        for st in &blk.stmts {
            let ok = match st {
                Stmt::Assign { dst, rv } => mem_place_ok(dst) && rvalue_ok(rv),
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    operand_ok(a)
                        && operand_ok(b)
                        && scalar_slot(dst_val).is_some()
                        && scalar_slot(dst_flag).is_some()
                }
                // M5.4a：memmove/Repeat 两族（逐元素 CLIF 循环）
                Stmt::Copy { dst, src, .. } => place_ok(dst) && place_ok(src),
                Stmt::RepeatScalar { dst, val, .. } => place_ok(dst) && operand_ok(val),
                Stmt::RepeatBytes { first, .. } => place_ok(first),
                // M5.4b-1：MemCopy/MemSet/Volatile/原子/栅栏
                Stmt::MemCopy {
                    dst, src, count, ..
                } => operand_ok(dst) && operand_ok(src) && operand_ok(count),
                Stmt::MemSet {
                    dst, val, count, ..
                } => operand_ok(dst) && operand_ok(val) && operand_ok(count),
                Stmt::VolatileLoad { addr, dst, .. } => operand_ok(addr) && place_ok(dst),
                Stmt::VolatileStore { addr, src, .. } => operand_ok(addr) && place_ok(src),
                Stmt::AtomicStore { addr, val, .. } => operand_ok(addr) && operand_ok(val),
                Stmt::AtomicRmw { addr, val, dst, .. } => {
                    operand_ok(addr) && operand_ok(val) && mem_place_ok(dst)
                }
                Stmt::AtomicCxchg {
                    addr,
                    expected,
                    new,
                    dst_val,
                    dst_ok,
                    ..
                } => {
                    operand_ok(addr)
                        && operand_ok(expected)
                        && operand_ok(new)
                        && mem_place_ok(dst_val)
                        && mem_place_ok(dst_ok)
                }
                Stmt::Fence { .. } => true,
                // M5.4b-3：128 位整族 + f128 宽通道（全有去处——CLIF I128 或助手）
                Stmt::Bin128 { a, b, dst, .. } => {
                    place_ok(a)
                        && match b {
                            ir::Bin128Rhs::Wide(w) => place_ok(w),
                            ir::Bin128Rhs::Scalar(o) => operand_ok(o),
                        }
                        && place_ok(dst)
                }
                Stmt::Bit128 { src, dst, .. } => place_ok(src) && place_ok(dst),
                Stmt::Bit128Count { src, dst, .. } => place_ok(src) && mem_place_ok(dst),
                Stmt::NicheDiscr128 { tag, dst, .. } => place_ok(tag) && mem_place_ok(dst),
                Stmt::Wide128ToFloat { src, dst, .. } => place_ok(src) && mem_place_ok(dst),
                Stmt::FloatToWide128 { src, dst, .. } => operand_ok(src) && place_ok(dst),
                Stmt::F128Bin { a, b, dst, .. } => place_ok(a) && place_ok(b) && place_ok(dst),
                Stmt::F128MathBin { a, b, dst, .. } => {
                    place_ok(a)
                        && match b {
                            ir::F128Rhs::Wide(w) => place_ok(w),
                            ir::F128Rhs::Scalar(o) => operand_ok(o),
                        }
                        && place_ok(dst)
                }
                Stmt::F128Un { a, dst, .. } => place_ok(a) && place_ok(dst),
                Stmt::F128Fma { a, b, c, dst } => {
                    place_ok(a) && place_ok(b) && place_ok(c) && place_ok(dst)
                }
                Stmt::F128FromScalar { src, dst, .. } => operand_ok(src) && place_ok(dst),
                Stmt::F128ToScalar { src, dst, .. } => place_ok(src) && mem_place_ok(dst),
                Stmt::F128FromWideInt { src, dst, .. } => place_ok(src) && place_ok(dst),
                Stmt::F128ToWideInt { src, dst, .. } => place_ok(src) && place_ok(dst),
                _ => false,
            };
            if !ok {
                return false;
            }
        }
        let ok = match &blk.term {
            Terminator::Goto(_) | Terminator::Return | Terminator::Unreachable => true,
            Terminator::SwitchInt { discr, targets, .. } => match discr {
                SwitchDiscr::Scalar(op) => {
                    // 判别值必须落在 u64（宽度 ≤64 时天然成立；防御断言）
                    operand_ok(op) && targets.iter().all(|(v, _)| *v <= u64::MAX as u128)
                }
                // M5.4b-3：128 位判别通道已接
                SwitchDiscr::Wide(pe) => place_ok(pe),
            },
            Terminator::Call {
                callee,
                args,
                ret,
                unwind,
                ..
            } => {
                // unwind-transparent：只收 Continue（穿透）；cleanup/terminate 边 v2（LSDA 期）。
                // callee ABI 不设限：全标量 ABI 走 PLT 快路，否则调用点直接 c2i 回解释
                // （interp 本就吃展平 av，任意 ABI 语义一致——panic 类冷路径的归宿）。
                matches!(unwind, UnwindAction::Continue)
                    && args.iter().all(operand_ok)
                    && matches!(ret, RetDest::Ignore | RetDest::Scalar(ScalarPlace::Slot(_)))
                    && shared.module.funcs.get(*callee as usize).is_some()
            }
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

// ===== 编译器 =====

struct Compiler {
    shared: &'static Shared,
    module: JITModule,
    fbc: FunctionBuilderContext,
    c2i: ClifFuncId,
    unreachable: ClifFuncId,
    /// M5.4a：Copy/帧清零的宿主 memmove/memset 通道
    memmove: ClifFuncId,
    memset: ClifFuncId,
    /// M5.4b-1：MemCmp（compare_bytes intrinsic）
    memcmp: ClifFuncId,
    /// M5.4b-1：除零诊断退出（interp engine_abort 同文案同码）
    div_zero: ClifFuncId,
    /// M5.4b-1：volatile 读/写（interp opaque 字节载体同一实现）
    volatile_load: ClifFuncId,
    volatile_store: ClifFuncId,
    /// 本批 (clif id, unwind info)——finalize 后统一注册 eh_frame
    pending_unwind: Vec<(ClifFuncId, UnwindInfo)>,
}

impl Compiler {
    fn new(shared: &'static Shared) -> Self {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("mirvm_c2i", mirvm_c2i as *const u8);
        jb.symbol("mirvm_jit_unreachable", mirvm_jit_unreachable as *const u8);
        jb.symbol("mirvm_jit_div_zero", mirvm_jit_div_zero as *const u8);
        jb.symbol("mirvm_volatile_load", mirvm_volatile_load as *const u8);
        jb.symbol("mirvm_volatile_store", mirvm_volatile_store as *const u8);
        // M5.4b-3 助手注册表
        jb.symbol("mirvm_bin128_ovf", mirvm_bin128_ovf as *const u8);
        jb.symbol("mirvm_f128_bin", mirvm_f128_bin as *const u8);
        jb.symbol("mirvm_f128_cmp", mirvm_f128_cmp as *const u8);
        jb.symbol("mirvm_f128_un", mirvm_f128_un as *const u8);
        jb.symbol("mirvm_f128_math", mirvm_f128_math as *const u8);
        jb.symbol(
            "mirvm_f128_from_scalar",
            mirvm_f128_from_scalar as *const u8,
        );
        jb.symbol("mirvm_f128_to_scalar", mirvm_f128_to_scalar as *const u8);
        jb.symbol("mirvm_f128_from_wide", mirvm_f128_from_wide as *const u8);
        jb.symbol("mirvm_f128_to_wide", mirvm_f128_to_wide as *const u8);
        jb.symbol("mirvm_float_to_wide", mirvm_float_to_wide as *const u8);
        jb.symbol("mirvm_wide_to_f16", mirvm_wide_to_f16 as *const u8);
        jb.symbol("mirvm_f16_bin", mirvm_f16_bin as *const u8);
        jb.symbol("mirvm_f16_cmp", mirvm_f16_cmp as *const u8);
        jb.symbol("mirvm_f16_neg", mirvm_f16_neg as *const u8);
        jb.symbol("mirvm_f16_cast", mirvm_f16_cast as *const u8);
        jb.symbol("mirvm_f16_to_int", mirvm_f16_to_int as *const u8);
        jb.symbol("mirvm_f16_from_int", mirvm_f16_from_int as *const u8);
        jb.symbol("memmove", libc::memmove as *const u8);
        jb.symbol("memset", libc::memset as *const u8);
        jb.symbol("memcmp", libc::memcmp as *const u8);
        for (n, p) in libm_syms() {
            jb.symbol(n, p as *const u8);
        }
        let mut module = JITModule::new(jb);

        let mut sig_c2i = module.make_signature();
        for _ in 0..4 {
            sig_c2i.params.push(AbiParam::new(types::I64));
        }
        let c2i = module
            .declare_function("mirvm_c2i", Linkage::Import, &sig_c2i)
            .unwrap();
        let mut sig_unr = module.make_signature();
        sig_unr.params.push(AbiParam::new(types::I64));
        let unreachable = module
            .declare_function("mirvm_jit_unreachable", Linkage::Import, &sig_unr)
            .unwrap();
        // memmove(d, s, n) -> d；memset(d, c, n) -> d（M5.4a Copy/帧清零通道）
        let mut sig_mm = module.make_signature();
        for _ in 0..3 {
            sig_mm.params.push(AbiParam::new(types::I64));
        }
        sig_mm.returns.push(AbiParam::new(types::I64));
        let memmove = module
            .declare_function("memmove", Linkage::Import, &sig_mm)
            .unwrap();
        let memset = module
            .declare_function("memset", Linkage::Import, &sig_mm)
            .unwrap();
        // memcmp(s1, s2, n) -> c_int（i32！I64 返回声明会把 sextend.i64 喂给
        // verifier——diff_cargo ecosystem 实测抓获）
        let mut sig_memcmp = module.make_signature();
        for _ in 0..3 {
            sig_memcmp.params.push(AbiParam::new(types::I64));
        }
        sig_memcmp.returns.push(AbiParam::new(types::I32));
        let memcmp = module
            .declare_function("memcmp", Linkage::Import, &sig_memcmp)
            .unwrap();
        let div_zero = module
            .declare_function("mirvm_jit_div_zero", Linkage::Import, &sig_unr)
            .unwrap();
        let volatile_load = module
            .declare_function("mirvm_volatile_load", Linkage::Import, &sig_mm)
            .unwrap();
        let volatile_store = module
            .declare_function("mirvm_volatile_store", Linkage::Import, &sig_mm)
            .unwrap();

        Compiler {
            shared,
            module,
            fbc: FunctionBuilderContext::new(),
            c2i,
            unreachable,
            memmove,
            memset,
            memcmp,
            div_zero,
            volatile_load,
            volatile_store,
            pending_unwind: Vec::new(),
        }
    }

    fn fast_sig(&mut self, nparams: usize, has_ret: bool) -> Signature {
        let mut sig = self.module.make_signature();
        for _ in 0..nparams {
            sig.params.push(AbiParam::new(types::I64));
        }
        if has_ret {
            sig.returns.push(AbiParam::new(types::I64));
        }
        sig
    }

    /// 编译一个函数（过阈值请求）。拒绝/失败 = 静默维持解释。
    fn compile(&mut self, func: u32) {
        let jit = &self.shared.jit;
        if jit.slots[func as usize].load(Ordering::Acquire) != 0 {
            return; // 已编译
        }
        let Some(body) = self.shared.module.funcs.get(func as usize) else {
            return;
        };
        if !admit(self.shared, body) {
            return;
        }
        let (nparams, has_ret) = callee_abi(body).expect("admit 已验");

        // PLT 快路 callee 的槽预热：未编译者发 c2i 蹦床（fast 形状，调用点形状恒定）。
        // 非全标量 ABI / 实参数不合的 callee 不在此列——其调用点直接 c2i（cold path）。
        let mut callees: Vec<(u32, usize, bool)> = Vec::new();
        for blk in &body.blocks {
            if let Terminator::Call { callee, args, .. } = &blk.term
                && *callee != func
                && !callees.iter().any(|(c, _, _)| c == callee)
                && let Some((cn, cret)) = callee_abi(&self.shared.module.funcs[*callee as usize])
                && cn == args.len()
            {
                callees.push((*callee, cn, cret));
            }
        }
        for (c, cn, cret) in callees {
            if jit.slots_fast[c as usize].load(Ordering::Acquire) == 0 {
                if let Some(tramp) = self.define_c2i_trampoline(c, cn, cret) {
                    jit.slots_fast[c as usize].store(tramp as u64, Ordering::Release);
                }
            }
        }

        // 静默失败纪律（m5.3-design D4 / 防静默错值：编译失败 = 维持解释，绝不向
        // stderr 吐 panic——差分 oracle 的 stderr 逐字节比对会被线程 id 污染，实测抓获）
        let Some(fast_id) = self.define_fast(func, body, nparams, has_ret) else {
            return;
        };
        let Some(packed_id) = self.define_packed(func, body, nparams, has_ret, fast_id) else {
            return;
        };
        if self.module.finalize_definitions().is_err() {
            return;
        }
        self.register_pending_eh_frames();

        let fast = self.module.get_finalized_function(fast_id) as u64;
        let packed = self.module.get_finalized_function(packed_id) as u64;
        // 发布序：先 fast（自递归/他人调我）后 packed（interp 才可能进入编译码）
        jit.slots_fast[func as usize].store(fast, Ordering::Release);
        jit.slots[func as usize].store(packed, Ordering::Release);
    }

    /// c2i 蹦床：fast 签名，打包实参进栈上数组，调 mirvm_c2i 回解释器。
    /// 任何编译失败 = None（调用方跳过本槽预热，静默维持解释）。
    fn define_c2i_trampoline(&mut self, target: u32, nparams: usize, has_ret: bool) -> Option<*const u8> {
        let sig = self.fast_sig(nparams, has_ret);
        let id = self
            .module
            .declare_function(&format!("t{target}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let args_ss = b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                (nparams.max(1) * 8) as u32,
                3,
            ));
            for (i, p) in params.iter().enumerate() {
                b.ins().stack_store(*p, args_ss, (i * 8) as i32);
            }
            let ret_ss =
                b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
            let fref = self.module.declare_func_in_func(self.c2i, b.func);
            let fv = b.ins().iconst(types::I64, target as i64);
            let ap = b.ins().stack_addr(types::I64, args_ss, 0);
            let nv = b.ins().iconst(types::I64, nparams as i64);
            let rp = b.ins().stack_addr(types::I64, ret_ss, 0);
            b.ins().call(fref, &[fv, ap, nv, rp]);
            if has_ret {
                let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                b.ins().return_(&[lo]);
            } else {
                b.ins().return_(&[]);
            }
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function 失败: {e}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui));
        }
        self.module.clear_context(&mut cctx);
        if self.module.finalize_definitions().is_err() {
            return None;
        }
        self.register_pending_eh_frames();
        Some(self.module.get_finalized_function(id))
    }

    /// fast 本体：字节码块 → CLIF；槽 → SSA 变量（I64 零扩到宽不变量）。
    /// 任何编译失败 = None（静默维持解释——绝不 panic 污染 stderr 差分）。
    fn define_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        nparams: usize,
        has_ret: bool,
    ) -> Option<ClifFuncId> {
        let sig = self.fast_sig(nparams, has_ret);
        let id = self
            .module
            .declare_function(&format!("f{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let frame_offs = analyze_frame(body);
            let frame_ss = if frame_offs.is_empty() {
                None
            } else {
                Some(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    body.frame_size,
                    body.frame_align.trailing_zeros() as u8,
                )))
            };
            let mut tr = Translator {
                shared: self.shared,
                module: &mut self.module,
                b: &mut b,
                vars: std::collections::HashMap::new(),
                frame_offs,
                frame_ss,
                unreachable: self.unreachable,
                c2i: self.c2i,
                memmove: self.memmove,
                memset: self.memset,
                memcmp: self.memcmp,
                div_zero: self.div_zero,
                volatile_load: self.volatile_load,
                volatile_store: self.volatile_store,
            };
            tr.build(func, body, has_ret);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function 失败: {e}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui));
        }
        self.module.clear_context(&mut cctx);
        Some(id)
    }

    /// packed 入口：`(args: *const u64, ret: *mut u64)`——interp i2c 一跳。
    fn define_packed(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        nparams: usize,
        has_ret: bool,
        fast: ClifFuncId,
    ) -> Option<ClifFuncId> {
        let _ = body;
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function(&format!("p{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let ps = b.block_params(entry).to_vec();
            let (argp, retp) = (ps[0], ps[1]);
            let mut args: Vec<Value> = Vec::with_capacity(nparams);
            for i in 0..nparams {
                args.push(
                    b.ins()
                        .load(types::I64, MemFlagsData::trusted(), argp, (i * 8) as i32),
                );
            }
            let fref = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(fref, &args);
            let lo = if has_ret {
                b.inst_results(call)[0]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            let zero = b.ins().iconst(types::I64, 0);
            b.ins().store(MemFlagsData::trusted(), lo, retp, 0);
            b.ins().store(MemFlagsData::trusted(), zero, retp, 8);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if std::env::var_os("MIRVM_JIT_DEBUG").is_some() {
                eprintln!("mirvm-jit-debug: define_function 失败: {e}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui));
        }
        self.module.clear_context(&mut cctx);
        Some(id)
    }

    /// spike5 管线：FrameTable → eh_frame 字节 → 逐 FDE __register_frame（libgcc
    /// 语义；CIE 判别 = 长度域后 4 字节为 0）。字节 leak（FDE 注册要求终身有效）。
    fn register_pending_eh_frames(&mut self) {
        if self.pending_unwind.is_empty() {
            return;
        }
        use gimli::RunTimeEndian;
        use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
        let isa = self.module.isa();
        let mut table = FrameTable::default();
        let cie = isa.create_systemv_cie().expect("systemv cie");
        let cie_id = table.add_cie(cie);
        for (id, ui) in self.pending_unwind.drain(..) {
            if let UnwindInfo::SystemV(info) = ui {
                let addr = self.module.get_finalized_function(id) as u64;
                table.add_fde(cie_id, info.to_fde(Address::Constant(addr)));
            }
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe {
            let start = buf.as_ptr();
            let end = start.add(buf.len());
            let mut cur = start;
            while cur < end {
                let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
                if len == 0 {
                    break;
                }
                let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
                if cie_ptr != 0 {
                    __register_frame(cur);
                }
                cur = cur.add(len + 4);
            }
        }
    }
}

// ===== 函数体翻译 =====

/// 帧模型 v2 的槽存储分派（M5.4a，m5.4-design §3.1）：取址分析保守全集——
/// 任何被 `PlaceExpr::Local`/Mem/AddrOf/Ref/Copy/Repeat 等通道触及的 frame offset
/// 一律落栈帧内存；其余槽维持 SSA 提升（v1 的 I64 零扩到宽不变量原样）。
struct Translator<'a, 'b> {
    shared: &'static Shared,
    module: &'a mut JITModule,
    b: &'a mut FunctionBuilder<'b>,
    vars: std::collections::HashMap<u32, Variable>,
    /// 落帧 offset 集（analyze_frame 产出，区间模型）
    frame_offs: FrameMap,
    /// guest 帧栈槽（frame_offs 非空时创建；frame_size 字节、frame_align 对齐）
    frame_ss: Option<StackSlot>,
    unreachable: ClifFuncId,
    c2i: ClifFuncId,
    memmove: ClifFuncId,
    memset: ClifFuncId,
    memcmp: ClifFuncId,
    div_zero: ClifFuncId,
    volatile_load: ClifFuncId,
    volatile_store: ClifFuncId,
}

impl Translator<'_, '_> {
    fn var(&mut self, off: u32) -> Variable {
        if let Some(&v) = self.vars.get(&off) {
            return v;
        }
        let v = self.b.declare_var(types::I64);
        self.vars.insert(off, v);
        v
    }

    fn mask_val(&mut self, v: Value, w: Width) -> Value {
        if w == Width::W64 {
            return v;
        }
        self.b.ins().band_imm(v, w.mask() as i64)
    }

    /// 槽不变量下的符号扩展视图（i64）：w=64 原样；否则 ireduce→sextend。
    fn sext_val(&mut self, v: Value, w: Width) -> Value {
        let t = match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => return v,
        };
        let narrow = self.b.ins().ireduce(t, v);
        self.b.ins().sextend(types::I64, narrow)
    }

    fn narrow_ty(w: Width) -> cranelift_codegen::ir::Type {
        match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => types::I64,
        }
    }

    /// 读槽（分派：落帧 → 栈槽 load + 零扩；SSA → use_var）
    fn read_slot(&mut self, s: Slot) -> Value {
        if self.frame_offs.contains(s.off) {
            let ss = self.frame_ss.expect("落帧 offset 必有帧槽");
            let v = self
                .b
                .ins()
                .stack_load(Self::narrow_ty(s.width), ss, s.off as i32);
            if s.width == Width::W64 {
                v
            } else {
                self.b.ins().uextend(types::I64, v)
            }
        } else {
            let v = self.var(s.off);
            self.b.use_var(v)
        }
    }

    /// 写槽（分派：落帧 → 掩宽 + 窄化 + 栈槽 store；SSA → 掩宽 def_var）
    fn write_slot(&mut self, s: Slot, v: Value) {
        let masked = self.mask_val(v, s.width);
        if self.frame_offs.contains(s.off) {
            let ss = self.frame_ss.expect("落帧 offset 必有帧槽");
            let n = if s.width == Width::W64 {
                masked
            } else {
                self.b.ins().ireduce(Self::narrow_ty(s.width), masked)
            };
            self.b.ins().stack_store(n, ss, s.off as i32);
        } else {
            let var = self.var(s.off);
            self.b.def_var(var, masked);
        }
    }

    /// 取帧内 offset 的真地址（取址分析已保证其落帧）
    fn addr_of_local(&mut self, off: u32) -> Value {
        let ss = self
            .frame_ss
            .expect("取址 offset 必落帧（analyze_frame 全集）");
        self.b.ins().stack_addr(types::I64, ss, off as i32)
    }

    /// PlaceExpr 求值（interp::eval_place_addr 逐位镜像；Deref/Offset 为 wrapping 语义）
    fn place_addr(&mut self, pe: &ir::PlaceExpr) -> Value {
        let mut addr = match pe.base {
            ir::PlaceBase::Local(off) => self.addr_of_local(off),
            ir::PlaceBase::Static(a) => self.b.ins().iconst(types::I64, a as i64),
        };
        for step in pe.steps.iter() {
            match step {
                ir::PlaceStep::Deref => {
                    addr = self
                        .b
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), addr, 0)
                }
                ir::PlaceStep::Offset(d) => addr = self.b.ins().iadd_imm(addr, i64::from(*d)),
                ir::PlaceStep::IndexScaled { idx, stride } => {
                    let i = self.read_slot(*idx);
                    let scaled = self.b.ins().imul_imm(i, *stride as i64);
                    addr = self.b.ins().iadd(addr, scaled);
                }
                ir::PlaceStep::VTableAlignOffset {
                    meta,
                    unaligned,
                    packed,
                } => {
                    // interp 恒等式：align = *(vtable+16)；packed 取 min；非 2 幂/溢出即
                    // abort（JIT 侧 = mirvm_jit_trap 诊断退出，与 interp engine_abort 同口径）
                    let (vtable, _) = self.operand(meta);
                    let mut align =
                        self.b
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), vtable, 16);
                    if let Some(p) = packed {
                        let p = self.b.ins().iconst(types::I64, *p as i64);
                        align = self.b.ins().umin(align, p);
                    }
                    // 2 幂检查：align != 0 && (align & (align-1)) == 0，否则 trap
                    let is_zero = self.b.ins().icmp_imm(IntCC::Equal, align, 0);
                    let am1 = self.b.ins().iadd_imm(align, -1);
                    let pow2 = self.b.ins().band(align, am1);
                    let not_pow2 = self.b.ins().icmp_imm(IntCC::NotEqual, pow2, 0);
                    let bad = self.b.ins().bor(is_zero, not_pow2);
                    self.trap_if(bad, "dyn vtable alignment 非 2 的幂");
                    // (unaligned + align-1) & !(align-1)；checked_add 溢出 → trap
                    let uv = self.b.ins().iconst(types::I64, *unaligned as i64);
                    let sum = self
                        .b
                        .ins()
                        .uadd_overflow_trap(uv, am1, TrapCode::user(2).unwrap());
                    let off = self.b.ins().band_not(sum, am1);
                    addr = self.b.ins().iadd(addr, off);
                }
            }
        }
        addr
    }

    /// 条件即诊断退出（与 interp engine_abort 同口径的 JIT 形态）。
    fn trap_if(&mut self, cond: Value, _msg: &'static str) {
        let t_blk = self.b.create_block();
        let f_blk = self.b.create_block();
        self.b.ins().brif(cond, t_blk, &[], f_blk, &[]);
        self.b.switch_to_block(t_blk);
        let fref = self
            .module
            .declare_func_in_func(self.unreachable, self.b.func);
        let fv = self.b.ins().iconst(types::I64, 0);
        self.b.ins().call(fref, &[fv]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(f_blk);
    }

    /// M5.4b-1 除零分支：cond 真 → 调 mirvm_jit_div_zero（interp 同文案同码退出）。
    /// wide=false 64 位（kind 0/1），wide=true 128 位（kind 2/3）。
    fn div_zero_if(&mut self, cond: Value, is_rem: bool, wide: bool) {
        let t_blk = self.b.create_block();
        let f_blk = self.b.create_block();
        self.b.ins().brif(cond, t_blk, &[], f_blk, &[]);
        self.b.switch_to_block(t_blk);
        let fref = self.module.declare_func_in_func(self.div_zero, self.b.func);
        let kind = match (wide, is_rem) {
            (false, false) => 0,
            (false, true) => 1,
            (true, false) => 2,
            (true, true) => 3,
        };
        let kv = self.b.ins().iconst(types::I64, kind);
        self.b.ins().call(fref, &[kv]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(f_blk);
    }

    /// 标量落点写（Slot → write_slot；Mem → 掩宽窄化 store）。
    fn write_scalar_place(&mut self, sp: &ScalarPlace, v: Value) {
        match sp {
            ScalarPlace::Slot(s) => {
                let s = *s;
                self.write_slot(s, v);
            }
            ScalarPlace::Mem { expr, width } => {
                let a = self.place_addr(expr);
                let masked = self.mask_val(v, *width);
                let n = if *width == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(*width), masked)
                };
                self.b.ins().store(MemFlagsData::trusted(), n, a, 0);
            }
        }
    }

    // ===== M5.4b-2 浮点通道（值 = I64 槽里的位型，与 interp 同一表示）=====

    fn float_ty(w: ir::FloatW) -> cranelift_codegen::ir::Type {
        match w {
            ir::FloatW::F32 => types::F32,
            ir::FloatW::F64 => types::F64,
            ir::FloatW::F16 => unreachable!("f16 走助手（M5.4b-3）"),
        }
    }

    /// 槽位型 → 浮点寄存器值（bitcast；F32 先 ireduce）
    fn as_float(&mut self, v: Value, w: ir::FloatW) -> Value {
        match w {
            ir::FloatW::F32 => {
                let n = self.b.ins().ireduce(types::I32, v);
                self.b.ins().bitcast(types::F32, MemFlagsData::trusted(), n)
            }
            ir::FloatW::F64 => self.b.ins().bitcast(types::F64, MemFlagsData::trusted(), v),
            ir::FloatW::F16 => unreachable!("f16 走助手（M5.4b-3）"),
        }
    }

    /// 浮点寄存器值 → 槽位型（bitcast 回来；F32 再 uextend）
    fn as_bits(&mut self, v: Value, w: ir::FloatW) -> Value {
        match w {
            ir::FloatW::F32 => {
                let n = self.b.ins().bitcast(types::I32, MemFlagsData::trusted(), v);
                self.b.ins().uextend(types::I64, n)
            }
            ir::FloatW::F64 => self.b.ins().bitcast(types::I64, MemFlagsData::trusted(), v),
            ir::FloatW::F16 => unreachable!("f16 走助手（M5.4b-3）"),
        }
    }

    /// 一元/二元 libm 调用（按宽选 f32/f64 后缀符号；interp 的 libm 通道同源）
    fn call_libm_un(&mut self, name: &str, a: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = format!("{}{}", name, if t == types::F32 { "f" } else { "" });
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(&fname, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("libm 符号缺失: {fname}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a]);
        self.b.inst_results(call)[0]
    }

    fn call_libm_bin(&mut self, name: &str, a: Value, b: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = format!("{}{}", name, if t == types::F32 { "f" } else { "" });
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.params.push(AbiParam::new(t));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(&fname, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("libm 符号缺失: {fname}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a, b]);
        self.b.inst_results(call)[0]
    }

    fn call_powi(&mut self, a: Value, n_i32: Value, w: ir::FloatW) -> Value {
        let t = Self::float_ty(w);
        let fname = if t == types::F32 {
            "__powisf2"
        } else {
            "__powidf2"
        };
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(t));
        sig.params.push(AbiParam::new(types::I32));
        sig.returns.push(AbiParam::new(t));
        let fid = self
            .module
            .declare_function(fname, Linkage::Import, &sig)
            .expect("compiler-builtins powi 符号缺失");
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, &[a, n_i32]);
        self.b.inst_results(call)[0]
    }

    // ===== M5.4b-3 宽值通道（16 字节 place ↔ (lo,hi) 对/I128）=====

    fn read_wide(&mut self, pe: &ir::PlaceExpr) -> (Value, Value) {
        let a = self.place_addr(pe);
        let lo = self.b.ins().load(types::I64, MemFlagsData::trusted(), a, 0);
        let hi = self.b.ins().load(types::I64, MemFlagsData::trusted(), a, 8);
        (lo, hi)
    }

    fn write_wide(&mut self, pe: &ir::PlaceExpr, lo: Value, hi: Value) {
        let a = self.place_addr(pe);
        self.b.ins().store(MemFlagsData::trusted(), lo, a, 0);
        self.b.ins().store(MemFlagsData::trusted(), hi, a, 8);
    }

    fn i128_of(&mut self, lo: Value, hi: Value) -> Value {
        self.b.ins().iconcat(lo, hi)
    }

    fn iconst128(&mut self, v: u128) -> Value {
        let lo = self.b.ins().iconst(types::I64, v as u64 as i64);
        let hi = self.b.ins().iconst(types::I64, (v >> 64) as u64 as i64);
        self.b.ins().iconcat(lo, hi)
    }

    /// 16 字节 out 型助手调用：栈槽接 (lo,hi) 结果并写回 place。
    fn call_out128(&mut self, name: &str, args: &[Value], dst: &ir::PlaceExpr) {
        let ss =
            self.b
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
        let outp = self.b.ins().stack_addr(types::I64, ss, 0);
        let mut a: Vec<Value> = args.to_vec();
        a.push(outp);
        let mut sig = self.module.make_signature();
        for _ in &a {
            sig.params.push(AbiParam::new(types::I64));
        }
        let fid = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("助手符号缺失: {name}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        self.b.ins().call(fref, &a);
        let lo = self.b.ins().stack_load(types::I64, ss, 0);
        let hi = self.b.ins().stack_load(types::I64, ss, 8);
        self.write_wide(dst, lo, hi);
    }

    /// 单返回 u64 的助手调用。
    fn call_helper1(&mut self, name: &str, args: &[Value]) -> Value {
        let mut sig = self.module.make_signature();
        for _ in args {
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64));
        let fid = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("助手符号缺失: {name}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, args);
        self.b.inst_results(call)[0]
    }

    fn operand(&mut self, op: &Operand) -> (Value, Width) {
        match op {
            Operand::Slot(s) => (self.read_slot(*s), s.width),
            Operand::Imm { bits, width } => {
                let v = self
                    .b
                    .ins()
                    .iconst(types::I64, (*bits & width.mask()) as i64);
                (v, *width)
            }
            Operand::Mem { expr, width } => {
                let a = self.place_addr(expr);
                let v = self
                    .b
                    .ins()
                    .load(Self::narrow_ty(*width), MemFlagsData::trusted(), a, 0);
                let v = if *width == Width::W64 {
                    v
                } else {
                    self.b.ins().uextend(types::I64, v)
                };
                (v, *width)
            }
            Operand::AddrOf(expr) => {
                let a = self.place_addr(expr);
                (a, Width::W64)
            }
            Operand::SubImm { base, sub } => {
                let (v, w) = self.operand(base);
                (self.b.ins().iadd_imm(v, (*sub as i64).wrapping_neg()), w)
            }
        }
    }

    fn def_slot(&mut self, s: Slot, v: Value) {
        self.write_slot(s, v);
    }

    fn build(&mut self, func: u32, body: &ir::FuncBody, has_ret: bool) {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        let blocks: Vec<_> = (0..body.blocks.len())
            .map(|_| self.b.create_block())
            .collect();

        self.b.switch_to_block(entry);
        // 槽变量 def 0（确定化；interp 帧不清零，但有效 MIR 无读前未写路径）。
        // 帧内存同步清零（v1 确定化纪律的延伸：JIT-on/off 差分对任何 MIR 形状确定）。
        let mut offs: Vec<u32> = Vec::new();
        collect_ssa_offs(body, &self.frame_offs, &mut offs);
        let zero = self.b.ins().iconst(types::I64, 0);
        for off in offs {
            let var = self.var(off);
            self.b.def_var(var, zero);
        }
        if let Some(ss) = self.frame_ss {
            let fref = self.module.declare_func_in_func(self.memset, self.b.func);
            let dst = self.b.ins().stack_addr(types::I64, ss, 0);
            let c0 = self.b.ins().iconst(types::I64, 0);
            let n = self.b.ins().iconst(types::I64, i64::from(body.frame_size));
            self.b.ins().call(fref, &[dst, c0, n]);
        }
        // 参数落槽（packed/interp 侧按同一展平序）
        let params = self.b.block_params(entry).to_vec();
        let mut pi = 0usize;
        for p in &body.params {
            if let ParamAbi::Scalar(s) = p {
                self.def_slot(*s, params[pi]);
                pi += 1;
            }
        }
        self.b.ins().jump(blocks[0], &[]);

        for (bi, blk) in body.blocks.iter().enumerate() {
            self.b.switch_to_block(blocks[bi]);
            for st in &blk.stmts {
                self.stmt(st);
            }
            self.term(func, body, &blk.term, &blocks, has_ret);
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match st {
            Stmt::Assign { dst, rv } => {
                let v = self.rvalue(rv);
                match dst {
                    ScalarPlace::Slot(s) => {
                        let s = *s;
                        self.def_slot(s, v);
                    }
                    ScalarPlace::Mem { expr, width } => {
                        // 内存落点：掩宽 + 窄化 + store（与 interp mem_write 同口径）
                        let a = self.place_addr(expr);
                        let masked = self.mask_val(v, *width);
                        let n = if *width == Width::W64 {
                            masked
                        } else {
                            self.b.ins().ireduce(Self::narrow_ty(*width), masked)
                        };
                        self.b.ins().store(MemFlagsData::trusted(), n, a, 0);
                    }
                }
            }
            Stmt::AssignOverflow {
                op,
                signed,
                a,
                b,
                dst_val,
                dst_flag,
            } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (val, flag) = self.int_ovf(*op, *signed, av, bv, w);
                let (sv, sf) = match (dst_val, dst_flag) {
                    (ScalarPlace::Slot(v), ScalarPlace::Slot(f)) => (*v, *f),
                    _ => unreachable!("admit 已排除"),
                };
                self.def_slot(sv, val);
                self.def_slot(sf, flag);
            }
            Stmt::Copy { dst, src, size } => {
                // memmove 语义（interp std::ptr::copy 同源：guest 侧重叠是 UB，
                // 引擎不因此崩——防御性一致）
                let d = self.place_addr(dst);
                let s = self.place_addr(src);
                let n = self.b.ins().iconst(types::I64, i64::from(*size));
                let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                self.b.ins().call(fref, &[d, s, n]);
            }
            Stmt::RepeatScalar {
                dst,
                val,
                count,
                elem_size,
            } => {
                // interp 循环镜像：for i in 0..count { mem_write(d + i*elem, w, v) }
                let d = self.place_addr(dst);
                let (v, w) = self.operand(val);
                debug_assert_eq!(w.bytes(), *elem_size);
                let masked = self.mask_val(v, w);
                let n = if w == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(w), masked)
                };
                self.repeat_loop(d, n, *count, u64::from(*elem_size), false);
            }
            Stmt::RepeatBytes {
                first,
                count,
                elem_size,
            } => {
                // interp 镜像：for i in 1..count { 逐元素 memmove（元素 0 不变 ⇒
                // 与 interp 的 copy_nonoverlapping 逐元素结果一致） }
                let src = self.place_addr(first);
                self.repeat_loop(src, src, *count, *elem_size, true);
            }
            // ===== M5.4b-1 内存/原子补面 =====
            Stmt::MemCopy {
                dst,
                src,
                count,
                elem_size,
                overlap,
            } => {
                // intrinsic copy/copy_nonoverlapping：memmove 通道（overlap 为真时
                // 与 interp 的 ptr::copy 同义；非重叠场景 memcpy 结果相同）
                let _ = overlap;
                let (d, _) = self.operand(dst);
                let (s, _) = self.operand(src);
                let (c, _) = self.operand(count);
                let es = self.b.ins().iconst(types::I64, *elem_size as i64);
                let n = self.b.ins().imul(c, es);
                let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                self.b.ins().call(fref, &[d, s, n]);
            }
            Stmt::MemSet {
                dst,
                val,
                count,
                elem_size,
            } => {
                let (d, _) = self.operand(dst);
                let (v, _) = self.operand(val);
                let (c, _) = self.operand(count);
                let es = self.b.ins().iconst(types::I64, *elem_size as i64);
                let n = self.b.ins().imul(c, es);
                let fref = self.module.declare_func_in_func(self.memset, self.b.func);
                self.b.ins().call(fref, &[d, v, n]);
            }
            Stmt::VolatileLoad { addr, dst, size } => {
                let (p, _) = self.operand(addr);
                let d = self.place_addr(dst);
                let n = self.b.ins().iconst(types::I64, i64::from(*size));
                let fref = self
                    .module
                    .declare_func_in_func(self.volatile_load, self.b.func);
                self.b.ins().call(fref, &[p, d, n]);
            }
            Stmt::VolatileStore { addr, src, size } => {
                let (p, _) = self.operand(addr);
                let s = self.place_addr(src);
                let n = self.b.ins().iconst(types::I64, i64::from(*size));
                let fref = self
                    .module
                    .declare_func_in_func(self.volatile_store, self.b.func);
                self.b.ins().call(fref, &[p, s, n]);
            }
            Stmt::AtomicStore { addr, val, order } => {
                let (p, _) = self.operand(addr);
                let (v, w) = self.operand(val);
                let masked = self.mask_val(v, w);
                let n = if w == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(w), masked)
                };
                let _ = order; // CLIF 原子恒 SeqCst（合规强化，见 R::AtomicLoad 注）
                self.b.ins().atomic_store(MemFlagsData::trusted(), n, p);
            }
            Stmt::AtomicRmw {
                op,
                addr,
                val,
                dst,
                order,
            } => {
                let (p, _) = self.operand(addr);
                let (v, w) = self.operand(val);
                let masked = self.mask_val(v, w);
                let n = if w == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(w), masked)
                };
                let _ = order;
                let old = self.b.ins().atomic_rmw(
                    Self::narrow_ty(w),
                    MemFlagsData::trusted(),
                    clif_rmw_op(*op),
                    p,
                    n,
                );
                let old = if w == Width::W64 {
                    old
                } else {
                    self.b.ins().uextend(types::I64, old)
                };
                self.write_scalar_place(dst, old);
            }
            Stmt::AtomicCxchg {
                addr,
                expected,
                new,
                dst_val,
                dst_ok,
                weak,
                succ,
                fail,
            } => {
                // CLIF atomic_cas = strong CAS（weak 用 strong 合规：weak 允许假失败
                // 但不禁止成功）；succ/fail 序 → SeqCst（合规强化）
                let (p, _) = self.operand(addr);
                let (e, w) = self.operand(expected);
                let (n, _) = self.operand(new);
                let e_masked = self.mask_val(e, w);
                let n_masked = self.mask_val(n, w);
                let (e_n, n_n) = if w == Width::W64 {
                    (e_masked, n_masked)
                } else {
                    (
                        self.b.ins().ireduce(Self::narrow_ty(w), e_masked),
                        self.b.ins().ireduce(Self::narrow_ty(w), n_masked),
                    )
                };
                let _ = (weak, succ, fail);
                let old = self
                    .b
                    .ins()
                    .atomic_cas(MemFlagsData::trusted(), p, e_n, n_n);
                let old_ext = if w == Width::W64 {
                    old
                } else {
                    self.b.ins().uextend(types::I64, old)
                };
                // ok = (old == expected)（按宽掩后比较，与 interp 的 compare_exchange 同口径）
                let ok8 = self.b.ins().icmp(IntCC::Equal, old_ext, e_masked);
                let ok = self.b.ins().uextend(types::I64, ok8);
                self.write_scalar_place(dst_val, old_ext);
                self.write_scalar_place(dst_ok, ok);
            }
            Stmt::Fence {
                single_thread,
                order,
                ..
            } => {
                if !single_thread {
                    let _ = order;
                    self.b.ins().fence();
                }
                // single_thread = compiler fence（无指令，编译屏障在 JIT 码内天然成立）
            }
            // ===== M5.4b-3 128 位整族 =====
            Stmt::Bin128 {
                op,
                signed,
                a,
                b,
                dst,
                with_overflow,
            } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = match b {
                    ir::Bin128Rhs::Wide(w) => self.read_wide(w),
                    ir::Bin128Rhs::Scalar(o) => {
                        let (v, _) = self.operand(o);
                        let z = self.b.ins().iconst(types::I64, 0);
                        (v, z)
                    }
                };
                // with_overflow 的 Add/Sub/Mul：helper（Rust overflowing_* 精确语义）
                if *with_overflow && matches!(op, IntBinOp::Add | IntBinOp::Sub | IntBinOp::Mul) {
                    let op_idx = match op {
                        IntBinOp::Add => 0,
                        IntBinOp::Sub => 1,
                        _ => 2,
                    };
                    let s = self.b.ins().iconst(types::I64, *signed as i64);
                    let oi = self.b.ins().iconst(types::I64, op_idx);
                    let ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        4,
                    ));
                    let outp = self.b.ins().stack_addr(types::I64, ss, 0);
                    let flag =
                        self.call_helper1("mirvm_bin128_ovf", &[oi, s, alo, ahi, blo, bhi, outp]);
                    let lo = self.b.ins().stack_load(types::I64, ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ss, 8);
                    self.write_wide(dst, lo, hi);
                    // 旗标写 dst+16（interp 同布局：(u128, bool) 旗标在 +16）
                    let da = self.place_addr(dst);
                    let f8 = self.b.ins().ireduce(types::I8, flag);
                    self.b.ins().store(MemFlagsData::trusted(), f8, da, 16);
                    return;
                }
                let x = self.i128_of(alo, ahi);
                let y = self.i128_of(blo, bhi);
                let r = match op {
                    IntBinOp::Add => self.b.ins().iadd(x, y),
                    IntBinOp::Sub => self.b.ins().isub(x, y),
                    IntBinOp::Mul => self.b.ins().imul(x, y),
                    IntBinOp::BitAnd => self.b.ins().band(x, y),
                    IntBinOp::BitOr => self.b.ins().bor(x, y),
                    IntBinOp::BitXor => self.b.ins().bxor(x, y),
                    IntBinOp::Shl => self.b.ins().ishl(x, blo),
                    IntBinOp::Shr => {
                        if *signed {
                            self.b.ins().sshr(x, blo)
                        } else {
                            self.b.ins().ushr(x, blo)
                        }
                    }
                    IntBinOp::Div | IntBinOp::Rem => {
                        let y = self.i128_of(blo, bhi);
                        let is_rem = matches!(op, IntBinOp::Rem);
                        let zero = self.b.ins().icmp_imm(IntCC::Equal, y, 0);
                        self.div_zero_if(zero, is_rem, true);
                        if *signed {
                            let neg1 = self.b.ins().icmp_imm(IntCC::Equal, y, -1);
                            let triv_blk = self.b.create_block();
                            let norm_blk = self.b.create_block();
                            let join_blk = self.b.create_block();
                            self.b.ins().brif(neg1, triv_blk, &[], norm_blk, &[]);
                            self.b.switch_to_block(triv_blk);
                            let tv = if is_rem {
                                self.b.ins().iconst(types::I64, 0)
                            } else {
                                x
                            };
                            self.b.ins().jump(join_blk, &[tv.into()]);
                            self.b.switch_to_block(norm_blk);
                            let nv = if is_rem {
                                self.b.ins().srem(x, y)
                            } else {
                                self.b.ins().sdiv(x, y)
                            };
                            self.b.ins().jump(join_blk, &[nv.into()]);
                            self.b.switch_to_block(join_blk);
                            self.b.append_block_param(join_blk, types::I128)
                        } else if is_rem {
                            self.b.ins().urem(x, y)
                        } else {
                            self.b.ins().udiv(x, y)
                        }
                    }
                };
                let (lo, hi) = {
                    let pair = self.b.ins().isplit(r);
                    (pair.0, pair.1)
                };
                self.write_wide(dst, lo, hi);
            }
            Stmt::Bit128 { op, src, dst } => {
                use ir::BitUnOp as B;
                let (lo, hi) = self.read_wide(src);
                let (rlo, rhi) = match op {
                    B::Bswap => {
                        // u128::swap_bytes = 半字互换 + 各自 bswap
                        let a = self.b.ins().bswap(hi);
                        let b = self.b.ins().bswap(lo);
                        (a, b)
                    }
                    B::Bitreverse => {
                        // u128::reverse_bits = 半字互换 + 各自 bitrev
                        let a = self.b.ins().bitrev(hi);
                        let b = self.b.ins().bitrev(lo);
                        (a, b)
                    }
                    _ => unreachable!("Bit128 只 bswap/bitreverse"),
                };
                self.write_wide(dst, rlo, rhi);
            }
            Stmt::Bit128Count { op, src, dst } => {
                use ir::BitUnOp as B;
                let (lo, hi) = self.read_wide(src);
                let r = match op {
                    B::Popcount => {
                        let a = self.b.ins().popcnt(lo);
                        let b = self.b.ins().popcnt(hi);
                        self.b.ins().iadd(a, b)
                    }
                    B::Ctlz => {
                        let hz = self.b.ins().icmp_imm(IntCC::Equal, hi, 0);
                        let c_lo = self.b.ins().clz(lo);
                        let c64 = self.b.ins().iadd_imm(c_lo, 64);
                        let c_hi = self.b.ins().clz(hi);
                        self.b.ins().select(hz, c64, c_hi)
                    }
                    B::Cttz => {
                        let lz = self.b.ins().icmp_imm(IntCC::Equal, lo, 0);
                        let c_hi = self.b.ins().ctz(hi);
                        let c64 = self.b.ins().iadd_imm(c_hi, 64);
                        let c_lo = self.b.ins().ctz(lo);
                        self.b.ins().select(lz, c64, c_lo)
                    }
                    _ => unreachable!("Bit128Count 只 popcount/ctlz/cttz"),
                };
                self.write_scalar_place(dst, r);
            }
            Stmt::NicheDiscr128 {
                tag,
                niche_start,
                variants_start,
                variants_len,
                untagged,
                dst,
            } => {
                // interp 恒等式：rel = tag - niche_start（u128 wrapping）；rel < len →
                // variants_start + rel，否则 untagged
                let (tlo, thi) = self.read_wide(tag);
                let t = self.i128_of(tlo, thi);
                let ns = self.iconst128(*niche_start);
                let rel = self.b.ins().isub(t, ns);
                let len = self.iconst128(*variants_len as u128);
                let hit = self.b.ins().icmp(IntCC::UnsignedLessThan, rel, len);
                let (rlo, _) = {
                    let pair = self.b.ins().isplit(rel);
                    (pair.0, pair.1)
                };
                let vs = self.b.ins().iconst(types::I64, *variants_start as i64);
                let hit_v = self.b.ins().iadd(vs, rlo);
                let un_v = self.b.ins().iconst(types::I64, *untagged as i64);
                let r = self.b.ins().select(hit, hit_v, un_v);
                self.write_scalar_place(dst, r);
            }
            Stmt::Wide128ToFloat {
                src,
                signed,
                to,
                dst,
            } => {
                // i128/u128 → f16/f32/f64：f32/f64 走 compiler-builtins float*ti* 族
                // （Rust i128 as f32/f64 的同一批符号）；f16 走 mirvm_wide_to_f16
                let (lo, hi) = self.read_wide(src);
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                let f = match to {
                    ir::FloatW::F16 => {
                        let bits = self.call_helper1("mirvm_wide_to_f16", &[lo, hi, s]);
                        self.mask_val(bits, Width::W16)
                    }
                    ir::FloatW::F32 => {
                        let fname = if *signed {
                            "__floattisf"
                        } else {
                            "__floatuntisf"
                        };
                        let r = self.call_helper1(fname, &[lo, hi]);
                        let n = self.b.ins().ireduce(types::I32, r);
                        let f32v = self.b.ins().bitcast(types::F32, MemFlagsData::trusted(), n);
                        self.as_bits(f32v, ir::FloatW::F32)
                    }
                    ir::FloatW::F64 => {
                        let fname = if *signed {
                            "__floattidf"
                        } else {
                            "__floatuntidf"
                        };
                        let r = self.call_helper1(fname, &[lo, hi]);
                        self.as_bits(r, ir::FloatW::F64)
                    }
                };
                self.write_scalar_place(dst, f);
            }
            Stmt::FloatToWide128 {
                src,
                from,
                signed,
                dst,
            } => {
                let (v, _) = self.operand(src);
                let bits = match from {
                    ir::FloatW::F16 => self.mask_val(v, Width::W16),
                    ir::FloatW::F32 => self.mask_val(v, Width::W32),
                    ir::FloatW::F64 => v,
                };
                let kind = self.b.ins().iconst(
                    types::I64,
                    match from {
                        ir::FloatW::F16 => 0,
                        ir::FloatW::F32 => 1,
                        ir::FloatW::F64 => 2,
                    },
                );
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                self.call_out128("mirvm_float_to_wide", &[bits, kind, s], dst);
            }
            // ===== M5.4b-3 f128 宽通道（全走助手）=====
            Stmt::F128Bin { op, a, b, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let oi = self.b.ins().iconst(
                    types::I64,
                    match op {
                        ir::FloatOp::Add => 0,
                        ir::FloatOp::Sub => 1,
                        ir::FloatOp::Mul => 2,
                        ir::FloatOp::Rem => 3,
                        ir::FloatOp::Div => 4,
                    },
                );
                self.call_out128("mirvm_f128_bin", &[oi, alo, ahi, blo, bhi], dst);
            }
            Stmt::F128MathBin { op, a, b, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = match b {
                    ir::F128Rhs::Wide(w) => self.read_wide(w),
                    ir::F128Rhs::Scalar(o) => {
                        let (v, _) = self.operand(o);
                        let z = self.b.ins().iconst(types::I64, 0);
                        (v, z)
                    }
                };
                let oi = self.b.ins().iconst(
                    types::I64,
                    match op {
                        ir::MathBinOp::Pow => 0,
                        ir::MathBinOp::Powi => 1,
                        ir::MathBinOp::Copysign => 2,
                        ir::MathBinOp::Minnum => 3,
                        ir::MathBinOp::Maxnum => 4,
                    },
                );
                let z = self.b.ins().iconst(types::I64, 0);
                self.call_out128("mirvm_f128_math", &[oi, alo, ahi, blo, bhi, z, z], dst);
            }
            Stmt::F128Un { op, a, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let oi = match op {
                    ir::F128UnOp::Neg => 0,
                    ir::F128UnOp::Math(m) => {
                        (match m {
                            ir::MathUnOp::Sqrt => 1,
                            ir::MathUnOp::Sin => 2,
                            ir::MathUnOp::Cos => 3,
                            ir::MathUnOp::Exp => 4,
                            ir::MathUnOp::Exp2 => 5,
                            ir::MathUnOp::Ln => 6,
                            ir::MathUnOp::Log2 => 7,
                            ir::MathUnOp::Log10 => 8,
                            ir::MathUnOp::Fabs => 9,
                            ir::MathUnOp::Floor => 10,
                            ir::MathUnOp::Ceil => 11,
                            ir::MathUnOp::Trunc => 12,
                            ir::MathUnOp::Round => 13,
                            ir::MathUnOp::RoundTiesEven => 14,
                        }) as i64
                    }
                };
                let oiv = self.b.ins().iconst(types::I64, oi);
                self.call_out128("mirvm_f128_un", &[oiv, alo, ahi], dst);
            }
            Stmt::F128Fma { a, b, c, dst } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let (clo, chi) = self.read_wide(c);
                let oi = self.b.ins().iconst(types::I64, 5);
                self.call_out128("mirvm_f128_math", &[oi, alo, ahi, blo, bhi, clo, chi], dst);
            }
            Stmt::F128FromScalar { src, kind, dst } => {
                let (v, _) = self.operand(src);
                let k = self.b.ins().iconst(
                    types::I64,
                    match kind {
                        ir::F128Scalar::F(ir::FloatW::F16) => 0,
                        ir::F128Scalar::F(ir::FloatW::F32) => 1,
                        ir::F128Scalar::F(ir::FloatW::F64) => 2,
                        ir::F128Scalar::Int { signed: true } => 3,
                        ir::F128Scalar::Int { signed: false } => 4,
                    },
                );
                self.call_out128("mirvm_f128_from_scalar", &[k, v], dst);
            }
            Stmt::F128ToScalar { src, kind, w, dst } => {
                let (alo, ahi) = self.read_wide(src);
                let k = self.b.ins().iconst(
                    types::I64,
                    match kind {
                        ir::F128Scalar::F(ir::FloatW::F16) => 0,
                        ir::F128Scalar::F(ir::FloatW::F32) => 1,
                        ir::F128Scalar::F(ir::FloatW::F64) => 2,
                        ir::F128Scalar::Int { signed: true } => 3,
                        ir::F128Scalar::Int { signed: false } => 4,
                    },
                );
                let r = self.call_helper1("mirvm_f128_to_scalar", &[k, alo, ahi]);
                let r = self.mask_val(r, *w);
                self.write_scalar_place(dst, r);
            }
            Stmt::F128FromWideInt { src, signed, dst } => {
                let (lo, hi) = self.read_wide(src);
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                self.call_out128("mirvm_f128_from_wide", &[s, lo, hi], dst);
            }
            Stmt::F128ToWideInt { src, signed, dst } => {
                let (alo, ahi) = self.read_wide(src);
                let s = self.b.ins().iconst(types::I64, *signed as i64);
                self.call_out128("mirvm_f128_to_wide", &[s, alo, ahi], dst);
            }
            _ => unreachable!("admit 已排除"),
        }
    }

    /// Repeat 两族的共用循环骨架：memmove_elem=true 时逐元素 memmove（RepeatBytes，
    /// 起始 i=1）；否则按标量存（RepeatScalar，起始 i=0）。
    fn repeat_loop(
        &mut self,
        base: Value,
        val_or_src: Value,
        count: u64,
        elem_size: u64,
        memmove_elem: bool,
    ) {
        let count_v = self.b.ins().iconst(types::I64, count as i64);
        let elem_v = self.b.ins().iconst(types::I64, elem_size as i64);
        let head = self.b.create_block();
        let body_blk = self.b.create_block();
        let tail = self.b.create_block();
        let ivar = self.b.declare_var(types::I64);
        let start = self
            .b
            .ins()
            .iconst(types::I64, if memmove_elem { 1 } else { 0 });
        self.b.def_var(ivar, start);
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(head);
        let iv = self.b.use_var(ivar);
        let done = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, iv, count_v);
        self.b.ins().brif(done, tail, &[], body_blk, &[]);
        self.b.switch_to_block(body_blk);
        let off = self.b.ins().imul(iv, elem_v);
        let p = self.b.ins().iadd(base, off);
        if memmove_elem {
            let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
            self.b.ins().call(fref, &[p, val_or_src, elem_v]);
        } else {
            self.b
                .ins()
                .store(MemFlagsData::trusted(), val_or_src, p, 0);
        }
        let iv2 = self.b.use_var(ivar);
        let inc = self.b.ins().iadd_imm(iv2, 1);
        self.b.def_var(ivar, inc);
        self.b.ins().jump(head, &[]);
        self.b.switch_to_block(tail);
    }

    fn rvalue(&mut self, rv: &ir::Rvalue) -> Value {
        use ir::Rvalue as R;
        match rv {
            R::Use(a) => self.operand(a).0,
            R::IntBin { op, signed, a, b } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                self.int_bin(*op, *signed, av, bv, w)
            }
            R::IntCmp { cc, signed, a, b } => {
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (x, y) = if *signed {
                    (self.sext_val(av, w), self.sext_val(bv, w))
                } else {
                    (av, bv) // 槽不变量已 zext
                };
                let c = match (cc, signed) {
                    (IntCc::Eq, _) => IntCC::Equal,
                    (IntCc::Ne, _) => IntCC::NotEqual,
                    (IntCc::Lt, true) => IntCC::SignedLessThan,
                    (IntCc::Le, true) => IntCC::SignedLessThanOrEqual,
                    (IntCc::Gt, true) => IntCC::SignedGreaterThan,
                    (IntCc::Ge, true) => IntCC::SignedGreaterThanOrEqual,
                    (IntCc::Lt, false) => IntCC::UnsignedLessThan,
                    (IntCc::Le, false) => IntCC::UnsignedLessThanOrEqual,
                    (IntCc::Gt, false) => IntCC::UnsignedGreaterThan,
                    (IntCc::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                };
                let b1 = self.b.ins().icmp(c, x, y);
                self.b.ins().uextend(types::I64, b1)
            }
            R::NotBits(a) => {
                let (v, w) = self.operand(a);
                let n = self.b.ins().bnot(v);
                self.mask_val(n, w)
            }
            R::NotBool(a) => {
                let (v, _) = self.operand(a);
                self.b.ins().bxor_imm(v, 1)
            }
            R::Neg(a) => {
                let (v, w) = self.operand(a);
                let n = self.b.ins().ineg(v);
                self.mask_val(n, w)
            }
            R::Cast { from, to, a } => {
                let (v, _) = self.operand(a);
                let x = if from.1 {
                    let s = self.sext_val(v, from.0);
                    // sext 后按 64 位视图，再掩到目标宽
                    s
                } else {
                    self.mask_val(v, from.0)
                };
                self.mask_val(x, *to)
            }
            // ===== M5.4a 内存/地址族 =====
            R::Ref(expr) => self.place_addr(expr),
            R::PtrOffset { ptr, count, stride } => {
                // 真实地址模型位透传（wrapping；与 interp 同）
                let (p, _) = self.operand(ptr);
                let (c, _) = self.operand(count);
                let scaled = self.b.ins().imul_imm(c, *stride as i64);
                self.b.ins().iadd(p, scaled)
            }
            R::PtrDiff { a, b, stride } => {
                // (a - b) / stride（i64 除法；stride 为冻结常量，admit 已拒 0）
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                let d = self.b.ins().isub(av, bv);
                let sv = self.b.ins().iconst(types::I64, *stride as i64);
                self.b.ins().sdiv(d, sv)
            }
            R::UMax { a, b } => {
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                self.b.ins().umax(av, bv)
            }
            // ===== M5.4b-1 标量补面 =====
            R::IntSat { op, signed, a, b } => {
                // interp int_saturating 镜像：int_ovf 判方向后取 clamp
                let (av, w) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (val, ovf) = self.int_ovf(*op, *signed, av, bv, w);
                let ovf8 = self.b.ins().icmp_imm(IntCC::NotEqual, ovf, 0);
                let m = w.mask() as i64;
                let clamp = if *signed {
                    let (x, y) = (self.sext_val(av, w), self.sext_val(bv, w));
                    let toward_max = match op {
                        OvfOp::Add => self.b.ins().icmp_imm(IntCC::SignedGreaterThan, y, 0),
                        OvfOp::Sub => self.b.ins().icmp_imm(IntCC::SignedLessThan, y, 0),
                        OvfOp::Mul => {
                            let x0 = self.b.ins().icmp_imm(IntCC::SignedGreaterThan, x, 0);
                            let y0 = self.b.ins().icmp_imm(IntCC::SignedGreaterThan, y, 0);
                            self.b.ins().icmp(IntCC::Equal, x0, y0)
                        }
                    };
                    let maxv = self.b.ins().iconst(types::I64, m >> 1);
                    let minv = self
                        .b
                        .ins()
                        .iconst(types::I64, (((w.mask() >> 1) + 1) & w.mask()) as i64);
                    self.b.ins().select(toward_max, maxv, minv)
                } else {
                    self.b
                        .ins()
                        .iconst(types::I64, if matches!(op, OvfOp::Sub) { 0 } else { m })
                };
                let masked = self.mask_val(val, w);
                self.b.ins().select(ovf8, clamp, masked)
            }
            R::BitUn { op, a } => {
                let (v, w) = self.operand(a);
                use ir::BitUnOp as B;
                match op {
                    B::Popcount | B::Ctlz | B::Cttz => {
                        let n = if w == Width::W64 {
                            v
                        } else {
                            self.b.ins().ireduce(Self::narrow_ty(w), v)
                        };
                        let r = match op {
                            B::Popcount => self.b.ins().popcnt(n),
                            B::Ctlz => self.b.ins().clz(n),
                            B::Cttz => self.b.ins().ctz(n),
                            _ => unreachable!(),
                        };
                        if w == Width::W64 {
                            r
                        } else {
                            self.b.ins().uextend(types::I64, r)
                        }
                    }
                    B::Bswap => {
                        if w == Width::W8 {
                            // interp：W8 恒等（v & 0xff）
                            self.mask_val(v, w)
                        } else {
                            let n = self.b.ins().ireduce(Self::narrow_ty(w), v);
                            let r = self.b.ins().bswap(n);
                            self.b.ins().uextend(types::I64, r)
                        }
                    }
                    B::Bitreverse => {
                        let n = if w == Width::W64 {
                            v
                        } else {
                            self.b.ins().ireduce(Self::narrow_ty(w), v)
                        };
                        let r = self.b.ins().bitrev(n);
                        if w == Width::W64 {
                            r
                        } else {
                            self.b.ins().uextend(types::I64, r)
                        }
                    }
                }
            }
            R::MemCmp { a, b, n } => {
                // 宿主 memcmp import（i32 结果符号扩展；interp 同通道）
                let (pa, _) = self.operand(a);
                let (pb, _) = self.operand(b);
                let (nv, _) = self.operand(n);
                let fref = self.module.declare_func_in_func(self.memcmp, self.b.func);
                let call = self.b.ins().call(fref, &[pa, pb, nv]);
                let r32 = self.b.inst_results(call)[0];
                self.b.ins().sextend(types::I64, r32)
            }
            R::AtomicLoad { addr, width, order } => {
                // CLIF 原子 = SeqCst（0.133 无弱序；合规强化——D8j 弱序恢复目前只在
                // interp，JIT 侧统一最强序，RAM non-det 包络内，记账 m5-log）
                let (p, _) = self.operand(addr);
                let _ = order;
                let v =
                    self.b
                        .ins()
                        .atomic_load(Self::narrow_ty(*width), MemFlagsData::trusted(), p);
                if *width == Width::W64 {
                    v
                } else {
                    self.b.ins().uextend(types::I64, v)
                }
            }
            // ===== M5.4b-2 浮点 f32/f64 + Math 系 =====
            R::FloatBin { op, fw, a, b } => {
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                if matches!(fw, ir::FloatW::F16) {
                    // f16 走助手（interp 宿主直算通道；op 码表同 f128_bin）
                    let oi = self.b.ins().iconst(
                        types::I64,
                        match op {
                            ir::FloatOp::Add => 0,
                            ir::FloatOp::Sub => 1,
                            ir::FloatOp::Mul => 2,
                            ir::FloatOp::Rem => 3,
                            ir::FloatOp::Div => 4,
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_bin", &[oi, av, bv]);
                    self.mask_val(r, Width::W16)
                } else {
                    let fa = self.as_float(av, *fw);
                    let fb = self.as_float(bv, *fw);
                    let r = match op {
                        ir::FloatOp::Add => self.b.ins().fadd(fa, fb),
                        ir::FloatOp::Sub => self.b.ins().fsub(fa, fb),
                        ir::FloatOp::Mul => self.b.ins().fmul(fa, fb),
                        ir::FloatOp::Div => self.b.ins().fdiv(fa, fb),
                        // IEEE fmod（Rust % 浮点语义）：libm fmod 通道（interp 同源）
                        ir::FloatOp::Rem => self.call_libm_bin("fmod", fa, fb, *fw),
                    };
                    self.as_bits(r, *fw)
                }
            }
            R::FloatCmp { cc, fw, a, b } => {
                // IEEE 偏序语义（NaN 全 false 除 Ne）：CLIF ordered 族 + Ne=NotEqual
                use cranelift_codegen::ir::condcodes::FloatCC;
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                if matches!(fw, ir::FloatW::F16) {
                    let ci = self.b.ins().iconst(
                        types::I64,
                        match cc {
                            IntCc::Eq => 0,
                            IntCc::Ne => 1,
                            IntCc::Lt => 2,
                            IntCc::Le => 3,
                            IntCc::Gt => 4,
                            IntCc::Ge => 5,
                        },
                    );
                    self.call_helper1("mirvm_f16_cmp", &[ci, av, bv])
                } else {
                    let fa = self.as_float(av, *fw);
                    let fb = self.as_float(bv, *fw);
                    let c = match cc {
                        IntCc::Eq => FloatCC::Equal,
                        IntCc::Ne => FloatCC::NotEqual,
                        IntCc::Lt => FloatCC::LessThan,
                        IntCc::Le => FloatCC::LessThanOrEqual,
                        IntCc::Gt => FloatCC::GreaterThan,
                        IntCc::Ge => FloatCC::GreaterThanOrEqual,
                    };
                    let b1 = self.b.ins().fcmp(c, fa, fb);
                    self.b.ins().uextend(types::I64, b1)
                }
            }
            R::FloatNeg { fw, a } => {
                let (av, _) = self.operand(a);
                if matches!(fw, ir::FloatW::F16) {
                    let r = self.call_helper1("mirvm_f16_neg", &[av]);
                    self.mask_val(r, Width::W16)
                } else {
                    let fa = self.as_float(av, *fw);
                    let r = self.b.ins().fneg(fa);
                    self.as_bits(r, *fw)
                }
            }
            R::FloatCast { from, to, a } => {
                let (av, _) = self.operand(a);
                if matches!(from, ir::FloatW::F16) || matches!(to, ir::FloatW::F16) {
                    // f16 参与的互转走助手（kind: 1=f16→f32 2=f16→f64 3=f32→f16 4=f64→f16）
                    let k = self.b.ins().iconst(
                        types::I64,
                        match (from, to) {
                            (ir::FloatW::F16, ir::FloatW::F32) => 1,
                            (ir::FloatW::F16, ir::FloatW::F64) => 2,
                            (ir::FloatW::F32, ir::FloatW::F16) => 3,
                            (ir::FloatW::F64, ir::FloatW::F16) => 4,
                            _ => unreachable!("f16 互转组合外无此类"),
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_cast", &[k, av]);
                    let w = match to {
                        ir::FloatW::F16 => Width::W16,
                        ir::FloatW::F32 => Width::W32,
                        ir::FloatW::F64 => Width::W64,
                    };
                    self.mask_val(r, w)
                } else if from == to {
                    self.mask_val(
                        av,
                        match to {
                            ir::FloatW::F32 => Width::W32,
                            ir::FloatW::F64 => Width::W64,
                            ir::FloatW::F16 => Width::W16,
                        },
                    )
                } else {
                    let fa = self.as_float(av, *from);
                    let r = match (from, to) {
                        (ir::FloatW::F32, ir::FloatW::F64) => self.b.ins().fpromote(types::F64, fa),
                        (ir::FloatW::F64, ir::FloatW::F32) => self.b.ins().fdemote(types::F32, fa),
                        _ => unreachable!("f16 互转走助手"),
                    };
                    self.as_bits(r, *to)
                }
            }
            R::FloatToInt {
                from,
                to,
                signed,
                a,
            } => {
                // Rust `as` 饱和语义（NaN→0、越界→边界）：
                // signed W32/64 = fcvt_to_sint_sat 直达；signed W8/16 = I32 饱和后
                // 再按目标域钳；unsigned = fcvt_to_uint_sat(I64) 后按 mask 钳（u32
                // 域 ⊂ u64，须先钳到 u32::MAX 再掩，Rust 语义）
                let (av, _) = self.operand(a);
                if matches!(from, ir::FloatW::F16) {
                    // f16 → int：助手（kind: 0=i8 1=u8 2=i16 3=u16 4=i32 5=u32 6=i64 7=u64）
                    let k = self.b.ins().iconst(
                        types::I64,
                        match (to, signed) {
                            (Width::W8, true) => 0,
                            (Width::W8, false) => 1,
                            (Width::W16, true) => 2,
                            (Width::W16, false) => 3,
                            (Width::W32, true) => 4,
                            (Width::W32, false) => 5,
                            (Width::W64, true) => 6,
                            (Width::W64, false) => 7,
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_to_int", &[k, av]);
                    self.mask_val(r, *to)
                } else {
                    let fa = self.as_float(av, *from);
                    if *signed {
                        let i64v = match to {
                            Width::W64 => self.b.ins().fcvt_to_sint_sat(types::I64, fa),
                            _ => {
                                let v32 = self.b.ins().fcvt_to_sint_sat(types::I32, fa);
                                self.b.ins().sextend(types::I64, v32)
                            }
                        };
                        match to {
                            Width::W64 => i64v,
                            Width::W32 => self.mask_val(i64v, *to),
                            _ => {
                                // W8/16：I32 饱和值再钳到 [iN::MIN, iN::MAX]
                                let (lo, hi) = match to {
                                    Width::W8 => (i8::MIN as i64, i8::MAX as i64),
                                    Width::W16 => (i16::MIN as i64, i16::MAX as i64),
                                    _ => unreachable!(),
                                };
                                let hi_v = self.b.ins().iconst(types::I64, hi);
                                let lo_v = self.b.ins().iconst(types::I64, lo);
                                let c1 = self.b.ins().smin(i64v, hi_v);
                                let c2 = self.b.ins().smax(c1, lo_v);
                                self.mask_val(c2, *to)
                            }
                        }
                    } else {
                        let u64v = self.b.ins().fcvt_to_uint_sat(types::I64, fa);
                        let m = self.b.ins().iconst(types::I64, to.mask() as i64);
                        self.b.ins().umin(u64v, m)
                    }
                }
            }
            R::IntToFloat { from, to, a } => {
                let (av, _) = self.operand(a);
                if matches!(to, ir::FloatW::F16) {
                    // int → f16：助手（kind 同 to_int 码表）
                    let (fw, signed) = *from;
                    let k = self.b.ins().iconst(
                        types::I64,
                        match (fw, signed) {
                            (Width::W8, true) => 0,
                            (Width::W8, false) => 1,
                            (Width::W16, true) => 2,
                            (Width::W16, false) => 3,
                            (Width::W32, true) => 4,
                            (Width::W32, false) => 5,
                            (Width::W64, true) => 6,
                            (Width::W64, false) => 7,
                        },
                    );
                    let r = self.call_helper1("mirvm_f16_from_int", &[k, av]);
                    self.mask_val(r, Width::W16)
                } else {
                    let (fw, signed) = *from;
                    let t = Self::float_ty(*to);
                    let x = if signed {
                        self.sext_val(av, fw)
                    } else {
                        self.mask_val(av, fw)
                    };
                    let src = if fw == Width::W64 {
                        x
                    } else {
                        self.b.ins().ireduce(types::I32, x)
                    };
                    let f = if signed {
                        self.b.ins().fcvt_from_sint(t, src)
                    } else {
                        self.b.ins().fcvt_from_uint(t, src)
                    };
                    self.as_bits(f, *to)
                }
            }
            R::MathUn { op, fw, a } => {
                use ir::MathUnOp as M;
                let (av, _) = self.operand(a);
                let fa = self.as_float(av, *fw);
                let r = match op {
                    M::Sqrt => self.call_libm_un("sqrt", fa, *fw),
                    M::Sin => self.call_libm_un("sin", fa, *fw),
                    M::Cos => self.call_libm_un("cos", fa, *fw),
                    M::Exp => self.call_libm_un("exp", fa, *fw),
                    M::Exp2 => self.call_libm_un("exp2", fa, *fw),
                    M::Ln => self.call_libm_un("log", fa, *fw),
                    M::Log2 => self.call_libm_un("log2", fa, *fw),
                    M::Log10 => self.call_libm_un("log10", fa, *fw),
                    M::Fabs => self.call_libm_un("fabs", fa, *fw),
                    M::Floor => self.call_libm_un("floor", fa, *fw),
                    M::Ceil => self.call_libm_un("ceil", fa, *fw),
                    M::Trunc => self.call_libm_un("trunc", fa, *fw),
                    M::Round => self.call_libm_un("round", fa, *fw),
                    // round_ties_even = C99 rint（与 interp/Rust 同源）
                    M::RoundTiesEven => self.call_libm_un("rint", fa, *fw),
                };
                self.as_bits(r, *fw)
            }
            R::MathBin { op, fw, a, b } => {
                use ir::MathBinOp as M;
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                let fa = self.as_float(av, *fw);
                let fb = self.as_float(bv, *fw);
                let r = match op {
                    M::Pow => self.call_libm_bin("pow", fa, fb, *fw),
                    M::Powi => {
                        let n32 = self.b.ins().ireduce(types::I32, bv);
                        self.call_powi(fa, n32, *fw)
                    }
                    M::Copysign => self.call_libm_bin("copysign", fa, fb, *fw),
                    M::Minnum => self.call_libm_bin("fmin", fa, fb, *fw),
                    M::Maxnum => self.call_libm_bin("fmax", fa, fb, *fw),
                };
                self.as_bits(r, *fw)
            }
            R::MathFma { fw, a, b, c } => {
                // fma 单次舍入（宿主 mul_add 同源；fmuladd 允许融合/不融合两结果，
                // 融合恒在允许集合内——与 interp 取融合同侧）
                let (av, _) = self.operand(a);
                let (bv, _) = self.operand(b);
                let (cv, _) = self.operand(c);
                let fa = self.as_float(av, *fw);
                let fb = self.as_float(bv, *fw);
                let fc = self.as_float(cv, *fw);
                let r = self.b.ins().fma(fa, fb, fc);
                self.as_bits(r, *fw)
            }
            // ===== M5.4b-3 f128 比较（Rvalue 侧的宽通道）=====
            R::F128Cmp { cc, a, b } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let ci = self.b.ins().iconst(
                    types::I64,
                    match cc {
                        IntCc::Eq => 0,
                        IntCc::Ne => 1,
                        IntCc::Lt => 2,
                        IntCc::Le => 3,
                        IntCc::Gt => 4,
                        IntCc::Ge => 5,
                    },
                );
                self.call_helper1("mirvm_f128_cmp", &[ci, alo, ahi, blo, bhi])
            }
            // ===== M5.4b-3 128 位整数比较（Rvalue 侧）=====
            R::Cmp128 { cc, signed, a, b } => {
                let (alo, ahi) = self.read_wide(a);
                let (blo, bhi) = self.read_wide(b);
                let x = self.i128_of(alo, ahi);
                let y = self.i128_of(blo, bhi);
                let c = match (cc, signed) {
                    (IntCc::Eq, _) => IntCC::Equal,
                    (IntCc::Ne, _) => IntCC::NotEqual,
                    (IntCc::Lt, true) => IntCC::SignedLessThan,
                    (IntCc::Le, true) => IntCC::SignedLessThanOrEqual,
                    (IntCc::Gt, true) => IntCC::SignedGreaterThan,
                    (IntCc::Ge, true) => IntCC::SignedGreaterThanOrEqual,
                    (IntCc::Lt, false) => IntCC::UnsignedLessThan,
                    (IntCc::Le, false) => IntCC::UnsignedLessThanOrEqual,
                    (IntCc::Gt, false) => IntCC::UnsignedGreaterThan,
                    (IntCc::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                };
                let b1 = self.b.ins().icmp(c, x, y);
                self.b.ins().uextend(types::I64, b1)
            }
            _ => unreachable!("admit 已排除"),
        }
    }

    /// interp::int_bin 的逐位镜像（Add/Sub/Mul 掩后签名无关；移位 mod-64 同
    /// wrapping_shl/shr；符号 Shr 用 sext 视图算术右移，b&63 与 CLIF mod-64 一致）。
    fn int_bin(&mut self, op: IntBinOp, signed: bool, a: Value, b: Value, w: Width) -> Value {
        let r = match op {
            IntBinOp::Add => self.b.ins().iadd(a, b),
            IntBinOp::Sub => self.b.ins().isub(a, b),
            IntBinOp::Mul => self.b.ins().imul(a, b),
            IntBinOp::BitAnd => return self.b.ins().band(a, b),
            IntBinOp::BitOr => return self.b.ins().bor(a, b),
            IntBinOp::BitXor => return self.b.ins().bxor(a, b),
            IntBinOp::Shl => {
                // signed 分支的 sext 高位左移后必然溢出掩区（见 interp 注释），同型
                let s = self.b.ins().ishl(a, b);
                return self.mask_val(s, w);
            }
            IntBinOp::Shr => {
                let s = if signed {
                    let x = self.sext_val(a, w);
                    self.b.ins().sshr(x, b)
                } else {
                    self.b.ins().ushr(a, b)
                };
                return self.mask_val(s, w);
            }
            IntBinOp::Div | IntBinOp::Rem => {
                // M5.4b-1：零检 → mirvm_jit_div_zero（interp 同文案同码）；
                // signed 的 MIN/-1 用分支特判（x86 idiv #DE，CLIF sdiv 直接发 idiv）。
                let is_rem = matches!(op, IntBinOp::Rem);
                let zero = self.b.ins().icmp_imm(IntCC::Equal, b, 0);
                self.div_zero_if(zero, is_rem, false);
                if signed {
                    let neg1 = self.b.ins().icmp_imm(IntCC::Equal, b, -1);
                    let triv_blk = self.b.create_block();
                    let norm_blk = self.b.create_block();
                    let join_blk = self.b.create_block();
                    self.b.ins().brif(neg1, triv_blk, &[], norm_blk, &[]);
                    self.b.switch_to_block(triv_blk);
                    let tv = if is_rem {
                        self.b.ins().iconst(types::I64, 0)
                    } else {
                        a
                    };
                    self.b.ins().jump(join_blk, &[tv.into()]);
                    self.b.switch_to_block(norm_blk);
                    let nv = if is_rem {
                        self.b.ins().srem(a, b)
                    } else {
                        self.b.ins().sdiv(a, b)
                    };
                    self.b.ins().jump(join_blk, &[nv.into()]);
                    self.b.switch_to_block(join_blk);
                    let r = self.b.append_block_param(join_blk, types::I64);
                    self.mask_val(r, w)
                } else {
                    let r = if is_rem {
                        self.b.ins().urem(a, b)
                    } else {
                        self.b.ins().udiv(a, b)
                    };
                    self.mask_val(r, w)
                }
            }
        };
        self.mask_val(r, w)
    }

    /// interp::int_ovf 的逐位镜像（128 位提升的 64 位恒等式）：
    /// - unsigned w<64：和/积在 64 位内精确 ⇒ ovf = 精确值 > mask；Sub ovf = a<b。
    /// - unsigned W64：Add ovf = 回绕（r<a）；Mul ovf = umulhi≠0；Sub 同上。
    /// - signed w<64：sext 后 64 位精确 ⇒ 与 [lo,hi] 比界。
    /// - signed W64：Add/Sub 标准符号恒等式；Mul ovf = smulhi ≠ (r>>63)。
    fn int_ovf(&mut self, op: OvfOp, signed: bool, a: Value, b: Value, w: Width) -> (Value, Value) {
        let bb = &mut *self.b;
        if !signed {
            match (op, w) {
                (OvfOp::Sub, _) => {
                    let r = bb.ins().isub(a, b);
                    let f8 = bb.ins().icmp(IntCC::UnsignedLessThan, a, b);
                    let f = bb.ins().uextend(types::I64, f8);
                    (self.mask_val(r, w), f)
                }
                (_, Width::W64) => match op {
                    OvfOp::Add => {
                        let r = bb.ins().iadd(a, b);
                        let f8 = bb.ins().icmp(IntCC::UnsignedLessThan, r, a);
                        let f = bb.ins().uextend(types::I64, f8);
                        (r, f)
                    }
                    OvfOp::Mul => {
                        let r = bb.ins().imul(a, b);
                        let hi = bb.ins().umulhi(a, b);
                        let f8 = bb.ins().icmp_imm(IntCC::NotEqual, hi, 0);
                        let f = bb.ins().uextend(types::I64, f8);
                        (r, f)
                    }
                    OvfOp::Sub => unreachable!(),
                },
                (_, _) => {
                    // w<64：64 位内精确
                    let exact = match op {
                        OvfOp::Add => bb.ins().iadd(a, b),
                        OvfOp::Mul => bb.ins().imul(a, b),
                        OvfOp::Sub => unreachable!(),
                    };
                    let f8 = bb
                        .ins()
                        .icmp_imm(IntCC::UnsignedGreaterThan, exact, w.mask() as i64);
                    let f = bb.ins().uextend(types::I64, f8);
                    (self.mask_val(exact, w), f)
                }
            }
        } else {
            let x = self.sext_val(a, w);
            let y = self.sext_val(b, w);
            let bb = &mut *self.b;
            if w == Width::W64 {
                let r = match op {
                    OvfOp::Add => bb.ins().iadd(x, y),
                    OvfOp::Sub => bb.ins().isub(x, y),
                    OvfOp::Mul => bb.ins().imul(x, y),
                };
                let f8 = match op {
                    // add：符号同入异出；sub：入异且出与被减数异
                    OvfOp::Add => {
                        let t1 = bb.ins().bxor(r, x);
                        let t2 = bb.ins().bxor(r, y);
                        let t = bb.ins().band(t1, t2);
                        bb.ins().icmp_imm(IntCC::SignedLessThan, t, 0)
                    }
                    OvfOp::Sub => {
                        let t1 = bb.ins().bxor(x, y);
                        let t2 = bb.ins().bxor(r, x);
                        let t = bb.ins().band(t1, t2);
                        bb.ins().icmp_imm(IntCC::SignedLessThan, t, 0)
                    }
                    OvfOp::Mul => {
                        let hi = bb.ins().smulhi(x, y);
                        let sgn = bb.ins().sshr_imm(r, 63);
                        bb.ins().icmp(IntCC::NotEqual, hi, sgn)
                    }
                };
                let f = bb.ins().uextend(types::I64, f8);
                (self.mask_val(r, w), f)
            } else {
                let r = match op {
                    OvfOp::Add => bb.ins().iadd(x, y),
                    OvfOp::Sub => bb.ins().isub(x, y),
                    OvfOp::Mul => bb.ins().imul(x, y),
                };
                let (lo, hi) = match w {
                    Width::W8 => (i8::MIN as i64, i8::MAX as i64),
                    Width::W16 => (i16::MIN as i64, i16::MAX as i64),
                    Width::W32 => (i32::MIN as i64, i32::MAX as i64),
                    Width::W64 => unreachable!(),
                };
                let under = bb.ins().icmp_imm(IntCC::SignedLessThan, r, lo);
                let over = bb.ins().icmp_imm(IntCC::SignedGreaterThan, r, hi);
                let f8 = bb.ins().bor(under, over);
                let f = bb.ins().uextend(types::I64, f8);
                (self.mask_val(r, w), f)
            }
        }
    }

    fn term(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        t: &Terminator,
        blocks: &[cranelift_codegen::ir::Block],
        has_ret: bool,
    ) {
        match t {
            Terminator::Goto(bb) => {
                self.b.ins().jump(blocks[*bb as usize], &[]);
            }
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => match discr {
                SwitchDiscr::Scalar(op) => {
                    let (v, _) = self.operand(op);
                    // icmp+brif 链（v1；值稀疏，br_table 留优化项）
                    for (val, bb) in targets {
                        let hit = self.b.ins().icmp_imm(IntCC::Equal, v, *val as u64 as i64);
                        let next = self.b.create_block();
                        self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(blocks[*otherwise as usize], &[]);
                }
                SwitchDiscr::Wide(pe) => {
                    // M5.4b-3：128 位判别——place 一次读全 128 位（iconcat），逐目标
                    // I128 常量比较（D8k：targets 与 discriminator 都保完整 128 位）
                    let (lo, hi) = self.read_wide(pe);
                    let v = self.i128_of(lo, hi);
                    for (val, bb) in targets {
                        let c = self.iconst128(*val);
                        let hit = self.b.ins().icmp(IntCC::Equal, v, c);
                        let next = self.b.create_block();
                        self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(blocks[*otherwise as usize], &[]);
                }
            },
            Terminator::Call {
                callee,
                args,
                ret,
                target,
                ..
            } => {
                let mut av: Vec<Value> = Vec::with_capacity(args.len());
                for a in args {
                    av.push(self.operand(a).0);
                }
                let cb = &self.shared.module.funcs[*callee as usize];
                let plt = callee_abi(cb).filter(|(cn, _)| *cn == av.len());
                if let Some((_, cret)) = plt {
                    // 热路：PLT 内存间接——load slots_fast[callee] + call_indirect
                    //（恒定形状；蹦床→fast 的升级对调用点透明）
                    let slot_addr = &self.shared.jit.slots_fast[*callee as usize]
                        as *const std::sync::atomic::AtomicU64
                        as i64;
                    let ap = self.b.ins().iconst(types::I64, slot_addr);
                    let fp = self
                        .b
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), ap, 0);
                    let sig = {
                        let mut s = self.module.make_signature();
                        for _ in 0..av.len() {
                            s.params.push(AbiParam::new(types::I64));
                        }
                        if cret {
                            s.returns.push(AbiParam::new(types::I64));
                        }
                        s
                    };
                    let sigref = self.b.import_signature(sig);
                    let call = self.b.ins().call_indirect(sigref, fp, &av);
                    if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                        let lo = if cret {
                            self.b.inst_results(call)[0]
                        } else {
                            self.b.ins().iconst(types::I64, 0)
                        };
                        self.def_slot(*s, lo);
                    }
                } else {
                    // 冷路：调用点直接 c2i（打包展平实参回解释器——interp 本就吃
                    // 展平 av，callee 任意 ABI 语义一致；panic 类分支的归宿）
                    let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (av.len().max(1) * 8) as u32,
                        3,
                    ));
                    for (i, v) in av.iter().enumerate() {
                        self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                    }
                    let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    let fref = self.module.declare_func_in_func(self.c2i, self.b.func);
                    let fv = self.b.ins().iconst(types::I64, *callee as i64);
                    let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                    let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                    let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                    self.b.ins().call(fref, &[fv, ap, nv, rp]);
                    if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                        let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                        self.def_slot(*s, lo);
                    }
                }
                self.b.ins().jump(blocks[*target as usize], &[]);
            }
            Terminator::Return => {
                if has_ret {
                    let RetAbi::Scalar(s) = body.ret else {
                        unreachable!()
                    };
                    let var = self.var(s.off);
                    let v = self.b.use_var(var);
                    self.b.ins().return_(&[v]);
                } else {
                    self.b.ins().return_(&[]);
                }
            }
            Terminator::Unreachable => {
                let fref = self
                    .module
                    .declare_func_in_func(self.unreachable, self.b.func);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                self.b.ins().call(fref, &[fv]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            _ => unreachable!("admit 已排除"),
        }
    }
}

/// M5.4a 取址分析（保守全集，m5.4-design §3.1/Q1）：收集必须落栈帧内存的 frame
/// offset——任何被 PlaceExpr::Local/Mem/AddrOf/Ref/Copy/Repeat/Indirect-ABI 触及者。
/// 判据 = 宁多勿漏：误提升（地址被取的槽错放 SSA）是错值级，多落帧只是慢一点。
/// or-pattern 全枚举 Stmt/Terminator——新增 place 通道变体 = 非穷尽编译错误。
/// 落帧集：区间模型（m5.4-design §3.1「触及即落帧」保守全集的完整实现）。
/// 任何被 Ref/AddrOf/Copy/Repeat/Volatile/128 位·SIMD place 通道的【字节区间】触及的
/// 槽一律落帧。只记基址会把区间内槽误提升为 SSA：标量写进变量、place 通道读物理帧
/// （恒 0/旧值）= 错值级 miscompile——M5.4b regex SIGSEGV 的实锤根因正是 Copy src
/// 区间 [96,112) 内的槽 104 漏落帧（Weak::drop 读空指针 +0x10）。
#[derive(Default)]
struct FrameMap {
    ranges: Vec<(u32, u32)>,
}

impl FrameMap {
    fn add(&mut self, a: u32, b: u32) {
        if a < b {
            self.ranges.push((a, b));
        }
    }
    fn contains(&self, off: u32) -> bool {
        self.ranges.iter().any(|&(a, b)| a <= off && off < b)
    }
    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// scan_place 的触及范围：Bytes = 从基址起 n 字节；Escape = 地址逃逸
/// （Ref/AddrOf/Indirect 返回落点），本地不可知 → 保守到帧尾。
#[derive(Clone, Copy)]
enum Extent {
    Bytes(u32),
    Escape,
}

fn analyze_frame(body: &ir::FuncBody) -> FrameMap {
    let fsz = body.frame_size;
    /// 帧相关段 = 首 Deref/动态步之前。Offset 累加后：
    /// - 遇 Deref：指针槽本体 8 字节落帧即止（之后是 pointee，与帧无关）；
    /// - 遇动态步（IndexScaled/VTableAlignOffset）：运行期地址，保守 [pos, 帧尾)；
    /// - 步序耗尽：按 extent 落 [pos, pos+n) 或 [pos, 帧尾)。
    fn scan_place(out: &mut FrameMap, pe: &ir::PlaceExpr, extent: Extent, fsz: u32) {
        let ir::PlaceBase::Local(base) = pe.base else {
            return;
        };
        let mut pos: i64 = base as i64;
        for step in pe.steps.iter() {
            match step {
                ir::PlaceStep::Offset(k) => pos += *k as i64,
                ir::PlaceStep::Deref => {
                    let p = pos.clamp(0, fsz as i64) as u32;
                    out.add(p, p.saturating_add(8).min(fsz));
                    return;
                }
                ir::PlaceStep::IndexScaled { .. } | ir::PlaceStep::VTableAlignOffset { .. } => {
                    let p = pos.clamp(0, fsz as i64) as u32;
                    out.add(p, fsz);
                    return;
                }
            }
        }
        let p = pos.clamp(0, fsz as i64) as u32;
        let end = match extent {
            Extent::Bytes(n) => p.saturating_add(n).min(fsz),
            Extent::Escape => fsz,
        };
        // 帧末 ZST 取址（corpus 批8 c_starlark_eval 实锤）：落在帧尾（p==fsz）的
        // Escape/空 Bytes 产生退化区间 `(fsz,fsz)`，被 FrameMap::add 的 `a<b` 静默
        // 丢弃——落帧集整个为空时 frame_ss 缺席，Ref 的 addr_of_local expect 炸
        // 「必落帧」。语义上该地址是合法的"帧末+1"（ZST 永不解引用），与 interp
        // 的 base+off 口径一致：补一个帧内 1 字节活口锚强制帧物化。
        if p == end
            && let Some(anchor) = fsz.checked_sub(1)
        {
            out.add(anchor, fsz);
            return;
        }
        out.add(p, end);
    }
    fn scan_op(out: &mut FrameMap, op: &Operand, fsz: u32) {
        match op {
            Operand::Mem { expr, width } => scan_place(out, expr, Extent::Bytes(width.bytes()), fsz),
            Operand::AddrOf(expr) => scan_place(out, expr, Extent::Escape, fsz),
            Operand::SubImm { base, .. } => scan_op(out, base, fsz),
            Operand::Slot(_) | Operand::Imm { .. } => {}
        }
    }
    fn scan_sp(out: &mut FrameMap, sp: &ScalarPlace, fsz: u32) {
        if let ScalarPlace::Mem { expr, width } = sp {
            scan_place(out, expr, Extent::Bytes(width.bytes()), fsz);
        }
    }
    fn scan_ret(out: &mut FrameMap, r: &RetDest, fsz: u32) {
        match r {
            RetDest::Ignore => {}
            RetDest::Scalar(sp) => scan_sp(out, sp, fsz),
            RetDest::Pair(a, b) => {
                scan_sp(out, a, fsz);
                scan_sp(out, b, fsz);
            }
            // 被调方经 sret 写整个返回聚合，尺寸本地不可知 → Escape
            RetDest::Indirect(pe) => scan_place(out, pe, Extent::Escape, fsz),
        }
    }
    fn scan_rv(out: &mut FrameMap, rv: &ir::Rvalue, fsz: u32) {
        use ir::Rvalue as R;
        match rv {
            R::Ref(pe) => scan_place(out, pe, Extent::Escape, fsz),
            R::Use(o)
            | R::NotBits(o)
            | R::NotBool(o)
            | R::Neg(o)
            | R::Cast { a: o, .. }
            | R::BitUn { a: o, .. } => scan_op(out, o, fsz),
            R::IntBin { a, b, .. }
            | R::IntCmp { a, b, .. }
            | R::PtrDiff { a, b, .. }
            | R::UMax { a, b }
            | R::IntSat { a, b, .. }
            | R::MemCmp { a, b, .. }
            | R::IntCmp3 { a, b, .. }
            | R::FloatBin { a, b, .. }
            | R::FloatCmp { a, b, .. }
            | R::MathBin { a, b, .. } => {
                scan_op(out, a, fsz);
                scan_op(out, b, fsz);
            }
            R::PtrOffset { ptr, count, .. } => {
                scan_op(out, ptr, fsz);
                scan_op(out, count, fsz);
            }
            R::MathFma { a, b, c, .. } => {
                scan_op(out, a, fsz);
                scan_op(out, b, fsz);
                scan_op(out, c, fsz);
            }
            R::NicheDiscr { tag, .. }
            | R::MathUn { a: tag, .. }
            | R::FloatNeg { a: tag, .. }
            | R::FloatCast { a: tag, .. }
            | R::FloatToInt { a: tag, .. }
            | R::IntToFloat { a: tag, .. }
            | R::AtomicLoad { addr: tag, .. } => scan_op(out, tag, fsz),
            R::F128Cmp { a, b, .. } | R::Cmp128 { a, b, .. } => {
                scan_place(out, a, Extent::Bytes(16), fsz);
                scan_place(out, b, Extent::Bytes(16), fsz);
            }
            R::SimdBitmask {
                a,
                lanes,
                lane_bytes,
            }
            | R::SimdReduce {
                a,
                lanes,
                lane_bytes,
                ..
            }
            | R::SimdReduceArith {
                a,
                lanes,
                lane_bytes,
                ..
            } => scan_place(out, a, Extent::Bytes(*lanes as u32 * *lane_bytes as u32), fsz),
            R::TlsRef(_) => {}
        }
    }
    /// SIMD place 的字节宽（lanes × lane_bytes 全向量）。
    fn simd_ext(lanes: &u16, lane_bytes: &u8) -> Extent {
        Extent::Bytes(*lanes as u32 * *lane_bytes as u32)
    }
    /// Repeat 系的字节宽（count × elem_size，饱和；scan_place 内再收帧尾）。
    fn rep_ext(count: &u64, elem_size: &u64) -> Extent {
        Extent::Bytes(count.saturating_mul(*elem_size).min(u32::MAX as u64) as u32)
    }
    let mut out = FrameMap::default();
    for blk in &body.blocks {
        for st in &blk.stmts {
            match st {
                Stmt::Assign { dst, rv } => {
                    scan_sp(&mut out, dst, fsz);
                    scan_rv(&mut out, rv, fsz);
                }
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    scan_op(&mut out, a, fsz);
                    scan_op(&mut out, b, fsz);
                    scan_sp(&mut out, dst_val, fsz);
                    scan_sp(&mut out, dst_flag, fsz);
                }
                // Copy/Repeat/Volatile：place 通道按【整个字节区间】落帧（m5.4-design
                // §3.1「触及即落帧」）——只记基址 = 区间内槽误提升 = 错值级（实锤根因）
                Stmt::Copy { dst, src, size } => {
                    scan_place(&mut out, dst, Extent::Bytes(*size), fsz);
                    scan_place(&mut out, src, Extent::Bytes(*size), fsz);
                }
                Stmt::RepeatScalar {
                    dst,
                    val,
                    count,
                    elem_size,
                } => {
                    scan_place(&mut out, dst, rep_ext(count, &(*elem_size as u64)), fsz);
                    scan_op(&mut out, val, fsz);
                }
                Stmt::RepeatBytes {
                    first,
                    count,
                    elem_size,
                } => scan_place(&mut out, first, rep_ext(count, elem_size), fsz),
                Stmt::VolatileLoad { addr, dst, size } => {
                    scan_op(&mut out, addr, fsz);
                    scan_place(&mut out, dst, Extent::Bytes(*size), fsz);
                }
                Stmt::VolatileStore { addr, src, size } => {
                    scan_op(&mut out, addr, fsz);
                    scan_place(&mut out, src, Extent::Bytes(*size), fsz);
                }
                Stmt::AtomicStore { addr, val, .. } => {
                    scan_op(&mut out, addr, fsz);
                    scan_op(&mut out, val, fsz);
                }
                Stmt::AtomicCxchg {
                    addr,
                    expected,
                    new,
                    dst_val,
                    dst_ok,
                    ..
                } => {
                    scan_op(&mut out, addr, fsz);
                    scan_op(&mut out, expected, fsz);
                    scan_op(&mut out, new, fsz);
                    scan_sp(&mut out, dst_val, fsz);
                    scan_sp(&mut out, dst_ok, fsz);
                }
                Stmt::AtomicRmw { addr, val, dst, .. } => {
                    scan_op(&mut out, addr, fsz);
                    scan_op(&mut out, val, fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::MemCopy {
                    dst, src, count, ..
                } => {
                    scan_op(&mut out, dst, fsz);
                    scan_op(&mut out, src, fsz);
                    scan_op(&mut out, count, fsz);
                }
                Stmt::MemSet {
                    dst, val, count, ..
                } => {
                    scan_op(&mut out, dst, fsz);
                    scan_op(&mut out, val, fsz);
                    scan_op(&mut out, count, fsz);
                }
                Stmt::SimdBin {
                    dst,
                    a,
                    b,
                    lanes,
                    lane_bytes,
                    ..
                }
                | Stmt::SimdSelectBitmask {
                    dst,
                    a,
                    b,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdFma {
                    dst,
                    a,
                    b,
                    c,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, c, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdUn {
                    dst,
                    a,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdCast {
                    dst,
                    src,
                    lanes,
                    src_bytes,
                    dst_bytes,
                    ..
                } => {
                    scan_place(
                        &mut out,
                        dst,
                        Extent::Bytes(*lanes as u32 * *dst_bytes as u32),
                        fsz,
                    );
                    scan_place(
                        &mut out,
                        src,
                        Extent::Bytes(*lanes as u32 * *src_bytes as u32),
                        fsz,
                    );
                }
                Stmt::SimdExtractDyn {
                    src,
                    idx,
                    dst,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, src, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, idx, fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::SimdArithOffset {
                    ptrs,
                    offsets,
                    dst,
                    lanes,
                    ..
                } => {
                    // 地址向量：lanes × 8 字节（指针/偏移均按机器字宽）
                    let ext = Extent::Bytes(*lanes as u32 * 8);
                    scan_place(&mut out, ptrs, ext, fsz);
                    scan_place(&mut out, offsets, ext, fsz);
                    scan_place(&mut out, dst, ext, fsz);
                }
                Stmt::SimdFunnel {
                    dst,
                    a,
                    b,
                    shift,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, shift, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdSelect {
                    mask,
                    a,
                    b,
                    dst,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, mask, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdGather {
                    passthru,
                    ptrs,
                    mask,
                    dst,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, passthru, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, ptrs, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, mask, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdScatter {
                    values,
                    ptrs,
                    mask,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, values, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, ptrs, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, mask, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdMaskedLoad {
                    mask,
                    base,
                    passthru,
                    dst,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, mask, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, base, fsz);
                    scan_place(&mut out, passthru, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdMaskedStore {
                    mask,
                    base,
                    values,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, mask, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, base, fsz);
                    scan_place(&mut out, values, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdInsertDyn {
                    src,
                    idx,
                    val,
                    dst,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, src, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, idx, fsz);
                    scan_op(&mut out, val, fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdSplat {
                    dst,
                    val,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, val, fsz);
                }
                // 128 位族：place 通道恒 16 字节
                Stmt::Bin128 { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    if let ir::Bin128Rhs::Wide(w) = b {
                        scan_place(&mut out, w, Extent::Bytes(16), fsz);
                    }
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::Sat128 { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    scan_place(&mut out, b, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::Wide128ToFloat { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::FloatToWide128 { src, dst, .. } => {
                    scan_op(&mut out, src, fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::Bit128 { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::Bit128Count { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::F128Bin { a, b, dst, .. } | Stmt::F128Fma { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    scan_place(&mut out, b, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128MathBin { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    if let ir::F128Rhs::Wide(w) = b {
                        scan_place(&mut out, w, Extent::Bytes(16), fsz);
                    }
                    if let ir::F128Rhs::Scalar(o) = b {
                        scan_op(&mut out, o, fsz);
                    }
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128Un { a, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128FromScalar { src, dst, .. } => {
                    scan_op(&mut out, src, fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128ToScalar { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::F128FromWideInt { src, dst, .. } | Stmt::F128ToWideInt { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::NicheDiscr128 { tag, dst, .. } => {
                    scan_place(&mut out, tag, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::Trap(_) | Stmt::Nop | Stmt::Fence { .. } => {}
            }
        }
        match &blk.term {
            Terminator::Goto(_) | Terminator::Return | Terminator::Unreachable => {}
            Terminator::SwitchInt { discr, .. } => match discr {
                SwitchDiscr::Scalar(o) => scan_op(&mut out, o, fsz),
                SwitchDiscr::Wide(pe) => scan_place(&mut out, pe, Extent::Bytes(16), fsz),
            },
            Terminator::Call { args, ret, .. }
            | Terminator::CallBuiltin { args, ret, .. }
            | Terminator::CallForeign { args, ret, .. } => {
                for a in args {
                    scan_op(&mut out, a, fsz);
                }
                scan_ret(&mut out, ret, fsz);
            }
            // callee 操作数同扫（fn-ptr 可能经 Mem/Deref 链读帧槽——同类潜在漏项）
            Terminator::CallIndirect {
                callee, args, ret, ..
            } => {
                scan_op(&mut out, callee, fsz);
                for a in args {
                    scan_op(&mut out, a, fsz);
                }
                scan_ret(&mut out, ret, fsz);
            }
            Terminator::InlineAsm { ins, outs, .. } => {
                for (_, o) in ins {
                    scan_op(&mut out, o, fsz);
                }
                for (_, sp) in outs {
                    scan_sp(&mut out, sp, fsz);
                }
            }
            Terminator::Resume | Terminator::TerminateAbort | Terminator::Trap(_) => {}
        }
    }
    // Indirect ABI（M5.4c 准入；保守纳入——取址性最强）：槽本体 = sret/参数指针 8 字节
    if let RetAbi::Indirect {
        ret_off, sret_off, ..
    } = &body.ret
    {
        out.add(*ret_off, (*ret_off).saturating_add(8).min(fsz));
        out.add(*sret_off, (*sret_off).saturating_add(8).min(fsz));
    }
    for p in &body.params {
        if let ParamAbi::Indirect { off, .. } = p {
            out.add(*off, (*off).saturating_add(8).min(fsz));
        }
    }
    out
}

/// ir::RmwOp → CLIF AtomicRmwOp（一一对应；D8j 冻结的有符号性经 interp 选 AtomicI*/U*
/// 同源——CLIF 的 Max/Min 同理分有/无符号两族）。
fn clif_rmw_op(op: ir::RmwOp) -> cranelift_codegen::ir::AtomicRmwOp {
    use cranelift_codegen::ir::AtomicRmwOp as C;
    match op {
        ir::RmwOp::Xchg => C::Xchg,
        ir::RmwOp::Add => C::Add,
        ir::RmwOp::Sub => C::Sub,
        ir::RmwOp::And => C::And,
        ir::RmwOp::Or => C::Or,
        ir::RmwOp::Xor => C::Xor,
        ir::RmwOp::Nand => C::Nand,
        ir::RmwOp::Max => C::Smax,
        ir::RmwOp::Min => C::Smin,
        ir::RmwOp::UMax => C::Umax,
        ir::RmwOp::UMin => C::Umin,
    }
}

/// 收集 SSA 候选槽偏移（def 0 初始化用）= 全部 Slot 引用减去落帧集。
fn collect_ssa_offs(body: &ir::FuncBody, frame_offs: &FrameMap, out: &mut Vec<u32>) {
    let mut push = |s: &Slot| {
        if !frame_offs.contains(s.off) && !out.contains(&s.off) {
            out.push(s.off);
        }
    };
    let op = |o: &Operand, push: &mut dyn FnMut(&Slot)| {
        if let Operand::Slot(s) = o {
            push(s);
        }
    };
    if let RetAbi::Scalar(s) = &body.ret {
        push(s);
    }
    for p in &body.params {
        if let ParamAbi::Scalar(s) = p {
            push(s);
        }
    }
    for blk in &body.blocks {
        for st in &blk.stmts {
            match st {
                Stmt::Assign { dst, rv } => {
                    if let ScalarPlace::Slot(s) = dst {
                        push(s);
                    }
                    use ir::Rvalue as R;
                    match rv {
                        R::Use(a)
                        | R::NotBits(a)
                        | R::NotBool(a)
                        | R::Neg(a)
                        | R::Cast { a, .. }
                        | R::BitUn { a, .. } => op(a, &mut push),
                        R::IntBin { a, b, .. }
                        | R::IntCmp { a, b, .. }
                        | R::PtrDiff { a, b, .. }
                        | R::UMax { a, b } => {
                            op(a, &mut push);
                            op(b, &mut push);
                        }
                        R::PtrOffset { ptr, count, .. } => {
                            op(ptr, &mut push);
                            op(count, &mut push);
                        }
                        _ => {}
                    }
                }
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    op(a, &mut push);
                    op(b, &mut push);
                    if let ScalarPlace::Slot(s) = dst_val {
                        push(s);
                    }
                    if let ScalarPlace::Slot(s) = dst_flag {
                        push(s);
                    }
                }
                _ => {}
            }
        }
        match &blk.term {
            Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(o),
                ..
            } => op(o, &mut push),
            Terminator::Call { args, ret, .. } => {
                for a in args {
                    op(a, &mut push);
                }
                if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                    push(s);
                }
            }
            _ => {}
        }
    }
}

// ===== M5.4 前置 probe：LSDA 管线最小验证（cg_clif GccExceptTable 同构）=====
//
// 验证链（任一环失败即 M5.4 LSDA 方案需要重评）：
// try_call（tag0=cleanup，`BlockArg::TryCallExn(0)` 传异常指针）→ 从
// `buffer.call_sites()` 手工构建 GccExceptTable（ret_addr-1 单字节 call-site 项）
// → CIE(rust_eh_personality, absptr) + FDE.lsda → `__register_frame` →
// 宿主 panic 载荷（resume_unwind，与 spike3/M4.2 同形态）→ cleanup pad 执行 →
// `_Unwind_Resume(exn)` 续传至宿主 catch_unwind。
#[cfg(all(test, target_arch = "x86_64", target_os = "linux"))]
mod lsda_probe {
    use cranelift_codegen::ir::{
        AbiParam, BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
        InstBuilder, Signature, types,
    };
    use cranelift_codegen::isa::unwind::UnwindInfo;
    use cranelift_codegen::isa::{CallConv, TargetIsa};
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
    use cranelift_jit::{JITBuilder, JITModule};
    use cranelift_module::{Linkage, Module as ClifModule};
    use gimli::RunTimeEndian;
    use gimli::write::{Address, EhFrame, EndianVec, FrameTable};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// pad 执行标记（0=未走, 1=pad 已走, 2=正常返回）；宿主侧断言用
    static PAD_MARK: AtomicU64 = AtomicU64::new(0);

    /// 宿主 panic 载荷源（spike3/M4.2 同形态：resume_unwind 携带 Rust payload）
    extern "C-unwind" fn probe_raise() {
        std::panic::resume_unwind(Box::new(0x2a_i32));
    }
    extern "C-unwind" fn probe_mark(x: u64) {
        PAD_MARK.store(x, Ordering::SeqCst);
    }
    extern "C-unwind" fn probe_unwind_resume(ex: *mut u8) -> ! {
        unsafe { _Unwind_Resume(ex) }
    }
    unsafe extern "C" {
        fn _Unwind_Resume(ex: *mut u8) -> !;
        fn rust_eh_personality();
    }

    /// 手工 GccExceptTable（cleanup-only，无 type_info；cg_clif 版式 + **全覆盖**）：
    /// - 无 handler 的调用点：(ret_addr-1, len=1, lpad=0, action=0) —— 命中即
    ///   EHAction::None（rust find_eh_action 的 cs_lpad==0 分支）
    /// - cleanup handler 调用点：(ret_addr-1, len=1, pad, action=0)
    /// **rust 版 find_eh_action 对"ip 不在表中"返回 EHAction::Terminate（= _URC_FATAL），
    /// 与 libgcc 的 __gcc_personality_v0（no-entry = None）不同——call-site 表必须覆盖
    /// 函数内全部调用点**（cg_clif 对无 handler 站点同样发 lpad=0 项的原因）。
    /// 项按 buffer.call_sites() 序（= 指令序，满足 rust 解析器的有序表假设）。
    fn build_lsda(call_sites: &[(u64, Option<u64>)]) -> Vec<u8> {
        fn uleb(out: &mut Vec<u8>, mut v: u64) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                out.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        let mut out = vec![0xff, 0xff, 0x01]; // lpStart=omit, ttype=omit, csEncoding=uleb128
        let mut body = Vec::new();
        for &(ret_addr, pad) in call_sites {
            uleb(&mut body, ret_addr - 1);
            uleb(&mut body, 1);
            uleb(&mut body, pad.unwrap_or(0));
            uleb(&mut body, 0); // action=0
        }
        uleb(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
        out
    }

    /// 定义后取 (UnwindInfo, [(ret_addr, Option<landing_pad>)])——全调用点
    /// （cg_clif add_function 同数据源同口径：无 handler → None（lpad=0 项））
    fn unwind_and_sites(
        isa: &dyn TargetIsa,
        cctx: &cranelift_codegen::Context,
    ) -> (UnwindInfo, Vec<(u64, Option<u64>)>) {
        let cc = cctx.compiled_code().unwrap();
        let ui = cc.create_unwind_info(isa).unwrap().expect("unwind_info");
        let mut cs = Vec::new();
        for site in cc.buffer.call_sites() {
            if site.exception_handlers.is_empty() {
                cs.push((u64::from(site.ret_addr), None));
            }
            for h in site.exception_handlers {
                if let cranelift_codegen::FinalizedMachExceptionHandler::Tag(tag, lp) = h {
                    assert_eq!(tag.as_u32(), 0, "probe 只发 cleanup tag");
                    cs.push((u64::from(site.ret_addr), Some(u64::from(*lp))));
                }
            }
        }
        (ui, cs)
    }

    /// 二分定位（分支 -1）：纯宿主基线——catch_unwind(probe_raise) 无 JIT 参与。
    /// 此分支若挂 = 测试二进制的 unwind 基线本身坏了，与 JIT 无关。
    #[test]
    fn host_baseline_catch() {
        let r = std::panic::catch_unwind(|| probe_raise());
        let p = r.expect_err("宿主基线应收到 payload");
        assert_eq!(*p.downcast::<i32>().unwrap(), 0x2a);
    }

    /// 二分定位（probe 分支 0）：导入符号直调——probe_mark 可见即 import 调用链好。
    #[test]
    fn import_call_works() {
        PAD_MARK.store(0, Ordering::SeqCst);
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_mark", probe_mark as *const u8);
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();
        let mark_sig = {
            let mut s = module.make_signature();
            s.params.push(AbiParam::new(types::I64));
            s
        };
        let mark = module
            .declare_function("probe_mark", Linkage::Import, &mark_sig)
            .unwrap();
        let caller_id = module
            .declare_function("caller", Linkage::Local, &mark_sig)
            .unwrap();
        {
            let mut cctx = module.make_context();
            cctx.func.signature = mark_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let mref = module.declare_func_in_func(mark, b.func);
            let x = b.ins().iconst(types::I64, 7);
            b.ins().call(mref, &[x]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(caller_id, &mut cctx).unwrap();
            module.clear_context(&mut cctx);
        }
        module.finalize_definitions().unwrap();
        let addr = module.get_finalized_function(caller_id) as u64;
        let f: unsafe extern "C-unwind" fn(u64) = unsafe { std::mem::transmute(addr) };
        unsafe { f(0) };
        assert_eq!(PAD_MARK.load(Ordering::SeqCst), 7, "import 直调未生效");
    }

    /// 二分定位（分支 A0）：单 JIT 帧穿越（caller 直调 probe_raise，无中间帧）
    #[test]
    fn cfi_single_frame() {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_raise", probe_raise as *const u8);
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();
        let empty_sig = module.make_signature();
        let raise = module
            .declare_function("probe_raise", Linkage::Import, &empty_sig)
            .unwrap();
        let caller_id = module
            .declare_function("caller", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_caller;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raise, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(caller_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_caller = ui;
            module.clear_context(&mut cctx);
        }
        module.finalize_definitions().unwrap();
        let mut table = FrameTable::default();
        let cie = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
        let caller_addr = module.get_finalized_function(caller_id) as u64;
        if let UnwindInfo::SystemV(info) = ui_caller {
            table.add_fde(cie, info.to_fde(Address::Constant(caller_addr)));
        } else {
            panic!("无 SystemV UnwindInfo");
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
        unsafe {
            let start = buf.as_ptr();
            let end = start.add(buf.len());
            let mut cur = start;
            while cur < end {
                let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
                if len == 0 {
                    break;
                }
                let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
                if cie_ptr != 0 {
                    __register_frame(cur);
                }
                cur = cur.add(len + 4);
            }
        }
        let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
        let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
        let payload = result
            .expect_err("单帧分支应收到 payload")
            .downcast::<i32>()
            .expect("载荷类型错");
        assert_eq!(*payload, 0x2a);
    }

    /// 二分定位（probe 分支 A）：纯 CFI 穿越——无 personality/LSDA，宿主 panic 经
    /// 两个 JIT 帧（普通 call）传回宿主 catch_unwind。此分支不过 = 基础注册坏；
    /// 过 = 问题在 LSDA/personality/pad 半区。
    #[test]
    fn cfi_only_passthrough() {
        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_raise", probe_raise as *const u8);
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();
        let empty_sig = module.make_signature();
        let raise = module
            .declare_function("probe_raise", Linkage::Import, &empty_sig)
            .unwrap();

        let raiser_id = module
            .declare_function("raiser", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_raiser;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raise, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(raiser_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_raiser = ui;
            module.clear_context(&mut cctx);
        }
        let caller_id = module
            .declare_function("caller", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_caller;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raiser_id, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(caller_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_caller = ui;
            module.clear_context(&mut cctx);
        }
        module.finalize_definitions().unwrap();

        let mut table = FrameTable::default();
        let cie = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
        let raiser_addr = module.get_finalized_function(raiser_id) as u64;
        let caller_addr = module.get_finalized_function(caller_id) as u64;
        for (ui, addr) in [(ui_raiser, raiser_addr), (ui_caller, caller_addr)] {
            if let UnwindInfo::SystemV(info) = ui {
                table.add_fde(cie, info.to_fde(Address::Constant(addr)));
            } else {
                panic!("无 SystemV UnwindInfo");
            }
        }
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
        unsafe {
            let start = buf.as_ptr();
            let end = start.add(buf.len());
            let mut cur = start;
            while cur < end {
                let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
                if len == 0 {
                    break;
                }
                let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
                if cie_ptr != 0 {
                    __register_frame(cur);
                }
                cur = cur.add(len + 4);
            }
        }

        let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
        let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
        let payload = result
            .expect_err("CFI-only 分支应收到宿主 payload（基础注册疑似坏）")
            .downcast::<i32>()
            .expect("载荷类型错");
        assert_eq!(*payload, 0x2a);
    }

    #[test]
    fn lsda_cleanup_pad_executes_and_resume_continues() {
        PAD_MARK.store(0, Ordering::SeqCst);

        let mut fb = settings::builder();
        fb.set("opt_level", "speed").unwrap();
        fb.set("unwind_info", "true").unwrap();
        fb.set("preserve_frame_pointers", "true").unwrap();
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(fb))
            .unwrap();
        let mut jb = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jb.symbol("probe_raise", probe_raise as *const u8);
        jb.symbol("probe_mark", probe_mark as *const u8);
        jb.symbol("_Unwind_Resume", {
            unsafe extern "C" {
                fn _Unwind_Resume(ex: *mut u8) -> !;
            }
            _Unwind_Resume as *const u8
        });
        let mut module = JITModule::new(jb);
        let mut fbc = FunctionBuilderContext::new();

        let empty_sig = module.make_signature(); // () -> ()
        let mark_sig = {
            let mut s = module.make_signature();
            s.params.push(AbiParam::new(types::I64));
            s
        };
        let resume_sig = {
            let mut s = module.make_signature();
            s.params.push(AbiParam::new(types::I64));
            s
        };
        let raise = module
            .declare_function("probe_raise", Linkage::Import, &empty_sig)
            .unwrap();
        let mark = module
            .declare_function("probe_mark", Linkage::Import, &mark_sig)
            .unwrap();
        let resume = module
            .declare_function("_Unwind_Resume", Linkage::Import, &resume_sig)
            .unwrap();

        // raiser：调 probe_raise（宿主 resume_unwind 载荷经其帧穿过）
        let raiser_id = module
            .declare_function("raiser", Linkage::Local, &empty_sig)
            .unwrap();
        let ui_raiser;
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            b.switch_to_block(entry);
            let rref = module.declare_func_in_func(raise, b.func);
            b.ins().call(rref, &[]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
            module.define_function(raiser_id, &mut cctx).unwrap();
            let (ui, _) = unwind_and_sites(module.isa(), &cctx);
            ui_raiser = ui;
            module.clear_context(&mut cctx);
        }

        // caller：try_call(raiser)；normal → ok(mark 2)；tag0 pad(mark 1 → _Unwind_Resume(exn))
        let caller_id = module
            .declare_function("caller", Linkage::Local, &empty_sig)
            .unwrap();
        let (ui_caller, call_sites);
        {
            let mut cctx = module.make_context();
            cctx.func.signature = empty_sig.clone();
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut fbc);
            let entry = b.create_block();
            let ok = b.create_block();
            let pad = b.create_block();
            b.append_block_param(pad, types::I64); // TryCallExn(0) 的落点块参
            b.switch_to_block(entry);

            let rref = module.declare_func_in_func(raiser_id, b.func);
            let sig0 = b.func.import_signature(Signature::new(CallConv::SystemV));
            let normal = BlockCall::new(ok, [], &mut b.func.dfg.value_lists);
            let pad_call = b.func.dfg.block_call(pad, &[BlockArg::TryCallExn(0)]);
            let et = b.func.dfg.exception_tables.push(ExceptionTableData::new(
                sig0,
                normal,
                [ExceptionTableItem::Tag(
                    ExceptionTag::with_number(0).unwrap(),
                    pad_call,
                )],
            ));
            b.ins().try_call(rref, &[], et);

            b.switch_to_block(ok);
            let mref = module.declare_func_in_func(mark, b.func);
            let two = b.ins().iconst(types::I64, 2);
            b.ins().call(mref, &[two]);
            b.ins().return_(&[]);

            b.switch_to_block(pad);
            let exn = b.block_params(pad)[0];
            let mref2 = module.declare_func_in_func(mark, b.func);
            let one = b.ins().iconst(types::I64, 1);
            b.ins().call(mref2, &[one]);
            let resref = module.declare_func_in_func(resume, b.func);
            b.ins().call(resref, &[exn]);
            b.ins()
                .trap(cranelift_codegen::ir::TrapCode::user(1).unwrap());
            b.seal_all_blocks();
            b.finalize();

            module.define_function(caller_id, &mut cctx).unwrap();
            let (ui, cs) = unwind_and_sites(module.isa(), &cctx);
            ui_caller = ui;
            call_sites = cs;
            module.clear_context(&mut cctx);
        }
        assert!(
            call_sites.len() >= 2,
            "caller 应有多个 call-site（try_call + 其余调用点全覆盖）"
        );
        module.finalize_definitions().unwrap();

        // eh_frame：CIE0 无 personality（raiser）；CIE1 = rust_eh_personality + lsda（caller）。
        // personality 走 DW.ref 间接（cg_clif 形态）：CIE 的 personality 指针指向一个
        // 持有真 personality 地址的静态 u64——absptr 直嵌在本环境被证伪（空 LSDA 也 abort）。
        static PERS_REF: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        PERS_REF.store(rust_eh_personality as *const u8 as u64, Ordering::SeqCst);
        let mut table = FrameTable::default();
        let cie_plain = table.add_cie(module.isa().create_systemv_cie().expect("cie"));
        let mut cie_pers = module.isa().create_systemv_cie().expect("cie");
        cie_pers.lsda_encoding = Some(gimli::DW_EH_PE_absptr);
        cie_pers.personality = Some((
            gimli::DwEhPe(gimli::DW_EH_PE_indirect.0 | gimli::DW_EH_PE_absptr.0),
            Address::Constant(&PERS_REF as *const std::sync::atomic::AtomicU64 as u64),
        ));
        let cie_pers_id = table.add_cie(cie_pers);

        let raiser_addr = module.get_finalized_function(raiser_id) as u64;
        let caller_addr = module.get_finalized_function(caller_id) as u64;
        if let UnwindInfo::SystemV(info) = ui_raiser {
            table.add_fde(cie_plain, info.to_fde(Address::Constant(raiser_addr)));
        } else {
            panic!("raiser 无 SystemV UnwindInfo");
        }
        let lsda_bytes = build_lsda(&call_sites);
        let lsda_addr = lsda_bytes.as_ptr() as u64;
        std::mem::forget(lsda_bytes); // FDE/LSDA 终身有效（probe 进程期）
        if let UnwindInfo::SystemV(info) = ui_caller {
            let mut fde = info.to_fde(Address::Constant(caller_addr));
            fde.lsda = Some(Address::Constant(lsda_addr));
            table.add_fde(cie_pers_id, fde);
        } else {
            panic!("caller 无 SystemV UnwindInfo");
        }

        // spike5 同款注册：FrameTable → eh_frame 字节 + 终止零长 + 逐 FDE __register_frame
        let mut eh = EhFrame(EndianVec::new(RunTimeEndian::Little));
        table.write_eh_frame(&mut eh).unwrap();
        let mut bytes = eh.0.into_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let buf: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
        unsafe {
            let start = buf.as_ptr();
            let end = start.add(buf.len());
            let mut cur = start;
            while cur < end {
                let len = u32::from_le_bytes(std::ptr::read(cur as *const [u8; 4])) as usize;
                if len == 0 {
                    break;
                }
                let cie_ptr = u32::from_le_bytes(std::ptr::read(cur.add(4) as *const [u8; 4]));
                if cie_ptr != 0 {
                    __register_frame(cur);
                }
                cur = cur.add(len + 4);
            }
        }

        // 全链点火：宿主 catch_unwind 应收到 42；pad 应已走（mark=1，而非 2）
        let caller_fn: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(caller_addr) };
        let result = std::panic::catch_unwind(|| unsafe { caller_fn() });
        assert!(
            result.is_err(),
            "caller 未抛出（pad/unwind 链断裂；PAD_MARK={}）",
            PAD_MARK.load(Ordering::SeqCst)
        );
        let payload = result.unwrap_err();
        assert_eq!(payload.downcast_ref::<i32>(), Some(&0x2a), "载荷丢失/替换");
        assert_eq!(
            PAD_MARK.load(Ordering::SeqCst),
            1,
            "cleanup pad 未执行（LSDA/personality 未命中）"
        );
    }
}
