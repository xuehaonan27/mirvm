//! Address-taken analysis for one function body: which frame offsets must stay in
//! stack-frame memory instead of being promoted to SSA slots.
//!
//! The model is conservative and interval-based: any frame offset that a place
//! channel's byte interval can touch stays on the frame. Over-approximating only
//! costs speed; a missed offset would be promoted to an SSA slot while the JIT
//! writes the physical frame, so readers would see a stale zero -- a wrong-value
//! miscompile.

use super::*;

/// Set of frame offsets that must be materialized in stack-frame memory.
///
/// Criterion: over-approximate rather than miss. A wrongly promoted slot -- one
/// whose address is taken but which stayed in SSA -- is a wrong-value miscompile,
/// while an extra frame slot only costs speed. Each place channel (Ref/AddrOf/
/// Copy/Repeat/Volatile, ...) therefore contributes its whole byte interval, not
/// just its base: a scalar stored through a place channel and read back would
/// otherwise read the physical frame (0 or stale) instead of the SSA value.
///
/// The Stmt/Terminator matches below enumerate every place channel, so adding a
/// variant is a non-exhaustive-match compile error rather than a silent miss.
#[derive(Default)]
pub(super) struct FrameMap {
    ranges: Vec<(u32, u32)>,
    /// A live ZST address in a zero-byte frame: with `fsz == 0`, `&Local(0)` is a
    /// legal one-past-the-end ZST address that the interval model cannot express,
    /// so the frame is forced to materialize at size 1.
    force: bool,
}

impl FrameMap {
    fn add(&mut self, a: u32, b: u32) {
        if a < b {
            self.ranges.push((a, b));
        }
    }
    pub(super) fn contains(&self, off: u32) -> bool {
        self.ranges.iter().any(|&(a, b)| a <= off && off < b)
    }
    /// Whether the frame needs a stack slot; a zero-byte frame still does when
    /// `force` is set.
    pub(super) fn needs_frame(&self) -> bool {
        !self.ranges.is_empty() || self.force
    }
}

/// How far a place reaches from its base: `Bytes(n)` covers `n` bytes from the
/// base; `Escape` means the address escapes (Ref/AddrOf/Indirect return the
/// location) so the extent is locally unknown and covers to the end of the frame.
#[derive(Clone, Copy)]
enum Extent {
    Bytes(u32),
    Escape,
}

pub(super) fn analyze_frame(body: &ir::FuncBody) -> FrameMap {
    let fsz = body.frame_size;
    /// Only the frame-relative prefix before the first Deref or dynamic step is
    /// scanned. With each Offset step added to `pos`:
    /// - Deref: the 8-byte pointer slot itself lands on the frame; the pointee is
    ///   not frame-relative, so scanning stops there;
    /// - dynamic step (IndexScaled/VTableAlignOffset): the address is only known at
    ///   run time, so conservatively cover `[pos, fsz)`;
    /// - steps exhausted: cover `[pos, pos+n)` or `[pos, fsz)` per the extent.
    fn scan_place(out: &mut FrameMap, pe: &ir::PlaceExpr, extent: Extent, fsz: u32) {
        let ir::PlaceBase::Local(base) = pe.base else {
            return;
        };
        let mut pos: i64 = base as i64;
        for step in pe.steps.iter() {
            match step {
                ir::PlaceStep::Offset(k) => pos += *k as i64,
                ir::PlaceStep::Deref => {
                    let p = pos.clamp(0, fsz as i64) as u32;
                    out.add(p, p.saturating_add(8).min(fsz));
                    return;
                }
                ir::PlaceStep::IndexScaled { .. } | ir::PlaceStep::VTableAlignOffset { .. } => {
                    let p = pos.clamp(0, fsz as i64) as u32;
                    out.add(p, fsz);
                    return;
                }
            }
        }
        let p = pos.clamp(0, fsz as i64) as u32;
        let end = match extent {
            Extent::Bytes(n) => p.saturating_add(n).min(fsz),
            Extent::Escape => fsz,
        };
        // An address taken at the very end of the frame (`p == fsz`) produces the
        // degenerate interval `(fsz, fsz)`, which `FrameMap::add` drops through its
        // `a < b` guard. If that empties the set, no frame slot is created and
        // `addr_of_local` trips its "must land on the frame" expect. The address is
        // legal one-past-the-end and a ZST is never dereferenced, so anchor one live
        // byte inside the frame to force materialization. A zero-byte frame has no
        // such byte, so record `force` instead: the frame materializes at size 1
        // with the address still `base + 0`.
        if p == end {
            if let Some(anchor) = fsz.checked_sub(1) {
                out.add(anchor, fsz);
            } else {
                out.force = true;
            }
            return;
        }
        out.add(p, end);
    }
    fn scan_op(out: &mut FrameMap, op: &Operand, fsz: u32) {
        match op {
            Operand::Mem { expr, width } => {
                scan_place(out, expr, Extent::Bytes(width.bytes()), fsz)
            }
            Operand::AddrOf(expr) => scan_place(out, expr, Extent::Escape, fsz),
            Operand::SubImm { base, .. } => scan_op(out, base, fsz),
            Operand::Slot(_) | Operand::Imm { .. } | Operand::AddrImm(_) => {}
        }
    }
    fn scan_sp(out: &mut FrameMap, sp: &ScalarPlace, fsz: u32) {
        if let ScalarPlace::Mem { expr, width } = sp {
            scan_place(out, expr, Extent::Bytes(width.bytes()), fsz);
        }
    }
    fn scan_ret(out: &mut FrameMap, r: &RetDest, fsz: u32) {
        match r {
            RetDest::Ignore => {}
            RetDest::Scalar(sp) => scan_sp(out, sp, fsz),
            RetDest::Pair(a, b) => {
                scan_sp(out, a, fsz);
                scan_sp(out, b, fsz);
            }
            // The callee writes a whole return aggregate through sret, so the size
            // is locally unknown and the place escapes.
            RetDest::Indirect(pe) => scan_place(out, pe, Extent::Escape, fsz),
        }
    }
    fn scan_rv(out: &mut FrameMap, rv: &ir::Rvalue, fsz: u32) {
        use crate::vm::ir::Rvalue as R;
        match rv {
            R::Ref(pe) => scan_place(out, pe, Extent::Escape, fsz),
            R::Use(o)
            | R::NotBits(o)
            | R::NotBool(o)
            | R::Neg(o)
            | R::Cast { a: o, .. }
            | R::BitUn { a: o, .. } => scan_op(out, o, fsz),
            R::IntBin { a, b, .. }
            | R::IntCmp { a, b, .. }
            | R::PtrDiff { a, b, .. }
            | R::UMax { a, b }
            | R::IntSat { a, b, .. }
            | R::MemCmp { a, b, .. }
            | R::IntCmp3 { a, b, .. }
            | R::FloatBin { a, b, .. }
            | R::FloatCmp { a, b, .. }
            | R::MathBin { a, b, .. } => {
                scan_op(out, a, fsz);
                scan_op(out, b, fsz);
            }
            R::PtrOffset { ptr, count, .. } => {
                scan_op(out, ptr, fsz);
                scan_op(out, count, fsz);
            }
            R::MathFma { a, b, c, .. } => {
                scan_op(out, a, fsz);
                scan_op(out, b, fsz);
                scan_op(out, c, fsz);
            }
            R::NicheDiscr { tag, .. }
            | R::MathUn { a: tag, .. }
            | R::FloatNeg { a: tag, .. }
            | R::FloatCast { a: tag, .. }
            | R::FloatToInt { a: tag, .. }
            | R::IntToFloat { a: tag, .. }
            | R::AtomicLoad { addr: tag, .. } => scan_op(out, tag, fsz),
            R::F128Cmp { a, b, .. } | R::Cmp128 { a, b, .. } => {
                scan_place(out, a, Extent::Bytes(16), fsz);
                scan_place(out, b, Extent::Bytes(16), fsz);
            }
            R::SimdBitmask {
                a,
                lanes,
                lane_bytes,
            }
            | R::SimdReduce {
                a,
                lanes,
                lane_bytes,
                ..
            }
            | R::SimdReduceArith {
                a,
                lanes,
                lane_bytes,
                ..
            } => scan_place(
                out,
                a,
                Extent::Bytes(*lanes as u32 * *lane_bytes as u32),
                fsz,
            ),
            R::TlsRef(_) => {}
        }
    }
    /// Byte width of a SIMD place: the whole vector, `lanes * lane_bytes`.
    fn simd_ext(lanes: &u16, lane_bytes: &u8) -> Extent {
        Extent::Bytes(*lanes as u32 * *lane_bytes as u32)
    }
    /// Byte width of the Repeat family: `count * elem_size`, saturating. The clamp
    /// to the frame end happens inside `scan_place`.
    fn rep_ext(count: &u64, elem_size: &u64) -> Extent {
        Extent::Bytes(count.saturating_mul(*elem_size).min(u32::MAX as u64) as u32)
    }
    let mut out = FrameMap::default();
    // ABI flattening puts two ranges on the frame: each Indirect parameter's byte
    // interval, which is the prologue memmove destination, and an indirect
    // return's ret_off interval, which is the Return memmove source.
    for p in &body.params {
        if let ParamAbi::Indirect { off, size } = p {
            out.add(*off, off.saturating_add(*size).min(fsz));
        }
    }
    if let RetAbi::Indirect { ret_off, size, .. } = &body.ret {
        out.add(*ret_off, ret_off.saturating_add(*size).min(fsz));
    }
    for blk in &body.blocks {
        for st in &blk.stmts {
            match st {
                Stmt::Assign { dst, rv } => {
                    scan_sp(&mut out, dst, fsz);
                    scan_rv(&mut out, rv, fsz);
                }
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    scan_op(&mut out, a, fsz);
                    scan_op(&mut out, b, fsz);
                    scan_sp(&mut out, dst_val, fsz);
                    scan_sp(&mut out, dst_flag, fsz);
                }
                // Copy/Repeat/Volatile place channels land their whole byte
                // interval on the frame; recording only the base would promote the
                // slots inside the interval to SSA and miscompile.
                Stmt::Copy { dst, src, size } => {
                    scan_place(&mut out, dst, Extent::Bytes(*size), fsz);
                    scan_place(&mut out, src, Extent::Bytes(*size), fsz);
                }
                Stmt::RepeatScalar {
                    dst,
                    val,
                    count,
                    elem_size,
                } => {
                    scan_place(&mut out, dst, rep_ext(count, &(*elem_size as u64)), fsz);
                    scan_op(&mut out, val, fsz);
                }
                Stmt::RepeatBytes {
                    first,
                    count,
                    elem_size,
                } => scan_place(&mut out, first, rep_ext(count, elem_size), fsz),
                Stmt::VolatileLoad { addr, dst, size } => {
                    scan_op(&mut out, addr, fsz);
                    scan_place(&mut out, dst, Extent::Bytes(*size), fsz);
                }
                Stmt::VolatileStore { addr, src, size } => {
                    scan_op(&mut out, addr, fsz);
                    scan_place(&mut out, src, Extent::Bytes(*size), fsz);
                }
                Stmt::AtomicStore { addr, val, .. } => {
                    scan_op(&mut out, addr, fsz);
                    scan_op(&mut out, val, fsz);
                }
                Stmt::AtomicCxchg {
                    addr,
                    expected,
                    new,
                    dst_val,
                    dst_ok,
                    ..
                } => {
                    scan_op(&mut out, addr, fsz);
                    scan_op(&mut out, expected, fsz);
                    scan_op(&mut out, new, fsz);
                    scan_sp(&mut out, dst_val, fsz);
                    scan_sp(&mut out, dst_ok, fsz);
                }
                Stmt::AtomicRmw { addr, val, dst, .. } => {
                    scan_op(&mut out, addr, fsz);
                    scan_op(&mut out, val, fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::MemCopy {
                    dst, src, count, ..
                } => {
                    scan_op(&mut out, dst, fsz);
                    scan_op(&mut out, src, fsz);
                    scan_op(&mut out, count, fsz);
                }
                Stmt::MemSet {
                    dst, val, count, ..
                } => {
                    scan_op(&mut out, dst, fsz);
                    scan_op(&mut out, val, fsz);
                    scan_op(&mut out, count, fsz);
                }
                Stmt::SimdBin {
                    dst,
                    a,
                    b,
                    lanes,
                    lane_bytes,
                    ..
                }
                | Stmt::SimdSelectBitmask {
                    dst,
                    a,
                    b,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdFma {
                    dst,
                    a,
                    b,
                    c,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, c, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdUn {
                    dst,
                    a,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdCast {
                    dst,
                    src,
                    lanes,
                    src_bytes,
                    dst_bytes,
                    ..
                } => {
                    scan_place(
                        &mut out,
                        dst,
                        Extent::Bytes(*lanes as u32 * *dst_bytes as u32),
                        fsz,
                    );
                    scan_place(
                        &mut out,
                        src,
                        Extent::Bytes(*lanes as u32 * *src_bytes as u32),
                        fsz,
                    );
                }
                Stmt::SimdExtractDyn {
                    src,
                    idx,
                    dst,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, src, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, idx, fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::SimdArithOffset {
                    ptrs,
                    offsets,
                    dst,
                    lanes,
                    ..
                } => {
                    // Address vector: `lanes * 8` bytes, since pointers and offsets
                    // are machine-word sized.
                    let ext = Extent::Bytes(*lanes as u32 * 8);
                    scan_place(&mut out, ptrs, ext, fsz);
                    scan_place(&mut out, offsets, ext, fsz);
                    scan_place(&mut out, dst, ext, fsz);
                }
                Stmt::SimdFunnel {
                    dst,
                    a,
                    b,
                    shift,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, shift, simd_ext(lanes, lane_bytes), fsz);
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
                    // Mask lanes are `mask_bytes` wide, possibly a different width
                    // than the data lanes, matching the interpreter.
                    scan_place(&mut out, mask, simd_ext(lanes, mask_bytes), fsz);
                    scan_place(&mut out, a, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, b, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
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
                    scan_place(&mut out, passthru, simd_ext(lanes, lane_bytes), fsz);
                    // Pointer lanes are always 8 bytes, independent of the data
                    // lane width, matching the interpreter.
                    scan_place(&mut out, ptrs, Extent::Bytes(*lanes as u32 * 8), fsz);
                    scan_place(&mut out, mask, simd_ext(lanes, mask_bytes), fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdScatter {
                    values,
                    ptrs,
                    mask,
                    mask_bytes,
                    lanes,
                    lane_bytes,
                } => {
                    scan_place(&mut out, values, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, ptrs, Extent::Bytes(*lanes as u32 * 8), fsz);
                    scan_place(&mut out, mask, simd_ext(lanes, mask_bytes), fsz);
                }
                Stmt::SimdMaskedLoad {
                    mask,
                    mask_bytes,
                    base,
                    passthru,
                    dst,
                    lanes,
                    lane_bytes,
                } => {
                    scan_place(&mut out, mask, simd_ext(lanes, mask_bytes), fsz);
                    scan_op(&mut out, base, fsz);
                    scan_place(&mut out, passthru, simd_ext(lanes, lane_bytes), fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdMaskedStore {
                    mask,
                    mask_bytes,
                    base,
                    values,
                    lanes,
                    lane_bytes,
                } => {
                    scan_place(&mut out, mask, simd_ext(lanes, mask_bytes), fsz);
                    scan_op(&mut out, base, fsz);
                    scan_place(&mut out, values, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdInsertDyn {
                    src,
                    idx,
                    val,
                    dst,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, src, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, idx, fsz);
                    scan_op(&mut out, val, fsz);
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                }
                Stmt::SimdSplat {
                    dst,
                    val,
                    lanes,
                    lane_bytes,
                    ..
                } => {
                    scan_place(&mut out, dst, simd_ext(lanes, lane_bytes), fsz);
                    scan_op(&mut out, val, fsz);
                }
                // 128-bit family: place channels are 16 bytes wide, except an
                // overflow-producing dst, which is a (u128, bool) layout with the
                // flag at dst+16, i.e. 17 bytes of footprint. Under-covering it
                // would promote the flag slot to SSA, so the JIT would write the
                // physical frame while readers take the SSA zero.
                Stmt::Bin128 {
                    a,
                    b,
                    dst,
                    with_overflow,
                    ..
                } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    if let ir::Bin128Rhs::Wide(w) = b {
                        scan_place(&mut out, w, Extent::Bytes(16), fsz);
                    }
                    let dext = Extent::Bytes(if *with_overflow { 17 } else { 16 });
                    scan_place(&mut out, dst, dext, fsz);
                }
                Stmt::Sat128 { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    scan_place(&mut out, b, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::Wide128ToFloat { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::FloatToWide128 { src, dst, .. } => {
                    scan_op(&mut out, src, fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::Bit128 { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::Bit128Count { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::F128Bin { a, b, dst, .. } | Stmt::F128Fma { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    scan_place(&mut out, b, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128MathBin { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    if let ir::F128Rhs::Wide(w) = b {
                        scan_place(&mut out, w, Extent::Bytes(16), fsz);
                    }
                    if let ir::F128Rhs::Scalar(o) = b {
                        scan_op(&mut out, o, fsz);
                    }
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128Un { a, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128FromScalar { src, dst, .. } => {
                    scan_op(&mut out, src, fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::F128ToScalar { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::F128FromWideInt { src, dst, .. } | Stmt::F128ToWideInt { src, dst, .. } => {
                    scan_place(&mut out, src, Extent::Bytes(16), fsz);
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
                }
                Stmt::NicheDiscr128 { tag, dst, .. } => {
                    scan_place(&mut out, tag, Extent::Bytes(16), fsz);
                    scan_sp(&mut out, dst, fsz);
                }
                Stmt::Trap(_) | Stmt::Nop | Stmt::Fence { .. } => {}
            }
        }
        match &blk.term {
            Terminator::Goto(_) | Terminator::Return | Terminator::Unreachable => {}
            Terminator::SwitchInt { discr, .. } => match discr {
                SwitchDiscr::Scalar(o) => scan_op(&mut out, o, fsz),
                SwitchDiscr::Wide(pe) => scan_place(&mut out, pe, Extent::Bytes(16), fsz),
            },
            Terminator::Call { args, ret, .. }
            | Terminator::CallBuiltin { args, ret, .. }
            | Terminator::CallForeign { args, ret, .. } => {
                for a in args {
                    scan_op(&mut out, a, fsz);
                }
                scan_ret(&mut out, ret, fsz);
            }
            // Scan the callee operand too: a fn-ptr can read a frame slot through a
            // Mem/Deref chain.
            Terminator::CallIndirect {
                callee, args, ret, ..
            } => {
                scan_op(&mut out, callee, fsz);
                for a in args {
                    scan_op(&mut out, a, fsz);
                }
                scan_ret(&mut out, ret, fsz);
            }
            Terminator::InlineAsm { ins, outs, .. } => {
                for (_, o) in ins {
                    match o {
                        ir::AsmIoVal::Scalar(o) => scan_op(&mut out, o, fsz),
                        ir::AsmIoVal::VecBytes(pe, size) => {
                            scan_place(&mut out, pe, Extent::Bytes(*size), fsz)
                        }
                    }
                }
                for (_, d) in outs {
                    match d {
                        ir::AsmIoDst::Scalar(sp) => scan_sp(&mut out, sp, fsz),
                        ir::AsmIoDst::VecBytes(pe, size) => {
                            scan_place(&mut out, pe, Extent::Bytes(*size), fsz)
                        }
                    }
                }
            }
            Terminator::Resume | Terminator::TerminateAbort | Terminator::Trap(_) => {}
        }
    }
    // Indirect ABI, conservatively included because it is the most address-taken
    // case: the slot itself holds an sret or parameter pointer, 8 bytes.
    if let RetAbi::Indirect {
        ret_off, sret_off, ..
    } = &body.ret
    {
        out.add(*ret_off, (*ret_off).saturating_add(8).min(fsz));
        out.add(*sret_off, (*sret_off).saturating_add(8).min(fsz));
    }
    for p in &body.params {
        if let ParamAbi::Indirect { off, .. } = p {
            out.add(*off, (*off).saturating_add(8).min(fsz));
        }
    }
    out
}
