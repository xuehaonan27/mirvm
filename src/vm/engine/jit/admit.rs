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
        // T1-d：三路比较 / niche 判别（CLIF 内联，interp 恒等式镜像）
        R::IntCmp3 { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::NicheDiscr { tag, .. } => operand_ok(tag),
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
        // T1-b：guest TLS 取址（mirvm_tls_ref 助手同本体）
        R::TlsRef(_) => true,
        // T1-d：SIMD rvalue 三件（mirvm_simd_rv 助手，interp 共享本体；
        // lane 非法形态不拒——运行期走本体同文案 abort，与 interp 行为一致）
        R::SimdBitmask { a, .. } => place_ok(a),
        R::SimdReduce { a, .. } => place_ok(a),
        R::SimdReduceArith { a, .. } => place_ok(a),
    }
}

/// callee 的 fast 签名形态（T1-a，镜像 interp ABI v2 展平序——frame-abi §10-3
/// 已被引擎冻结，JIT 只是实现同一展平，不引入第二种聚合约定）。
///
/// 展平序（与 interp prologue/Call 臂严格同序）：
/// `[sret 前插首参?] [params 展平: Scalar=1 / Pair=2 / Indirect{off,size}=1 /
/// Zst=0] [track_caller 幻影尾参 +1]`；返回 I64 数 = Zst/Indirect=0、Scalar=1、
/// Pair=2（RetAbi::Indirect 经 sret memcpy，无 I64 返回）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CalleeAbi {
    /// fast 签名 I64 实参数（含 sret 前插与幻影尾参）
    pub nparams: usize,
    /// fast 签名 I64 返回数（0/1/2）
    pub nrets: usize,
    /// RetAbi::Indirect：首参 = 目的真地址（packed 壳随之直传）
    pub sret: bool,
}

/// callee ABI 计算（T1-a 起全形态可接；Option 形态暂存，T1-d 准入放开后复审）。
pub(super) fn callee_abi(body: &ir::FuncBody) -> Option<CalleeAbi> {
    let mut nparams = usize::from(matches!(body.ret, RetAbi::Indirect { .. }));
    for p in &body.params {
        nparams += match p {
            ParamAbi::Zst => 0,
            ParamAbi::Scalar(_) => 1,
            ParamAbi::Pair(_, _) => 2,
            ParamAbi::Indirect { .. } => 1,
        };
    }
    nparams += usize::from(body.caller_loc_off.is_some());
    let nrets = match body.ret {
        RetAbi::Zst | RetAbi::Indirect { .. } => 0,
        RetAbi::Scalar(_) => 1,
        RetAbi::Pair(_, _) => 2,
    };
    Some(CalleeAbi {
        nparams,
        nrets,
        sret: matches!(body.ret, RetAbi::Indirect { .. }),
    })
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
                // T1-d：SIMD 15 件 + Sat128（mirvm_simd_stmt 助手，interp
                // simd_exec 共享本体）——place 字段 place_ok、Operand 字段
                // operand_ok、SimdExtractDyn 的 dst 是 ScalarPlace 用 mem_place_ok
                Stmt::SimdBin { dst, a, b, .. } => place_ok(dst) && place_ok(a) && place_ok(b),
                Stmt::SimdUn { dst, a, .. } => place_ok(dst) && place_ok(a),
                Stmt::SimdFma { dst, a, b, c, .. } => {
                    place_ok(dst) && place_ok(a) && place_ok(b) && place_ok(c)
                }
                Stmt::SimdFunnel {
                    dst, a, b, shift, ..
                } => place_ok(dst) && place_ok(a) && place_ok(b) && place_ok(shift),
                Stmt::SimdCast { dst, src, .. } => place_ok(dst) && place_ok(src),
                Stmt::SimdSelect {
                    mask, a, b, dst, ..
                } => place_ok(mask) && place_ok(a) && place_ok(b) && place_ok(dst),
                Stmt::SimdSelectBitmask {
                    mask, a, b, dst, ..
                } => operand_ok(mask) && place_ok(a) && place_ok(b) && place_ok(dst),
                Stmt::SimdGather {
                    passthru,
                    ptrs,
                    mask,
                    dst,
                    ..
                } => place_ok(passthru) && place_ok(ptrs) && place_ok(mask) && place_ok(dst),
                Stmt::SimdScatter {
                    values, ptrs, mask, ..
                } => place_ok(values) && place_ok(ptrs) && place_ok(mask),
                Stmt::SimdMaskedLoad {
                    mask,
                    base,
                    passthru,
                    dst,
                    ..
                } => place_ok(mask) && operand_ok(base) && place_ok(passthru) && place_ok(dst),
                Stmt::SimdMaskedStore {
                    mask, base, values, ..
                } => place_ok(mask) && operand_ok(base) && place_ok(values),
                Stmt::SimdExtractDyn { src, idx, dst, .. } => {
                    place_ok(src) && operand_ok(idx) && mem_place_ok(dst)
                }
                Stmt::SimdInsertDyn {
                    src, idx, val, dst, ..
                } => place_ok(src) && operand_ok(idx) && operand_ok(val) && place_ok(dst),
                Stmt::SimdArithOffset {
                    ptrs, offsets, dst, ..
                } => place_ok(ptrs) && place_ok(offsets) && place_ok(dst),
                Stmt::SimdSplat { dst, val, .. } => place_ok(dst) && operand_ok(val),
                Stmt::Sat128 { a, b, dst, .. } => place_ok(a) && place_ok(b) && place_ok(dst),
                // T1-d：Trap 占位（mirvm_jit_trap 助手同 interp 文案）/ Nop
                Stmt::Trap(_) | Stmt::Nop => true,
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
                // T1-c：unwind 三向全开——Continue（PLT/c2i，CFI 纯穿透）、
                // Cleanup（try_call + pad 跳 cleanup 块）、Terminate
                // （mirvm_call_terminate 助手）。callee ABI 不设限：PLT 快路按
                // CalleeAbi 形态匹配（T1-a），否则调用点直接 c2i 回解释。
                // 调用点 ret 落点同 interp 全形态（Ignore/Scalar/Pair/Indirect 前插）。
                matches!(
                    unwind,
                    UnwindAction::Continue | UnwindAction::Cleanup(_) | UnwindAction::Terminate
                ) && args.iter().all(operand_ok)
                    && matches!(
                        ret,
                        RetDest::Ignore
                            | RetDest::Scalar(ScalarPlace::Slot(_))
                            | RetDest::Pair(ScalarPlace::Slot(_), ScalarPlace::Slot(_))
                            | RetDest::Indirect(_)
                    )
                    && shared.module.funcs.get(*callee as usize).is_some()
            }
            // T1-b：CallIndirect（mirvm_call_indirect 助手，interp 臂同派发）——
            // unwind 三向全开（同 Call）；callee 与全部实参可求值；ret 全形态
            Terminator::CallIndirect {
                callee,
                args,
                ret,
                unwind,
                ..
            } => {
                matches!(
                    unwind,
                    UnwindAction::Continue | UnwindAction::Cleanup(_) | UnwindAction::Terminate
                ) && operand_ok(callee)
                    && args.iter().all(operand_ok)
                    && matches!(
                        ret,
                        RetDest::Ignore
                            | RetDest::Scalar(ScalarPlace::Slot(_))
                            | RetDest::Pair(ScalarPlace::Slot(_), ScalarPlace::Slot(_))
                            | RetDest::Indirect(_)
                    )
            }
            // T1-b：InlineAsm（asm-stub 真地址直调，槽 ABI 同 interp；ins/outs
            // 的 VecBytes place 帧分析已全量扫描——admit 即放行）
            Terminator::InlineAsm { .. } => true,
            // T1-b：CallForeign（mirvm_call_foreign 助手，interp 臂同构）——
            // unwind 三向全开（同 Call）；实参可求值；ret 形态同 interp 支持面
            // （Pair 返回 interp 亦 engine_abort——留解释即保持同诊断）
            Terminator::CallForeign {
                args, ret, unwind, ..
            } => {
                matches!(
                    unwind,
                    UnwindAction::Continue | UnwindAction::Cleanup(_) | UnwindAction::Terminate
                ) && args.iter().all(operand_ok)
                    && matches!(
                        ret,
                        RetDest::Ignore
                            | RetDest::Scalar(ScalarPlace::Slot(_))
                            | RetDest::Indirect(_)
                    )
            }
            // T1-b：CallBuiltin（mirvm_call_builtin/mirvm_alloc 助手，interp
            // exec_builtin 同一本体）——unwind 三向全开（同 Call）；实参可求值；
            // ret 落点同 interp 全形态
            Terminator::CallBuiltin {
                args, ret, unwind, ..
            } => {
                matches!(
                    unwind,
                    UnwindAction::Continue | UnwindAction::Cleanup(_) | UnwindAction::Terminate
                ) && args.iter().all(operand_ok)
                    && matches!(
                        ret,
                        RetDest::Ignore
                            | RetDest::Scalar(ScalarPlace::Slot(_))
                            | RetDest::Pair(ScalarPlace::Slot(_), ScalarPlace::Slot(_))
                            | RetDest::Indirect(_)
                    )
            }
            // T1-c：Resume（exception_slot → _Unwind_Resume 续传）与
            // TerminateAbort（mirvm_jit_terminate_abort 助手）
            Terminator::Resume | Terminator::TerminateAbort => true,
            // T1-d：Trap-stub（mirvm_jit_trap 助手，interp 同文案同 exit(70)）
            Terminator::Trap(_) => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

// ===== 编译器 =====
