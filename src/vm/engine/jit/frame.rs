//! FrameMap/analyze_frame（自 jit_compile.rs J9 整搬）：取址分析保守
//! 全集区间模型——任何被 place 通道【字节区间】触及的 frame offset 一律
//! 落栈帧内存（误提升 = 错值级，多落帧只是慢）。scan_* 五函数随族。

use super::*;

/// M5.4a 取址分析（保守全集，m5.4-design §3.1/Q1）：收集必须落栈帧内存的 frame
/// offset——任何被 PlaceExpr::Local/Mem/AddrOf/Ref/Copy/Repeat/Indirect-ABI 触及者。
/// 判据 = 宁多勿漏：误提升（地址被取的槽错放 SSA）是错值级，多落帧只是慢一点。
/// or-pattern 全枚举 Stmt/Terminator——新增 place 通道变体 = 非穷尽编译错误。
/// 落帧集：区间模型（m5.4-design §3.1「触及即落帧」保守全集的完整实现）。
/// 任何被 Ref/AddrOf/Copy/Repeat/Volatile/128 位·SIMD place 通道的【字节区间】触及的
/// 槽一律落帧。只记基址会把区间内槽误提升为 SSA：标量写进变量、place 通道读物理帧
/// （恒 0/旧值）= 错值级 miscompile——M5.4b regex SIGSEGV 的实锤根因正是 Copy src
/// 区间 [96,112) 内的槽 104 漏落帧（Weak::drop 读空指针 +0x10）。
#[derive(Default)]
pub(super) struct FrameMap {
    ranges: Vec<(u32, u32)>,
    /// 0 字节帧的 ZST 活地址需求（fsz=0 时 `&Local(0)` 的合法 one-past/ZST 地址，
    /// corpus c_rustpython_mini 实锤）：区间模型无法表达，强制以 1 字节尺寸物化帧。
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
    /// 是否需要物化帧（含 0 字节强征档）
    pub(super) fn needs_frame(&self) -> bool {
        !self.ranges.is_empty() || self.force
    }
}

/// scan_place 的触及范围：Bytes = 从基址起 n 字节；Escape = 地址逃逸
/// （Ref/AddrOf/Indirect 返回落点），本地不可知 → 保守到帧尾。
#[derive(Clone, Copy)]
pub(super) enum Extent {
    Bytes(u32),
    Escape,
}

pub(super) fn analyze_frame(body: &ir::FuncBody) -> FrameMap {
    let fsz = body.frame_size;
    /// 帧相关段 = 首 Deref/动态步之前。Offset 累加后：
    /// - 遇 Deref：指针槽本体 8 字节落帧即止（之后是 pointee，与帧无关）；
    /// - 遇动态步（IndexScaled/VTableAlignOffset）：运行期地址，保守 [pos, 帧尾)；
    /// - 步序耗尽：按 extent 落 [pos, pos+n) 或 [pos, 帧尾)。
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
        // 帧末 ZST 取址（corpus 批8 c_starlark_eval 实锤）：落在帧尾（p==fsz）的
        // Escape/空 Bytes 产生退化区间 `(fsz,fsz)`，被 FrameMap::add 的 `a<b` 静默
        // 丢弃——落帧集整个为空时 frame_ss 缺席，Ref 的 addr_of_local expect 炸
        // 「必落帧」。语义上该地址是合法的"帧末+1"（ZST 永不解引用），与 interp
        // 的 base+off 口径一致：补一个帧内 1 字节活口锚强制帧物化。
        // 0 字节帧形态（corpus 批9 c_rustpython_mini 实锤，f3679
        // mem::drop::<ZST 自定义 Drop>）：fsz.checked_sub(1) 无处落锚，
        // 记 force——define_fast 以 1 字节尺寸物化帧（地址仍 base+0）。
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
            Operand::Mem { expr, width } => scan_place(out, expr, Extent::Bytes(width.bytes()), fsz),
            Operand::AddrOf(expr) => scan_place(out, expr, Extent::Escape, fsz),
            Operand::SubImm { base, .. } => scan_op(out, base, fsz),
            Operand::Slot(_) | Operand::Imm { .. } => {}
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
            // 被调方经 sret 写整个返回聚合，尺寸本地不可知 → Escape
            RetDest::Indirect(pe) => scan_place(out, pe, Extent::Escape, fsz),
        }
    }
    fn scan_rv(out: &mut FrameMap, rv: &ir::Rvalue, fsz: u32) {
        use crate::vm::engine::ir::Rvalue as R;
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
            } => scan_place(out, a, Extent::Bytes(*lanes as u32 * *lane_bytes as u32), fsz),
            R::TlsRef(_) => {}
        }
    }
    /// SIMD place 的字节宽（lanes × lane_bytes 全向量）。
    fn simd_ext(lanes: &u16, lane_bytes: &u8) -> Extent {
        Extent::Bytes(*lanes as u32 * *lane_bytes as u32)
    }
    /// Repeat 系的字节宽（count × elem_size，饱和；scan_place 内再收帧尾）。
    fn rep_ext(count: &u64, elem_size: &u64) -> Extent {
        Extent::Bytes(count.saturating_mul(*elem_size).min(u32::MAX as u64) as u32)
    }
    let mut out = FrameMap::default();
    // T1-a：ABI v2 展平带来的帧责任——Indirect 参数字节区间（prologue memmove
    // 目的地）与 RetAbi::Indirect 的 ret_off 区间（Return 的 memmove 源）
    // 必落帧（帧模型 v2「触及即落帧」的 ABI 展平面）。
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
                // Copy/Repeat/Volatile：place 通道按【整个字节区间】落帧（m5.4-design
                // §3.1「触及即落帧」）——只记基址 = 区间内槽误提升 = 错值级（实锤根因）
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
                    // 地址向量：lanes × 8 字节（指针/偏移均按机器字宽）
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
                    // mask lane 宽 = mask_bytes（可与数据 lane 异宽，interp 同口径）
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
                    // 指针 lane 恒 8 字节（interp 同口径），与数据 lane 宽无关
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
                // 128 位族：place 通道恒 16 字节
                Stmt::Bin128 { a, b, dst, .. } => {
                    scan_place(&mut out, a, Extent::Bytes(16), fsz);
                    if let ir::Bin128Rhs::Wide(w) = b {
                        scan_place(&mut out, w, Extent::Bytes(16), fsz);
                    }
                    scan_place(&mut out, dst, Extent::Bytes(16), fsz);
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
            // callee 操作数同扫（fn-ptr 可能经 Mem/Deref 链读帧槽——同类潜在漏项）
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
    // Indirect ABI（M5.4c 准入；保守纳入——取址性最强）：槽本体 = sret/参数指针 8 字节
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
