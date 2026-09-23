//! JIT admission: `scalar_slot`/`operand_ok`/`place_ok`/`mem_place_ok`/`rvalue_ok`/`callee_abi`/`admit`.
//! The decision is pure and read-only, and a rejection means the function stays interpreted forever rather
//! than being partially compiled. Caller = `compiler.rs` worker/compile.

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
        // Memory/address operands (place evaluation channel fully inlined).
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => place_ok(expr),
        Operand::SubImm { base, .. } => operand_ok(base),
    }
}

/// PlaceExpr admission: every base is acceptable (Local = frame slot, Static = absolute address immediate);
/// VTableAlignOffset's meta needs `operand_ok` (the interpreter identity is inlined here; a non-power-of-two
/// or overflowing meta traps).
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
    use crate::vm::ir::Rvalue as R;
    match rv {
        R::Use(a) | R::NotBits(a) | R::NotBool(a) | R::Neg(a) => operand_ok(a),
        R::Cast { a, .. } => operand_ok(a),
        // Div/Rem (zero check plus the signed MIN/-1 special case).
        R::IntBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::IntCmp { a, b, .. } => operand_ok(a) && operand_ok(b),
        // Memory/address family.
        R::Ref(pe) => place_ok(pe),
        R::PtrOffset { ptr, count, .. } => operand_ok(ptr) && operand_ok(count),
        R::PtrDiff { a, b, stride } => *stride != 0 && operand_ok(a) && operand_ok(b),
        R::UMax { a, b } => operand_ok(a) && operand_ok(b),
        // Three-way compare / niche discriminant (CLIF inlined, mirroring the interpreter identity).
        R::IntCmp3 { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::NicheDiscr { tag, .. } => operand_ok(tag),
        // Scalar complement.
        R::IntSat { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::BitUn { a, .. } => operand_ok(a),
        R::MemCmp { a, b, n } => operand_ok(a) && operand_ok(b) && operand_ok(n),
        R::AtomicLoad { addr, .. } => operand_ok(addr),
        // Floating point (f16 also accepted, via helper).
        R::FloatBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::FloatCmp { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::FloatNeg { a, .. } => operand_ok(a),
        R::FloatCast { a, .. } => operand_ok(a),
        R::FloatToInt { a, .. } => operand_ok(a),
        R::IntToFloat { a, .. } => operand_ok(a),
        R::MathUn { a, .. } => operand_ok(a),
        R::MathBin { a, b, .. } => operand_ok(a) && operand_ok(b),
        R::MathFma { a, b, c, .. } => operand_ok(a) && operand_ok(b) && operand_ok(c),
        // f128 / 128-bit comparison (place channel).
        R::F128Cmp { a, b, .. } => place_ok(a) && place_ok(b),
        R::Cmp128 { a, b, .. } => place_ok(a) && place_ok(b),
        // Guest TLS address-taking (same body as the mirvm_tls_ref helper).
        R::TlsRef(_) => true,
        // SIMD rvalue triple. The interpreter shares the mirvm_simd_rv helper body. Illegal lane shapes are
        // not rejected here: the runtime aborts with the same message as the interpreter.
        R::SimdBitmask { a, .. } => place_ok(a),
        R::SimdReduce { a, .. } => place_ok(a),
        R::SimdReduceArith { a, .. } => place_ok(a),
    }
}

/// Callee fast-signature shape, mirroring the interpreter's ABI v2 flattening order. The JIT implements
/// that same flattening rather than introducing a second aggregate convention.
///
/// Flattening order, strictly the same as the interpreter's prologue and `Call` arm:
/// `[sret pre-pended first arg?] [params flattened: Scalar=1 / Pair=2 / Indirect{off,size}=1 /
/// Zst=0] [track_caller phantom tail arg +1]`; I64 return count: Zst/Indirect = 0, Scalar = 1,
/// Pair = 2 (`RetAbi::Indirect` returns via sret memcpy, no I64 return).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CalleeAbi {
    /// fast-signature I64 actual-arg count (including sret prepend and phantom tail arg)
    pub nparams: usize,
    /// fast-signature I64 return count (0/1/2)
    pub nrets: usize,
    /// RetAbi::Indirect: first arg = real destination address (packed shell passed straight through)
    pub sret: bool,
}

/// Callee ABI calculation. Every shape is acceptable: once `admit` has accepted a body, this can always
/// derive it, so it returns `Some`. The `Option` is kept only for call-site convention
/// (`.expect`/`.filter` readability).
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
                // memmove/Repeat families (per-element CLIF loop).
                Stmt::Copy { dst, src, .. } => place_ok(dst) && place_ok(src),
                Stmt::RepeatScalar { dst, val, .. } => place_ok(dst) && operand_ok(val),
                Stmt::RepeatBytes { first, .. } => place_ok(first),
                // MemCopy/MemSet/Volatile/atomic/fence.
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
                // 128-bit integer family + f128 wide channel (all have destinations: CLIF I128 or helper).
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
                // SIMD 15 items + Sat128. The shared semantics::simd bodies back the mirvm_simd_stmt helper
                // body. place fields use place_ok, Operand fields use operand_ok, and SimdExtractDyn's
                // ScalarPlace dst uses mem_place_ok.
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
                // Trap placeholder (the mirvm_jit_trap helper uses the same message as the interpreter) / Nop.
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
                // 128-bit discriminant channel.
                SwitchDiscr::Wide(pe) => place_ok(pe),
            },
            Terminator::Call {
                callee,
                args,
                ret,
                unwind,
                ..
            } => {
                // All three unwind actions are admitted: Continue (PLT/c2i, CFI pure pass-through),
                // Cleanup (try_call + pad jump to the cleanup block) and Terminate
                // (mirvm_call_terminate helper). The callee ABI is unrestricted: the PLT fast path matches
                // the CalleeAbi shape, otherwise the call site falls back to the interpreter via c2i.
                // The call-site return destination has the same full shape as the interpreter
                // (Ignore/Scalar/Pair/Indirect prepend).
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
            // CallIndirect (mirvm_call_indirect helper, same dispatch as the interpreter arm): all three
            // unwind actions are admitted (same as Call); callee and all args are evaluable; ret full shape.
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
            // InlineAsm (asm-stub real-address direct call, slot ABI same as the interpreter). The ins/outs
            // VecBytes place frame analysis has already been fully scanned, so admitting here means approve.
            Terminator::InlineAsm { .. } => true,
            // CallForeign (mirvm_call_foreign helper, isomorphic to the interpreter arm): all three unwind
            // actions are admitted (same as Call); args are evaluable; the ret shape matches the interpreter's
            // supported surface (a Pair return also engine_aborts there, so staying interpreted keeps the
            // same diagnosis).
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
            // CallBuiltin (mirvm_call_builtin/mirvm_alloc helper, same body as the interpreter's
            // exec_builtin): all three unwind actions are admitted (same as Call); args are evaluable; the
            // ret destination has the same full shape as the interpreter.
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
            // Resume (exception_slot → _Unwind_Resume) and TerminateAbort (mirvm_jit_terminate_abort
            // helper).
            Terminator::Resume | Terminator::TerminateAbort => true,
            // Trap-stub (mirvm_jit_trap helper, same message and error code 70 as the interpreter).
            Terminator::Trap(_) => true,
        };
        if !ok {
            return false;
        }
    }
    true
}
