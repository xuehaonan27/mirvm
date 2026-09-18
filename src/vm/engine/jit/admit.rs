//! JIT admission family (moved whole from jit_compile.rs J6): scalar_slot/operand_ok/
//! place_ok/mem_place_ok/rvalue_ok/callee_abi/admit — pure read-only decision,
//! rejection = stays interpreted forever (no substandard compilation). Caller = compiler.rs worker/compile.

use super::*;

pub(super) fn scalar_slot(p: &ScalarPlace) -> Option<Slot> {
    match p {
        ScalarPlace::Slot(s) => Some(*s),
        ScalarPlace::Mem { .. } => None,
    }
}

pub(super) fn operand_ok(op: &Operand) -> bool {
    match op {
        Operand::Slot(_) | Operand::Imm { .. } | Operand::AddrImm(_) => true,
        // M5.4a: memory/address operands (place evaluation channel fully inlined)
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => place_ok(expr),
        Operand::SubImm { base, .. } => operand_ok(base),
    }
}

/// PlaceExpr admission: all bases OK (Local=frame slot/Static=absolute address immediate);
/// VTableAlignOffset meta needs operand_ok (interp identity inlined, power-of-two/overflow → trap).
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
        // M5.4b-1: Div/Rem wired (zero check + signed MIN/-1 branch special case)
        R::IntBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::IntCmp { a, b, .. } => operand_ok(a) && operand_ok(b),
        // M5.4a memory/address family
        R::Ref(pe) => place_ok(pe),
        R::PtrOffset { ptr, count, .. } => operand_ok(ptr) && operand_ok(count),
        R::PtrDiff { a, b, stride } => *stride != 0 && operand_ok(a) && operand_ok(b),
        R::UMax { a, b } => operand_ok(a) && operand_ok(b),
        // T1-d: three-way compare / niche discriminant (CLIF inlined, mirror of interp identity)
        R::IntCmp3 { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::NicheDiscr { tag, .. } => operand_ok(tag),
        // M5.4b-1 scalar complement
        R::IntSat { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::BitUn { a, .. } => operand_ok(a),
        R::MemCmp { a, b, n } => operand_ok(a) && operand_ok(b) && operand_ok(n),
        R::AtomicLoad { addr, .. } => operand_ok(addr),
        // M5.4b-2 floating point (f16 also accepted, via helper)
        R::FloatBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::FloatCmp { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::FloatNeg { a, .. } => operand_ok(a),
        R::FloatCast { a, .. } => operand_ok(a),
        R::FloatToInt { a, .. } => operand_ok(a),
        R::IntToFloat { a, .. } => operand_ok(a),
        R::MathUn { a, .. } => operand_ok(a),
        R::MathBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::MathFma { a, b, c, .. } => operand_ok(a) && operand_ok(b) && operand_ok(c),
        // M5.4b-3 f128/128-bit comparison (place channel)
        R::F128Cmp { a, b, .. } => place_ok(a) && place_ok(b),
        R::Cmp128 { a, b, .. } => place_ok(a) && place_ok(b),
        // T1-b: guest TLS address-taking (mirvm_tls_ref helper same body)
        R::TlsRef(_) => true,
        // T1-d: SIMD rvalue triple (mirvm_simd_rv helper, interp shares body;
        // illegal lane shapes not rejected — runtime aborts with same message as body, consistent with interp)
        R::SimdBitmask { a, .. } => place_ok(a),
        R::SimdReduce { a, .. } => place_ok(a),
        R::SimdReduceArith { a, .. } => place_ok(a),
    }
}

/// callee fast-signature shape (T1-a, mirror of interp ABI v2 flattening order — frame-abi §10-3
/// already frozen by engine, JIT just implements the same flattening without introducing a second aggregate convention).
///
/// Flattening order (strictly same as interp prologue/Call arm):
/// `[sret pre-pended first arg?] [params flattened: Scalar=1 / Pair=2 / Indirect{off,size}=1 /
/// Zst=0] [track_caller phantom tail arg +1]`; I64 return count = Zst/Indirect=0, Scalar=1,
/// Pair=2 (RetAbi::Indirect via sret memcpy, no I64 return).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CalleeAbi {
    /// fast-signature I64 actual-arg count (including sret prepend and phantom tail arg)
    pub nparams: usize,
    /// fast-signature I64 return count (0/1/2)
    pub nrets: usize,
    /// RetAbi::Indirect: first arg = real destination address (packed shell passed straight through)
    pub sret: bool,
}

/// callee ABI calculation (T1-a onward all shapes acceptable — after admit's three tables exhaust any FuncBody can
/// derive it, always Some; Option shape kept for call-site convention (.expect/.filter readability), no re-review.
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
                // M5.4a: memmove/Repeat two families (per-element CLIF loop)
                Stmt::Copy { dst, src, .. } => place_ok(dst) && place_ok(src),
                Stmt::RepeatScalar { dst, val, .. } => place_ok(dst) && operand_ok(val),
                Stmt::RepeatBytes { first, .. } => place_ok(first),
                // M5.4b-1: MemCopy/MemSet/Volatile/atomic/fence
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
                // M5.4b-3: 128-bit integer family + f128 wide channel (all have destinations — CLIF I128 or helper)
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
                // T1-d: SIMD 15 items + Sat128 (mirvm_simd_stmt helper, interp
                // simd_exec shares body) — place fields use place_ok, Operand fields
                // use operand_ok, SimdExtractDyn dst is ScalarPlace so uses mem_place_ok
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
                // T1-d: Trap placeholder (mirvm_jit_trap helper same message as interp) / Nop
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
                    // discriminant value must fit in u64 (naturally holds when width ≤64; defensive assert)
                    operand_ok(op) && targets.iter().all(|(v, _)| *v <= u64::MAX as u128)
                }
                // M5.4b-3: 128-bit discriminant channel wired
                SwitchDiscr::Wide(pe) => place_ok(pe),
            },
            Terminator::Call {
                callee,
                args,
                ret,
                unwind,
                ..
            } => {
                // T1-c: unwind three-way fully open — Continue (PLT/c2i, CFI pure pass-through),
                // Cleanup (try_call + pad jump to cleanup block), Terminate
                // (mirvm_call_terminate helper). callee ABI unrestricted: PLT fast path matches
                // CalleeAbi shape (T1-a), otherwise call site falls back c2i to interpreter.
                // Call-site ret destination same full shape as interp (Ignore/Scalar/Pair/Indirect prepend).
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
            // T1-b: CallIndirect (mirvm_call_indirect helper, same dispatch as interp arm) —
            // unwind three-way fully open (same as Call); callee and all args evaluable; ret full shape
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
            // T1-b: InlineAsm (asm-stub real-address direct call, slot ABI same as interp; ins/outs
            // VecBytes place frame analysis already fully scanned — admit means approve)
            Terminator::InlineAsm { .. } => true,
            // T1-b: CallForeign (mirvm_call_foreign helper, isomorphic to interp arm) —
            // unwind three-way fully open (same as Call); args evaluable; ret shape same as interp support surface
            // (Pair return also engine_abort in interp — staying interpreted keeps same diagnosis)
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
            // T1-b: CallBuiltin (mirvm_call_builtin/mirvm_alloc helper, interp
            // exec_builtin same body) — unwind three-way fully open (same as Call); args evaluable;
            // ret destination same full shape as interp
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
            // T1-c: Resume (exception_slot → _Unwind_Resume resume) and
            // TerminateAbort (mirvm_jit_terminate_abort helper)
            Terminator::Resume | Terminator::TerminateAbort => true,
            // T1-d: Trap-stub (mirvm_jit_trap helper, same message and error code 70 as interp)
            Terminator::Trap(_) => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

// ===== compiler =====
