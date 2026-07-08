//! 类型化 interp_frame：M4 引擎解释器。
//!
//! 结构与 spike3 同形（Call 宿主递归=模型 A、Return 拷回、restore、raw-ptr ctx +
//! 字段级瞬态借用）。M4.1：place 求值（地址表达式 → 真地址裸读写，帧/堆/statics 统一）
//! + 调用约定 v2（标量 1 槽 / pair 2 槽 / 大聚合 indirect+sret）。
//! unwind 边本期只记录（Assert 失败/除零/Trap = 引擎诊断退出；M4.2 接 CleanupGuard）。

use std::process::exit;

use super::ctx::{Ctx, Shared};
use super::frame::ByteRegion;
use super::ir::{
    Block, FuncBody, IntBinOp, IntCc, Module, Operand, OvfOp, ParamAbi, PlaceBase, PlaceExpr,
    PlaceStep, RetAbi, RetDest, Rvalue, ScalarPlace, Slot, Stmt, Terminator, Width,
};

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

/// 引擎诊断退出（M4.0：Trap/Assert 失败/除零统一走这里；M4.2 起 Assert 变真 panic）。
fn engine_abort(what: &str) -> ! {
    eprintln!("mirvm[m4-engine]: {what}");
    exit(70)
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
            PlaceStep::Offset(o) => addr = addr.wrapping_add(*o as u64),
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
    let ord = if signed { sext(a, w).cmp(&sext(b, w)) } else { (a & w.mask()).cmp(&(b & w.mask())) };
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
            let x = if from.1 { sext(v, from.0) as u64 } else { v & from.0.mask() };
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
        Rvalue::NicheDiscr { tag, niche_start, variants_start, variants_len, untagged } => {
            let (t, w) = eval_operand(ctx, base, tag);
            let rel = t.wrapping_sub(*niche_start) & w.mask();
            if rel < *variants_len { variants_start + rel } else { *untagged }
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
        Rvalue::FloatToInt { from64, to, signed, a } => {
            let (av, _) = eval_operand(ctx, base, a);
            // f32→f64 精确保值 ⇒ 统一经 f64；宿主 `as` 即 Rust 饱和语义（NaN→0、越界→边界）
            let x = if *from64 { f64::from_bits(av) } else { f32::from_bits(av as u32) as f64 };
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
            let x: f64 = if from.1 { sext(av, from.0) as f64 } else { (av & from.0.mask()) as f64 };
            if *to64 {
                x.to_bits()
            } else {
                // 经 f64 中转对 ≤32 位整数无双舍入问题；u64/i64→f32 用直转
                let f: f32 = if from.1 { sext(av, from.0) as f32 } else { (av & from.0.mask()) as f32 };
                f.to_bits() as u64
            }
        }
    }
}

fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign { dst, rv } => {
            let v = eval_rvalue(ctx, base, rv);
            place_write(ctx, base, dst, v);
        }
        Stmt::AssignOverflow { op, signed, a, b, dst_val, dst_flag } => {
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
        Stmt::RepeatScalar { dst, val, count, elem_size } => {
            let d = eval_place_addr(ctx, base, dst);
            let (v, w) = eval_operand(ctx, base, val);
            debug_assert_eq!(w.bytes(), *elem_size);
            for i in 0..*count {
                mem_write(d + i * *elem_size as u64, w, v);
            }
        }
        Stmt::Trap(reason) => engine_abort(&format!("TRAP: {reason}")),
        Stmt::Nop => {}
    }
}

/// 模型 A：guest 调用 = 宿主递归（spike1/3 验证的形状）。
/// 调用约定 v2：实参展平 `&[u64]`（pair 占 2 槽、indirect 传地址），返回 (lo, hi)。
fn interp_frame(ctx: *mut Ctx, func: u32, args: &[u64]) -> (u64, u64) {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    let base = region_reserve(ctx, body.frame_size, body.frame_align);
    // prologue：按 ParamAbi 消费实参槽
    let mut ai = 0usize;
    // Indirect 返回：隐藏首实参 = 目的真地址，存入 sret 槽
    if let RetAbi::Indirect { sret_off, .. } = body.ret {
        slot_write(ctx, base, Slot { off: sret_off, width: Width::W64 }, args[ai]);
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

    let mut blk = 0usize;
    loop {
        let block: &Block = &body.blocks[blk];
        for stmt in &block.stmts {
            exec_stmt(ctx, base, stmt);
        }
        match &block.term {
            Terminator::Goto(t) => blk = *t as usize,
            Terminator::SwitchInt { discr, targets, otherwise } => {
                let (d, _) = eval_operand(ctx, base, discr);
                blk = targets
                    .iter()
                    .find(|(v, _)| *v == d as u128)
                    .map(|(_, b)| *b)
                    .unwrap_or(*otherwise) as usize;
            }
            Terminator::Call { callee, args: aops, ret, target, .. } => {
                let mut av: Vec<u64> = Vec::with_capacity(aops.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    av.push(eval_place_addr(ctx, base, dst));
                }
                av.extend(aops.iter().map(|o| eval_operand(ctx, base, o).0));
                let (lo, hi) = interp_frame(ctx, *callee, &av); // ← 宿主递归 = guest 帧上 native 栈
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
            Terminator::CallBuiltin { builtin, ret, target, .. } => {
                use super::ir::Builtin;
                let r = match builtin {
                    // 分配前哨兵：空操作
                    Builtin::NoAllocShim => 0,
                    // alloc 系：lower 已前置 Stmt::Trap（不可达）；防御性再 Trap
                    other => engine_abort(&format!(
                        "引擎原语 {other:?} 未实现（堆内建，M4.1 第 5 步）"
                    )),
                };
                match ret {
                    RetDest::Scalar(p) => place_write(ctx, base, p, r),
                    _ => {}
                }
                blk = *target as usize;
            }
            Terminator::Assert { cond, expected, msg, target, .. } => {
                let (c, _) = eval_operand(ctx, base, cond);
                if (c != 0) != *expected {
                    // M4.0：引擎诊断退出；M4.2 起变 guest panic + unwind
                    engine_abort(&format!("guest assert 失败: {msg}（fn {}）", body.name));
                }
                blk = *target as usize;
            }
            Terminator::Return => {
                let r = match body.ret {
                    RetAbi::Zst => (0, 0),
                    RetAbi::Scalar(rs) => (slot_read(ctx, base, rs), 0),
                    RetAbi::Pair(lo, hi) => (slot_read(ctx, base, lo), slot_read(ctx, base, hi)),
                    RetAbi::Indirect { ret_off, size, sret_off } => {
                        let dst = slot_read(ctx, base, Slot { off: sret_off, width: Width::W64 });
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
                region_restore(ctx, base);
                return r;
            }
            Terminator::Unreachable => engine_abort(&format!("到达 Unreachable（fn {}）", body.name)),
            Terminator::Trap(reason) => {
                engine_abort(&format!("TRAP: {reason}（fn {}）", body.name))
            }
        }
    }
}

/// dev 入口（M4.0 gate）：按导出名调一个函数。
pub fn run_export(shared: &Shared, name: &str, args: &[u64]) -> Result<u64, String> {
    let Some(&id) = shared.module.exports.get(name) else {
        let mut names: Vec<&str> = shared.module.exports.keys().map(|k| &**k).collect();
        names.sort();
        names.retain(|n| !n.starts_with("_ZN") && !n.starts_with("_R"));
        return Err(format!("导出函数 `{name}` 不存在；可用: {names:?}"));
    };
    let mut ctx = Ctx::new(shared);
    Ok(interp_frame(&mut ctx as *mut Ctx, id, args).0)
}
