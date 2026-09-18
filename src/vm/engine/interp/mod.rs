//! 类型化 interp_frame：M4 引擎解释器。
//!
//! 结构与 spike3 同形（Call 宿主递归=模型 A、Return 拷回、restore、raw-ptr ctx +
//! 字段级瞬态借用）。M4.1：place 求值（地址表达式 → 真地址裸读写，帧/堆/statics 统一）
//! + 调用约定 v2（标量 1 槽 / pair 2 槽 / 大聚合 indirect+sret）。
//!
//! M4.2 unwind：guest panic 外包一层 MIRVM 自有异常，其中保留 guest 标准库
//! 原始异常指针和所属 Engine。解释帧以 raw catch 取得实际穿帧的异常指针；
//! `unwind_edge` 在每个可 unwind 终止子前设置，landing boundary 先按该异常的身份
//! 决定是否跑 cleanup，再续传。`FrameGuard` 只负责恢复操作数区和影子帧。
//! catch 点只消费本 Engine 的 guest panic；异主异常、EngineFault 和宿主异常
//! 原样续传。

use std::cell::Cell;
use std::mem::MaybeUninit;
use std::sync::Mutex;

use super::ctx::{Ctx, Engine, Shared};
use super::frame::ByteRegion;
use super::ir::{
    AsmIoDst, AsmIoVal, Bb, Block, FfiAgg, FfiKind, FfiLeaf, FuncBody, IntBinOp, IntCc, Module,
    Operand, OvfOp, ParamAbi, PlaceBase, PlaceExpr, PlaceStep, RetAbi, RetDest, Rvalue,
    ScalarPlace, Slot, Stmt, SwitchDiscr, Terminator, UnwindAction, Width,
};

#[derive(Debug)]
pub struct RunError {
    pub kind: RunErrorKind,
    pub message: String,
    pub exit_code: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunErrorKind {
    MissingEntry,
    MissingExport,
    EngineFault,
    EngineClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOutcome<T> {
    Returned(T),
    GuestPanic,
}

/// Untyped two-register return used by the trusted raw export surface.
///
/// Scalar exports use `lo`; pair returns use both words. An indirect return
/// still writes through the caller-provided sret pointer in `args[0]`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RawReturn {
    pub lo: u64,
    pub hi: u64,
}

impl<T> RunOutcome<T> {
    pub fn into_returned(self) -> Option<T> {
        match self {
            Self::Returned(value) => Some(value),
            Self::GuestPanic => None,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 发起 guest panic：内层指针仍完全由 guest 标准库管理，外层只标记
/// MIRVM 异常身份和所属 Engine。
pub(crate) fn raise_guest(exception: u64) -> ! {
    let ctx = super::ctx::current();
    let shared = unsafe { (*ctx).shared_arc() };
    super::unwind::raise_guest(shared, exception)
}

#[inline]
pub(super) fn region_reserve(ctx: *mut Ctx, size: u32, align: u32) -> usize {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.reserve(size, align)
}
#[inline]
pub(super) fn region_restore(ctx: *mut Ctx, base: usize) {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.restore(base);
}
#[inline]
pub(super) fn slot_read(ctx: *mut Ctx, base: usize, s: Slot) -> u64 {
    let r: &ByteRegion = unsafe { &(*ctx).region };
    r.read(base, s)
}
#[inline]
pub(super) fn slot_write(ctx: *mut Ctx, base: usize, s: Slot, v: u64) {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.write(base, s, v);
}

/// 真地址裸读（fast：guest 合法假设，无范围检查——真实地址模型）。
#[inline]
pub(super) fn mem_read(addr: u64, w: Width) -> u64 {
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
pub(super) fn mem_write(addr: u64, w: Width, v: u64) {
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

mod call;
mod runblocks;
mod rvalue;
mod services;
pub(crate) mod simd_exec;
mod stmt;
mod volatile;

#[cfg(feature = "cranelift")]
pub(crate) use call::exec_builtin;
pub(crate) use call::{call_guest_ffi, interp_frame, ret_abi_of};
use services::run_atexit_callbacks;

pub(crate) fn discard_engine_state(engine_id: u64) {
    services::discard_atexit_callbacks(engine_id);
}

#[cfg(test)]
pub(crate) fn seed_engine_state_for_test(engine_id: u64) {
    services::seed_atexit_callback(engine_id);
}

#[cfg(test)]
pub(crate) fn has_engine_state_for_test(engine_id: u64) -> bool {
    services::has_atexit_callbacks(engine_id)
}
#[cfg(any(test, feature = "cranelift"))]
pub(crate) use volatile::{mem_read_volatile, mem_write_volatile};

pub(crate) fn engine_abort(what: &str) -> ! {
    let ctx = super::ctx::current();
    super::unwind::raise_engine_fault(ctx, what.to_owned(), 70)
}

/// guest TLS 实例真地址（M4.4 D3）：首访惰性物化——heap 分配 + 冻结模板拷贝。
/// 每线程一份（Ctx 是 thread-local）；guest dtor 与实例内存均在线程退出链收回。
pub(crate) fn tls_addr(ctx: *mut Ctx, id: u32) -> u64 {
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
        let template = module.resolve_link_addr(t.template);
        std::ptr::copy_nonoverlapping(template as *const u8, addr as *mut u8, t.size as usize);
        let tls = &mut (*ctx).tls;
        if tls.len() <= id as usize {
            tls.resize(id as usize + 1, 0);
        }
        tls[id as usize] = addr;
    }
    addr
}

/// 地址表达式求值 → 真地址（place 求值核心；帧基址是真地址 ⇒ 全程裸地址算术）。
pub(super) fn eval_place_addr(ctx: *mut Ctx, base: usize, expr: &PlaceExpr) -> u64 {
    let mut addr = match expr.base {
        PlaceBase::Local(off) => base as u64 + off as u64,
        PlaceBase::Static(a) => unsafe { &(*(*ctx).shared).module }.resolve_link_addr(a),
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
pub(super) fn sext(bits: u64, w: Width) -> i64 {
    match w {
        Width::W8 => bits as u8 as i8 as i64,
        Width::W16 => bits as u16 as i16 as i64,
        Width::W32 => bits as u32 as i32 as i64,
        Width::W64 => bits as i64,
    }
}

pub(super) fn eval_operand(ctx: *mut Ctx, base: usize, op: &Operand) -> (u64, Width) {
    match op {
        Operand::Slot(s) => (slot_read(ctx, base, *s), s.width),
        Operand::Mem { expr, width } => {
            let addr = eval_place_addr(ctx, base, expr);
            (mem_read(addr, *width), *width)
        }
        Operand::Imm { bits, width } => (*bits, *width),
        Operand::AddrImm(addr) => (
            unsafe { &(*(*ctx).shared).module }.resolve_link_addr(*addr),
            Width::W64,
        ),
        Operand::AddrOf(expr) => (eval_place_addr(ctx, base, expr), Width::W64),
        Operand::SubImm { base: b, sub } => {
            let (v, w) = eval_operand(ctx, base, b);
            (v.wrapping_sub(*sub) & w.mask(), w)
        }
    }
}

pub(super) fn place_write(ctx: *mut Ctx, base: usize, p: &ScalarPlace, v: u64) {
    match p {
        ScalarPlace::Slot(s) => slot_write(ctx, base, *s, v),
        ScalarPlace::Mem { expr, width } => {
            let addr = eval_place_addr(ctx, base, expr);
            mem_write(addr, *width, v);
        }
    }
}

pub(super) fn int_bin(op: IntBinOp, signed: bool, a: u64, b: u64, w: Width) -> u64 {
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
pub(super) fn f128_read(p: u64) -> f128 {
    f128::from_bits(unsafe { (p as *const u128).read_unaligned() })
}
pub(super) fn f128_write(p: u64, v: f128) {
    unsafe { (p as *mut u128).write_unaligned(v.to_bits()) }
}

/// 冻结 MemOrd → 宿主 Ordering（D8j：guest 请求什么序就执行什么序）。
pub(super) fn host_ord(o: super::ir::MemOrd) -> std::sync::atomic::Ordering {
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
pub(super) fn bit_un(op: super::ir::BitUnOp, v: u64, w: Width) -> u64 {
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
pub(super) fn int_saturating(op: OvfOp, signed: bool, av: u64, bv: u64, w: Width) -> u64 {
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

pub(super) fn int_cmp(cc: IntCc, signed: bool, a: u64, b: u64, w: Width) -> u64 {
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
pub(super) fn int_ovf(op: OvfOp, signed: bool, a: u64, b: u64, w: Width) -> (u64, bool) {
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

struct FrameGuard {
    ctx: *mut Ctx,
    depth_active: bool,
    base: Option<usize>,
    shadow_active: bool,
    /// 动态 LSDA：当前可 unwind 终止子的 cleanup 边（Call 前设置、返回后清除）
    unwind_edge: Cell<Option<Bb>>,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        unsafe {
            if self.shadow_active {
                (*self.ctx).shadow.pop(); // D8e：影子帧出栈（与 depth 同生命周期）
            }
            if let Some(base) = self.base {
                region_restore(self.ctx, base);
            }
            if self.depth_active {
                (*self.ctx).depth -= 1;
            }
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
pub(crate) fn call_guest(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let jit = unsafe { &(*(*ctx).shared).jit };
    if jit.enabled {
        let call_compiled = |entry: u64| -> (u64, u64) {
            // i2c：packed 入口（M5.3b；发布序 fast→packed，Acquire 已见全部前置写）
            type Packed = extern "C-unwind" fn(*const u64, *mut u64);
            let f: Packed = unsafe { std::mem::transmute(entry as usize) };
            let mut ret = [0u64; 2];
            f(args.as_ptr(), ret.as_mut_ptr());
            (ret[0], ret[1])
        };
        // strict 失败哨兵（MIRVM_JIT_SYNC）：可准入编译失败在任何后续调用点
        // 都响亮 abort——哨兵只在 sync 模式由 worker 写入（非 sync 永不出现）
        let fail_abort = |ctx: *mut Ctx| -> ! {
            let shared = unsafe { &*(*ctx).shared };
            engine_abort(&format!(
                "JIT strict: f{func}({}) meets compilation threshold but failed to be compiled",
                shared.module.funcs[func as usize].name
            ))
        };
        // Dispatch into the current activation's code domain. This is the single
        // point where a trace run stops consulting plain entries; the plain arm
        // borrows the historical fields, so the plain path is unchanged.
        let domain_slots = jit.slots_for(crate::vm::engine::ctx::current_code_domain());
        // If there's compiled code, then call it
        let mut entry =
            domain_slots.slots[func as usize].load(std::sync::atomic::Ordering::Acquire);
        if entry == crate::vm::engine::jit::FAIL_SENTINEL && jit.sync {
            fail_abort(ctx);
        }
        if entry != 0 && entry != crate::vm::engine::jit::FAIL_SENTINEL {
            return call_compiled(entry);
        }

        // If function not compiled yet, collect statistics, may send compilation request
        // and interpret it for now.
        let prev = jit.counters[func as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Meets compilation threshold and only send compilation request exactly once.
        // Counter keeps growing later but compilation request would not be send multiple times.
        if prev + 1 == jit.threshold
            && let Some(q) = jit.queue.lock().unwrap().as_ref()
        {
            let _ = q.send(func);
        }
        // SYNC 验证模式（audit F-05）：已投递（本次或更早）→ 等待发布/
        // 失败哨兵。threshold=1 的语义由此从「首调请求编译」升为「首调
        // 同步编译发布」——逢调即编的差分从此证明编译码真被执行
        if jit.sync && prev + 1 >= jit.threshold {
            let mut spins = 0u32;
            loop {
                entry =
                    domain_slots.slots[func as usize].load(std::sync::atomic::Ordering::Acquire);
                if entry == crate::vm::engine::jit::FAIL_SENTINEL {
                    fail_abort(ctx);
                }
                if entry != 0 {
                    return call_compiled(entry);
                }
                spins += 1;
                if spins >= 1 << 28 {
                    engine_abort(
                        "JIT strict release timeout (compiler thread dead or queue broken)",
                    );
                }
                std::thread::yield_now();
            }
        }
    }
    interp_frame(ctx, func, args)
}

/// Return an uncaught guest panic to the guest standard library before the
/// Engine reports it. `cleanup` lowers the guest panic counter and yields the
/// opaque two-word panic Box; its own guest drop glue then runs the payload's
/// destructor and allocator route. No host-side Rust layout is assumed here.
fn dispose_uncaught_guest_panic(ctx: *mut Ctx, payload: super::unwind::GuestPanicPayload) {
    payload.transfer(|shared, inner| {
        let Some(plan) = shared.module.guest_panic_cleanup else {
            eprintln!("mirvm[m4-engine]: executable Module has no guest panic cleanup plan");
            std::process::abort()
        };
        match super::unwind::catch_raw(|| {
            let (data, vtable) = call_guest(ctx, plan.cleanup, &[inner]);
            let mut opaque_box = [data, vtable];
            call_guest(ctx, plan.drop_payload, &[opaque_box.as_mut_ptr() as u64]);
        }) {
            Ok(()) => {}
            Err(exception) => exception.abort_during_panic_cleanup(),
        }
    });
}

pub(crate) fn dispose_guest_panic_during_startup(
    shared: &std::sync::Arc<Shared>,
    payload: super::unwind::GuestPanicPayload,
) {
    let activation = super::ctx::activate(shared);
    dispose_uncaught_guest_panic(activation.ctx(), payload);
}

/// C1：FnId 的返回通道（thunk 分流——Indirect sret 直传 vs 小档重打包的判定源）。
pub fn run_main(engine: &Engine) -> Result<RunOutcome<i32>, RunError> {
    let lease = engine.execution_lease().map_err(|_| RunError {
        kind: RunErrorKind::EngineClosed,
        message: "Engine is closed".into(),
        exit_code: 70,
    })?;
    let shared = lease.shared();
    let Some(entry) = shared.module.entry else {
        return Err(RunError {
            kind: RunErrorKind::MissingEntry,
            message: "no `main` entry (lib crate?)".into(),
            exit_code: 2,
        });
    };
    let activation = super::ctx::activate(shared);
    let ctx_ptr = activation.ctx();
    let main_run = super::ctx::begin_main_run(ctx_ptr);
    super::ctx::set_fork_baseline(shared); // D8f：钉住单 guest 线程的 fork 守卫基线
    let args = [
        shared.module.resolve_link_addr(entry.main_addr),
        entry.argc,
        entry.argv_ptr,
        entry.sigpipe as u64,
    ];
    let outcome = match super::unwind::catch_raw(|| call_guest(ctx_ptr, entry.lang_start, &args).0)
    {
        Ok(code) => {
            if main_run.finish() {
                RunOutcome::GuestPanic
            } else {
                RunOutcome::Returned(code as i32)
            }
        }
        Err(exception) => match exception.take_mirvm(shared) {
            Ok(super::unwind::MirvmPayload::Guest(payload)) => {
                dispose_uncaught_guest_panic(ctx_ptr, payload);
                RunOutcome::GuestPanic
            }
            Ok(super::unwind::MirvmPayload::EngineFault(fault)) => {
                let fault = super::ctx::drain_current_thread_signal_deliveries_after_fault(
                    ctx_ptr,
                    fault.finish(),
                );
                return Err(RunError {
                    kind: RunErrorKind::EngineFault,
                    message: fault.message,
                    exit_code: fault.code,
                });
            }
            Ok(super::unwind::MirvmPayload::EngineClosed) => {
                return Err(RunError {
                    kind: RunErrorKind::EngineClosed,
                    message: "Engine closed during execution".into(),
                    exit_code: 70,
                });
            }
            Err(exception) => exception.resume_or_rethrow(),
        },
    };
    let exit_code = match outcome {
        RunOutcome::Returned(code) => code,
        RunOutcome::GuestPanic => 101,
    };
    run_atexit_callbacks(ctx_ptr, exit_code);
    Ok(outcome)
}

/// dev 入口（M4.0 gate）：按导出名调一个函数。
/// 顶层 catch：guest panic 穿出导出函数 = 未捕获 panic → 诊断 + 退出码 101
/// （native lang_start 语义的近似；完整启动链 M4.3）。宿主 panic（VM bug）原样续传。
///
/// # Safety
///
/// `args` is an untyped ABI slot array. It must match the export's exact
/// lowered parameters. Every slot interpreted as a pointer/reference must
/// satisfy the guest type's validity, alignment, aliasing and lifetime rules
/// for the entire call. For an indirect return, `args[0]` must be the valid
/// sret destination required by that lowered ABI; the returned `RawReturn`
/// words are meaningful only for scalar/pair return ABIs.
pub unsafe fn run_export(
    engine: &Engine,
    name: &str,
    args: &[u64],
) -> Result<RunOutcome<RawReturn>, RunError> {
    let lease = engine.execution_lease().map_err(|_| RunError {
        kind: RunErrorKind::EngineClosed,
        message: "Engine is closed".into(),
        exit_code: 70,
    })?;
    let shared = lease.shared();
    let Some(&id) = shared.module.exports.get(name) else {
        let mut names: Vec<&str> = shared.module.exports.keys().map(|k| &**k).collect();
        names.sort();
        names.retain(|n| !n.starts_with("_ZN") && !n.starts_with("_R"));
        return Err(RunError {
            kind: RunErrorKind::MissingExport,
            message: format!("export `{name}` doesn't exist, available: {names:?}"),
            exit_code: 2,
        });
    };
    let activation = super::ctx::activate(shared);
    let ctx_ptr = activation.ctx();
    super::ctx::set_fork_baseline(shared); // D8f
    match super::unwind::catch_raw(|| call_guest(ctx_ptr, id, args)) {
        Ok((lo, hi)) => {
            run_atexit_callbacks(ctx_ptr, 0);
            Ok(RunOutcome::Returned(RawReturn { lo, hi }))
        }
        Err(exception) => match exception.take_mirvm(shared) {
            Ok(super::unwind::MirvmPayload::Guest(payload)) => {
                dispose_uncaught_guest_panic(ctx_ptr, payload);
                Ok(RunOutcome::GuestPanic)
            }
            Ok(super::unwind::MirvmPayload::EngineFault(fault)) => {
                let fault = super::ctx::drain_current_thread_signal_deliveries_after_fault(
                    ctx_ptr,
                    fault.finish(),
                );
                Err(RunError {
                    kind: RunErrorKind::EngineFault,
                    message: fault.message,
                    exit_code: fault.code,
                })
            }
            Ok(super::unwind::MirvmPayload::EngineClosed) => Err(RunError {
                kind: RunErrorKind::EngineClosed,
                message: "Engine closed during execution".into(),
                exit_code: 70,
            }),
            Err(exception) => exception.resume_or_rethrow(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;

    use super::{eval_place_addr, mem_read_volatile, mem_write_volatile};
    use crate::vm::engine::ctx::{Engine, Shared};
    use crate::vm::engine::ir::{
        Block, FuncBody, Module, Operand, PlaceBase, PlaceExpr, PlaceStep, RetAbi, Rvalue,
        ScalarPlace, Slot, Stmt, Terminator, Width,
    };

    #[test]
    fn engine_fault_returns_from_export_instead_of_exiting_the_host() {
        let mut module = Module::default();
        module.funcs.push(FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::Trap("broken bytecode".into()),
            }],
            name: "trap_export".into(),
        });
        module.exports.insert("trap_export".into(), 0);
        let engine = Engine::new(Shared::new(module));

        let err = unsafe { super::run_export(&engine, "trap_export", &[]) }.unwrap_err();
        assert_eq!(err.kind, super::RunErrorKind::EngineFault);
        assert_eq!(err.exit_code, 70);
        assert!(err.message.contains("broken bytecode"), "{err}");
    }

    #[test]
    fn raw_export_preserves_both_pair_return_words() {
        let lo = Slot {
            off: 0,
            width: Width::W64,
        };
        let hi = Slot {
            off: 8,
            width: Width::W64,
        };
        let mut module = Module::default();
        module.funcs.push(FuncBody {
            frame_size: 16,
            frame_align: 8,
            ret: RetAbi::Pair(lo, hi),
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: vec![
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(lo),
                        rv: Rvalue::Use(Operand::Imm {
                            bits: 0x0123_4567_89ab_cdef,
                            width: Width::W64,
                        }),
                    },
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(hi),
                        rv: Rvalue::Use(Operand::Imm {
                            bits: 0xfedc_ba98_7654_3210,
                            width: Width::W64,
                        }),
                    },
                ],
                term: Terminator::Return,
            }],
            name: "pair_export".into(),
        });
        module.exports.insert("pair".into(), 0);
        let engine = Engine::new(Shared::new(module));

        let result = unsafe { super::run_export(&engine, "pair", &[]) }
            .unwrap()
            .into_returned()
            .unwrap();
        assert_eq!(result.lo, 0x0123_4567_89ab_cdef);
        assert_eq!(result.hi, 0xfedc_ba98_7654_3210);
    }

    #[test]
    fn dyn_tail_alignment_preserves_prefixes_larger_than_four_gibibytes() {
        let vtable = [0u64, 0, 32];
        let unaligned = u32::MAX as u64 + 18;
        let expr = PlaceExpr {
            base: PlaceBase::Local(0x1000),
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
