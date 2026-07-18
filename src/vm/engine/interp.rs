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
use std::sync::Mutex;

use super::ctx::{Ctx, Shared};
use super::frame::ByteRegion;
use super::ir::{
    AsmIoDst, AsmIoVal, Bb, Block, FfiAgg, FfiKind, FfiLeaf, FuncBody, IntBinOp, IntCc, Module,
    Operand, OvfOp, ParamAbi, PlaceBase, PlaceExpr, PlaceStep, RetAbi, RetDest, Rvalue,
    ScalarPlace, Slot, Stmt, SwitchDiscr, Terminator, UnwindAction, Width,
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
pub(crate) fn mem_read_volatile(addr: u64, dst: u64, size: u32) {
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
pub(crate) fn mem_write_volatile(addr: u64, src: u64, size: u32) {
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
pub(crate) fn engine_abort(what: &str) -> ! {
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

/// f128 place 位读/写（16 字节非对齐安全；D8c 宽通道公共小件）。
fn f128_read(p: u64) -> f128 {
    f128::from_bits(unsafe { (p as *const u128).read_unaligned() })
}
fn f128_write(p: u64, v: f128) {
    unsafe { (p as *mut u128).write_unaligned(v.to_bits()) }
}

/// 冻结 MemOrd → 宿主 Ordering（D8j：guest 请求什么序就执行什么序）。
fn host_ord(o: super::ir::MemOrd) -> std::sync::atomic::Ordering {
    use std::sync::atomic::Ordering as O;
    match o {
        super::ir::MemOrd::Relaxed => O::Relaxed,
        super::ir::MemOrd::Acquire => O::Acquire,
        super::ir::MemOrd::Release => O::Release,
        super::ir::MemOrd::AcqRel => O::AcqRel,
        super::ir::MemOrd::SeqCst => O::SeqCst,
    }
}

/// 位单目（BitUn rvalue 与 SIMD lane 共用，D8b）。
fn bit_un(op: super::ir::BitUnOp, v: u64, w: Width) -> u64 {
    use super::ir::BitUnOp as B;
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

/// 饱和加/减/乘（IntSat rvalue 与 SIMD SatAdd/SatSub 共用，D8b）。
fn int_saturating(op: OvfOp, signed: bool, av: u64, bv: u64, w: Width) -> u64 {
    let (v, ovf) = int_ovf(op, signed, av, bv, w);
    if !ovf {
        v
    } else if signed {
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
        Rvalue::FloatBin { op, fw, a, b } => {
            use super::ir::{FloatOp as F, FloatW};
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            macro_rules! fb {
                ($t:ty, $wide:expr) => {{
                    let (x, y) = (<$t>::from_bits(av as _), <$t>::from_bits(bv as _));
                    (match op {
                        F::Add => x + y,
                        F::Sub => x - y,
                        F::Mul => x * y,
                        F::Div => x / y,
                        F::Rem => x % y,
                    })
                    .to_bits() as u64
                }};
            }
            match fw {
                FloatW::F16 => fb!(f16, false),
                FloatW::F32 => fb!(f32, false),
                FloatW::F64 => fb!(f64, true),
            }
        }
        Rvalue::UMax { a, b } => eval_operand(ctx, base, a)
            .0
            .max(eval_operand(ctx, base, b).0),
        Rvalue::MathUn { op, fw, a } => {
            use super::ir::{FloatW, MathUnOp as M};
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
            match fw {
                // f16 数学：std 实现即 promote-f32 计算再回舍——与 native 对 *f16
                // 的下降同源（sqrt 经 f32 双舍入安全有数学保证）
                FloatW::F16 => un!(f16::from_bits(av as u16)).to_bits() as u64,
                FloatW::F32 => un!(f32::from_bits(av as u32)).to_bits() as u64,
                FloatW::F64 => un!(f64::from_bits(av)).to_bits(),
            }
        }
        Rvalue::MathBin { op, fw, a, b } => {
            use super::ir::{FloatW, MathBinOp as M};
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
            match fw {
                FloatW::F16 => {
                    bin!(f16::from_bits(av as u16), f16::from_bits(bv as u16)).to_bits() as u64
                }
                FloatW::F32 => {
                    bin!(f32::from_bits(av as u32), f32::from_bits(bv as u32)).to_bits() as u64
                }
                FloatW::F64 => bin!(f64::from_bits(av), f64::from_bits(bv)).to_bits(),
            }
        }
        Rvalue::MathFma { fw, a, b, c } => {
            use super::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let (cv, _) = eval_operand(ctx, base, c);
            macro_rules! fma {
                ($t:ty) => {
                    <$t>::from_bits(av as _)
                        .mul_add(<$t>::from_bits(bv as _), <$t>::from_bits(cv as _))
                        .to_bits() as u64
                };
            }
            match fw {
                FloatW::F16 => fma!(f16),
                FloatW::F32 => fma!(f32),
                FloatW::F64 => fma!(f64),
            }
        }
        Rvalue::FloatCmp { cc, fw, a, b } => {
            use super::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            macro_rules! fc {
                ($t:ty) => {{
                    let (x, y) = (<$t>::from_bits(av as _), <$t>::from_bits(bv as _));
                    match cc {
                        IntCc::Eq => x == y,
                        IntCc::Ne => x != y,
                        IntCc::Lt => x < y,
                        IntCc::Le => x <= y,
                        IntCc::Gt => x > y,
                        IntCc::Ge => x >= y,
                    }
                }};
            }
            (match fw {
                FloatW::F16 => fc!(f16),
                FloatW::F32 => fc!(f32),
                FloatW::F64 => fc!(f64),
            }) as u64
        }
        Rvalue::F128Cmp { cc, a, b } => {
            let (x, y) = (
                f128_read(eval_place_addr(ctx, base, a)),
                f128_read(eval_place_addr(ctx, base, b)),
            );
            (match cc {
                IntCc::Eq => x == y,
                IntCc::Ne => x != y,
                IntCc::Lt => x < y,
                IntCc::Le => x <= y,
                IntCc::Gt => x > y,
                IntCc::Ge => x >= y,
            }) as u64
        }
        Rvalue::FloatNeg { fw, a } => {
            use super::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            match fw {
                FloatW::F16 => (-f16::from_bits(av as u16)).to_bits() as u64,
                FloatW::F32 => (-f32::from_bits(av as u32)).to_bits() as u64,
                FloatW::F64 => (-f64::from_bits(av)).to_bits(),
            }
        }
        Rvalue::FloatCast { from, to, a } => {
            use super::ir::FloatW as W;
            let (av, _) = eval_operand(ctx, base, a);
            // 全组合宿主 `as`（同宽位透传）
            match (from, to) {
                (W::F16, W::F16) | (W::F32, W::F32) | (W::F64, W::F64) => av,
                (W::F16, W::F32) => (f16::from_bits(av as u16) as f32).to_bits() as u64,
                (W::F16, W::F64) => (f16::from_bits(av as u16) as f64).to_bits(),
                (W::F32, W::F16) => (f32::from_bits(av as u32) as f16).to_bits() as u64,
                (W::F32, W::F64) => (f32::from_bits(av as u32) as f64).to_bits(),
                (W::F64, W::F16) => (f64::from_bits(av) as f16).to_bits() as u64,
                (W::F64, W::F32) => (f64::from_bits(av) as f32).to_bits() as u64,
            }
        }
        Rvalue::FloatToInt {
            from,
            to,
            signed,
            a,
        } => {
            let (av, _) = eval_operand(ctx, base, a);
            // f16/f32→f64 精确保值 ⇒ 统一经 f64；宿主 `as` 即 Rust 饱和语义（NaN→0、越界→边界）
            let x = match from {
                super::ir::FloatW::F16 => f16::from_bits(av as u16) as f64,
                super::ir::FloatW::F32 => f32::from_bits(av as u32) as f64,
                super::ir::FloatW::F64 => f64::from_bits(av),
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
        Rvalue::IntToFloat { from, to, a } => {
            use super::ir::FloatW;
            let (av, _) = eval_operand(ctx, base, a);
            // 每目标宽度都用宿主直转（`as` 正确舍入；避免中转双舍入）
            macro_rules! i2f {
                ($t:ty) => {
                    (if from.1 {
                        sext(av, from.0) as $t
                    } else {
                        (av & from.0.mask()) as $t
                    })
                    .to_bits() as u64
                };
            }
            match to {
                FloatW::F16 => i2f!(f16),
                FloatW::F32 => i2f!(f32),
                FloatW::F64 => i2f!(f64),
            }
        }
        Rvalue::BitUn { op, a } => {
            let (v, w) = eval_operand(ctx, base, a);
            bit_un(*op, v, w)
        }
        Rvalue::AtomicLoad { addr, width, order } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let o = host_ord(*order);
            // 真宿主原子指令（spike4 义务）；序按 guest 请求（D8j）
            unsafe {
                match width {
                    Width::W8 => AtomicU8::from_ptr(p as *mut u8).load(o) as u64,
                    Width::W16 => AtomicU16::from_ptr(p as *mut u16).load(o) as u64,
                    Width::W32 => AtomicU32::from_ptr(p as *mut u32).load(o) as u64,
                    Width::W64 => AtomicU64::from_ptr(p as *mut u64).load(o),
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
            int_saturating(*op, *signed, av, bv, w)
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
        Rvalue::SimdReduceArith {
            op,
            lane,
            a,
            lanes,
            lane_bytes,
        } => {
            use super::ir::{LaneKind, SimdReduceOp as R};
            let pa = eval_place_addr(ctx, base, a);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            let mut acc = mem_read(pa, lw);
            for i in 1..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw);
                acc = match *lane {
                    LaneKind::Int { signed } => match op {
                        R::Add => acc.wrapping_add(x) & lw.mask(),
                        R::Mul => acc.wrapping_mul(x) & lw.mask(),
                        R::And => acc & x,
                        R::Or => acc | x,
                        R::Xor => acc ^ x,
                        R::Min | R::Max => {
                            let take_x = if signed {
                                let (a, b) = (sext(acc, lw), sext(x, lw));
                                if matches!(op, R::Min) { b < a } else { b > a }
                            } else if matches!(op, R::Min) {
                                x < acc
                            } else {
                                x > acc
                            };
                            if take_x { x } else { acc }
                        }
                    },
                    LaneKind::Float => {
                        macro_rules! fr {
                            ($t:ty) => {{
                                let (fa, fx) = (<$t>::from_bits(acc as _), <$t>::from_bits(x as _));
                                (match op {
                                    R::Add => fa + fx,
                                    R::Mul => fa * fx,
                                    // minnum/maxnum 语义（与 LLVM reduce.fmin/fmax 一致）
                                    R::Min => fa.min(fx),
                                    R::Max => fa.max(fx),
                                    R::And | R::Or | R::Xor => engine_abort(&format!(
                                        "simd reduce {op:?} 不适用于浮点 lane"
                                    )),
                                })
                                .to_bits() as u64
                            }};
                        }
                        match lw {
                            Width::W32 => fr!(f32),
                            Width::W64 => fr!(f64),
                            _ => engine_abort("浮点 lane 宽度非 4/8（lower 校验缺口）"),
                        }
                    }
                };
            }
            acc
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
        Stmt::AtomicStore { addr, val, order } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (v, w) = eval_operand(ctx, base, val);
            let o = host_ord(*order);
            unsafe {
                match w {
                    Width::W8 => AtomicU8::from_ptr(p as *mut u8).store(v as u8, o),
                    Width::W16 => AtomicU16::from_ptr(p as *mut u16).store(v as u16, o),
                    Width::W32 => AtomicU32::from_ptr(p as *mut u32).store(v as u32, o),
                    Width::W64 => AtomicU64::from_ptr(p as *mut u64).store(v, o),
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
            succ,
            fail,
        } => {
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (e, w) = eval_operand(ctx, base, expected);
            let (n, _) = eval_operand(ctx, base, new);
            macro_rules! cx {
                ($t:ty, $at:ty) => {{
                    let a = unsafe { <$at>::from_ptr(p as *mut $t) };
                    let (so, fo) = (host_ord(*succ), host_ord(*fail));
                    let r = if *weak {
                        a.compare_exchange_weak(e as $t, n as $t, so, fo)
                    } else {
                        a.compare_exchange(e as $t, n as $t, so, fo)
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
        Stmt::AtomicRmw {
            op,
            addr,
            val,
            dst,
            order,
        } => {
            use super::ir::RmwOp as R;
            use std::sync::atomic::*;
            let (p, _) = eval_operand(ctx, base, addr);
            let (v, w) = eval_operand(ctx, base, val);
            let o = host_ord(*order);
            macro_rules! rmw {
                ($t:ty, $at:ty, $it:ty, $iat:ty) => {{
                    let a = unsafe { <$at>::from_ptr(p as *mut $t) };
                    (match op {
                        R::Xchg => a.swap(v as $t, o),
                        R::Add => a.fetch_add(v as $t, o),
                        R::Sub => a.fetch_sub(v as $t, o),
                        R::And => a.fetch_and(v as $t, o),
                        R::Or => a.fetch_or(v as $t, o),
                        R::Xor => a.fetch_xor(v as $t, o),
                        R::Nand => a.fetch_nand(v as $t, o),
                        // fetch_max/min：有符号变体经同址 AtomicI*（位型回写零扩展）
                        R::UMax => a.fetch_max(v as $t, o),
                        R::UMin => a.fetch_min(v as $t, o),
                        R::Max => unsafe { <$iat>::from_ptr(p as *mut $it) }
                            .fetch_max(v as $it, o) as $t,
                        R::Min => unsafe { <$iat>::from_ptr(p as *mut $it) }
                            .fetch_min(v as $it, o) as $t,
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
            lane,
            dst,
            a,
            b,
            lanes,
            lane_bytes,
        } => {
            use super::ir::{LaneKind, SimdBinOp as S};
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw);
                let y = mem_read(pb + i * lb, lw);
                let r: u64 = match *lane {
                    LaneKind::Int { signed } => {
                        let cmp = |cc| (int_cmp(cc, signed, x, y, lw) != 0) as u64 * lw.mask();
                        match op {
                            S::Eq => cmp(IntCc::Eq),
                            S::Ne => cmp(IntCc::Ne),
                            S::Lt => cmp(IntCc::Lt),
                            S::Le => cmp(IntCc::Le),
                            S::Gt => cmp(IntCc::Gt),
                            S::Ge => cmp(IntCc::Ge),
                            S::And => x & y,
                            S::Or => x | y,
                            S::Xor => x ^ y,
                            S::Add => x.wrapping_add(y) & lw.mask(),
                            S::Sub => x.wrapping_sub(y) & lw.mask(),
                            S::Mul => x.wrapping_mul(y) & lw.mask(),
                            S::Div | S::Rem => {
                                if y == 0 {
                                    engine_abort("simd 整除以零（guest UB）");
                                }
                                let o = if matches!(op, S::Div) {
                                    IntBinOp::Div
                                } else {
                                    IntBinOp::Rem
                                };
                                int_bin(o, signed, x, y, lw)
                            }
                            S::SatAdd => int_saturating(OvfOp::Add, signed, x, y, lw),
                            S::SatSub => int_saturating(OvfOp::Sub, signed, x, y, lw),
                            S::MinNum | S::MaxNum => engine_abort(&format!(
                                "simd {op:?} 不适用于整数 lane（lower 校验缺口）"
                            )),
                            S::Shl | S::Shr => {
                                if y >= u64::from(lw.bytes() * 8) {
                                    engine_abort("simd 移位量超过 lane 位宽（guest UB）");
                                }
                                let o = if matches!(op, S::Shl) {
                                    IntBinOp::Shl
                                } else {
                                    IntBinOp::Shr
                                };
                                int_bin(o, signed, x, y, lw)
                            }
                        }
                    }
                    // 浮点 lane（D8b）：IEEE 语义直算——比较不是位比较
                    //（+0.0==−0.0、NaN 不自反），算术不是整数加。
                    LaneKind::Float => {
                        macro_rules! fl {
                            ($t:ty, $xb:expr, $yb:expr) => {{
                                let (fx, fy) = (
                                    <$t>::from_bits($xb as _),
                                    <$t>::from_bits($yb as _),
                                );
                                let cmp = |t: bool| t as u64 * lw.mask();
                                match op {
                                    S::Eq => cmp(fx == fy),
                                    S::Ne => cmp(fx != fy),
                                    S::Lt => cmp(fx < fy),
                                    S::Le => cmp(fx <= fy),
                                    S::Gt => cmp(fx > fy),
                                    S::Ge => cmp(fx >= fy),
                                    S::Add => (fx + fy).to_bits() as u64,
                                    S::Sub => (fx - fy).to_bits() as u64,
                                    S::Mul => (fx * fy).to_bits() as u64,
                                    S::Div => (fx / fy).to_bits() as u64,
                                    S::Rem => (fx % fy).to_bits() as u64,
                                    S::MinNum => fx.min(fy).to_bits() as u64,
                                    S::MaxNum => fx.max(fy).to_bits() as u64,
                                    S::And | S::Or | S::Xor | S::SatAdd | S::SatSub
                                    | S::Shl | S::Shr => engine_abort(&format!(
                                        "simd {op:?} 不适用于浮点 lane（lower 校验缺口）"
                                    )),
                                }
                            }};
                        }
                        match lw {
                            Width::W32 => fl!(f32, x, y),
                            Width::W64 => fl!(f64, x, y),
                            _ => engine_abort("浮点 lane 宽度非 4/8（lower 校验缺口）"),
                        }
                    }
                };
                mem_write(pd + i * lb, lw, r);
            }
        }
        Stmt::SimdUn {
            op,
            lane,
            dst,
            a,
            lanes,
            lane_bytes,
        } => {
            use super::ir::{BitUnOp, LaneKind, SimdUnOp as U};
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw);
                let r: u64 = match (*lane, op) {
                    (LaneKind::Int { .. }, U::Neg) => x.wrapping_neg() & lw.mask(),
                    (LaneKind::Int { .. }, U::Ctlz) => bit_un(BitUnOp::Ctlz, x, lw),
                    (LaneKind::Int { .. }, U::Cttz) => bit_un(BitUnOp::Cttz, x, lw),
                    (LaneKind::Int { .. }, U::Ctpop) => bit_un(BitUnOp::Popcount, x, lw),
                    (LaneKind::Int { .. }, U::Bswap) => bit_un(BitUnOp::Bswap, x, lw),
                    (LaneKind::Int { .. }, U::Bitreverse) => bit_un(BitUnOp::Bitreverse, x, lw),
                    (LaneKind::Float, _) => {
                        macro_rules! fu {
                            ($t:ty) => {{
                                let f = <$t>::from_bits(x as _);
                                (match op {
                                    U::Neg => -f,
                                    U::Fabs => f.abs(),
                                    U::Fsqrt => f.sqrt(),
                                    U::Ceil => f.ceil(),
                                    U::Floor => f.floor(),
                                    U::Round => f.round(),
                                    U::RoundTiesEven => f.round_ties_even(),
                                    U::Trunc => f.trunc(),
                                    U::Fsin => f.sin(),
                                    U::Fcos => f.cos(),
                                    U::Fexp => f.exp(),
                                    U::Fexp2 => f.exp2(),
                                    U::Flog => f.ln(),
                                    U::Flog2 => f.log2(),
                                    U::Flog10 => f.log10(),
                                    U::Ctlz | U::Cttz | U::Ctpop | U::Bswap
                                    | U::Bitreverse => engine_abort(&format!(
                                        "simd {op:?} 不适用于浮点 lane（lower 校验缺口）"
                                    )),
                                })
                                .to_bits() as u64
                            }};
                        }
                        match lw {
                            Width::W32 => fu!(f32),
                            Width::W64 => fu!(f64),
                            _ => engine_abort("浮点 lane 宽度非 4/8（lower 校验缺口）"),
                        }
                    }
                    (LaneKind::Int { .. }, other) => engine_abort(&format!(
                        "simd {other:?} 不适用于整数 lane（lower 校验缺口）"
                    )),
                };
                mem_write(pd + i * lb, lw, r);
            }
        }
        Stmt::SimdFma {
            dst,
            a,
            b,
            c,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let pc = eval_place_addr(ctx, base, c);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let (x, y, z) = (
                    mem_read(pa + i * lb, lw),
                    mem_read(pb + i * lb, lw),
                    mem_read(pc + i * lb, lw),
                );
                let r = match lw {
                    Width::W32 => f32::from_bits(x as u32)
                        .mul_add(f32::from_bits(y as u32), f32::from_bits(z as u32))
                        .to_bits() as u64,
                    Width::W64 => f64::from_bits(x)
                        .mul_add(f64::from_bits(y), f64::from_bits(z))
                        .to_bits(),
                    _ => engine_abort("simd_fma lane 宽度非 4/8（lower 校验缺口）"),
                };
                mem_write(pd + i * lb, lw, r);
            }
        }
        Stmt::SimdFunnel {
            left,
            dst,
            a,
            b,
            shift,
            lanes,
            lane_bytes,
        } => {
            let pd = eval_place_addr(ctx, base, dst);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let ps = eval_place_addr(ctx, base, shift);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            let bits = u64::from(lw.bytes() * 8);
            for i in 0..*lanes as u64 {
                let x = mem_read(pa + i * lb, lw) as u128;
                let y = mem_read(pb + i * lb, lw) as u128;
                let s = mem_read(ps + i * lb, lw);
                if s >= bits {
                    engine_abort("simd_funnel 移位量超过 lane 位宽（guest UB）");
                }
                // 拼接 [a:b]（2W 位），窗口取高/低 W 位
                let cat = (x << bits) | y;
                let r = if *left { cat << s >> bits } else { cat >> s };
                mem_write(pd + i * lb, lw, r as u64 & lw.mask());
            }
        }
        Stmt::SimdCast {
            dst,
            src,
            lanes,
            src_lane,
            src_bytes,
            dst_lane,
            dst_bytes,
        } => {
            use super::ir::LaneKind as L;
            let pd = eval_place_addr(ctx, base, dst);
            let ps = eval_place_addr(ctx, base, src);
            let (sb, db) = (*src_bytes as u64, *dst_bytes as u64);
            let sw = Width::from_bytes(sb).expect("src lane 宽度");
            let dw = Width::from_bytes(db).expect("dst lane 宽度");
            for i in 0..*lanes as u64 {
                let v = mem_read(ps + i * sb, sw);
                let r: u64 = match (*src_lane, *dst_lane) {
                    (L::Int { signed }, L::Int { .. }) => {
                        // 窄化截断 / 加宽按源符号扩展
                        let x = if signed { sext(v, sw) as u64 } else { v };
                        x & dw.mask()
                    }
                    (L::Int { signed }, L::Float) => {
                        let (f16b, f32b, f64b) = if signed {
                            let x = sext(v, sw);
                            (
                                (x as f16).to_bits() as u64,
                                (x as f32).to_bits() as u64,
                                (x as f64).to_bits(),
                            )
                        } else {
                            (
                                (v as f16).to_bits() as u64,
                                (v as f32).to_bits() as u64,
                                (v as f64).to_bits(),
                            )
                        };
                        match dw {
                            Width::W16 => f16b,
                            Width::W32 => f32b,
                            Width::W64 => f64b,
                            _ => engine_abort("simd_cast 浮点 lane 宽度非 2/4/8"),
                        }
                    }
                    (L::Float, L::Int { signed }) => {
                        // f32/f16→f64 精确保值 ⇒ 统一经 f64；宿主 `as` 即饱和语义
                        //（simd_as；simd_cast 界外是 guest UB，饱和值在允许集合内）
                        let x = match sw {
                            Width::W16 => {
                                f64::from(f32::from_bits(super::x86::f16_to_f32_sw(v as u16)))
                            }
                            Width::W32 => f32::from_bits(v as u32) as f64,
                            Width::W64 => f64::from_bits(v),
                            _ => engine_abort("simd_cast 浮点 lane 宽度非 2/4/8"),
                        };
                        let out = if signed {
                            match dw {
                                Width::W8 => x as i8 as u64,
                                Width::W16 => x as i16 as u64,
                                Width::W32 => x as i32 as u64,
                                Width::W64 => x as i64 as u64,
                            }
                        } else {
                            match dw {
                                Width::W8 => x as u8 as u64,
                                Width::W16 => x as u16 as u64,
                                Width::W32 => x as u32 as u64,
                                Width::W64 => x as u64,
                            }
                        };
                        out & dw.mask()
                    }
                    (L::Float, L::Float) => match (sw, dw) {
                        (Width::W32, Width::W64) => (f32::from_bits(v as u32) as f64).to_bits(),
                        (Width::W64, Width::W32) => (f64::from_bits(v) as f32).to_bits() as u64,
                        // f16 lane（D8c 向量形态）：确定性软件模型——native 在
                        // target_feature(f16c) 函数内经 VCVTPH2PS/VCVTPS2PH 执行硬件
                        // 语义（sNaN qbit 强置等）；宿主 libcall 的 NaN 位行为随构建
                        // 目标漂移，不可依赖（half 探针 h0x7c01 实锤）
                        (Width::W16, Width::W32) => {
                            u64::from(super::x86::f16_to_f32_sw(v as u16))
                        }
                        (Width::W16, Width::W64) => {
                            f64::from(f32::from_bits(super::x86::f16_to_f32_sw(v as u16)))
                                .to_bits()
                        }
                        (Width::W32, Width::W16) => {
                            u64::from(super::x86::f32_to_f16_sw(
                                v as u32,
                                super::x86::HalfRound::Rne,
                            ))
                        }
                        (Width::W64, Width::W16) => {
                            (f64::from_bits(v) as f16).to_bits() as u64
                        }
                        _ => v, // 同宽：位透传
                    },
                };
                mem_write(pd + i * db, dw, r);
            }
        }
        Stmt::SimdSelect {
            mask,
            mask_bytes,
            a,
            b,
            dst,
            lanes,
            lane_bytes,
        } => {
            let pm = eval_place_addr(ctx, base, mask);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let pd = eval_place_addr(ctx, base, dst);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                // mask lane 全 1/全 0（类型不变量）：按符号位（末字节最高位）判
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                let src = if top >> 7 != 0 { pa } else { pb };
                mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
            }
        }
        Stmt::SimdSelectBitmask {
            mask,
            a,
            b,
            dst,
            lanes,
            lane_bytes,
        } => {
            let (m, _) = eval_operand(ctx, base, mask);
            let pa = eval_place_addr(ctx, base, a);
            let pb = eval_place_addr(ctx, base, b);
            let pd = eval_place_addr(ctx, base, dst);
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let src = if m >> i & 1 != 0 { pa } else { pb };
                mem_write(pd + i * lb, lw, mem_read(src + i * lb, lw));
            }
        }
        Stmt::SimdGather {
            passthru,
            ptrs,
            mask,
            mask_bytes,
            dst,
            lanes,
            lane_bytes,
        } => {
            let pv = eval_place_addr(ctx, base, passthru);
            let pp = eval_place_addr(ctx, base, ptrs);
            let pm = eval_place_addr(ctx, base, mask);
            let pd = eval_place_addr(ctx, base, dst);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                // 假 lane 绝不佯读（指针可能无效——这正是 mask 的语义）
                let v = if top >> 7 != 0 {
                    mem_read(mem_read(pp + i * 8, Width::W64), lw)
                } else {
                    mem_read(pv + i * lb, lw)
                };
                mem_write(pd + i * lb, lw, v);
            }
        }
        Stmt::SimdScatter {
            values,
            ptrs,
            mask,
            mask_bytes,
            lanes,
            lane_bytes,
        } => {
            let pv = eval_place_addr(ctx, base, values);
            let pp = eval_place_addr(ctx, base, ptrs);
            let pm = eval_place_addr(ctx, base, mask);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                if top >> 7 != 0 {
                    mem_write(
                        mem_read(pp + i * 8, Width::W64),
                        lw,
                        mem_read(pv + i * lb, lw),
                    );
                }
            }
        }
        Stmt::SimdMaskedLoad {
            mask,
            mask_bytes,
            base: base_op,
            passthru,
            dst,
            lanes,
            lane_bytes,
        } => {
            let pm = eval_place_addr(ctx, base, mask);
            let (pbase, _) = eval_operand(ctx, base, base_op);
            let pv = eval_place_addr(ctx, base, passthru);
            let pd = eval_place_addr(ctx, base, dst);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                let v = if top >> 7 != 0 {
                    mem_read(pbase + i * lb, lw)
                } else {
                    mem_read(pv + i * lb, lw)
                };
                mem_write(pd + i * lb, lw, v);
            }
        }
        Stmt::SimdMaskedStore {
            mask,
            mask_bytes,
            base: base_op,
            values,
            lanes,
            lane_bytes,
        } => {
            let pm = eval_place_addr(ctx, base, mask);
            let (pbase, _) = eval_operand(ctx, base, base_op);
            let pv = eval_place_addr(ctx, base, values);
            let (mb, lb) = (*mask_bytes as u64, *lane_bytes as u64);
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            for i in 0..*lanes as u64 {
                let top = unsafe { *((pm + i * mb + mb - 1) as *const u8) };
                if top >> 7 != 0 {
                    mem_write(pbase + i * lb, lw, mem_read(pv + i * lb, lw));
                }
            }
        }
        Stmt::SimdExtractDyn {
            src,
            idx,
            dst,
            lanes,
            lane_bytes,
        } => {
            let ps = eval_place_addr(ctx, base, src);
            let (i, _) = eval_operand(ctx, base, idx);
            if i >= u64::from(*lanes) {
                engine_abort(&format!(
                    "simd_extract_dyn 索引 {i} 越界（lanes={lanes}，guest UB）"
                ));
            }
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            place_write(ctx, base, dst, mem_read(ps + i * lb, lw));
        }
        Stmt::SimdInsertDyn {
            src,
            idx,
            val,
            dst,
            lanes,
            lane_bytes,
        } => {
            let ps = eval_place_addr(ctx, base, src);
            let pd = eval_place_addr(ctx, base, dst);
            let (i, _) = eval_operand(ctx, base, idx);
            if i >= u64::from(*lanes) {
                engine_abort(&format!(
                    "simd_insert_dyn 索引 {i} 越界（lanes={lanes}，guest UB）"
                ));
            }
            let lb = *lane_bytes as u64;
            let lw = Width::from_bytes(lb).expect("lane 宽度");
            let (v, _) = eval_operand(ctx, base, val);
            let total = u64::from(*lanes) * lb;
            // dst 可能与 src 同址（x = insert_dyn(x,…)）：整体搬运用 memmove
            unsafe {
                std::ptr::copy(ps as *const u8, pd as *mut u8, total as usize);
            }
            mem_write(pd + i * lb, lw, v);
        }
        Stmt::SimdArithOffset {
            ptrs,
            offsets,
            stride,
            dst,
            lanes,
        } => {
            let pp = eval_place_addr(ctx, base, ptrs);
            let po = eval_place_addr(ctx, base, offsets);
            let pd = eval_place_addr(ctx, base, dst);
            for i in 0..*lanes as u64 {
                let p = mem_read(pp + i * 8, Width::W64);
                let off = mem_read(po + i * 8, Width::W64);
                mem_write(
                    pd + i * 8,
                    Width::W64,
                    p.wrapping_add(off.wrapping_mul(*stride)),
                );
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
        Stmt::Sat128 {
            op,
            signed,
            a,
            b,
            dst,
        } => {
            let pa = eval_place_addr(ctx, base, a);
            let x = unsafe { (pa as *const u128).read_unaligned() };
            let pb = eval_place_addr(ctx, base, b);
            let y = unsafe { (pb as *const u128).read_unaligned() };
            let r = if *signed {
                let (xs, ys) = (x as i128, y as i128);
                (match op {
                    OvfOp::Add => xs.saturating_add(ys),
                    OvfOp::Sub => xs.saturating_sub(ys),
                    OvfOp::Mul => xs.saturating_mul(ys),
                }) as u128
            } else {
                match op {
                    OvfOp::Add => x.saturating_add(y),
                    OvfOp::Sub => x.saturating_sub(y),
                    OvfOp::Mul => x.saturating_mul(y),
                }
            };
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(r) };
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
            to,
            dst,
        } => {
            use super::ir::FloatW;
            let p = eval_place_addr(ctx, base, src);
            let x = unsafe { (p as *const u128).read_unaligned() };
            macro_rules! w2f {
                ($t:ty) => {
                    (if *signed { x as i128 as $t } else { x as $t }).to_bits() as u64
                };
            }
            let bits = match to {
                FloatW::F16 => w2f!(f16),
                FloatW::F32 => w2f!(f32),
                FloatW::F64 => w2f!(f64),
            };
            place_write(ctx, base, dst, bits);
        }
        Stmt::Bit128 { op, src, dst } => {
            use super::ir::BitUnOp as B;
            let x = unsafe { (eval_place_addr(ctx, base, src) as *const u128).read_unaligned() };
            let r = match op {
                B::Bswap => x.swap_bytes(),
                B::Bitreverse => x.reverse_bits(),
                _ => engine_abort("Bit128 只承载 bswap/bitreverse"),
            };
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(r) };
        }
        Stmt::Bit128Count { op, src, dst } => {
            use super::ir::BitUnOp as B;
            let x = unsafe { (eval_place_addr(ctx, base, src) as *const u128).read_unaligned() };
            let r = match op {
                B::Popcount => x.count_ones(),
                B::Ctlz => x.leading_zeros(),
                B::Cttz => x.trailing_zeros(),
                _ => engine_abort("Bit128Count 只承载 ctpop/ctlz/cttz"),
            };
            place_write(ctx, base, dst, r as u64);
        }
        Stmt::FloatToWide128 {
            src,
            from,
            signed,
            dst,
        } => {
            use super::ir::FloatW;
            let (v, _) = eval_operand(ctx, base, src);
            // f16/f32→f64 精确保值 ⇒ 统一经 f64；宿主 `as` 即饱和语义（NaN→0、越界→边界）
            let x = match from {
                FloatW::F16 => f16::from_bits(v as u16) as f64,
                FloatW::F32 => f32::from_bits(v as u32) as f64,
                FloatW::F64 => f64::from_bits(v),
            };
            let bits: u128 = if *signed {
                x as i128 as u128
            } else {
                x as u128
            };
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(bits) };
        }
        // ===== f128 宽通道（D8c）=====
        Stmt::F128Bin { op, a, b, dst } => {
            use super::ir::FloatOp as F;
            let (x, y) = (
                f128_read(eval_place_addr(ctx, base, a)),
                f128_read(eval_place_addr(ctx, base, b)),
            );
            let r = match op {
                F::Add => x + y,
                F::Sub => x - y,
                F::Mul => x * y,
                F::Div => x / y,
                F::Rem => x % y,
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128MathBin { op, a, b, dst } => {
            use super::ir::{F128Rhs, MathBinOp as M};
            let x = f128_read(eval_place_addr(ctx, base, a));
            let r = match (op, b) {
                (M::Powi, F128Rhs::Scalar(o)) => x.powi(eval_operand(ctx, base, o).0 as i32),
                (M::Powi, F128Rhs::Wide(_)) => engine_abort("f128 powi rhs 形态"),
                (op, F128Rhs::Wide(pb)) => {
                    let y = f128_read(eval_place_addr(ctx, base, pb));
                    match op {
                        M::Pow => x.powf(y),
                        M::Copysign => x.copysign(y),
                        M::Minnum => x.min(y),
                        M::Maxnum => x.max(y),
                        M::Powi => unreachable!(),
                    }
                }
                (_, F128Rhs::Scalar(_)) => engine_abort("f128 math rhs 形态"),
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128Un { op, a, dst } => {
            use super::ir::{F128UnOp as U, MathUnOp as M};
            let x = f128_read(eval_place_addr(ctx, base, a));
            let r = match op {
                U::Neg => -x,
                U::Math(m) => match m {
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
                },
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128Fma { a, b, c, dst } => {
            let x = f128_read(eval_place_addr(ctx, base, a));
            let y = f128_read(eval_place_addr(ctx, base, b));
            let z = f128_read(eval_place_addr(ctx, base, c));
            f128_write(eval_place_addr(ctx, base, dst), x.mul_add(y, z));
        }
        Stmt::F128FromScalar { src, kind, dst } => {
            use super::ir::{F128Scalar as K, FloatW};
            let (v, w) = eval_operand(ctx, base, src);
            let r: f128 = match kind {
                K::F(FloatW::F16) => f16::from_bits(v as u16) as f128,
                K::F(FloatW::F32) => f32::from_bits(v as u32) as f128,
                K::F(FloatW::F64) => f64::from_bits(v) as f128,
                K::Int { signed: true } => sext(v, w) as f128,
                K::Int { signed: false } => (v & w.mask()) as f128,
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128ToScalar { src, kind, w, dst } => {
            use super::ir::{F128Scalar as K, FloatW};
            let x = f128_read(eval_place_addr(ctx, base, src));
            let bits: u64 = match kind {
                K::F(FloatW::F16) => (x as f16).to_bits() as u64,
                K::F(FloatW::F32) => (x as f32).to_bits() as u64,
                K::F(FloatW::F64) => (x as f64).to_bits(),
                // `as` 饱和语义（NaN→0、越界→边界）
                K::Int { signed: true } => match w {
                    Width::W8 => x as i8 as u64,
                    Width::W16 => x as i16 as u64,
                    Width::W32 => x as i32 as u64,
                    Width::W64 => x as i64 as u64,
                },
                K::Int { signed: false } => match w {
                    Width::W8 => x as u8 as u64,
                    Width::W16 => x as u16 as u64,
                    Width::W32 => x as u32 as u64,
                    Width::W64 => x as u64,
                },
            };
            place_write(ctx, base, dst, bits & w.mask());
        }
        Stmt::F128FromWideInt { src, signed, dst } => {
            let p = eval_place_addr(ctx, base, src);
            let x = unsafe { (p as *const u128).read_unaligned() };
            let r: f128 = if *signed {
                x as i128 as f128
            } else {
                x as f128
            };
            f128_write(eval_place_addr(ctx, base, dst), r);
        }
        Stmt::F128ToWideInt { src, signed, dst } => {
            let x = f128_read(eval_place_addr(ctx, base, src));
            let bits: u128 = if *signed {
                x as i128 as u128
            } else {
                x as u128
            };
            let pd = eval_place_addr(ctx, base, dst);
            unsafe { (pd as *mut u128).write_unaligned(bits) };
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
        Stmt::Fence {
            single_thread,
            order,
        } => {
            use std::sync::atomic::{compiler_fence, fence};
            let o = host_ord(*order);
            if *single_thread {
                compiler_fence(o);
            } else {
                fence(o);
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
        unsafe {
            (*self.ctx).depth -= 1;
            (*self.ctx).shadow.pop(); // D8e：影子帧出栈（与 depth 同生命周期）
        }
    }
}

/// 块序列执行的出口。
enum Exit {
    Ret(u64, u64),
    /// cleanup 链尾（Resume）：返回 guard.drop，宿主 unwind 自动继续
    Resume,
}

/// 按 D4 fn 条目真地址派发（CallIndirect / catch_unwind 的 try/catch fn 共用）。
// ===== signal 异步窄化（D8d）=====
/// guest 信号 handler → AS-trampoline 真码地址。async 信号（可安全 run-to-completion）
/// 复用 M4.4 thunk 工厂（attach + interp_frame，签名 `(i32)->void`）；sync 故障信号
/// （SEGV/BUS/FPE/ILL/TRAP）的 guest handler 响亮拒绝——宿主故障与 guest 故障不可分辨，
/// 伪造恢复=静默错值。handler 必须是已知 guest fn 条目（非 guest 地址不接）。
fn signal_thunk(ctx: *mut Ctx, signum: libc::c_int, handler: u64) -> libc::sighandler_t {
    // 同步故障信号：guest handler 不可支持（诊断退出而非静默）
    if matches!(
        signum,
        libc::SIGSEGV | libc::SIGBUS | libc::SIGFPE | libc::SIGILL | libc::SIGTRAP
    ) {
        engine_abort(&format!(
            "guest handler for synchronous fault signal {signum}（SEGV/BUS/FPE/ILL/TRAP：\
             宿主与 guest 故障不可分辨，D8l）"
        ));
    }
    let shared: &'static Shared = unsafe { &*(*ctx).shared };
    let Some(&func) = shared.module.fn_addrs.get(&handler) else {
        engine_abort(&format!(
            "signal handler {handler:#x} 不是已知 guest fn 条目"
        ));
    };
    // 信号 handler ABI = `extern "C" fn(c_int)`；thunk 工厂造真码入口 + 边界 attach。
    let sig = super::ir::ForeignSig {
        args: vec![FfiKind::I32],
        ret: FfiKind::Void,
        fixed: None,
        thunk_args: vec![],
    };
    super::thunks::get_or_create(shared, handler, func, &sig) as libc::sighandler_t
}

// ===== backtrace 影子帧（D8e）=====
/// 合成 IP 基址：高位在用户地址空间之上、非页对齐 → 绝不与真实代码/数据地址撞，
/// dladdr 找不到（诚实 `<unknown>` 符号化，禁止伪造宿主符号）。
const FUNC_IP_BASE: u64 = 0x5f5f_0000_0000_0000;
fn func_synth_ip(func: u32) -> u64 {
    FUNC_IP_BASE + (func as u64) * 64
}

/// `_Unwind_Backtrace(trace_fn, arg)`（D8e）：逐影子帧（栈顶→底）调 guest trace_fn
/// (synth_ctx, arg)；trace_fn 返 0（_URC_NO_REASON）续，非 0 停。synth_ctx 指向一个
/// 存 IP 的小缓冲，`_Unwind_GetIP(ctx)` 从中读。返回 _URC_END_OF_STACK(5)。
fn unwind_backtrace(ctx: *mut Ctx, trace_fn: u64, arg: u64) -> u64 {
    // 快照影子帧（回调再入会 push/pop，不能借活栈迭代）。跳过栈顶自身
    //（_Unwind_Backtrace 的帧不该出现在回溯里，= native 语义）。
    let frames: Vec<u64> = {
        let s = unsafe { &(*ctx).shadow };
        s.iter().rev().skip(1).copied().collect()
    };
    for ip in frames {
        // synth _Unwind_Context = 单字缓冲存 IP（GetIP 读它）
        let cell: u64 = ip;
        let cell_ptr = &cell as *const u64 as u64;
        let r = call_fn_addr(ctx, trace_fn, &[cell_ptr, arg], "_Unwind_Backtrace").0;
        if r != 0 {
            break; // _URC_FOREIGN_EXCEPTION_CAUGHT / _URC_FAILURE 等 → 停
        }
    }
    5 // _URC_END_OF_STACK
}

// ===== atexit 家族（D8g）=====
// glibc 不导出 `atexit` 供 guest dlsym；引擎自持 LIFO 注册表 + 一个 native
// trampoline（经引擎自身链接的 libc `atexit` 挂载，非 dlsym）。进程收尾时 libc
// 在主线程调 trampoline，逐条 LIFO 解释执行 guest 回调（fresh Ctx attach）。
#[derive(Clone, Copy)]
enum AtexitKind {
    Plain,  // atexit：fn()
    CxaArg, // __cxa_atexit：fn(arg)
    OnExit, // on_exit：fn(status=0, arg)
}
struct AtexitEntry {
    func: u64,
    kind: AtexitKind,
    arg: u64,
}
static ATEXIT: Mutex<Vec<AtexitEntry>> = Mutex::new(Vec::new());
static ATEXIT_SHARED: AtomicU64Ptr = AtomicU64Ptr::new();

/// 进程期 Shared 的裸指针存放（trampoline 在无 Ctx 的退出线程上找回引擎）。
struct AtomicU64Ptr(std::sync::atomic::AtomicUsize);
impl AtomicU64Ptr {
    const fn new() -> Self {
        Self(std::sync::atomic::AtomicUsize::new(0))
    }
    fn set(&self, p: *const Shared) {
        self.0
            .store(p as usize, std::sync::atomic::Ordering::SeqCst);
    }
    fn get(&self) -> *const Shared {
        self.0.load(std::sync::atomic::Ordering::SeqCst) as *const Shared
    }
}

fn atexit_register(func: u64, kind: AtexitKind, arg: u64) -> u64 {
    // fn 必须是已知 guest 条目（非 guest 回调不接——防静默）
    let shared = ATEXIT_SHARED.get();
    if shared.is_null() {
        engine_abort("atexit 在 Shared 发布前调用（引擎不变量）");
    }
    let module: &Module = unsafe { &(*shared).module };
    if !module.fn_addrs.contains_key(&func) {
        engine_abort(&format!("atexit 回调 {func:#x} 不是已知 guest fn 条目"));
    }
    let mut reg = ATEXIT.lock().unwrap();
    if reg.is_empty() {
        // 首注册：挂 native trampoline（引擎链接的 libc atexit，非 guest dlsym）
        unsafe { libc::atexit(run_atexit_callbacks) };
    }
    reg.push(AtexitEntry { func, kind, arg });
    0
}

/// libc 在进程收尾（主线程）调用：LIFO 解释执行 guest 回调。
extern "C" fn run_atexit_callbacks() {
    let shared = ATEXIT_SHARED.get();
    if shared.is_null() {
        return;
    }
    // fresh Ctx（退出线程可能非 guest 执行线程；attach 幂等）
    let ctx = super::ctx::attach(unsafe { &*shared });
    // LIFO：后注册先执行（C 语义）
    loop {
        let entry = {
            let mut reg = ATEXIT.lock().unwrap();
            match reg.pop() {
                Some(e) => e,
                None => break,
            }
        };
        let args: &[u64] = match entry.kind {
            AtexitKind::Plain => &[],
            AtexitKind::CxaArg => &[entry.arg],
            AtexitKind::OnExit => &[0, entry.arg],
        };
        // guest 回调 panic 穿到 C 退出路径 = abort（与 native 一致）
        let _ = call_fn_addr(ctx, entry.func, args, "atexit");
    }
}

fn call_fn_addr(ctx: *mut Ctx, addr: u64, args: &[u64], caller: &str) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let Some(&fid) = module.fn_addrs.get(&addr) else {
        engine_abort(&format!(
            "间接调用目标 {addr:#x} 不是已知 fn 条目（调用者 {caller}）"
        ));
    };
    call_guest(ctx, fid, args)
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

/// J1 单一派发点（M5.3a，m5.3-design §2.2）：guest 函数调用的必经口，收拢六处
/// 原 interp_frame 直调（Call/CallIndirect/CatchUnwind 回调/run_main/run_export/
/// thunk 蹦床；tsan_mt 豁免——Q4，TSan 通道不编 cranelift，收拢无意义）。
/// 槽非零 = 已发布编译码（M5.3b 起 i2c 直调 packed 入口）；零 = 计数 + 解释。
/// 计数 Relaxed（丢计只影响触发时刻）；槽 Acquire 配编译线程 Release（D4 协议）。
#[inline]
pub(crate) fn call_guest(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let jit = unsafe { &(*(*ctx).shared).jit };
    if jit.enabled {
        let entry = jit.slots[func as usize].load(std::sync::atomic::Ordering::Acquire);
        if entry != 0 {
            // i2c：packed 入口（M5.3b；发布序 fast→packed，Acquire 已见全部前置写）
            type Packed = extern "C-unwind" fn(*const u64, *mut u64);
            let f: Packed = unsafe { std::mem::transmute(entry as usize) };
            let mut ret = [0u64; 2];
            f(args.as_ptr(), ret.as_mut_ptr());
            return (ret[0], ret[1]);
        }
        let prev = jit.counters[func as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 恰好跨阈值的那一次投递（exactly-once；后续计数继续增长但不重复投递）
        if prev + 1 == jit.threshold
            && let Some(q) = jit.queue.lock().unwrap().as_ref()
        {
            let _ = q.send(func);
        }
    }
    interp_frame(ctx, func, args)
}

/// C1：FnId 的返回通道（thunk 分流——Indirect sret 直传 vs 小档重打包的判定源）。
pub(crate) fn ret_abi_of(ctx: *mut Ctx, func: u32) -> RetAbi {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    module.funcs[func as usize].ret
}

/// C1 FFI 入向封送：C 侧实参（marshal_args 产出——标量原样 / 聚合 = 聚合字节
/// 真地址）按 callee ParamAbi 展开成 ABI 实参槽后 `call_guest`（thunk 工厂与
/// P1 条目蹦床共用）。ret_addr = 按值聚合返回时 libffi 的结果缓冲地址——仅当
/// callee RetAbi::Indirect 时作隐藏首实参槽（sret 直传）；小档由调用方对
/// (lo,hi) 做 FfiAgg 重打包。
pub(crate) fn call_guest_ffi(
    ctx: *mut Ctx,
    func: u32,
    kinds: &[FfiKind],
    vals: &[u64],
    ret_addr: Option<u64>,
) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];
    let mut av: Vec<u64> = Vec::with_capacity(vals.len() + body.params.len() + 1);
    if let RetAbi::Indirect { .. } = body.ret {
        av.push(
            ret_addr.expect("C1：callee 按值聚合返回（RetAbi::Indirect）但无结果地址"),
        );
    }
    let mut ki = 0usize;
    for p in &body.params {
        match p {
            ParamAbi::Zst => {}
            ParamAbi::Scalar(_) => match kinds.get(ki) {
                Some(FfiKind::Agg(agg)) => {
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 0) });
                    ki += 1;
                }
                Some(_) => {
                    av.push(vals[ki]);
                    ki += 1;
                }
                None => {
                    engine_abort(&format!("C1 封送缺参（callee fn {} params {:?}）", body.name, body.params))
                }
            },
            ParamAbi::Pair(_, _) => match kinds.get(ki) {
                Some(FfiKind::Agg(agg)) => {
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 0) });
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 1) });
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 封送错配：callee `Pair` 参数遇到非标量 C 参（fn {} params {:?} kinds {:?}）",
                    body.name, body.params, kinds
                )),
            },
            ParamAbi::Indirect { .. } => match kinds.get(ki) {
                Some(FfiKind::Agg(_)) => {
                    av.push(vals[ki]);
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 封送错配：callee 按址参数遇到非标量 C 参（fn {} params {:?} kinds {:?}）",
                    body.name, body.params, kinds
                )),
            },
        }
    }
    if ki != vals.len() {
        engine_abort(&format!(
            "C1 封送槽数错配：callee fn {} 消费 {ki}，marshal 供 {}",
            body.name,
            vals.len()
        ));
    }
    call_guest(ctx, func, &av)
}

/// 读聚合声明序第 idx 个字段的值（Scalar 叶按宽度读；顶层嵌套叶与 Pair/Scalar
/// 参数形态结构性互斥——同 rustc layout 推导，出现即引擎不变量破坏）。
unsafe fn agg_leaf_at(addr: u64, agg: &FfiAgg, idx: usize) -> u64 {
    let Some(f) = agg.fields.get(idx) else {
        engine_abort("C1 封送：Pair 参数遇单字段聚合");
    };
    let FfiLeaf::Scalar(k) = &f.leaf else {
        engine_abort("C1 封送：顶层嵌套叶遇 Pair 参数");
    };
    let p = addr.wrapping_add(f.off as u64) as *const u8;
    unsafe {
        match k {
            FfiKind::I8 | FfiKind::U8 => p.read() as u64,
            FfiKind::I16 | FfiKind::U16 => (p as *const u16).read_unaligned() as u64,
            FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => {
                (p as *const u32).read_unaligned() as u64
            }
            FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                (p as *const u64).read_unaligned()
            }
            FfiKind::Void | FfiKind::Agg(_) => engine_abort("C1 封送：非法叶类"),
        }
    }
}

/// 模型 A：guest 调用 = 宿主递归（spike1/3 验证的形状）。
/// 调用约定 v2：实参展平 `&[u64]`（pair 占 2 槽、indirect 传地址），返回 (lo, hi)。
/// guest 栈溢出防护（M5.2 D8a）= **真栈字节守卫**：以本地变量地址近似宿主 SP，
/// 低于 Ctx 冻结的安全下界（线程栈低端 + 边距）即诊断退出——帧数不设固定上限
///（旧 8000 帧硬编码对 native 栈界严重失真：native 8MiB 主栈可容 ~10 万浅帧）。
/// 随线程真实栈自适应；native 语义 = SIGSEGV→"has overflowed its stack"，此处
/// 为诊断替身（ram-spec §7：溢出深度 unspecified，只承诺近似 native）。
pub(super) fn interp_frame(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    let depth = unsafe {
        (*ctx).depth += 1;
        (*ctx).depth
    };
    let sp_approx = &depth as *const u32 as usize;
    if unsafe { (*ctx).stack_floor } > sp_approx {
        engine_abort(&format!(
            "guest 栈溢出（宿主执行栈触及安全边距；解释深度 {depth}；fn {}）",
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
    // 影子帧入栈（D8e）：合成 IP = FUNC_IP_BASE + func×64（每 FuncId 唯一、非零、
    // 不可执行的 opaque token；作 backtrace 的 IP 恰好——从不解引用为代码）。
    unsafe { (*ctx).shadow.push(func_synth_ip(func)) };
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
                let (lo, hi) = call_guarding_terminate(unwind, || call_guest(ctx, *callee, &av)); // ← 宿主递归
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
                // P1（§7.6）：FFI 可派生条目值本身已是 stub 码址——跳过二次物化。
                for (pos, inner) in &sig.thunk_args {
                    let v = av[*pos];
                    if v != 0
                        && !super::codearena::is_stub_addr(v)
                        && let Some(&fid) = module.fn_addrs.get(&v)
                    {
                        let shared: &'static Shared = unsafe { &*(*ctx).shared };
                        av[*pos] = super::thunks::get_or_create(shared, v, fid, inner);
                    }
                }
                edge.set(cleanup_edge(unwind));
                let optional_libs: &[Box<str>] = &module.native_libs;
                let required_libs: &[Box<str>] = &module.required_native_libs;
                // C1：按值聚合返回 = Indirect 落点（调用点强制），ffi 层 memcpy 至
                // 目的真地址；标量返回照旧走 u64 通道
                let ret_dst = if let RetDest::Indirect(dst) = ret {
                    Some(eval_place_addr(ctx, base, dst))
                } else {
                    None
                };
                // D8a：guest 线程栈放大。解释帧宿主成本数十倍于 native 帧，按 guest
                // attr 原样创建的线程会在远浅于 native 的深度打穿宿主栈（SIGSEGV 而非
                // 诊断）。显式 stacksize（std::thread 恒显式）临时放大，调用后还原；
                // guest 自供栈（setstack）不动。栈尺寸属 unspecified（ram-spec §2）。
                let stack_restore = super::ffi::amplify_pthread_stack(sym, &av);
                let r = {
                    let ffi = unsafe { &mut (*ctx).ffi };
                    super::ffi::call(ffi, optional_libs, required_libs, sym, sig, &av, ret_dst)
                };
                if let Some((attr, orig)) = stack_restore {
                    unsafe { libc::pthread_attr_setstacksize(attr, orig) };
                }
                edge.set(None);
                let r = r.unwrap_or_else(|reason| {
                    engine_abort(&format!(
                        "foreign `{sym}` 的必需原生库装载失败（fn {}）: {reason}",
                        body.name
                    ))
                });
                let Some(r) = r else {
                    engine_abort(&format!(
                        "foreign `{sym}` 符号不存在（归档兜底表 / dlsym 全域均未命中；fn {}）",
                        body.name
                    ));
                };
                match ret {
                    RetDest::Ignore => {}
                    RetDest::Scalar(p) => place_write(ctx, base, p, r),
                    // C1：按值聚合字节已由 ffi 层 memcpy 至 dst
                    RetDest::Indirect(_) => {}
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
                if addr == 0 {
                    // extern weak 符号缺席取址 = NULL（native 同语义）；调用空
                    // fn-ptr 在 native 是 UB/SIGSEGV——VM 响亮诊断而非宿主崩溃。
                    engine_abort(&format!("间接调用空 fn 指针（调用者 {}）", body.name));
                }
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                edge.set(cleanup_edge(unwind));
                let (lo, hi) = if let Some(&fid) = module.fn_addrs.get(&addr) {
                    call_guarding_terminate(unwind, || call_guest(ctx, fid, &av))
                } else if let Some(nsig) = native_sig {
                    // FFI 反方向之二（M4.4）：guest 持 native 真码 fn ptr（运行期
                    // dlsym 所得，如 __pthread_get_minstack）→ 按冻结签名直调。
                    // C1：native_sig 聚合返回时首槽即目的地址（调用点已按
                    // RetDest::Indirect 压栈；libffi sret 不占参数位，剔除后直调）
                    let (ret_dst, arg_slice) = if matches!(nsig.ret, FfiKind::Agg(_)) {
                        (av.first().copied(), &av[1..])
                    } else {
                        (None, &av[..])
                    };
                    (
                        super::ffi::call_addr(addr as usize, nsig, arg_slice, ret_dst),
                        0,
                    )
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
                    Builtin::X86PsadBw128 | Builtin::X86PsadBw256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("psad.bw 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86PsadBw128) {
                                super::x86::psad_bw128(dst, a(0) as *const u8, a(1) as *const u8);
                            } else {
                                super::x86::psad_bw256(dst, a(0) as *const u8, a(1) as *const u8);
                            }
                        }
                        true
                    }
                    Builtin::X86Pclmulqdq => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pclmulqdq 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe {
                            super::x86::pclmulqdq(dst, a(0) as *const u8, a(1) as *const u8, a(2))
                        };
                        true
                    }
                    Builtin::X86AesEnc
                    | Builtin::X86AesEncLast
                    | Builtin::X86AesDec
                    | Builtin::X86AesDecLast => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("aesni 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, k) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86AesEnc => super::x86::aesenc(dst, x, k),
                                Builtin::X86AesEncLast => super::x86::aesenclast(dst, x, k),
                                Builtin::X86AesDec => super::x86::aesdec(dst, x, k),
                                _ => super::x86::aesdeclast(dst, x, k),
                            }
                        }
                        true
                    }
                    Builtin::X86AesImc => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("aesimc 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { super::x86::aesimc(dst, a(0) as *const u8) };
                        true
                    }
                    Builtin::X86AesKeygenAssist => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("aeskeygenassist 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { super::x86::aeskeygenassist(dst, a(0) as *const u8, a(1)) };
                        true
                    }
                    Builtin::X86Permd256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("permd 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        unsafe { super::x86::permd256(dst, a(0) as *const u8, a(1) as *const u8) };
                        true
                    }
                    Builtin::X86PmaddUbSw128
                    | Builtin::X86PmaddUbSw256
                    | Builtin::X86PmaddWd128
                    | Builtin::X86PmaddWd256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("pmadd 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86PmaddUbSw128 => super::x86::pmaddubsw128(dst, x, y),
                                Builtin::X86PmaddUbSw256 => super::x86::pmaddubsw256(dst, x, y),
                                Builtin::X86PmaddWd128 => super::x86::pmaddwd128(dst, x, y),
                                _ => super::x86::pmaddwd256(dst, x, y),
                            }
                        }
                        true
                    }
                    Builtin::X86GatherQPd256 | Builtin::X86GatherDPd256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("gather.pd.256 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        // (src vec, base 标量指针, vindex vec, mask vec, scale imm)
                        unsafe {
                            if matches!(builtin, Builtin::X86GatherQPd256) {
                                super::x86::gather_q_pd_256(
                                    dst,
                                    a(0) as *const u8,
                                    a(1),
                                    a(2) as *const u8,
                                    a(3) as *const u8,
                                    a(4),
                                );
                            } else {
                                super::x86::gather_d_pd_256(
                                    dst,
                                    a(0) as *const u8,
                                    a(1),
                                    a(2) as *const u8,
                                    a(3) as *const u8,
                                    a(4),
                                );
                            }
                        }
                        true
                    }
                    Builtin::X86Pmadd52Lo128
                    | Builtin::X86Pmadd52Hi128
                    | Builtin::X86Pmadd52Lo256
                    | Builtin::X86Pmadd52Hi256
                    | Builtin::X86Pmadd52Lo512
                    | Builtin::X86Pmadd52Hi512 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("vpmadd52 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y, z) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86Pmadd52Lo128 => {
                                    super::x86::vpmadd52::<2, false>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Hi128 => {
                                    super::x86::vpmadd52::<2, true>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Lo256 => {
                                    super::x86::vpmadd52::<4, false>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Hi256 => {
                                    super::x86::vpmadd52::<4, true>(dst, x, y, z)
                                }
                                Builtin::X86Pmadd52Lo512 => {
                                    super::x86::vpmadd52::<8, false>(dst, x, y, z)
                                }
                                _ => super::x86::vpmadd52::<8, true>(dst, x, y, z),
                            }
                        }
                        true
                    }
                    Builtin::X86MaxPs128
                    | Builtin::X86MinPs128
                    | Builtin::X86MaxPs256
                    | Builtin::X86MinPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("max/min.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            match builtin {
                                Builtin::X86MaxPs128 => super::x86::maxmin_ps::<4, true>(dst, x, y),
                                Builtin::X86MinPs128 => super::x86::maxmin_ps::<4, false>(dst, x, y),
                                Builtin::X86MaxPs256 => super::x86::maxmin_ps::<8, true>(dst, x, y),
                                _ => super::x86::maxmin_ps::<8, false>(dst, x, y),
                            }
                        }
                        true
                    }
                    Builtin::X86CmpPs128 | Builtin::X86CmpPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("cmp.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y, imm) = (a(0) as *const u8, a(1) as *const u8, a(2));
                        unsafe {
                            if matches!(builtin, Builtin::X86CmpPs128) {
                                super::x86::cmp_ps::<4>(dst, x, y, imm)
                            } else {
                                super::x86::cmp_ps::<8>(dst, x, y, imm)
                            }
                        }
                        true
                    }
                    Builtin::X86RoundPs128 | Builtin::X86RoundPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("round.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, imm) = (a(0) as *const u8, a(1));
                        unsafe {
                            if matches!(builtin, Builtin::X86RoundPs128) {
                                super::x86::round_ps::<4>(dst, x, imm)
                            } else {
                                super::x86::round_ps::<8>(dst, x, imm)
                            }
                        }
                        true
                    }
                    Builtin::X86CvtPs2dq128
                    | Builtin::X86CvttPs2dq128
                    | Builtin::X86CvtPs2dq256
                    | Builtin::X86CvttPs2dq256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("cvt(t).ps2dq 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let x = a(0) as *const u8;
                        unsafe {
                            match builtin {
                                Builtin::X86CvtPs2dq128 => super::x86::cvt_ps2dq::<4, false>(dst, x),
                                Builtin::X86CvttPs2dq128 => super::x86::cvt_ps2dq::<4, true>(dst, x),
                                Builtin::X86CvtPs2dq256 => super::x86::cvt_ps2dq::<8, false>(dst, x),
                                _ => super::x86::cvt_ps2dq::<8, true>(dst, x),
                            }
                        }
                        true
                    }
                    Builtin::X86BlendvPs128 | Builtin::X86BlendvPs256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("blendv.ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, y, m) = (a(0) as *const u8, a(1) as *const u8, a(2) as *const u8);
                        unsafe {
                            if matches!(builtin, Builtin::X86BlendvPs128) {
                                super::x86::blendv_ps::<4>(dst, x, y, m)
                            } else {
                                super::x86::blendv_ps::<8>(dst, x, y, m)
                            }
                        }
                        true
                    }
                    Builtin::X86Lddqu128 | Builtin::X86Lddqu256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("lddqu 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let src = a(0) as *const u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86Lddqu128) {
                                super::x86::lddqu::<16>(dst, src)
                            } else {
                                super::x86::lddqu::<32>(dst, src)
                            }
                        }
                        true
                    }
                    Builtin::X86Cvtps2ph128 | Builtin::X86Cvtps2ph256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("vcvtps2ph 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, imm) = (a(0) as *const u8, a(1));
                        unsafe {
                            if matches!(builtin, Builtin::X86Cvtps2ph128) {
                                super::x86::cvtps2ph::<4>(dst, x, imm)
                            } else {
                                super::x86::cvtps2ph::<8>(dst, x, imm)
                            }
                        }
                        true
                    }
                    Builtin::X86Cvtph2ps128 | Builtin::X86Cvtph2ps256 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("vcvtph2ps 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let x = a(0) as *const u8;
                        unsafe {
                            if matches!(builtin, Builtin::X86Cvtph2ps128) {
                                super::x86::cvtph2ps::<4>(dst, x)
                            } else {
                                super::x86::cvtph2ps::<8>(dst, x)
                            }
                        }
                        true
                    }
                    Builtin::X86PsllD128 | Builtin::X86PsrlD128 => {
                        let RetDest::Indirect(dst) = ret else {
                            engine_abort("ps{l,r}l.d 返回形态不是 indirect vector");
                        };
                        let dst = eval_place_addr(ctx, base, dst) as *mut u8;
                        let (x, c) = (a(0) as *const u8, a(1) as *const u8);
                        unsafe {
                            if matches!(builtin, Builtin::X86PsllD128) {
                                super::x86::pshift32::<4, true>(dst, x, c)
                            } else {
                                super::x86::pshift32::<4, false>(dst, x, c)
                            }
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
                    // 托管 Rust Heap（D3：mimalloc 后端，真地址直出）。
                    // 自定义 #[global_allocator]（corpus 批7 c_mimalloc 实锤修）：
                    // 分配是**程序级**语义——本模块登记 shim 时，任何镜像来源的
                    // builtin 臂（含 base 按 Default 会话烘的）一律经 guest shim
                    // 走用户分配器，否则跨堆 free = mimalloc 元数据 SIGSEGV。
                    Builtin::RustAlloc => match module.custom_alloc_shims {
                        Some(s) => {
                            let (lo, _) = call_guarding_terminate(unwind, || {
                                call_guest(ctx, s.alloc, &[a(0), a(1)])
                            });
                            lo
                        }
                        None => super::heap::alloc(a(0), a(1)),
                    },
                    Builtin::RustAllocZeroed => match module.custom_alloc_shims {
                        Some(s) => {
                            let (lo, _) = call_guarding_terminate(unwind, || {
                                call_guest(ctx, s.alloc_zeroed, &[a(0), a(1)])
                            });
                            lo
                        }
                        None => super::heap::alloc_zeroed(a(0), a(1)),
                    },
                    Builtin::RustRealloc => match module.custom_alloc_shims {
                        Some(s) => {
                            let (lo, _) = call_guarding_terminate(unwind, || {
                                call_guest(ctx, s.realloc, &[a(0), a(1), a(2), a(3)])
                            });
                            lo
                        }
                        None => super::heap::realloc(a(0), a(1), a(2), a(3)),
                    },
                    Builtin::RustDealloc => {
                        match module.custom_alloc_shims {
                            Some(s) => {
                                let _ = call_guarding_terminate(unwind, || {
                                    call_guest(ctx, s.dealloc, &[a(0), a(1), a(2)])
                                });
                            }
                            None => super::heap::dealloc(a(0), a(1), a(2)),
                        }
                        0
                    }
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
                    // fork（D8f）：仅 guest 单线程放行（子进程=全进程拷贝，解释器状态
                    // 天然一致；无其他 guest 线程 ⇒ 无跨线程锁死锁面）。多线程 fork
                    // 响亮拒绝（native 下同为雷区）。exec 族走 foreign 直通，不经此。
                    Builtin::HostFork => {
                        if super::ctx::guest_spawned_threads() {
                            engine_abort(
                                "fork() 时 guest 已派生额外线程：多线程 fork 后仅 forking \
                                 线程存活、其他线程持有的锁在子进程永久锁死（native 亦 UB）。\
                                 仅 guest 单线程时放行（D8f/D8l）",
                            );
                        }
                        unsafe { libc::fork() as u64 }
                    }
                    // atexit 家族（D8g）：登记 guest 回调，返回 0（成功）。
                    // __cxa_atexit(fn, arg, dso)：fn 收 arg；on_exit(fn, arg)：fn 收
                    //（status, arg）。atexit(fn)：无参。统一存 (fn, 形态, arg)。
                    Builtin::HostAtexit => atexit_register(a(0), AtexitKind::Plain, 0),
                    Builtin::HostCxaAtexit => atexit_register(a(0), AtexitKind::CxaArg, a(1)),
                    Builtin::HostOnExit => atexit_register(a(0), AtexitKind::OnExit, a(1)),
                    Builtin::HostSignal => {
                        let (signum, handler) = (a(0) as libc::c_int, a(1) as libc::sighandler_t);
                        // guest handler（非 DFL/IGN）：async 信号 → 物化 AS-trampoline
                        //（D8d）；sync 故障信号 → 响亮拒绝（宿主/guest 故障不可分辨）。
                        let real = if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
                            signal_thunk(ctx, signum, handler as u64)
                        } else {
                            handler
                        };
                        unsafe { libc::signal(signum, real) as u64 }
                    }
                    Builtin::HostSigaction => {
                        let (signum, act, oldact) = (a(0) as libc::c_int, a(1), a(2));
                        // guest handler 藏在 sigaction 结构里：thunk 后写一份改过 handler
                        // 的副本给内核（原结构不动——guest 可能复用/读回）。
                        let patched: Option<libc::sigaction> = (act != 0).then(|| {
                            let mut p = *unsafe { &*(act as *const libc::sigaction) };
                            let h = p.sa_sigaction;
                            if h != libc::SIG_DFL && h != libc::SIG_IGN {
                                p.sa_sigaction = signal_thunk(ctx, signum, h as u64) as usize;
                            }
                            p
                        });
                        let act_ptr = patched
                            .as_ref()
                            .map_or(std::ptr::null(), |p| p as *const libc::sigaction);
                        unsafe {
                            libc::sigaction(signum, act_ptr, oldact as *mut libc::sigaction) as u64
                        }
                    }
                    Builtin::Unsupported(name) => {
                        engine_abort(&format!("unsupported builtin `{}`", name.0))
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
                                    args: vec![FfiKind::I32, FfiKind::Ptr],
                                    ret: FfiKind::Void,
                                    fixed: None,
                                    thunk_args: vec![],
                                };
                                super::ffi::call_addr(cleanup as usize, &sig, &av, None);
                            }
                        }
                        0
                    }
                    // backtrace 影子帧（D8e）
                    Builtin::UnwindBacktrace => unwind_backtrace(ctx, a(0), a(1)),
                    Builtin::UnwindGetIp => mem_read(a(0), Width::W64),
                    Builtin::UnwindGetIpInfo => {
                        // (ctx, *ip_before_insn) → IP；*ip_before_insn=0（合成帧无此区分）
                        if a(1) != 0 {
                            mem_write(a(1), Width::W32, 0);
                        }
                        mem_read(a(0), Width::W64)
                    }
                    // 合成 IP 即函数入口 → 返回 ip 自身（enclosing fn start）
                    Builtin::UnwindFindEnclosing => a(0),
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
                    Builtin::X86Crc32U8 => unsafe {
                        u64::from(super::x86::crc32_u8(a(0) as u32, a(1) as u8))
                    },
                    Builtin::X86Crc32U16 => unsafe {
                        u64::from(super::x86::crc32_u16(a(0) as u32, a(1) as u16))
                    },
                    Builtin::X86Crc32U32 => unsafe {
                        u64::from(super::x86::crc32_u32(a(0) as u32, a(1) as u32))
                    },
                    Builtin::X86Crc32U64 => unsafe {
                        super::x86::crc32_u64(a(0), a(1))
                    },
                    Builtin::X86Pshufb128
                    | Builtin::X86Pshufb256
                    | Builtin::X86Sha256Msg1
                    | Builtin::X86Sha256Msg2
                    | Builtin::X86Sha256Rnds2
                    | Builtin::X86PsadBw128
                    | Builtin::X86PsadBw256
                    | Builtin::X86Pclmulqdq
                    | Builtin::X86AesEnc
                    | Builtin::X86AesEncLast
                    | Builtin::X86AesDec
                    | Builtin::X86AesDecLast
                    | Builtin::X86AesImc
                    | Builtin::X86AesKeygenAssist
                    | Builtin::X86Permd256
                    | Builtin::X86GatherQPd256
                    | Builtin::X86GatherDPd256
                    | Builtin::X86Pmadd52Lo128
                    | Builtin::X86Pmadd52Hi128
                    | Builtin::X86Pmadd52Lo256
                    | Builtin::X86Pmadd52Hi256
                    | Builtin::X86Pmadd52Lo512
                    | Builtin::X86Pmadd52Hi512
                    | Builtin::X86PmaddUbSw128
                    | Builtin::X86PmaddUbSw256
                    | Builtin::X86PmaddWd128
                    | Builtin::X86PmaddWd256
                    | Builtin::X86Cvtps2ph128
                    | Builtin::X86Cvtph2ps128
                    | Builtin::X86Cvtps2ph256
                    | Builtin::X86Cvtph2ps256
                    | Builtin::X86MaxPs128
                    | Builtin::X86MinPs128
                    | Builtin::X86MaxPs256
                    | Builtin::X86MinPs256
                    | Builtin::X86CmpPs128
                    | Builtin::X86CmpPs256
                    | Builtin::X86RoundPs128
                    | Builtin::X86RoundPs256
                    | Builtin::X86CvtPs2dq128
                    | Builtin::X86CvttPs2dq128
                    | Builtin::X86CvtPs2dq256
                    | Builtin::X86CvttPs2dq256
                    | Builtin::X86BlendvPs128
                    | Builtin::X86BlendvPs256
                    | Builtin::X86Lddqu128
                    | Builtin::X86Lddqu256
                    | Builtin::X86PsllD128
                    | Builtin::X86PsrlD128 => {
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
                    match op {
                        AsmIoVal::Scalar(o) => {
                            let (v, _) = eval_operand(ctx, base, o);
                            unsafe {
                                std::ptr::write_unaligned(bufp.add(*off as usize) as *mut u64, v)
                            };
                        }
                        // 批10：向量字节通道（xmm/ymm/zmm 16/32/64B 全宽拷贝）
                        AsmIoVal::VecBytes(pe, size) => {
                            let src = eval_place_addr(ctx, base, pe);
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    src as *const u8,
                                    bufp.add(*off as usize),
                                    *size as usize,
                                )
                            };
                        }
                    }
                }
                let addr = module.asm_stub_addrs[*stub as usize];
                let f: unsafe extern "C" fn(*mut u8) =
                    unsafe { std::mem::transmute::<u64, unsafe extern "C" fn(*mut u8)>(addr) };
                unsafe { f(bufp) };
                for (off, dst) in outs {
                    match dst {
                        AsmIoDst::Scalar(sp) => {
                            let v = unsafe {
                                std::ptr::read_unaligned(bufp.add(*off as usize) as *const u64)
                            };
                            place_write(ctx, base, sp, v);
                        }
                        AsmIoDst::VecBytes(pe, size) => {
                            let dst_addr = eval_place_addr(ctx, base, pe);
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    bufp.add(*off as usize) as *const u8,
                                    dst_addr as *mut u8,
                                    *size as usize,
                                )
                            };
                        }
                    }
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
    ATEXIT_SHARED.set(shared); // D8g：退出 trampoline 找回引擎
    super::ctx::set_fork_baseline(); // D8f：钉住单 guest 线程的 fork 守卫基线
    let args = [
        entry.main_addr,
        entry.argc,
        entry.argv_ptr,
        entry.sigpipe as u64,
    ];
    match panic::catch_unwind(AssertUnwindSafe(|| {
        call_guest(ctx_ptr, entry.lang_start, &args).0
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
    super::ctx::set_fork_baseline(); // D8f
    match panic::catch_unwind(AssertUnwindSafe(|| call_guest(ctx_ptr, id, args).0)) {
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
