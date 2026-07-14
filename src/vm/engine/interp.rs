//! 类型化 interp_frame：M4 引擎解释器。
//!
//! 结构与 spike3 同形（Call 宿主递归=模型 A、Return 拷回、restore、raw-ptr ctx +
//! 字段级瞬态借用）。M4.1：place 求值（地址表达式 → 真地址裸读写，帧/堆/statics 统一）
//! + 调用约定 v2（标量 1 槽 / pair 2 槽 / 大聚合 indirect+sret）。
//!
//! M4.2 unwind（spike3 协议平移）：**guest 异常 = 宿主 panic 载 `GuestPanic`**（同一平台
//! unwinder + personality，候选 A）。解释帧的 landing pad = `FrameGuard`（动态 LSDA：
//! `unwind_edge` 在每个可 unwind 终止子前设置）——unwind 穿帧时其 Drop 跑 cleanup 链
//! （`Resume` 结束=返回让 unwind 续传，单条 native 栈零协调）+ 恢复操作数区。
//! catch 点 downcast 区分 GuestPanic / 宿主 panic（VM bug 原样续传，绝不吞）。

use std::cell::Cell;
use std::mem::MaybeUninit;
use std::panic::{self, AssertUnwindSafe};
use std::process::exit;

use super::ctx::{Ctx, Shared};
use super::frame::ByteRegion;
use super::ir::{
    Bb, Block, FuncBody, IntBinOp, IntCc, Module, Operand, OvfOp, ParamAbi, PlaceBase, PlaceExpr,
    PlaceStep, RetAbi, RetDest, Rvalue, ScalarPlace, Slot, Stmt, SwitchDiscr, Terminator,
    UnwindAction, Width,
};

/// guest panic 的宿主载体（spike3 协议）：exception = guest 侧 `_Unwind_Exception` 指针
/// （panic_unwind 的 Exception 结构在 guest 堆闭环——Box::into_raw/from_raw 全在 guest
/// 解释执行，引擎只运载指针）。
pub struct GuestPanic {
    pub exception: u64,
}

/// 发起 guest panic（`resume_unwind` 不触发宿主 panic hook → 无噪声）。
pub(crate) fn raise_guest(exception: u64) -> ! {
    panic::resume_unwind(Box::new(GuestPanic { exception }))
}

#[inline]
fn region_reserve(ctx: *mut Ctx, size: u32, align: u32) -> usize {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.reserve(size, align)
}
#[inline]
fn region_restore(ctx: *mut Ctx, base: usize) {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.restore(base);
}
#[inline]
fn slot_read(ctx: *mut Ctx, base: usize, s: Slot) -> u64 {
    let r: &ByteRegion = unsafe { &(*ctx).region };
    r.read(base, s)
}
#[inline]
fn slot_write(ctx: *mut Ctx, base: usize, s: Slot, v: u64) {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.write(base, s, v);
}

/// 真地址裸读（fast：guest 合法假设，无范围检查——真实地址模型）。
#[inline]
fn mem_read(addr: u64, w: Width) -> u64 {
    let p = addr as *const u8;
    unsafe {
        match w {
            Width::W8 => p.read_unaligned() as u64,
            Width::W16 => (p as *const u16).read_unaligned() as u64,
            Width::W32 => (p as *const u32).read_unaligned() as u64,
            Width::W64 => (p as *const u64).read_unaligned(),
        }
    }
}

/// 真地址裸写。
#[inline]
fn mem_write(addr: u64, w: Width, v: u64) {
    let p = addr as *mut u8;
    unsafe {
        match w {
            Width::W8 => p.write_unaligned(v as u8),
            Width::W16 => (p as *mut u16).write_unaligned(v as u16),
            Width::W32 => (p as *mut u32).write_unaligned(v as u32),
            Width::W64 => (p as *mut u64).write_unaligned(v),
        }
    }
}

/// 把 guest 中的一个完整值作为 opaque 位型读入。`MaybeUninit<[u8; N]>`
/// 的对齐是 1，因此不会把 `[u8; N]` 之类低对齐 guest 类型错误地
/// 强化为宿主整数对齐；`MaybeUninit` 同时允许聚合值含未初始化 padding。
#[inline]
unsafe fn volatile_load_n<const N: usize>(src: *const u8, dst: *mut u8) {
    let value = unsafe { (src as *const MaybeUninit<[u8; N]>).read_volatile() };
    unsafe {
        std::ptr::copy_nonoverlapping((&value as *const MaybeUninit<[u8; N]>).cast::<u8>(), dst, N)
    };
}

/// 先按原始字节（包括可能未初始化的 padding）搬入 opaque 载体，
/// 再发出一个等宽 volatile store。
#[inline]
unsafe fn volatile_store_n<const N: usize>(dst: *mut u8, src: *const u8) {
    let mut value = MaybeUninit::<[u8; N]>::uninit();
    unsafe { std::ptr::copy_nonoverlapping(src, value.as_mut_ptr().cast::<u8>(), N) };
    unsafe { (dst as *mut MaybeUninit<[u8; N]>).write_volatile(value) };
}

/// 宽 memory-repr volatile 值的后端分解。先/后端都只接触
/// `MaybeUninit<[u8; N]>`，所以 padding 保持 opaque；16/8/4/2/1 的分块
/// 对应目标最终必须完成的若干机器访问，不承诺原子性。
#[inline]
unsafe fn volatile_load_chunks(mut src: *const u8, mut dst: *mut u8, mut size: usize) {
    while size >= 16 {
        unsafe { volatile_load_n::<16>(src, dst) };
        src = src.wrapping_add(16);
        dst = dst.wrapping_add(16);
        size -= 16;
    }
    if size >= 8 {
        unsafe { volatile_load_n::<8>(src, dst) };
        src = src.wrapping_add(8);
        dst = dst.wrapping_add(8);
        size -= 8;
    }
    if size >= 4 {
        unsafe { volatile_load_n::<4>(src, dst) };
        src = src.wrapping_add(4);
        dst = dst.wrapping_add(4);
        size -= 4;
    }
    if size >= 2 {
        unsafe { volatile_load_n::<2>(src, dst) };
        src = src.wrapping_add(2);
        dst = dst.wrapping_add(2);
        size -= 2;
    }
    if size == 1 {
        unsafe { volatile_load_n::<1>(src, dst) };
    }
}

#[inline]
unsafe fn volatile_store_chunks(mut dst: *mut u8, mut src: *const u8, mut size: usize) {
    while size >= 16 {
        unsafe { volatile_store_n::<16>(dst, src) };
        dst = dst.wrapping_add(16);
        src = src.wrapping_add(16);
        size -= 16;
    }
    if size >= 8 {
        unsafe { volatile_store_n::<8>(dst, src) };
        dst = dst.wrapping_add(8);
        src = src.wrapping_add(8);
        size -= 8;
    }
    if size >= 4 {
        unsafe { volatile_store_n::<4>(dst, src) };
        dst = dst.wrapping_add(4);
        src = src.wrapping_add(4);
        size -= 4;
    }
    if size >= 2 {
        unsafe { volatile_store_n::<2>(dst, src) };
        dst = dst.wrapping_add(2);
        src = src.wrapping_add(2);
        size -= 2;
    }
    if size == 1 {
        unsafe { volatile_store_n::<1>(dst, src) };
    }
}

#[inline]
fn mem_read_volatile(addr: u64, dst: u64, size: u32) {
    unsafe {
        match size {
            1 => volatile_load_n::<1>(addr as *const u8, dst as *mut u8),
            2 => volatile_load_n::<2>(addr as *const u8, dst as *mut u8),
            4 => volatile_load_n::<4>(addr as *const u8, dst as *mut u8),
            8 => volatile_load_n::<8>(addr as *const u8, dst as *mut u8),
            16 => volatile_load_n::<16>(addr as *const u8, dst as *mut u8),
            _ => {
                let size = size as usize;
                let mut snapshot = vec![MaybeUninit::<u8>::uninit(); size];
                volatile_load_chunks(addr as *const u8, snapshot.as_mut_ptr().cast::<u8>(), size);
                std::ptr::copy_nonoverlapping(snapshot.as_ptr().cast::<u8>(), dst as *mut u8, size);
            }
        }
    }
}

#[inline]
fn mem_write_volatile(addr: u64, src: u64, size: u32) {
    unsafe {
        match size {
            1 => volatile_store_n::<1>(addr as *mut u8, src as *const u8),
            2 => volatile_store_n::<2>(addr as *mut u8, src as *const u8),
            4 => volatile_store_n::<4>(addr as *mut u8, src as *const u8),
            8 => volatile_store_n::<8>(addr as *mut u8, src as *const u8),
            16 => volatile_store_n::<16>(addr as *mut u8, src as *const u8),
            _ => {
                let size = size as usize;
                let mut snapshot = vec![MaybeUninit::<u8>::uninit(); size];
                std::ptr::copy_nonoverlapping(
                    src as *const u8,
                    snapshot.as_mut_ptr().cast::<u8>(),
                    size,
                );
                volatile_store_chunks(addr as *mut u8, snapshot.as_ptr().cast::<u8>(), size);
            }
        }
    }
}

/// 引擎诊断退出（M4.0：Trap/Assert 失败/除零统一走这里；M4.2 起 Assert 变真 panic）。
fn engine_abort(what: &str) -> ! {
    eprintln!("mirvm[m4-engine]: {what}");
    exit(70)
}

/// guest TLS 实例真地址（M4.4 D3）：首访惰性物化——heap 分配 + 冻结模板拷贝。
/// 每线程一份（Ctx 是 thread_local）；v1 记账：线程退出不跑 dtor、实例泄漏。
fn tls_addr(ctx: *mut Ctx, id: u32) -> u64 {
    let tls: &Vec<u64> = unsafe { &(*ctx).tls };
    if let Some(&a) = tls.get(id as usize)
        && a != 0
    {
        return a;
    }
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let t = module.tls[id as usize];
    let addr = super::heap::alloc(t.size.max(1), t.align as u64);
    unsafe {
        std::ptr::copy_nonoverlapping(t.template as *const u8, addr as *mut u8, t.size as usize);
        let tls = &mut (*ctx).tls;
        if tls.len() <= id as usize {
            tls.resize(id as usize + 1, 0);
        }
        tls[id as usize] = addr;
    }
    addr
}

/// 地址表达式求值 → 真地址（place 求值核心；帧基址是真地址 ⇒ 全程裸地址算术）。
fn eval_place_addr(ctx: *mut Ctx, base: usize, expr: &PlaceExpr) -> u64 {
    let mut addr = match expr.base {
        PlaceBase::Local(off) => base as u64 + off as u64,
        PlaceBase::Static(a) => a,
    };
    for step in &expr.steps {
        match step {
            PlaceStep::Deref => addr = mem_read(addr, Width::W64),
            PlaceStep::Offset(o) => addr = addr.wrapping_add(*o as i64 as u64),
            PlaceStep::VTableAlignOffset {
                meta,
                unaligned,
                packed,
            } => {
                let (vtable, _) = eval_operand(ctx, base, meta);
                let mut align = mem_read(vtable.wrapping_add(2 * 8), Width::W64);
                if let Some(packed) = packed {
                    align = align.min(*packed);
                }
                if align == 0 || !align.is_power_of_two() {
                    engine_abort(&format!(
                        "dyn vtable alignment 非 2 的幂：{align}（vtable={vtable:#x}）"
                    ));
                }
                let offset = unaligned.checked_add(align - 1).unwrap_or_else(|| {
                    engine_abort(&format!(
                        "dyn 尾字段 offset 溢出：unaligned={unaligned} align={align}"
                    ))
                }) & !(align - 1);
                addr = addr.wrapping_add(offset);
            }
            PlaceStep::IndexScaled { idx, stride } => {
                let i = slot_read(ctx, base, *idx);
                addr = addr.wrapping_add(i.wrapping_mul(*stride));
            }
        }
    }
    addr
}

/// 符号扩展到 i64（按宽度）。
#[inline]
fn sext(bits: u64, w: Width) -> i64 {
    match w {
        Width::W8 => bits as u8 as i8 as i64,
        Width::W16 => bits as u16 as i16 as i64,
        Width::W32 => bits as u32 as i32 as i64,
        Width::W64 => bits as i64,
    }
}

fn eval_operand(ctx: *mut Ctx, base: usize, op: &Operand) -> (u64, Width) {
    match op {
        Operand::Slot(s) => (slot_read(ctx, base, *s), s.width),
        Operand::Mem { expr, width } => {
            let addr = eval_place_addr(ctx, base, expr);
            (mem_read(addr, *width), *width)
        }
        Operand::Imm { bits, width } => (*bits, *width),
        Operand::AddrOf(expr) => (eval_place_addr(ctx, base, expr), Width::W64),
        Operand::SubImm { base: b, sub } => {
            let (v, w) = eval_operand(ctx, base, b);
            (v.wrapping_sub(*sub) & w.mask(), w)
        }
    }
}

fn place_write(ctx: *mut Ctx, base: usize, p: &ScalarPlace, v: u64) {
    match p {
        ScalarPlace::Slot(s) => slot_write(ctx, base, *s, v),
        ScalarPlace::Mem { expr, width } => {
            let addr = eval_place_addr(ctx, base, expr);
            mem_write(addr, *width, v);
        }
    }
}

fn int_bin(op: IntBinOp, signed: bool, a: u64, b: u64, w: Width) -> u64 {
    let m = w.mask();
    let r = if signed {
        let (x, y) = (sext(a, w), sext(b, w));
        match op {
            IntBinOp::Add => x.wrapping_add(y) as u64,
            IntBinOp::Sub => x.wrapping_sub(y) as u64,
            IntBinOp::Mul => x.wrapping_mul(y) as u64,
            IntBinOp::Div => {
                if y == 0 {
                    engine_abort("guest 整除以零");
                }
                x.wrapping_div(y) as u64
            }
            IntBinOp::Rem => {
                if y == 0 {
                    engine_abort("guest 取余以零");
                }
                x.wrapping_rem(y) as u64
            }
            IntBinOp::BitAnd => a & b,
            IntBinOp::BitOr => a | b,
            IntBinOp::BitXor => a ^ b,
            IntBinOp::Shl => (x as u64).wrapping_shl(b as u32),
            IntBinOp::Shr => (x >> (b as u32 & 63)) as u64, // 算术右移
        }
    } else {
        match op {
            IntBinOp::Add => a.wrapping_add(b),
            IntBinOp::Sub => a.wrapping_sub(b),
            IntBinOp::Mul => a.wrapping_mul(b),
            IntBinOp::Div => {
                if b == 0 {
                    engine_abort("guest 整除以零");
                }
                a / b
            }
            IntBinOp::Rem => {
                if b == 0 {
                    engine_abort("guest 取余以零");
                }
                a % b
            }
            IntBinOp::BitAnd => a & b,
            IntBinOp::BitOr => a | b,
            IntBinOp::BitXor => a ^ b,
            IntBinOp::Shl => a.wrapping_shl(b as u32),
            IntBinOp::Shr => (a & m).wrapping_shr(b as u32), // 逻辑右移
        }
    };
    r & m
}

fn int_cmp(cc: IntCc, signed: bool, a: u64, b: u64, w: Width) -> u64 {
    let ord = if signed {
        sext(a, w).cmp(&sext(b, w))
    } else {
        (a & w.mask()).cmp(&(b & w.mask()))
    };
    let t = match cc {
        IntCc::Eq => ord.is_eq(),
        IntCc::Ne => ord.is_ne(),
        IntCc::Lt => ord.is_lt(),
        IntCc::Le => ord.is_le(),
        IntCc::Gt => ord.is_gt(),
        IntCc::Ge => ord.is_ge(),
    };
    t as u64
}

/// *WithOverflow：提升到 128 位算，按宽度/符号判溢出。
fn int_ovf(op: OvfOp, signed: bool, a: u64, b: u64, w: Width) -> (u64, bool) {
    if signed {
        let (x, y) = (sext(a, w) as i128, sext(b, w) as i128);
        let r = match op {
            OvfOp::Add => x + y,
            OvfOp::Sub => x - y,
            OvfOp::Mul => x * y,
        };
        let (lo, hi) = match w {
            Width::W8 => (i8::MIN as i128, i8::MAX as i128),
            Width::W16 => (i16::MIN as i128, i16::MAX as i128),
            Width::W32 => (i32::MIN as i128, i32::MAX as i128),
            Width::W64 => (i64::MIN as i128, i64::MAX as i128),
        };
        ((r as u64) & w.mask(), r < lo || r > hi)
    } else {
        let (x, y) = ((a & w.mask()) as u128, (b & w.mask()) as u128);
        let r = match op {
            OvfOp::Add => x + y,
            OvfOp::Sub => x.wrapping_sub(y),
            OvfOp::Mul => x * y,
        };
        let ovf = match op {
            OvfOp::Sub => x < y,
            _ => r > w.mask() as u128,
        };
        ((r as u64) & w.mask(), ovf)
    }
}

fn eval_rvalue(ctx: *mut Ctx, base: usize, rv: &Rvalue) -> u64 {
    match rv {
        Rvalue::Use(op) => eval_operand(ctx, base, op).0,
        Rvalue::TlsRef(id) => tls_addr(ctx, *id),
        Rvalue::IntBin { op, signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            int_bin(*op, *signed, av, bv, w)
        }
        Rvalue::IntCmp { cc, signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            int_cmp(*cc, *signed, av, bv, w)
        }
        Rvalue::NotBits(a) => {
            let (v, w) = eval_operand(ctx, base, a);
            !v & w.mask()
        }
        Rvalue::NotBool(a) => {
            let (v, _) = eval_operand(ctx, base, a);
            (v ^ 1) & 1
        }
        Rvalue::Neg(a) => {
            let (v, w) = eval_operand(ctx, base, a);
            v.wrapping_neg() & w.mask()
        }
        Rvalue::Cast { from, to, a } => {
            let (v, _) = eval_operand(ctx, base, a);
            let x = if from.1 {
                sext(v, from.0) as u64
            } else {
                v & from.0.mask()
            };
            x & to.mask()
        }
        Rvalue::Ref(expr) => eval_place_addr(ctx, base, expr),
        Rvalue::PtrOffset { ptr, count, stride } => {
            let (p, _) = eval_operand(ctx, base, ptr);
            let (c, cw) = eval_operand(ctx, base, count);
            // count 按有符号处理（ptr::sub 编译成负 count 的 offset）
            let delta = (sext(c, cw) as u64).wrapping_mul(*stride);
            p.wrapping_add(delta)
        }
        Rvalue::IntCmp3 { signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let ord = if *signed {
                sext(av, w).cmp(&sext(bv, w))
            } else {
                (av & w.mask()).cmp(&(bv & w.mask()))
            };
            (ord as i8 as u8) as u64
        }
        Rvalue::NicheDiscr {
            tag,
            niche_start,
            variants_start,
            variants_len,
            untagged,
        } => {
            let (t, w) = eval_operand(ctx, base, tag);
            let rel = t.wrapping_sub(*niche_start) & w.mask();
            if rel < *variants_len {
                variants_start + rel
            } else {
                *untagged
            }
        }
        Rvalue::FloatBin { op, is64, a, b } => {
            use super::ir::FloatOp as F;
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            if *is64 {
                let (x, y) = (f64::from_bits(av), f64::from_bits(bv));
                match op {
                    F::Add => x + y,
                    F::Sub => x - y,
                    F::Mul => x * y,
                    F::Div => x / y,
                    F::Rem => x % y,
                }
                .to_bits()
            } else {
                let (x, y) = (f32::from_bits(av as u32), f32::from_bits(bv as u32));
                (match op {
                    F::Add => x + y,
                    F::Sub => x - y,
                    F::Mul => x * y,
                    F::Div => x / y,
                    F::Rem => x % y,
                })
                .to_bits() as u64
            }
        }
        Rvalue::UMax { a, b } => eval_operand(ctx, base, a)
            .0
            .max(eval_operand(ctx, base, b).0),
        Rvalue::MathUn { op, is64, a } => {
            use super::ir::MathUnOp as M;
            let (av, _) = eval_operand(ctx, base, a);
            macro_rules! un {
                ($x:expr) => {{
                    let x = $x;
                    match op {
                        M::Sqrt => x.sqrt(),
                        M::Sin => x.sin(),
                        M::Cos => x.cos(),
                        M::Exp => x.exp(),
                        M::Exp2 => x.exp2(),
                        M::Ln => x.ln(),
                        M::Log2 => x.log2(),
                        M::Log10 => x.log10(),
                        M::Fabs => x.abs(),
                        M::Floor => x.floor(),
                        M::Ceil => x.ceil(),
                        M::Trunc => x.trunc(),
                        M::Round => x.round(),
                        M::RoundTiesEven => x.round_ties_even(),
                    }
                }};
            }
            if *is64 {
                un!(f64::from_bits(av)).to_bits()
            } else {
                un!(f32::from_bits(av as u32)).to_bits() as u64
            }
        }
        Rvalue::MathBin { op, is64, a, b } => {
            use super::ir::MathBinOp as M;
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            macro_rules! bin {
                ($x:expr, $y:expr) => {{
                    let x = $x;
                    match op {
                        M::Pow => x.powf($y),
                        M::Powi => x.powi(bv as i32),
                        M::Copysign => x.copysign($y),
                        M::Minnum => x.min($y),
                        M::Maxnum => x.max($y),
                    }
                }};
            }
            if *is64 {
                bin!(f64::from_bits(av), f64::from_bits(bv)).to_bits()
            } else {
                bin!(f32::from_bits(av as u32), f32::from_bits(bv as u32)).to_bits() as u64
            }
        }
        Rvalue::MathFma { is64, a, b, c } => {
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let (cv, _) = eval_operand(ctx, base, c);
            if *is64 {
                f64::from_bits(av)
                    .mul_add(f64::from_bits(bv), f64::from_bits(cv))
                    .to_bits()
            } else {
                f32::from_bits(av as u32)
                    .mul_add(f32::from_bits(bv as u32), f32::from_bits(cv as u32))
                    .to_bits() as u64
            }
        }
        Rvalue::FloatCmp { cc, is64, a, b } => {
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let t = if *is64 {
                let (x, y) = (f64::from_bits(av), f64::from_bits(bv));
                match cc {
                    IntCc::Eq => x == y,
                    IntCc::Ne => x != y,
                    IntCc::Lt => x < y,
                    IntCc::Le => x <= y,
                    IntCc::Gt => x > y,
                    IntCc::Ge => x >= y,
                }
            } else {
                let (x, y) = (f32::from_bits(av as u32), f32::from_bits(bv as u32));
                match cc {
                    IntCc::Eq => x == y,
                    IntCc::Ne => x != y,
                    IntCc::Lt => x < y,
                    IntCc::Le => x <= y,
                    IntCc::Gt => x > y,
                    IntCc::Ge => x >= y,
                }
            };
            t as u64
        }
        Rvalue::FloatNeg { is64, a } => {
            let (av, _) = eval_operand(ctx, base, a);
            if *is64 {
                (-f64::from_bits(av)).to_bits()
            } else {
                (-f32::from_bits(av as u32)).to_bits() as u64
            }
        }
        Rvalue::FloatCast { from64, to64, a } => {
            let (av, _) = eval_operand(ctx, base, a);
            match (from64, to64) {
                (true, false) => (f64::from_bits(av) as f32).to_bits() as u64,
                (false, true) => (f32::from_bits(av as u32) as f64).to_bits(),
                _ => av, // 同宽：位透传
            }
        }
        Rvalue::FloatToInt {
            from64,
            to,
            signed,
            a,
        } => {
            let (av, _) = eval_operand(ctx, base, a);
            // f32→f64 精确保值 ⇒ 统一经 f64；宿主 `as` 即 Rust 饱和语义（NaN→0、越界→边界）
            let x = if *from64 {
                f64::from_bits(av)
            } else {
                f32::from_bits(av as u32) as f64
            };
            let v: u64 = if *signed {
                match to {
                    Width::W8 => x as i8 as u64,
                    Width::W16 => x as i16 as u64,
                    Width::W32 => x as i32 as u64,
                    Width::W64 => x as i64 as u64,
                }
            } else {
                match to {
                    Width::W8 => x as u8 as u64,
                    Width::W16 => x as u16 as u64,
                    Width::W32 => x as u32 as u64,
                    Width::W64 => x as u64,
                }
            };
            v & to.mask()
        }
        Rvalue::IntToFloat { from, to64, a } => {
            let (av, _) = eval_operand(ctx, base, a);
            let x: f64 = if from.1 {
                sext(av, from.0) as f64
            } else {
                (av & from.0.mask()) as f64
            };
            if *to64 {
                x.to_bits()
            } else {
                // 经 f64 中转对 ≤32 位整数无双舍入问题；u64/i64→f32 用直转
                let f: f32 = if from.1 {
                    sext(av, from.0) as f32
                } else {
                    (av & from.0.mask()) as f32
                };
                f.to_bits() as u64
            }
        }
        Rvalue::BitUn { op, a } => {
            use super::ir::BitUnOp as B;
            let (v, w) = eval_operand(ctx, base, a);
            match (op, w) {
                (B::Popcount, _) => (v & w.mask()).count_ones() as u64,
                (B::Ctlz, Width::W8) => (v as u8).leading_zeros() as u64,
                (B::Ctlz, Width::W16) => (v as u16).leading_zeros() as u64,
                (B::Ctlz, Width::W32) => (v as u32).leading_zeros() as u64,
                (B::Ctlz, Width::W64) => v.leading_zeros() as u64,
                (B::Cttz, Width::W8) => (v as u8).trailing_zeros() as u64,
                (B::Cttz, Width::W16) => (v as u16).trailing_zeros() as u64,
                (B::Cttz, Width::W32) => (v as u32).trailing_zeros() as u64,
                (B::Cttz, Width::W64) => v.trailing_zeros() as u64,
                (B::Bswap, Width::W8) => v & 0xff,
                (B::Bswap, Width::W16) => (v as u16).swap_bytes() as u64,
                (B::Bswap, Width::W32) => (v as u32).swap_bytes() as u64,
                (B::Bswap, Width::W64) => v.swap_bytes(),
                (B::Bitreverse, Width::W8) => (v as u8).reverse_bits() as u64,
                (B::Bitreverse, Width::W16) => (v as u16).reverse_bits() as u64,
                (B::Bitreverse, Width::W32) => (v as u32).reverse_bits() as u64,
                (B::Bitreverse, Width::W64) => v.reverse_bits(),
            }
        }
        Rvalue::AtomicLoad { addr, width } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            // 真宿主原子指令（spike4 义务）；SeqCst 最强序
            unsafe {
                match width {
                    Width::W8 => AtomicU8::from_ptr(p as *mut u8).load(Ordering::SeqCst) as u64,
                    Width::W16 => AtomicU16::from_ptr(p as *mut u16).load(Ordering::SeqCst) as u64,
                    Width::W32 => AtomicU32::from_ptr(p as *mut u32).load(Ordering::SeqCst) as u64,
                    Width::W64 => AtomicU64::from_ptr(p as *mut u64).load(Ordering::SeqCst),
                }
            }
        }
        Rvalue::PtrDiff { a, b, stride } => {
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            ((av.wrapping_sub(bv) as i64) / *stride as i64) as u64
        }
        Rvalue::SimdBitmask {
            a,
            lanes,
            lane_bytes,
        } => {
            let pa = eval_place_addr(ctx, base, a);
            let lb = *lane_bytes as u64;
            let mut mask = 0u64;
            for i in 0..*lanes as u64 {
                // 小端 lane 的符号位在末字节最高位
                let top = unsafe { *((pa + i * lb + lb - 1) as *const u8) };
                mask |= ((top >> 7) as u64) << i;
            }
            mask
        }
        Rvalue::MemCmp { a, b, n } => {
            let (pa, _) = eval_operand(ctx, base, a);
            let (pb, _) = eval_operand(ctx, base, b);
            let (len, _) = eval_operand(ctx, base, n);
            let sa = unsafe { std::slice::from_raw_parts(pa as *const u8, len as usize) };
            let sb = unsafe { std::slice::from_raw_parts(pb as *const u8, len as usize) };
            (sa.cmp(sb) as i8 as i32) as u32 as u64
        }
        Rvalue::IntSat { op, signed, a, b } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let (v, ovf) = int_ovf(*op, *signed, av, bv, w);
            if !ovf {
                v
            } else if *signed {
                // 方向：加正溢出→MAX，其余按符号推
                let (x, y) = (sext(av, w), sext(bv, w));
                let toward_max = match op {
                    OvfOp::Add => y > 0,
                    OvfOp::Sub => y < 0,
                    OvfOp::Mul => (x > 0) == (y > 0),
                };
                let m = w.mask();
                if toward_max {
                    m >> 1
                } else {
                    ((m >> 1) + 1) & m
                }
            } else {
                match op {
                    OvfOp::Sub => 0,
                    _ => w.mask(),
                }
            }
        }
        Rvalue::SimdReduce {
            all,
            a,
            lanes,
            lane_bytes,
        } => {
            let pa = eval_place_addr(ctx, base, a);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            let mut acc = *all;
            for i in 0..*lanes as u64 {
                let truthy = mem_read(pa + i * lb, lw) != 0;
                if *all {
                    acc &= truthy;
                } else {
                    acc |= truthy;
                }
            }
            acc as u64
        }
        Rvalue::Cmp128 { cc, signed, a, b } => {
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let (x, y) = unsafe {
                (
                    (pa as *const u128).read_unaligned(),
                    (pb as *const u128).read_unaligned(),
                )
            };
            let ord = if *signed {
                (x as i128).cmp(&(y as i128))
            } else {
                x.cmp(&y)
            };
            let t = match cc {
                IntCc::Eq => ord.is_eq(),
                IntCc::Ne => ord.is_ne(),
                IntCc::Lt => ord.is_lt(),
                IntCc::Le => ord.is_le(),
                IntCc::Gt => ord.is_gt(),
                IntCc::Ge => ord.is_ge(),
            };
            t as u64
        }
    }
}

fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign { dst, rv } => {
            let v = eval_rvalue(ctx, base, rv);
            place_write(ctx, base, dst, v);
        }
        Stmt::AssignOverflow {
            op,
            signed,
            a,
            b,
            dst_val,
            dst_flag,
        } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let (v, f) = int_ovf(*op, *signed, av, bv, w);
            place_write(ctx, base, dst_val, v);
            place_write(ctx, base, dst_flag, f as u64);
        }
        Stmt::Copy { dst, src, size } => {
            let d = eval_place_addr(ctx, base, dst);
            let s = eval_place_addr(ctx, base, src);
            // memmove 语义（guest 侧重叠是 UB，但引擎自身不因此崩——防御性）
            unsafe { std::ptr::copy(s as *const u8, d as *mut u8, *size as usize) };
        }
        Stmt::RepeatScalar {
            dst,
            val,
            count,
            elem_size,
        } => {
            let d = eval_place_addr(ctx, base, dst);
            let (v, w) = eval_operand(ctx, base, val);
            debug_assert_eq!(w.bytes(), *elem_size);
            for i in 0..*count {
                mem_write(d + i * *elem_size as u64, w, v);
            }
        }
        Stmt::AtomicStore { addr, val } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (v, w) = eval_operand(ctx, base, val);
            unsafe {
                match w {
                    Width::W8 => AtomicU8::from_ptr(p as *mut u8).store(v as u8, Ordering::SeqCst),
                    Width::W16 => {
                        AtomicU16::from_ptr(p as *mut u16).store(v as u16, Ordering::SeqCst)
                    }
                    Width::W32 => {
                        AtomicU32::from_ptr(p as *mut u32).store(v as u32, Ordering::SeqCst)
                    }
                    Width::W64 => AtomicU64::from_ptr(p as *mut u64).store(v, Ordering::SeqCst),
                }
            }
        }
        Stmt::VolatileLoad { addr, dst, size } => {
            let (p, _) = eval_operand(ctx, base, addr);
            let d = eval_place_addr(ctx, base, dst);
            mem_read_volatile(p, d, *size);
        }
        Stmt::VolatileStore { addr, src, size } => {
            let (p, _) = eval_operand(ctx, base, addr);
            let s = eval_place_addr(ctx, base, src);
            mem_write_volatile(p, s, *size);
        }
        Stmt::AtomicCxchg {
            addr,
            expected,
            new,
            dst_val,
            dst_ok,
            weak,
        } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (e, w) = eval_operand(ctx, base, expected);
            let (n, _) = eval_operand(ctx, base, new);
            macro_rules! cx {
                ($t:ty, $at:ty) => {{
                    let a = unsafe { <$at>::from_ptr(p as *mut $t) };
                    let r = if *weak {
                        a.compare_exchange_weak(
                            e as $t,
                            n as $t,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                    } else {
                        a.compare_exchange(e as $t, n as $t, Ordering::SeqCst, Ordering::SeqCst)
                    };
                    match r {
                        Ok(old) => (old as u64, 1u64),
                        Err(old) => (old as u64, 0u64),
                    }
                }};
            }
            let (old, ok) = match w {
                Width::W8 => cx!(u8, AtomicU8),
                Width::W16 => cx!(u16, AtomicU16),
                Width::W32 => cx!(u32, AtomicU32),
                Width::W64 => cx!(u64, AtomicU64),
            };
            place_write(ctx, base, dst_val, old);
            place_write(ctx, base, dst_ok, ok);
        }
        Stmt::AtomicRmw { op, addr, val, dst } => {
            use super::ir::RmwOp as R;
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (v, w) = eval_operand(ctx, base, val);
            macro_rules! rmw {
                ($t:ty, $at:ty, $it:ty, $iat:ty) => {{
                    let a = unsafe { <$at>::from_ptr(p as *mut $t) };
                    (match op {
                        R::Xchg => a.swap(v as $t, Ordering::SeqCst),
                        R::Add => a.fetch_add(v as $t, Ordering::SeqCst),
                        R::Sub => a.fetch_sub(v as $t, Ordering::SeqCst),
                        R::And => a.fetch_and(v as $t, Ordering::SeqCst),
                        R::Or => a.fetch_or(v as $t, Ordering::SeqCst),
                        R::Xor => a.fetch_xor(v as $t, Ordering::SeqCst),
                        R::Nand => a.fetch_nand(v as $t, Ordering::SeqCst),
                        // fetch_max/min：有符号变体经同址 AtomicI*（位型回写零扩展）
                        R::UMax => a.fetch_max(v as $t, Ordering::SeqCst),
                        R::UMin => a.fetch_min(v as $t, Ordering::SeqCst),
                        R::Max => unsafe { <$iat>::from_ptr(p as *mut $it) }
                            .fetch_max(v as $it, Ordering::SeqCst)
                            as $t,
                        R::Min => unsafe { <$iat>::from_ptr(p as *mut $it) }
                            .fetch_min(v as $it, Ordering::SeqCst)
                            as $t,
                    }) as u64
                }};
            }
            let old = match w {
                Width::W8 => rmw!(u8, AtomicU8, i8, AtomicI8),
                Width::W16 => rmw!(u16, AtomicU16, i16, AtomicI16),
                Width::W32 => rmw!(u32, AtomicU32, i32, AtomicI32),
                Width::W64 => rmw!(u64, AtomicU64, i64, AtomicI64),
            };
            place_write(ctx, base, dst, old);
        }
        Stmt::MemCopy {
            dst,
            src,
            count,
            elem_size,
            overlap,
        } => {
            let (d, _) = eval_operand(ctx, base, dst);
            let (s, _) = eval_operand(ctx, base, src);
            let (c, _) = eval_operand(ctx, base, count);
            let bytes = (c as usize).wrapping_mul(*elem_size as usize);
            unsafe {
                if *overlap {
                    std::ptr::copy(s as *const u8, d as *mut u8, bytes);
                } else {
                    std::ptr::copy_nonoverlapping(s as *const u8, d as *mut u8, bytes);
                }
            }
        }
        Stmt::MemSet {
            dst,
            val,
            count,
            elem_size,
        } => {
            let (d, _) = eval_operand(ctx, base, dst);
            let (v, _) = eval_operand(ctx, base, val);
            let (c, _) = eval_operand(ctx, base, count);
            let bytes = (c as usize).wrapping_mul(*elem_size as usize);
            unsafe { std::ptr::write_bytes(d as *mut u8, v as u8, bytes) };
        }
        Stmt::SimdBin {
            op,
            dst,
            a,
            b,
            lanes,
            lane_bytes,
        } => {
            use super::ir::SimdBinOp as S;
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw);
                let y = mem_read(pb + i * lb, lw);
                let r = match op {
                    S::Eq => (int_cmp(IntCc::Eq, false, x, y, lw) != 0).then_some(lw.mask()),
                    S::Ne => (int_cmp(IntCc::Ne, false, x, y, lw) != 0).then_some(lw.mask()),
                    S::Lt { signed } => {
                        (int_cmp(IntCc::Lt, *signed, x, y, lw) != 0).then_some(lw.mask())
                    }
                    S::Le { signed } => {
                        (int_cmp(IntCc::Le, *signed, x, y, lw) != 0).then_some(lw.mask())
                    }
                    S::Gt { signed } => {
                        (int_cmp(IntCc::Gt, *signed, x, y, lw) != 0).then_some(lw.mask())
                    }
                    S::Ge { signed } => {
                        (int_cmp(IntCc::Ge, *signed, x, y, lw) != 0).then_some(lw.mask())
                    }
                    S::And => Some(x & y),
                    S::Or => Some(x | y),
                    S::Xor => Some(x ^ y),
                    S::Add => Some(x.wrapping_add(y) & lw.mask()),
                    S::Sub => Some(x.wrapping_sub(y) & lw.mask()),
                    S::Shl => {
                        if y >= u64::from(lw.bytes() * 8) {
                            engine_abort("simd_shl shift count 超过 lane 位宽（guest UB）");
                        }
                        Some(int_bin(IntBinOp::Shl, false, x, y, lw))
                    }
                    S::Shr { signed } => {
                        if y >= u64::from(lw.bytes() * 8) {
                            engine_abort("simd_shr shift count 超过 lane 位宽（guest UB）");
                        }
                        Some(int_bin(IntBinOp::Shr, *signed, x, y, lw))
                    }
                };
                mem_write(pd + i * lb, lw, r.unwrap_or(0));
            }
        }
        Stmt::SimdSplat {
            dst,
            val,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let (v, _) = eval_operand(ctx, base, val);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                mem_write(pd + i * lb, lw, v);
            }
        }
        Stmt::Bin128 {
            op,
            signed,
            a,
            b,
            dst,
            with_overflow,
        } => {
            use super::ir::Bin128Rhs;
            let pa = eval_place_addr(ctx, base, a);
            let x = unsafe { (pa as *const u128).read_unaligned() };
            let y = match b {
                Bin128Rhs::Wide(pb) => {
                    let pb = eval_place_addr(ctx, base, pb);
                    unsafe { (pb as *const u128).read_unaligned() }
                }
                Bin128Rhs::Scalar(o) => eval_operand(ctx, base, o).0 as u128,
            };
            let pd = eval_place_addr(ctx, base, dst);
            let (r, ovf): (u128, bool) = if *signed {
                let (xs, ys) = (x as i128, y as i128);
                let (v, o) = match op {
                    IntBinOp::Add => xs.overflowing_add(ys),
                    IntBinOp::Sub => xs.overflowing_sub(ys),
                    IntBinOp::Mul => xs.overflowing_mul(ys),
                    IntBinOp::Div => {
                        if ys == 0 {
                            engine_abort("guest 128 位整除以零");
                        }
                        (xs.wrapping_div(ys), false)
                    }
                    IntBinOp::Rem => {
                        if ys == 0 {
                            engine_abort("guest 128 位取余以零");
                        }
                        (xs.wrapping_rem(ys), false)
                    }
                    IntBinOp::BitAnd => (xs & ys, false),
                    IntBinOp::BitOr => (xs | ys, false),
                    IntBinOp::BitXor => (xs ^ ys, false),
                    IntBinOp::Shl => (xs.wrapping_shl(y as u32), false),
                    IntBinOp::Shr => (xs.wrapping_shr(y as u32), false),
                };
                (v as u128, o)
            } else {
                let (v, o) = match op {
                    IntBinOp::Add => x.overflowing_add(y),
                    IntBinOp::Sub => x.overflowing_sub(y),
                    IntBinOp::Mul => x.overflowing_mul(y),
                    IntBinOp::Div => {
                        if y == 0 {
                            engine_abort("guest 128 位整除以零");
                        }
                        (x / y, false)
                    }
                    IntBinOp::Rem => {
                        if y == 0 {
                            engine_abort("guest 128 位取余以零");
                        }
                        (x % y, false)
                    }
                    IntBinOp::BitAnd => (x & y, false),
                    IntBinOp::BitOr => (x | y, false),
                    IntBinOp::BitXor => (x ^ y, false),
                    IntBinOp::Shl => (x.wrapping_shl(y as u32), false),
                    IntBinOp::Shr => (x.wrapping_shr(y as u32), false),
                };
                (v, o)
            };
            unsafe { (pd as *mut u128).write_unaligned(r) };
            if *with_overflow {
                unsafe { *((pd + 16) as *mut u8) = ovf as u8 };
            }
        }
        Stmt::NicheDiscr128 {
            tag,
            niche_start,
            variants_start,
            variants_len,
            untagged,
            dst,
        } => {
            let p = eval_place_addr(ctx, base, tag);
            let t = unsafe { (p as *const u128).read_unaligned() };
            let rel = t.wrapping_sub(*niche_start);
            let v = if rel < *variants_len as u128 {
                variants_start.wrapping_add(rel as u64)
            } else {
                *untagged
            };
            place_write(ctx, base, dst, v);
        }
        Stmt::Wide128ToFloat {
            src,
            signed,
            to64,
            dst,
        } => {
            let p = eval_place_addr(ctx, base, src);
            let x = unsafe { (p as *const u128).read_unaligned() };
            let bits = if *to64 {
                (if *signed { x as i128 as f64 } else { x as f64 }).to_bits()
            } else {
                (if *signed { x as i128 as f32 } else { x as f32 }).to_bits() as u64
            };
            place_write(ctx, base, dst, bits);
        }
        Stmt::Trap(reason) => engine_abort(&format!("TRAP: {reason}")),
        Stmt::Nop => {}
        // `[expr; N]` 聚合元素：dst[0] 为模板铺满其余
        Stmt::RepeatBytes {
            first,
            count,
            elem_size,
        } => {
            let src = eval_place_addr(ctx, base, first);
            for i in 1..*count {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src as *const u8,
                        (src + i * elem_size) as *mut u8,
                        *elem_size as usize,
                    );
                }
            }
        }
        // 栅栏补真（M4.4 D4）：guest 任意序 → 宿主 SeqCst（最强序在 RAM non-det 包络内）
        Stmt::Fence { single_thread } => {
            use std::sync::atomic::{Ordering, compiler_fence, fence};
            if *single_thread {
                compiler_fence(Ordering::SeqCst);
            } else {
                fence(Ordering::SeqCst);
            }
        }
    }
}

/// 解释帧的 landing pad（spike3 CleanupGuard 的 M4 版）：unwind 穿帧时 Drop 在展开中
/// 执行——跑 cleanup 链（若 unwind_edge 有值）→ 恢复操作数区。正常 Return 也经 guard
/// drop 统一恢复（此时 edge 必为 None）。
struct FrameGuard {
    ctx: *mut Ctx,
    func: u32,
    base: usize,
    /// 动态 LSDA：当前可 unwind 终止子的 cleanup 边（Call 前设置、返回后清除）
    unwind_edge: Cell<Option<Bb>>,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if let Some(blk) = self.unwind_edge.get() {
            run_cleanup(self.ctx, self.func, self.base, blk);
        }
        region_restore(self.ctx, self.base);
        unsafe { (*self.ctx).depth -= 1 };
    }
}

/// 块序列执行的出口。
enum Exit {
    Ret(u64, u64),
    /// cleanup 链尾（Resume）：返回 guard.drop，宿主 unwind 自动继续
    Resume,
}

/// 按 D4 fn 条目真地址派发（CallIndirect / catch_unwind 的 try/catch fn 共用）。
fn call_fn_addr(ctx: *mut Ctx, addr: u64, args: &[u64], caller: &str) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let Some(&fid) = module.fn_addrs.get(&addr) else {
        engine_abort(&format!(
            "间接调用目标 {addr:#x} 不是已知 fn 条目（调用者 {caller}）"
        ));
    };
    interp_frame(ctx, fid, args)
}

/// unwind 边 → cleanup 目标块。
#[inline]
fn cleanup_edge(u: &UnwindAction) -> Option<Bb> {
    match u {
        UnwindAction::Cleanup(b) => Some(*b),
        _ => None,
    }
}

/// Terminate 边界的调用包装：panic 到此即 abort（double panic / extern "C" ABI 边界）。
#[inline]
fn call_guarding_terminate<R>(unwind: &UnwindAction, f: impl FnOnce() -> R) -> R {
    if let UnwindAction::Terminate = unwind {
        match panic::catch_unwind(AssertUnwindSafe(f)) {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "mirvm[m4-engine]: unwind 抵达 Terminate 边界（double panic/ABI）——abort"
                );
                std::process::abort()
            }
        }
    } else {
        f()
    }
}

/// guard.drop 里的 cleanup 链执行（landing pad 的宿主 Rust 写法）：从 cleanup 块跑到
/// `Resume`。链中 Call 可再入混合执行；链中再 panic：Terminate 边 abort，Continue 边
/// 穿出 Drop = 宿主 double-panic abort（与 native 一致）。
fn run_cleanup(ctx: *mut Ctx, func: u32, base: usize, entry: Bb) {
    // cleanup 内无嵌套 cleanup（MIR 不变量）——独立哑 edge
    let edge = Cell::new(None);
    match run_blocks(ctx, func, base, &edge, entry) {
        Exit::Resume => {} // 返回 guard，unwind 自动继续
        Exit::Ret(..) => engine_abort("cleanup 链以 Return 结束（MIR 不变量破坏）"),
    }
}

/// 模型 A：guest 调用 = 宿主递归（spike1/3 验证的形状）。
/// 调用约定 v2：实参展平 `&[u64]`（pair 占 2 槽、indirect 传地址），返回 (lo, hi)。
/// guest 递归深度上限（≈ native 栈界近似，frame-abi §9）。每解释帧背 ~1KB 宿主帧，
/// rustc 驱动线程栈 ~16MB → 8000 帧安全余量内（M5 编译帧更浅后可调大）。
const MAX_DEPTH: u32 = 8_000;

pub(super) fn interp_frame(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    let depth = unsafe {
        (*ctx).depth += 1;
        (*ctx).depth
    };
    if depth > MAX_DEPTH {
        engine_abort(&format!(
            "guest 栈溢出（解释帧深度 > {MAX_DEPTH}；fn {}）",
            body.name
        ));
    }

    let base = region_reserve(ctx, body.frame_size, body.frame_align);
    // prologue：按 ParamAbi 消费实参槽（槽数先验——不匹配给名字与期望，勿裸越界 panic）
    let needed: usize = matches!(body.ret, RetAbi::Indirect { .. }) as usize
        + body
            .params
            .iter()
            .map(|p| match p {
                ParamAbi::Zst => 0,
                ParamAbi::Scalar(_) | ParamAbi::Indirect { .. } => 1,
                ParamAbi::Pair(..) => 2,
            })
            .sum::<usize>()
        + body.caller_loc_off.is_some() as usize;
    if args.len() < needed {
        engine_abort(&format!(
            "ABI 不匹配：fn `{}` 期望 {needed} 实参槽（params {:?} ret {:?} loc {:?}），收到 {}",
            body.name,
            body.params,
            body.ret,
            body.caller_loc_off,
            args.len()
        ));
    }
    let mut ai = 0usize;
    // Indirect 返回：隐藏首实参 = 目的真地址，存入 sret 槽
    if let RetAbi::Indirect { sret_off, .. } = body.ret {
        slot_write(
            ctx,
            base,
            Slot {
                off: sret_off,
                width: Width::W64,
            },
            args[ai],
        );
        ai += 1;
    }
    for p in &body.params {
        match p {
            ParamAbi::Zst => {}
            ParamAbi::Scalar(s) => {
                slot_write(ctx, base, *s, args[ai]);
                ai += 1;
            }
            ParamAbi::Pair(lo, hi) => {
                slot_write(ctx, base, *lo, args[ai]);
                slot_write(ctx, base, *hi, args[ai + 1]);
                ai += 2;
            }
            ParamAbi::Indirect { off, size } => {
                let src = args[ai];
                ai += 1;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        src as *const u8,
                        (base as u64 + *off as u64) as *mut u8,
                        *size as usize,
                    )
                };
            }
        }
    }
    // #[track_caller]：&Location 隐藏尾实参
    if let Some(off) = body.caller_loc_off {
        let Some(&loc) = args.get(ai) else {
            engine_abort(&format!(
                "ABI 不匹配：track_caller fn `{}` 期望 location 尾实参（收到 {} 槽）",
                body.name,
                args.len()
            ));
        };
        slot_write(
            ctx,
            base,
            Slot {
                off,
                width: Width::W64,
            },
            loc,
        );
    }

    // 帧守卫：unwind 穿帧 = 跑 cleanup + 恢复区；正常返回 = 恢复区（edge 已空）
    let guard = FrameGuard {
        ctx,
        func,
        base,
        unwind_edge: Cell::new(None),
    };
    match run_blocks(ctx, func, base, &guard.unwind_edge, 0) {
        Exit::Ret(lo, hi) => (lo, hi), // guard drop → region 恢复
        Exit::Resume => engine_abort(&format!("Resume 出现在正常执行路径（fn {}）", body.name)),
    }
}

/// 块序列解释循环（主执行与 cleanup 链共用；spike3 的 unwind 化演进）。
fn run_blocks(ctx: *mut Ctx, func: u32, base: usize, edge: &Cell<Option<Bb>>, entry: Bb) -> Exit {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    let mut blk = entry as usize;
    loop {
        let block: &Block = &body.blocks[blk];
        for stmt in &block.stmts {
            exec_stmt(ctx, base, stmt);
        }
        match &block.term {
            Terminator::Goto(t) => blk = *t as usize,
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                let d = match discr {
                    SwitchDiscr::Scalar(discr) => eval_operand(ctx, base, discr).0 as u128,
                    SwitchDiscr::Wide(discr) => {
                        let addr = eval_place_addr(ctx, base, discr);
                        unsafe { (addr as *const u128).read_unaligned() }
                    }
                };
                blk = targets
                    .iter()
                    .find(|(v, _)| *v == d)
                    .map(|(_, b)| *b)
                    .unwrap_or(*otherwise) as usize;
            }
            Terminator::Call {
                callee,
                args: aops,
                ret,
                target,
                unwind,
            } => {
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                edge.set(cleanup_edge(unwind)); // callee 若 panic，本帧从这条边清理
                let (lo, hi) = call_guarding_terminate(unwind, || interp_frame(ctx, *callee, &av)); // ← 宿主递归
                edge.set(None);
                match ret {
                    RetDest::Ignore | RetDest::Indirect(_) => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, lo),
                    RetDest::Pair(pl, ph) => {
                        place_write(ctx, base, pl, lo);
                        place_write(ctx, base, ph, hi);
                    }
                }
                blk = *target as usize;
            }
            Terminator::CallForeign {
                sym,
                sig,
                args: aops,
                ret,
                target,
                unwind,
            } => {
                let mut av: Vec<u64> = aops.iter().map(|o| eval_operand(ctx, base, o).0).collect();
                // M4.4 D1：fn-ptr 实参位——guest fn 条目地址逃逸给 native 前物化 thunk
                // 真码；NULL 与已是 native 真码（反查未命中，guest 转传）原样直传。
                for (pos, inner) in &sig.thunk_args {
                    let v = av[*pos];
                    if v != 0
                        && let Some(&fid) = module.fn_addrs.get(&v)
                    {
                        let shared: &'static Shared = unsafe { &*(*ctx).shared };
                        av[*pos] = super::thunks::get_or_create(shared, v, fid, inner);
                    }
                }
                edge.set(cleanup_edge(unwind));
                let optional_libs: &[Box<str>] = &module.native_libs;
                let required_libs: &[Box<str>] = &module.required_native_libs;
                let r = {
                    let ffi = unsafe { &mut (*ctx).ffi };
                    super::ffi::call(ffi, optional_libs, required_libs, sym, sig, &av)
                };
                edge.set(None);
                let r = r.unwrap_or_else(|reason| {
                    engine_abort(&format!(
                        "foreign `{sym}` 的必需原生库装载失败（fn {}）: {reason}",
                        body.name
                    ))
                });
                let Some(r) = r else {
                    engine_abort(&format!(
                        "foreign `{sym}` 符号不存在（dlsym 全域未命中；fn {}）",
                        body.name
                    ));
                };
                match ret {
                    RetDest::Ignore => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, r),
                    other => engine_abort(&format!("foreign 返回形态 {other:?} 未支持")),
                }
                blk = *target as usize;
            }
            Terminator::CallIndirect {
                callee,
                args: aops,
                ret,
                target,
                unwind,
                null_ok,
                native_sig,
            } => {
                let (addr, _) = eval_operand(ctx, base, callee);
                if *null_ok && addr == 0 {
                    // dyn 虚 drop 空槽：无 Drop 的类型 = 空操作
                    blk = *target as usize;
                    continue;
                }
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                edge.set(cleanup_edge(unwind));
                let (lo, hi) = if let Some(&fid) = module.fn_addrs.get(&addr) {
                    call_guarding_terminate(unwind, || interp_frame(ctx, fid, &av))
                } else if let Some(nsig) = native_sig {
                    // FFI 反方向之二（M4.4）：guest 持 native 真码 fn ptr（运行期
                    // dlsym 所得，如 __pthread_get_minstack）→ 按冻结签名直调
                    (super::ffi::call_addr(addr as usize, nsig, &av), 0)
                } else {
                    engine_abort(&format!(
                        "间接调用目标 {addr:#x} 不是已知 fn 条目（调用者 {}）",
                        body.name
                    ));
                };
                edge.set(None);
                match ret {
                    RetDest::Ignore | RetDest::Indirect(_) => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, lo),
                    RetDest::Pair(pl, ph) => {
                        place_write(ctx, base, pl, lo);
                        place_write(ctx, base, ph, hi);
                    }
                }
                blk = *target as usize;
            }
            Terminator::CallBuiltin {
                builtin,
                args,
                ret,
                target,
                unwind,
            } => {
                use super::ir::Builtin;
                let a = |i: usize| eval_operand(ctx, base, &args[i]).0;
                edge.set(cleanup_edge(unwind)); // RaiseException 经此发起 unwind
                // x86 向量 intrinsic：参数是 indirect 向量地址，返回落到 sret place。
                // helper 本身带 target_feature，guest 的正常 CPUID 派发负责可达性。
                let vector_done = match builtin {
                    Builtin::X86Pshufb128 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pshufb128 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { super::x86::pshufb128(dst, a(0) as *const u8, a(1) as *const u8) };
                        true
                    }
                    Builtin::X86Pshufb256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pshufb256 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { super::x86::pshufb256(dst, a(0) as *const u8, a(1) as *const u8) };
                        true
                    }
                    Builtin::X86Sha256Msg1 | Builtin::X86Sha256Msg2 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("sha256msg 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86Sha256Msg1) {
                                super::x86::sha256msg1(dst, a(0) as *const u8, a(1) as *const u8);
                            } else {
                                super::x86::sha256msg2(dst, a(0) as *const u8, a(1) as *const u8);
                            }
                        }
                        true
                    }
                    Builtin::X86Sha256Rnds2 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("sha256rnds2 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            super::x86::sha256rnds2(
                                dst,
                                a(0) as *const u8,
                                a(1) as *const u8,
                                a(2) as *const u8,
                            );
                        }
                        true
                    }
                    _ => false,
                };
                if vector_done {
                    edge.set(None);
                    blk = *target as usize;
                    continue;
                }
                // LLVM 的 addcarry/subborrow 返回 `(flag, result)` ScalarPair，而其他
                // 现有 builtin 都是单标量。先走 pair 专用道，保持字段顺序与冻结 ABI 一致。
                let carry_result = match builtin {
                    Builtin::AddCarry64 => {
                        let carry_in = u64::from(a(0) != 0);
                        let (partial, carry1) = a(1).overflowing_add(a(2));
                        let (result, carry2) = partial.overflowing_add(carry_in);
                        Some(("addcarry.64", carry1 || carry2, result))
                    }
                    Builtin::SubBorrow64 => {
                        let borrow_in = u64::from(a(0) != 0);
                        let (partial, borrow1) = a(1).overflowing_sub(a(2));
                        let (result, borrow2) = partial.overflowing_sub(borrow_in);
                        Some(("subborrow.64", borrow1 || borrow2, result))
                    }
                    _ => None,
                };
                if let Some((name, flag, result)) = carry_result {
                    edge.set(None);
                    match ret {
                        RetDest::Pair(flag_dst, value) => {
                            place_write(ctx, base, flag_dst, u64::from(flag));
                            place_write(ctx, base, value, result);
                        }
                        other => {
                            engine_abort(&format!("{name} 返回形态 {other:?}，期望 ScalarPair"))
                        }
                    }
                    blk = *target as usize;
                    continue;
                }
                let r = match builtin {
                    // 分配前哨兵：空操作
                    Builtin::NoAllocShim => 0,
                    // 托管 Rust Heap（D3：mimalloc 后端，真地址直出）
                    Builtin::RustAlloc => super::heap::alloc(a(0), a(1)),
                    Builtin::RustAllocZeroed => super::heap::alloc_zeroed(a(0), a(1)),
                    Builtin::RustDealloc => {
                        super::heap::dealloc(a(0), a(1), a(2));
                        0
                    }
                    Builtin::RustRealloc => super::heap::realloc(a(0), a(1), a(2), a(3)),
                    // unwind 原语（spike3 的 raise）：宿主 unwinder 载 guest exception 指针
                    Builtin::UnwindRaise => raise_guest(a(0)),
                    // os:: 最小直通（真实地址零编组；M4.3 正式注册表）
                    Builtin::HostGetenv => unsafe {
                        libc::getenv(a(0) as *const libc::c_char) as u64
                    },
                    Builtin::HostWrite => unsafe {
                        libc::write(a(0) as i32, a(1) as *const libc::c_void, a(2) as usize) as u64
                    },
                    Builtin::HostStrlen => unsafe {
                        libc::strlen(a(0) as *const libc::c_char) as u64
                    },
                    Builtin::HostAbort => {
                        eprintln!("mirvm[m4-engine]: guest abort()");
                        std::process::abort()
                    }
                    Builtin::HostSignal => {
                        let (signum, handler) = (a(0) as libc::c_int, a(1) as libc::sighandler_t);
                        if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
                            engine_abort("unsupported builtin `signal` with guest handler");
                        }
                        unsafe { libc::signal(signum, handler) as u64 }
                    }
                    Builtin::HostSigaction => {
                        let (signum, act, oldact) = (a(0) as libc::c_int, a(1), a(2));
                        if act != 0 {
                            let handler =
                                unsafe { (*(act as *const libc::sigaction)).sa_sigaction };
                            if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
                                engine_abort("unsupported builtin `sigaction` with guest handler");
                            }
                        }
                        unsafe {
                            libc::sigaction(
                                signum,
                                act as *const libc::sigaction,
                                oldact as *mut libc::sigaction,
                            ) as u64
                        }
                    }
                    Builtin::Unsupported(name) => {
                        engine_abort(&format!("unsupported builtin `{name}`"))
                    }
                    Builtin::UnwindDeleteException => {
                        // Itanium `_Unwind_Exception`：exception_class @0，cleanup fn @8。
                        // guest panic 的 cleanup 是冻结 fn 条目；foreign exception 也可能
                        // 带 native cleanup，因此按地址域选择解释调用或 native FFI。
                        let exc = a(0);
                        let cleanup = mem_read(exc + 8, Width::W64);
                        if cleanup != 0 {
                            let av = [1, exc]; // _URC_FOREIGN_EXCEPTION_CAUGHT
                            if module.fn_addrs.contains_key(&cleanup) {
                                call_fn_addr(ctx, cleanup, &av, "_Unwind_DeleteException");
                            } else {
                                let sig = super::ir::ForeignSig {
                                    args: vec![super::ir::FfiKind::I32, super::ir::FfiKind::Ptr],
                                    ret: super::ir::FfiKind::Void,
                                    fixed: None,
                                    thunk_args: vec![],
                                };
                                super::ffi::call_addr(cleanup as usize, &sig, &av);
                            }
                        }
                        0
                    }
                    Builtin::CpuHintNop => 0,
                    Builtin::Breakpoint => {
                        // 真 int3：未被跟踪时 = SIGTRAP 终止（native 同语义）
                        unsafe {
                            std::arch::asm!("int3", options(nomem, nostack, preserves_flags))
                        };
                        0
                    }
                    Builtin::AddCarry64 => unreachable!("addcarry.64 已由 pair 通道处理"),
                    Builtin::SubBorrow64 => unreachable!("subborrow.64 已由 pair 通道处理"),
                    Builtin::Xgetbv => {
                        let xcr = a(0) as u32;
                        let (eax, edx): (u32, u32);
                        unsafe {
                            std::arch::asm!(
                                "xgetbv",
                                in("ecx") xcr,
                                out("eax") eax,
                                out("edx") edx,
                                options(nomem, nostack, preserves_flags),
                            );
                        }
                        (u64::from(edx) << 32) | u64::from(eax)
                    }
                    Builtin::X86Pshufb128
                    | Builtin::X86Pshufb256
                    | Builtin::X86Sha256Msg1
                    | Builtin::X86Sha256Msg2
                    | Builtin::X86Sha256Rnds2 => {
                        unreachable!("x86 vector builtin 已由 indirect vector 通道处理")
                    }
                    Builtin::HostSyscall => unsafe {
                        let n = a(0) as i64;
                        (match args.len() {
                            1 => libc::syscall(n),
                            2 => libc::syscall(n, a(1)),
                            3 => libc::syscall(n, a(1), a(2)),
                            4 => libc::syscall(n, a(1), a(2), a(3)),
                            5 => libc::syscall(n, a(1), a(2), a(3), a(4)),
                            6 => libc::syscall(n, a(1), a(2), a(3), a(4), a(5)),
                            _ => libc::syscall(n, a(1), a(2), a(3), a(4), a(5), a(6)),
                        }) as u64
                    },
                    // rust_try：宿主 catch；guest panic → 调 catch_fn(data, exc) 返 1
                    Builtin::CatchUnwind => {
                        let (try_fn, data, catch_fn) = (a(0), a(1), a(2));
                        match panic::catch_unwind(AssertUnwindSafe(|| {
                            call_fn_addr(ctx, try_fn, &[data], "catch_unwind.try")
                        })) {
                            Ok(_) => 0,
                            Err(e) => match e.downcast::<GuestPanic>() {
                                Ok(gp) => {
                                    call_fn_addr(
                                        ctx,
                                        catch_fn,
                                        &[data, gp.exception],
                                        "catch_unwind.catch",
                                    );
                                    1
                                }
                                // 宿主 panic（VM bug）不是 guest 异常：原样续传
                                Err(host) => panic::resume_unwind(host),
                            },
                        }
                    }
                };
                edge.set(None);
                match ret {
                    RetDest::Scalar(p) => place_write(ctx, base, p, r),
                    RetDest::Ignore => {}
                    other => engine_abort(&format!("引擎原语返回形态 {other:?} 未支持")),
                }
                blk = *target as usize;
            }
            Terminator::InlineAsm {
                stub,
                buf_size,
                ins,
                outs,
                target,
            } => {
                // asm-stub（M5.0 corpus §2.2 三面孔）：栈开 buf、按 ins 装槽、call
                // wrapper（fn(*mut u8)，rbx=buf 基址）、按 outs 取槽。三面孔无 unwind。
                #[repr(align(16))]
                struct AsmBuf([u8; 256]);
                let mut buf = AsmBuf([0u8; 256]);
                if *buf_size as usize > buf.0.len() {
                    engine_abort(&format!(
                        "asm 缓冲 {buf_size} 超上限 {}（fn {}）",
                        buf.0.len(),
                        body.name
                    ));
                }
                let bufp = buf.0.as_mut_ptr();
                for (off, op) in ins {
                    let (v, _) = eval_operand(ctx, base, op);
                    unsafe { std::ptr::write_unaligned(bufp.add(*off as usize) as *mut u64, v) };
                }
                let addr = module.asm_stub_addrs[*stub as usize];
                let f: unsafe extern "C" fn(*mut u8) =
                    unsafe { std::mem::transmute::<u64, unsafe extern "C" fn(*mut u8)>(addr) };
                unsafe { f(bufp) };
                for (off, dst) in outs {
                    let v =
                        unsafe { std::ptr::read_unaligned(bufp.add(*off as usize) as *const u64) };
                    place_write(ctx, base, dst, v);
                }
                blk = *target as usize;
            }
            Terminator::Return => {
                let r = match body.ret {
                    RetAbi::Zst => (0, 0),
                    RetAbi::Scalar(rs) => (slot_read(ctx, base, rs), 0),
                    RetAbi::Pair(lo, hi) => (slot_read(ctx, base, lo), slot_read(ctx, base, hi)),
                    RetAbi::Indirect {
                        ret_off,
                        size,
                        sret_off,
                    } => {
                        let dst = slot_read(
                            ctx,
                            base,
                            Slot {
                                off: sret_off,
                                width: Width::W64,
                            },
                        );
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                (base as u64 + ret_off as u64) as *const u8,
                                dst as *mut u8,
                                size as usize,
                            )
                        };
                        (0, 0)
                    }
                };
                // region 恢复由 FrameGuard 统一（正常/unwind 两路径一致）
                return Exit::Ret(r.0, r.1);
            }
            Terminator::Resume => return Exit::Resume,
            Terminator::TerminateAbort => {
                eprintln!("mirvm[m4-engine]: UnwindTerminate（double panic/ABI 边界）——abort");
                std::process::abort()
            }
            Terminator::Unreachable => {
                engine_abort(&format!("到达 Unreachable（fn {}）", body.name))
            }
            Terminator::Trap(reason) => {
                engine_abort(&format!("TRAP: {reason}（fn {}）", body.name))
            }
        }
    }
}

/// main 启动链（M4.3）：`lang_start(main fn-ptr, argc, argv, sigpipe) -> isize`
/// （cg_ssa create_entry_fn 同构——std 的 rt::lang_start 照常解释：sys::init/
/// args 存放/panic hook/Termination 全走 guest 代码，忠实性）。返回进程退出码。
pub fn run_main(shared: &'static Shared) -> i32 {
    let Some(entry) = shared.module.entry else {
        eprintln!("mirvm[m4-engine]: 无 main 入口（lib crate？）");
        return 2;
    };
    let ctx_ptr = super::ctx::attach(shared); // 主线程与 guest 线程同一 attach 形态
    let args = [
        entry.main_addr,
        entry.argc,
        entry.argv_ptr,
        entry.sigpipe as u64,
    ];
    match panic::catch_unwind(AssertUnwindSafe(|| {
        interp_frame(ctx_ptr, entry.lang_start, &args).0
    })) {
        Ok(code) => code as i32,
        Err(e) => match e.downcast::<GuestPanic>() {
            // lang_start 内部已 catch guest panic；穿到这 = panic 逃逸启动链（防御）
            Ok(_) => 101,
            Err(host) => panic::resume_unwind(host),
        },
    }
}

/// dev 入口（M4.0 gate）：按导出名调一个函数。
/// 顶层 catch：guest panic 穿出导出函数 = 未捕获 panic → 诊断 + 退出码 101
/// （native lang_start 语义的近似；完整启动链 M4.3）。宿主 panic（VM bug）原样续传。
pub fn run_export(shared: &'static Shared, name: &str, args: &[u64]) -> Result<u64, String> {
    let Some(&id) = shared.module.exports.get(name) else {
        let mut names: Vec<&str> = shared.module.exports.keys().map(|k| &**k).collect();
        names.sort();
        names.retain(|n| !n.starts_with("_ZN") && !n.starts_with("_R"));
        return Err(format!("导出函数 `{name}` 不存在；可用: {names:?}"));
    };
    let ctx_ptr = super::ctx::attach(shared);
    match panic::catch_unwind(AssertUnwindSafe(|| interp_frame(ctx_ptr, id, args).0)) {
        Ok(r) => Ok(r),
        Err(e) => match e.downcast::<GuestPanic>() {
            Ok(_) => {
                eprintln!("mirvm[m4-engine]: guest panic 未被捕获（== native 退出码 101）");
                exit(101)
            }
            Err(host) => panic::resume_unwind(host), // VM bug 绝不吞
        },
    }
}

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;

    use super::{eval_place_addr, mem_read_volatile, mem_write_volatile};
    use crate::vm::engine::ir::{Operand, PlaceBase, PlaceExpr, PlaceStep, Width};

    #[test]
    fn dyn_tail_alignment_preserves_prefixes_larger_than_four_gibibytes() {
        let vtable = [0u64, 0, 32];
        let unaligned = u32::MAX as u64 + 18;
        let expr = PlaceExpr {
            base: PlaceBase::Static(0x1000),
            steps: vec![PlaceStep::VTableAlignOffset {
                meta: Operand::Imm {
                    bits: vtable.as_ptr() as u64,
                    width: Width::W64,
                },
                unaligned,
                packed: None,
            }]
            .into_boxed_slice(),
        };
        let expected_offset = (unaligned + 31) & !31;
        assert_eq!(
            eval_place_addr(std::ptr::null_mut(), 0, &expr),
            0x1000 + expected_offset
        );
    }

    #[test]
    fn volatile_scalar_roundtrip_preserves_each_width() {
        for (size, value) in [
            (1, 0xa5),
            (2, 0xb6a5),
            (4, 0xd8c7_b6a5),
            (8, 0xf0e9_d8c7_b6a5_9483),
        ] {
            let mut storage = [0u8; 8];
            let mut got = 0u64;
            mem_write_volatile(
                storage.as_mut_ptr() as u64,
                (&value as *const u64) as u64,
                size,
            );
            mem_read_volatile(storage.as_ptr() as u64, (&mut got as *mut u64) as u64, size);
            let mask = if size == 8 {
                u64::MAX
            } else {
                (1u64 << (size * 8)) - 1
            };
            assert_eq!(got, value & mask);
        }
    }

    #[test]
    fn volatile_unaligned_roundtrip_does_not_require_host_alignment() {
        let mut storage = [0u8; 16];
        let addr = unsafe { storage.as_mut_ptr().add(1) } as u64;
        let value = 0xf0e9_d8c7_b6a5_9483;
        let mut got = 0u64;
        mem_write_volatile(addr, (&value as *const u64) as u64, 8);
        mem_read_volatile(addr, (&mut got as *mut u64) as u64, 8);
        assert_eq!(got, value);
    }

    #[test]
    fn volatile_16_byte_roundtrip_preserves_the_whole_value() {
        let mut storage = [0u8; 16];
        let value = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ];
        let mut got = [0u8; 16];
        mem_write_volatile(storage.as_mut_ptr() as u64, value.as_ptr() as u64, 16);
        mem_read_volatile(storage.as_ptr() as u64, got.as_mut_ptr() as u64, 16);
        assert_eq!(got, value);
    }

    #[test]
    fn volatile_16_byte_value_may_have_alignment_one() {
        let mut storage = [0u8; 24];
        let base = storage.as_mut_ptr() as usize;
        let offset = (9 - base % 8) % 8;
        let addr = unsafe { storage.as_mut_ptr().add(offset) } as u64;
        assert_eq!(addr % 8, 1);
        let value = [0xa5u8; 16];
        let mut got = [0u8; 16];
        mem_write_volatile(addr, value.as_ptr() as u64, 16);
        mem_read_volatile(addr, got.as_mut_ptr() as u64, 16);
        assert_eq!(got, value);
    }

    #[test]
    fn volatile_wide_store_preserves_every_byte() {
        let source: Vec<u8> = (0..137)
            .map(|index| (index as u8).wrapping_mul(17))
            .collect();
        let mut storage = vec![0u8; source.len() + 1];
        mem_write_volatile(
            unsafe { storage.as_mut_ptr().add(1) } as u64,
            source.as_ptr() as u64,
            source.len() as u32,
        );
        assert_eq!(&storage[1..], source.as_slice());
    }

    #[test]
    fn volatile_unaligned_31_byte_roundtrip_covers_every_chunk_width() {
        let source: Vec<u8> = (0..31)
            .map(|index| (index as u8).wrapping_mul(29))
            .collect();
        let mut storage = [0u8; 33];
        let mut got = [0u8; 31];
        let unaligned = unsafe { storage.as_mut_ptr().add(1) };
        mem_write_volatile(unaligned as u64, source.as_ptr() as u64, 31);
        mem_read_volatile(unaligned as u64, got.as_mut_ptr() as u64, 31);
        assert_eq!(got.as_slice(), source.as_slice());
    }

    #[test]
    fn volatile_wide_load_snapshots_before_overlapping_destination() {
        let mut storage: Vec<u8> = (0..160).map(|index| index as u8).collect();
        let expected = storage[..137].to_vec();
        let base = storage.as_mut_ptr();
        mem_read_volatile(base as u64, unsafe { base.add(7) } as u64, 137);
        assert_eq!(&storage[7..144], expected.as_slice());
    }

    #[test]
    fn volatile_wide_store_snapshots_before_overlapping_destination() {
        let mut storage: Vec<u8> = (0..160).map(|index| (index as u8) ^ 0xa5).collect();
        let expected = storage[..137].to_vec();
        let base = storage.as_mut_ptr();
        mem_write_volatile(unsafe { base.add(7) } as u64, base as u64, 137);
        assert_eq!(&storage[7..144], expected.as_slice());
    }

    #[test]
    fn volatile_padded_aggregate_never_interprets_padding_as_an_integer() {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Padded {
            tag: u8,
            value: u32,
        }

        let value = Padded {
            tag: 0xa5,
            value: 0x1234_5678,
        };
        let mut storage = MaybeUninit::<Padded>::uninit();
        let mut got = MaybeUninit::<Padded>::uninit();
        mem_write_volatile(
            storage.as_mut_ptr() as u64,
            (&value as *const Padded) as u64,
            size_of::<Padded>() as u32,
        );
        mem_read_volatile(
            storage.as_ptr() as u64,
            got.as_mut_ptr() as u64,
            size_of::<Padded>() as u32,
        );
        let got = unsafe { got.assume_init() };
        assert_eq!(got.tag, value.tag);
        assert_eq!(got.value, value.value);
    }
}
