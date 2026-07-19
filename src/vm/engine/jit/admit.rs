//! JIT 准入族（自 jit_compile.rs J6 整搬）：scalar_slot/operand_ok/
//! place_ok/mem_place_ok/rvalue_ok/callee_abi/admit——纯只读判定，
//! 拒绝 = 永留解释（不残次编译）。调用方 = compiler.rs 的 worker/compile。

use super::*;

pub(super) fn scalar_slot(p: &ScalarPlace) -> Option<Slot> {
    match p {
        ScalarPlace::Slot(s) => Some(*s),
        ScalarPlace::Mem { .. } => None,
    }
}

pub(super) fn operand_ok(op: &Operand) -> bool {
    match op {
        Operand::Slot(_) | Operand::Imm { .. } => true,
        // M5.4a：内存/地址操作数（place 求值通道全部内联）
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => place_ok(expr),
        Operand::SubImm { base, .. } => operand_ok(base),
    }
}

/// PlaceExpr 准入：base 全可（Local=帧槽/Static=绝对地址立即数）；
/// VTableAlignOffset 的 meta 需 operand_ok（interp 恒等式内联，2 幂/溢出 → trap）。
pub(super) fn place_ok(pe: &ir::PlaceExpr) -> bool {
    pe.steps.iter().all(|s| match s {
        ir::PlaceStep::Deref | ir::PlaceStep::Offset(_) | ir::PlaceStep::IndexScaled { .. } => true,
        ir::PlaceStep::VTableAlignOffset { meta, .. } => operand_ok(meta),
    })
}

pub(super) fn mem_place_ok(p: &ScalarPlace) -> bool {
    match p {
        ScalarPlace::Slot(_) => true,
        ScalarPlace::Mem { expr, .. } => place_ok(expr),
    }
}

pub(super) fn rvalue_ok(rv: &ir::Rvalue) -> bool {
    use crate::vm::engine::ir::Rvalue as R;
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
pub(super) fn callee_abi(body: &ir::FuncBody) -> Option<(usize, bool)> {
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

pub(super) fn admit(shared: &Shared, body: &ir::FuncBody) -> bool {
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

