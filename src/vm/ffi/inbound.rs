//! A native call into a guest function: the C shape of the call, converted to calling convention
//! v2 and back.
//!
//! A native caller hands over its arguments the way the C ABI lays them out -- a scalar by value,
//! an aggregate as the true address of its bytes -- and expects its result in that same shape.
//! The interpreter and the JIT both take flattened slots (a pair in two, an aggregate by
//! address), so something has to translate, and this is the one place that does: the libffi
//! trampolines in `thunks` call [`marshal_args`]/[`repack_ret`] around
//! [`call_guest_ffi`], and nothing else converts.
//!
//! What an aggregate *is* was decided when the signature was frozen (`ir::FfiAgg`); this module
//! only follows that description.

use std::ffi::c_void;

use crate::vm::ctx::Ctx;
use crate::vm::dispatch::call_guest;
use crate::vm::ir::{FfiAgg, FfiKind, FfiLeaf, FuncBody, Module, ParamAbi, RetAbi};
use crate::vm::unwind::engine_abort;

/// Move args by declared width (shared by trampoline / entry_trampoline; closure arg slots only
/// guarantee declared width is valid; engine value = width-masked bits, LE).
/// C1: aggregate arg = closure avalue always points to aggregate bytes (same shape across classes)
/// → pass the real byte address; callee-side ParamAbi expansion is mapped by
/// `dispatch::call_guest_ffi` according to FfiAgg.
pub(crate) unsafe fn marshal_args(kinds: &[FfiKind], args: *const *const c_void) -> Vec<u64> {
    let mut av: Vec<u64> = Vec::with_capacity(kinds.len());
    for (i, k) in kinds.iter().enumerate() {
        let p = unsafe { *args.add(i) } as *const u8;
        let v = unsafe {
            match k {
                FfiKind::Agg(_) => p as u64,
                FfiKind::I8 | FfiKind::U8 => p.read() as u64,
                FfiKind::I16 | FfiKind::U16 => (p as *const u16).read_unaligned() as u64,
                FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => {
                    (p as *const u32).read_unaligned() as u64
                }
                FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                    (p as *const u64).read_unaligned()
                }
                FfiKind::Void => 0, // lower already rejects ZST callback args (unreachable)
            }
        };
        av.push(v);
    }
    av
}

/// C1: aggregate return by value (ret = Agg, callee RetAbi non-Indirect small class) repacking —
/// (lo,hi) writes FfiAgg field bytes back in declared order (zero whole surface first to preserve
/// padding, then overwrite field bits; bit-identical to libffi rvalue's SysV byte image). Top-level
/// nested leaf and Pair/Scalar return channels are structurally mutually exclusive (same rustc
/// layout inference — occurrence means engine invariant violation).
pub(crate) unsafe fn repack_ret(result: *mut u8, agg: &FfiAgg, lo: u64, hi: u64) {
    unsafe { std::ptr::write_bytes(result, 0, agg.size as usize) };
    for (i, f) in agg.fields.iter().enumerate() {
        let (v, leaf) = match i {
            0 => (lo, &f.leaf),
            1 => (hi, &f.leaf),
            _ => crate::vm::unwind::engine_abort(
                "C1 repack: >2 top-level fields with Pair/Scalar return channel",
            ),
        };
        let FfiLeaf::Scalar(k) = leaf else {
            crate::vm::unwind::engine_abort(
                "C1 repack: top-level nested leaf with Pair/Scalar return channel",
            );
        };
        let dst = unsafe { result.add(f.off as usize) };
        unsafe {
            match k {
                FfiKind::I8 | FfiKind::U8 => dst.write(v as u8),
                FfiKind::I16 | FfiKind::U16 => (dst as *mut u16).write_unaligned(v as u16),
                FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => {
                    (dst as *mut u32).write_unaligned(v as u32)
                }
                FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                    (dst as *mut u64).write_unaligned(v)
                }
                FfiKind::Void | FfiKind::Agg(_) => {
                    crate::vm::unwind::engine_abort("C1 repack: illegal leaf kind")
                }
            }
        }
    }
}

/// C1 inbound FFI marshalling: expands the C-side arguments that `marshal_args` produced
/// (scalars as-is, aggregates as the true address of their bytes) into ABI argument slots
/// according to the callee's `ParamAbi`, then calls `call_guest`. Shared by the thunk factory
/// and the P1 entry trampoline. `ret_addr` is libffi's result buffer for a by-value aggregate
/// return; it becomes the hidden first argument slot only when the callee returns
/// `RetAbi::Indirect` (sret passed through), while small forms are re-packed into `FfiAgg`
/// from `(lo, hi)` by the caller.
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
        av.push(ret_addr.expect(
            "C1: callee returns an aggregate by value (RetAbi::Indirect) but has no result address",
        ));
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
                None => engine_abort(&format!(
                    "C1 marshalling is missing an argument (callee fn {} params {:?})",
                    body.name, body.params
                )),
            },
            ParamAbi::Pair(_, _) => match kinds.get(ki) {
                Some(FfiKind::Agg(agg)) => {
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 0) });
                    av.push(unsafe { agg_leaf_at(*vals.get_unchecked(ki), agg, 1) });
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 marshalling mismatch: a `Pair` callee parameter met a non-scalar C argument (fn {} params {:?} kinds {:?})",
                    body.name, body.params, kinds
                )),
            },
            ParamAbi::Indirect { .. } => match kinds.get(ki) {
                Some(FfiKind::Agg(_)) => {
                    av.push(vals[ki]);
                    ki += 1;
                }
                _ => engine_abort(&format!(
                    "C1 marshalling mismatch: a by-address callee parameter met a non-scalar C argument (fn {} params {:?} kinds {:?})",
                    body.name, body.params, kinds
                )),
            },
        }
    }
    if ki != vals.len() {
        engine_abort(&format!(
            "C1 marshalling slot count mismatch: callee fn {} consumes {ki}, marshal supplies {}",
            body.name,
            vals.len()
        ));
    }
    call_guest(ctx, func, &av)
}

/// Reads field `idx` of `agg`, in declaration order, at its declared width for a scalar leaf.
/// A top-level nested leaf is structurally exclusive with the Pair/Scalar parameter forms by
/// the same rustc layout derivation, so encountering one breaks an engine invariant.
unsafe fn agg_leaf_at(addr: u64, agg: &FfiAgg, idx: usize) -> u64 {
    let Some(f) = agg.fields.get(idx) else {
        engine_abort("C1 marshalling: a Pair parameter met a single-field aggregate");
    };
    let FfiLeaf::Scalar(k) = &f.leaf else {
        engine_abort("C1 marshalling: a top-level nested leaf met a Pair parameter");
    };
    let p = addr.wrapping_add(f.off as u64) as *const u8;
    unsafe {
        match k {
            FfiKind::I8 | FfiKind::U8 => p.read() as u64,
            FfiKind::I16 | FfiKind::U16 => (p as *const u16).read_unaligned() as u64,
            FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => (p as *const u32).read_unaligned() as u64,
            FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => {
                (p as *const u64).read_unaligned()
            }
            FfiKind::Void | FfiKind::Agg(_) => engine_abort("C1 marshalling: illegal leaf kind"),
        }
    }
}
