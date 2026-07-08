//! 类型化 interp_frame：M4 引擎解释器（M4.0 标量子集）。
//!
//! 结构与 spike3 同形（Call 宿主递归=模型 A、Return 拷回 ret 槽、restore、
//! raw-ptr ctx + 字段级瞬态借用），把 spike 的 u64 槽换成宽度类型化字节区。
//! unwind 边本期只记录（Assert 失败/除零/Trap = 引擎诊断退出；M4.2 接 CleanupGuard）。

use std::process::exit;

use super::ctx::{Ctx, Shared};
use super::frame::ByteRegion;
use super::ir::{
    Block, FuncBody, IntBinOp, IntCc, Module, Operand, OvfOp, Rvalue, Slot, Stmt, Terminator,
    Width,
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

/// 引擎诊断退出（M4.0：Trap/Assert 失败/除零统一走这里；M4.2 起 Assert 变真 panic）。
fn engine_abort(what: &str) -> ! {
    eprintln!("mirvm[m4-engine]: {what}");
    exit(70)
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
        Operand::Imm { bits, width } => (*bits, *width),
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
    }
}

fn exec_stmt(ctx: *mut Ctx, base: usize, stmt: &Stmt) {
    match stmt {
        Stmt::Assign { dst, rv } => {
            let v = eval_rvalue(ctx, base, rv);
            slot_write(ctx, base, *dst, v);
        }
        Stmt::AssignOverflow { op, signed, a, b, dst_val, dst_flag } => {
            let (av, w) = eval_operand(ctx, base, a);
            let (bv, _) = eval_operand(ctx, base, b);
            let (v, f) = int_ovf(*op, *signed, av, bv, w);
            slot_write(ctx, base, *dst_val, v);
            slot_write(ctx, base, *dst_flag, f as u64);
        }
        Stmt::Trap(reason) => engine_abort(&format!("TRAP: {reason}")),
        Stmt::Nop => {}
    }
}

/// 模型 A：guest 调用 = 宿主递归（spike1/3 验证的形状）。
fn interp_frame(ctx: *mut Ctx, func: u32, args: &[u64]) -> u64 {
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let body: &FuncBody = &module.funcs[func as usize];

    let base = region_reserve(ctx, body.frame_size, body.frame_align);
    for (p, a) in body.params.iter().zip(args) {
        if let Some(slot) = p {
            slot_write(ctx, base, *slot, *a);
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
                let av: Vec<u64> = aops.iter().map(|o| eval_operand(ctx, base, o).0).collect();
                let r = interp_frame(ctx, *callee, &av); // ← 宿主递归 = guest 帧上 native 栈
                if let Some(rs) = ret {
                    slot_write(ctx, base, *rs, r);
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
                    Some(rs) => slot_read(ctx, base, rs),
                    None => 0,
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
        return Err(format!("导出函数 `{name}` 不存在；可用: {names:?}"));
    };
    let mut ctx = Ctx::new(shared);
    Ok(interp_frame(&mut ctx as *mut Ctx, id, args))
}
