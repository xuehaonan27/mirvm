//! 逐 instance 降低：单态化 MIR body → 引擎 FuncBody。
//!
//! 纪律（M4.0 设计 §3）：**Trap-stub 全覆盖**——语句/终止子/布局遇不认识的构造，
//! 当前块降为 `Trap(诊断)`，绝不中止整个降低。诊断串标注"哪一期欠的账"。
//!
//! M4.1（m4.1-design §3.1）：place 编译（投影链 → 地址表达式，Field/Downcast 折叠为
//! 常量偏移、Deref/Index 留运行期步骤）+ 值分类四路（Zst/Scalar/Pair/Bytes）+
//! 调用约定 v2（pair 2 槽、聚合 indirect/sret）。

use rustc_abi::{HasDataLayout, TagEncoding, VariantIdx, Variants};
use rustc_middle::mir::{self, Body};
use rustc_middle::ty::{self, EarlyBinder, Instance, InstanceKind, Ty, TyCtxt, TypingEnv};

use super::frame::{self, FrameLayout, ValKind};
use super::{Callee, Linker};
use crate::vm::engine::ir::{
    self, Bb, IntBinOp, IntCc, Operand, OvfOp, ParamAbi, PlaceBase, PlaceExpr, PlaceStep, RetAbi,
    RetDest, Rvalue, ScalarPlace, Slot, Stmt, Terminator, Width,
};

/// 整函数不可降低时的占位体（被调用即 Trap，诊断给出原因）。
pub fn trap_body(name: &str, reason: &str) -> ir::FuncBody {
    ir::FuncBody {
        frame_size: 0,
        frame_align: 1,
        ret: RetAbi::Zst,
        params: Vec::new(),
        blocks: vec![ir::Block {
            stmts: Vec::new(),
            term: Terminator::Trap(format!("整函数未降低: {reason}").into_boxed_str()),
        }],
        name: name.into(),
    }
}

/// place 编译的中间产物：地址表达式 + 当前类型（+胖指针 meta 来源）。
struct PlaceLow<'tcx> {
    base: PlaceBase,
    steps: Vec<PlaceStep>,
    ty: Ty<'tcx>,
    /// 若 place 经 Deref 进入了 unsized pointee：meta（len/vtable ptr）的读取位置
    /// ——deref 前胖指针本体的第二半（Ref unsized place 重组胖指针用）
    meta: Option<Operand>,
}

impl<'tcx> PlaceLow<'tcx> {
    fn push_offset(&mut self, o: u64) {
        // 连续常量偏移折叠（Field 链一个 Offset）
        if o == 0 {
            return;
        }
        if let Some(PlaceStep::Offset(prev)) = self.steps.last_mut() {
            *prev += o as u32;
        } else if self.steps.is_empty() {
            // 纯帧内：直接折进基址偏移（保住快路径）
            if let PlaceBase::Local(off) = &mut self.base {
                *off += o as u32;
            } else {
                self.steps.push(PlaceStep::Offset(o as u32));
            }
        } else {
            self.steps.push(PlaceStep::Offset(o as u32));
        }
    }

    /// 纯帧内静态偏移（快路径槽）
    fn frame_direct(&self) -> Option<u32> {
        match (&self.base, self.steps.is_empty()) {
            (PlaceBase::Local(off), true) => Some(*off),
            _ => None,
        }
    }

    fn expr(&self) -> PlaceExpr {
        PlaceExpr { base: self.base, steps: self.steps.clone().into_boxed_slice() }
    }

    /// 追加了常量偏移的表达式（pair 两半访问用；不破坏自身）
    fn expr_plus(&self, o: u32) -> PlaceExpr {
        let mut steps = self.steps.clone();
        if o != 0 {
            if let Some(PlaceStep::Offset(prev)) = steps.last_mut() {
                *prev += o;
            } else {
                steps.push(PlaceStep::Offset(o));
            }
        }
        let mut base = self.base;
        if steps.is_empty()
            && o != 0
            && let PlaceBase::Local(_) = base
        {
            // steps 为空时 expr_plus 已把 o 塞进 steps（上面分支），不会到这里；防御
            base = self.base;
        }
        PlaceExpr { base, steps: steps.into_boxed_slice() }
    }

    /// 标量位置（快路径优先）
    fn scalar_place(&self, w: Width) -> ScalarPlace {
        match self.frame_direct() {
            Some(off) => ScalarPlace::Slot(Slot { off, width: w }),
            None => ScalarPlace::Mem { expr: self.expr(), width: w },
        }
    }

    /// 标量 operand（快路径优先）
    fn scalar_operand(&self, w: Width) -> Operand {
        match self.frame_direct() {
            Some(off) => Operand::Slot(Slot { off, width: w }),
            None => Operand::Mem { expr: self.expr(), width: w },
        }
    }

    /// pair 半的标量位置
    fn half_place(&self, half_off: u32, w: Width) -> ScalarPlace {
        match self.frame_direct() {
            Some(off) => ScalarPlace::Slot(Slot { off: off + half_off, width: w }),
            None => ScalarPlace::Mem { expr: self.expr_plus(half_off), width: w },
        }
    }

    fn half_operand(&self, half_off: u32, w: Width) -> Operand {
        match self.frame_direct() {
            Some(off) => Operand::Slot(Slot { off: off + half_off, width: w }),
            None => Operand::Mem { expr: self.expr_plus(half_off), width: w },
        }
    }
}

/// 泛化 operand（值分类四路）。
enum LoweredOp<'tcx> {
    Zst,
    Scalar(Operand),
    Pair(Operand, Operand),
    /// 聚合（memcpy 通道）：源 place + 尺寸
    Bytes { place: PlaceLow<'tcx>, size: u64 },
}

/// 枚举判别式的冻结编码（Direct 在 lower 期溶解为 Cast，niche 用 NicheDiscr rvalue）。
enum TagInfo {
    /// 单 variant / 无 variant：判别式是常量
    Single { discr: u64 },
    Direct { tag_off: u32, tag_w: Width, tag_signed: bool },
    Niche {
        tag_off: u32,
        tag_w: Width,
        niche_start: u64,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
}

struct LowerCx<'tcx, 'a> {
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    frame: FrameLayout<'tcx>,
    linker: &'a mut Linker<'tcx>,
    /// 追加的合成块（发散调用的落点等），最终接在 MIR 块之后
    extra_blocks: Vec<ir::Block>,
    mir_block_count: usize,
}

impl<'tcx> LowerCx<'tcx, '_> {
    fn layout_of(
        &self,
        ty: Ty<'tcx>,
    ) -> Result<rustc_middle::ty::layout::TyAndLayout<'tcx>, String> {
        frame::layout_of(self.tcx, self.typing_env, ty)
    }

    fn classify(&self, ty: Ty<'tcx>) -> Result<ValKind, String> {
        Ok(frame::classify(self.tcx, &self.layout_of(ty)?))
    }

    /// place 编译：投影链 → 地址表达式（Field/Downcast 折偏移，Deref/Index 留步骤）。
    fn resolve_place(&self, place: &mir::Place<'tcx>) -> Result<PlaceLow<'tcx>, String> {
        let info = &self.frame.locals[place.local.as_usize()];
        let mut p = PlaceLow {
            base: PlaceBase::Local(info.off),
            steps: Vec::new(),
            ty: info.ty,
            meta: None,
        };
        // Downcast 状态：Some(v) 时下一个 Field 的偏移查 variant 布局
        let mut variant: Option<VariantIdx> = None;
        for elem in place.projection {
            match elem {
                mir::ProjectionElem::Field(f, fty) => {
                    let layout = self.layout_of(p.ty)?;
                    let layout = match variant.take() {
                        Some(v) => layout.for_variant(&LayoutCxAt(self.tcx, self.typing_env), v),
                        None => layout,
                    };
                    p.push_offset(layout.fields.offset(f.as_usize()).bytes());
                    p.ty = fty;
                }
                mir::ProjectionElem::Downcast(_, v) => {
                    variant = Some(v);
                }
                mir::ProjectionElem::Deref => {
                    let pointee = p
                        .ty
                        .builtin_deref(true)
                        .ok_or_else(|| format!("Deref 非指针（ty={}）", p.ty))?;
                    // 进入 unsized pointee：记录胖指针 meta 的读取位置（本体 +8）
                    let pointee_layout = self.layout_of(pointee);
                    let unsized_pointee =
                        matches!(&pointee_layout, Ok(l) if l.is_unsized());
                    if unsized_pointee {
                        p.meta = Some(match p.frame_direct() {
                            Some(off) => {
                                Operand::Slot(Slot { off: off + 8, width: Width::W64 })
                            }
                            None => Operand::Mem { expr: p.expr_plus(8), width: Width::W64 },
                        });
                    } else {
                        p.meta = None;
                    }
                    p.steps.push(PlaceStep::Deref);
                    p.ty = pointee;
                }
                mir::ProjectionElem::Index(idx_local) => {
                    let elem_ty = elem_of(p.ty).ok_or_else(|| format!("Index 非序列（ty={}）", p.ty))?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    let idx_info = &self.frame.locals[idx_local.as_usize()];
                    let Some(w) = idx_info.kind.scalar() else {
                        return Err("Index 下标非标量".into());
                    };
                    p.steps
                        .push(PlaceStep::IndexScaled { idx: Slot { off: idx_info.off, width: w }, stride });
                    p.ty = elem_ty;
                    p.meta = None;
                }
                mir::ProjectionElem::ConstantIndex { offset, min_length: _, from_end } => {
                    let elem_ty = elem_of(p.ty)
                        .ok_or_else(|| format!("ConstantIndex 非序列（ty={}）", p.ty))?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    if from_end {
                        // 数组长度已知可折；slice 需运行期 len
                        if let ty::Array(_, n) = p.ty.kind() {
                            let n = n
                                .try_to_target_usize(self.tcx)
                                .ok_or("数组长度非常量")?;
                            p.push_offset((n - offset) * stride);
                        } else {
                            return Err("ConstantIndex from_end on slice（M4.1+）".into());
                        }
                    } else {
                        p.push_offset(offset * stride);
                    }
                    p.ty = elem_ty;
                    p.meta = None;
                }
                mir::ProjectionElem::OpaqueCast(t) | mir::ProjectionElem::UnwrapUnsafeBinder(t) => {
                    p.ty = t;
                }
                other => return Err(format!("投影 {other:?}（M4.1+）")),
            }
        }
        if variant.is_some() {
            // Downcast 结尾（无后续 Field）：place 类型仍是 enum，偏移不变——
            // 作为整体读写时按 enum 布局（Aggregate/SetDiscriminant 语境处理）
        }
        Ok(p)
    }

    /// place → 标量槽（调用方确定是标量语境）。
    fn place_scalar(&self, place: &mir::Place<'tcx>) -> Result<(PlaceLow<'tcx>, Width), String> {
        let p = self.resolve_place(place)?;
        let ValKind::Scalar(w) = self.classify(p.ty)? else {
            return Err(format!("非标量 place（ty={}，M4.1）", p.ty));
        };
        Ok((p, w))
    }

    /// 泛化操作数（四路值分类）。
    fn lower_operand(&self, op: &mir::Operand<'tcx>) -> Result<LoweredOp<'tcx>, String> {
        match op {
            mir::Operand::Copy(pl) | mir::Operand::Move(pl) => {
                let p = self.resolve_place(pl)?;
                Ok(match self.classify(p.ty)? {
                    ValKind::Zst => LoweredOp::Zst,
                    ValKind::Scalar(w) => LoweredOp::Scalar(p.scalar_operand(w)),
                    ValKind::Pair((ao, aw), (bo, bw)) => {
                        LoweredOp::Pair(p.half_operand(ao, aw), p.half_operand(bo, bw))
                    }
                    ValKind::Other { size } => LoweredOp::Bytes { place: p, size },
                })
            }
            mir::Operand::Constant(c) => {
                let ty = c.const_.ty();
                let layout = self.layout_of(ty)?;
                if layout.is_zst() {
                    return Ok(LoweredOp::Zst);
                }
                let width = frame::scalar_width(&layout)
                    .ok_or_else(|| format!("非标量常量（ty={ty}，M4.1 第 4 步常量池）"))?;
                let val = c
                    .const_
                    .eval(self.tcx, self.typing_env, c.span)
                    .map_err(|e| format!("常量求值失败: {e:?}"))?;
                match val {
                    mir::ConstValue::Scalar(mir::interpret::Scalar::Int(si)) => {
                        let bits = si.to_bits(si.size());
                        if bits > u64::MAX as u128 {
                            return Err("128 位常量（M4.1 第 3 步）".into());
                        }
                        Ok(LoweredOp::Scalar(Operand::Imm { bits: bits as u64, width }))
                    }
                    mir::ConstValue::Scalar(mir::interpret::Scalar::Ptr(..)) => {
                        Err("指针常量（static/fn-ptr，M4.1 第 4 步）".into())
                    }
                    other => Err(format!("常量形态 {other:?}（M4.1 第 4 步）")),
                }
            }
            // session 旗标查询（UbChecks 等）：lower 期折成 bool 立即数
            mir::Operand::RuntimeChecks(rc) => {
                use mir::RuntimeChecks as RC;
                let v = match rc {
                    RC::UbChecks => self.tcx.sess.ub_checks(),
                    RC::OverflowChecks => self.tcx.sess.overflow_checks(),
                    RC::ContractChecks => self.tcx.sess.contract_checks(),
                };
                Ok(LoweredOp::Scalar(Operand::Imm { bits: v as u64, width: Width::W8 }))
            }
        }
    }

    /// 标量操作数（标量语境；其他分类 = 语境错误诊断）。
    fn lower_operand_scalar(&self, op: &mir::Operand<'tcx>) -> Result<Operand, String> {
        match self.lower_operand(op)? {
            LoweredOp::Scalar(o) => Ok(o),
            LoweredOp::Zst => Err("意外的 ZST 操作数".into()),
            LoweredOp::Pair(..) => Err(format!("非标量操作数（pair，ty={}）", self.op_ty_str(op))),
            LoweredOp::Bytes { .. } => {
                Err(format!("非标量操作数（聚合，ty={}）", self.op_ty_str(op)))
            }
        }
    }

    fn op_ty(&self, op: &mir::Operand<'tcx>) -> Result<Ty<'tcx>, String> {
        Ok(match op {
            mir::Operand::Copy(p) | mir::Operand::Move(p) => self.resolve_place(p)?.ty,
            mir::Operand::Constant(c) => c.const_.ty(),
            mir::Operand::RuntimeChecks(_) => self.tcx.types.bool,
        })
    }

    fn op_ty_str(&self, op: &mir::Operand<'tcx>) -> String {
        self.op_ty(op).map(|t| t.to_string()).unwrap_or_else(|_| "?".into())
    }

    /// 枚举 tag 编码冻结（Discriminant 读 / SetDiscriminant 写共用）。
    fn tag_info(&self, ty: Ty<'tcx>) -> Result<TagInfo, String> {
        let layout = self.layout_of(ty)?;
        Ok(match &layout.variants {
            Variants::Empty => TagInfo::Single { discr: 0 }, // 不可及（读它 = guest UB）
            Variants::Single { index } => {
                let discr = ty
                    .discriminant_for_variant(self.tcx, *index)
                    .map(|d| d.val)
                    .unwrap_or(index.as_u32() as u128);
                TagInfo::Single { discr: u128_to_u64(discr)? }
            }
            Variants::Multiple { tag, tag_encoding, tag_field, .. } => {
                let dl = self.tcx.data_layout();
                let tag_off = layout.fields.offset(tag_field.as_usize()).bytes() as u32;
                let tag_w = Width::from_bytes(tag.size(dl).bytes())
                    .ok_or("128 位 tag（M4.1+）")?;
                let tag_signed = matches!(tag.primitive(), rustc_abi::Primitive::Int(_, true));
                match tag_encoding {
                    TagEncoding::Direct => TagInfo::Direct { tag_off, tag_w, tag_signed },
                    TagEncoding::Niche { untagged_variant, niche_variants, niche_start } => {
                        TagInfo::Niche {
                            tag_off,
                            tag_w,
                            niche_start: u128_to_u64(*niche_start & tag_w.mask() as u128)?,
                            variants_start: niche_variants.start.as_u32() as u64,
                            variants_len: (niche_variants.last.as_u32()
                                - niche_variants.start.as_u32())
                                as u64
                                + 1,
                            untagged: untagged_variant.as_u32() as u64,
                        }
                    }
                }
            }
        })
    }

    /// SetDiscriminant：lower 期溶解为对 tag 槽的常量写（niche 的 untagged = 无操作）。
    fn set_discr_stmts(
        &self,
        dst_p: &PlaceLow<'tcx>,
        enum_ty: Ty<'tcx>,
        vidx: VariantIdx,
    ) -> Result<Vec<Stmt>, String> {
        Ok(match self.tag_info(enum_ty)? {
            TagInfo::Single { .. } => vec![],
            TagInfo::Direct { tag_off, tag_w, .. } => {
                let discr = enum_ty
                    .discriminant_for_variant(self.tcx, vidx)
                    .map(|d| d.val)
                    .ok_or("Direct tag 无 discr")?;
                let bits = (discr as u64) & tag_w.mask();
                vec![Stmt::Assign {
                    dst: dst_p.half_place(tag_off, tag_w),
                    rv: Rvalue::Use(Operand::Imm { bits, width: tag_w }),
                }]
            }
            TagInfo::Niche { tag_off, tag_w, niche_start, variants_start, untagged, .. } => {
                let vi = vidx.as_u32() as u64;
                if vi == untagged {
                    vec![]
                } else {
                    let bits = vi.wrapping_sub(variants_start).wrapping_add(niche_start)
                        & tag_w.mask();
                    vec![Stmt::Assign {
                        dst: dst_p.half_place(tag_off, tag_w),
                        rv: Rvalue::Use(Operand::Imm { bits, width: tag_w }),
                    }]
                }
            }
        })
    }

    /// 把一个 MIR operand 写到 dst place 的字节偏移 off 处（Aggregate 字段落位）。
    fn write_at(
        &self,
        dst_p: &PlaceLow<'tcx>,
        off: u32,
        op: &mir::Operand<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        Ok(match self.lower_operand(op)? {
            LoweredOp::Zst => vec![],
            LoweredOp::Scalar(o) => {
                let w = o.width();
                vec![Stmt::Assign { dst: dst_p.half_place(off, w), rv: Rvalue::Use(o) }]
            }
            LoweredOp::Pair(l, h) => {
                let ValKind::Pair((ao, aw), (bo, bw)) = self.classify(self.op_ty(op)?)? else {
                    return Err("pair operand 分类漂移".into());
                };
                vec![
                    Stmt::Assign { dst: dst_p.half_place(off + ao, aw), rv: Rvalue::Use(l) },
                    Stmt::Assign { dst: dst_p.half_place(off + bo, bw), rv: Rvalue::Use(h) },
                ]
            }
            LoweredOp::Bytes { place, size } => {
                vec![Stmt::Copy { dst: dst_p.expr_plus(off), src: place.expr(), size: size as u32 }]
            }
        })
    }

    /// 把 src 泛化操作数写进 dst place（同型位搬运——Use/Transmute/位拷 cast 的共用道）。
    fn assign_lowered(
        &self,
        dst: &PlaceLow<'tcx>,
        dst_kind: ValKind,
        src: LoweredOp<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        Ok(match (dst_kind, src) {
            (ValKind::Zst, _) => vec![Stmt::Nop],
            (ValKind::Scalar(w), LoweredOp::Scalar(o)) => {
                vec![Stmt::Assign { dst: dst.scalar_place(w), rv: Rvalue::Use(o) }]
            }
            (ValKind::Pair((ao, aw), (bo, bw)), LoweredOp::Pair(l, h)) => vec![
                Stmt::Assign { dst: dst.half_place(ao, aw), rv: Rvalue::Use(l) },
                Stmt::Assign { dst: dst.half_place(bo, bw), rv: Rvalue::Use(h) },
            ],
            (ValKind::Other { size }, LoweredOp::Bytes { place, size: ssz }) => {
                debug_assert_eq!(size, ssz);
                vec![Stmt::Copy { dst: dst.expr(), src: place.expr(), size: size as u32 }]
            }
            // 位拷语境的跨分类（Transmute pair↔聚合等）：src 是 place 时走字节拷
            (ValKind::Pair(..) | ValKind::Scalar(_), LoweredOp::Bytes { place, size }) => {
                vec![Stmt::Copy { dst: dst.expr(), src: place.expr(), size: size as u32 }]
            }
            (ValKind::Other { size }, LoweredOp::Pair(l, h)) => {
                // pair 值写进聚合视图的 place：按半宽写两个标量（偏移 0 / align 后）
                // ——出现于 Transmute；两半偏移取 src 布局无从得，这里按紧凑 0/宽度对齐近似
                // 不可靠 → 诊断
                let _ = (size, l, h);
                return Err("Transmute pair→聚合（M4.1+）".into());
            }
            (k, s) => {
                return Err(format!(
                    "赋值分类不匹配（dst={k:?}, src={}，M4.1+）",
                    match s {
                        LoweredOp::Zst => "zst",
                        LoweredOp::Scalar(_) => "scalar",
                        LoweredOp::Pair(..) => "pair",
                        LoweredOp::Bytes { .. } => "bytes",
                    }
                ));
            }
        })
    }

    /// Assign 语句 → ir 语句（可能多条）。
    fn lower_assign(
        &self,
        dst: &mir::Place<'tcx>,
        rv: &mir::Rvalue<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        let dst_p = self.resolve_place(dst)?;
        let dst_kind = self.classify(dst_p.ty)?;

        // *WithOverflow：写 (值, 旗标) 标量对
        if let mir::Rvalue::BinaryOp(binop, box (a, b)) = rv {
            let ovf = match binop {
                mir::BinOp::AddWithOverflow => Some(OvfOp::Add),
                mir::BinOp::SubWithOverflow => Some(OvfOp::Sub),
                mir::BinOp::MulWithOverflow => Some(OvfOp::Mul),
                _ => None,
            };
            if let Some(op) = ovf {
                let ValKind::Pair((vo, vw), (fo, fw)) = dst_kind else {
                    return Err("溢出算术目标非 pair".into());
                };
                let a_ty = self.op_ty(a)?;
                return Ok(vec![Stmt::AssignOverflow {
                    op,
                    signed: frame::ty_signed(a_ty),
                    a: self.lower_operand_scalar(a)?,
                    b: self.lower_operand_scalar(b)?,
                    dst_val: dst_p.half_place(vo, vw),
                    dst_flag: dst_p.half_place(fo, fw),
                }]);
            }
        }

        if dst_kind.is_zst() {
            return Ok(vec![Stmt::Nop]); // 本期 rvalue 集无副作用
        }

        match rv {
            // WithRetag：Tree Borrows 的 retag 是检查器语义（P3 不检测）——fast machine 忽略。
            // CopyForDeref = Use；Reborrow = 同型位拷（用户 ADT reborrow，layout 相同）。
            mir::Rvalue::Use(op, _retag) => {
                let src = self.lower_operand(op)?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
            mir::Rvalue::CopyForDeref(pl) => {
                let src = self.lower_operand(&mir::Operand::Copy(*pl))?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
            mir::Rvalue::Reborrow(_, _, pl) => {
                let src = self.lower_operand(&mir::Operand::Copy(*pl))?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
            mir::Rvalue::Ref(_, _, pl) | mir::Rvalue::RawPtr(_, pl) => {
                let p = self.resolve_place(pl)?;
                let pointee_layout = self.layout_of(p.ty)?;
                if pointee_layout.is_unsized() {
                    // 胖指针：dst pair = (place 地址, meta)
                    let ValKind::Pair((ao, aw), (bo, bw)) = dst_kind else {
                        return Err("unsized Ref 目标非 pair".into());
                    };
                    let meta = match p.ty.kind() {
                        // &[T;N] 经投影到 [T] 不会出现；meta 来自 deref 链
                        _ => p
                            .meta
                            .clone()
                            .ok_or_else(|| format!("unsized Ref 无 meta 来源（ty={}，M4.1+）", p.ty))?,
                    };
                    Ok(vec![
                        Stmt::Assign { dst: dst_p.half_place(ao, aw), rv: Rvalue::Ref(p.expr()) },
                        Stmt::Assign { dst: dst_p.half_place(bo, bw), rv: Rvalue::Use(meta) },
                    ])
                } else {
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("Ref 目标非标量".into());
                    };
                    Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv: Rvalue::Ref(p.expr()) }])
                }
            }
            mir::Rvalue::BinaryOp(binop, box (a, b)) => {
                // 指针算术
                if let mir::BinOp::Offset = binop {
                    let ptr_ty = self.op_ty(a)?;
                    let pointee = ptr_ty
                        .builtin_deref(true)
                        .ok_or_else(|| format!("Offset 非指针（ty={ptr_ty}）"))?;
                    let stride = self.layout_of(pointee)?.size.bytes();
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("Offset 目标非标量".into());
                    };
                    return Ok(vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::PtrOffset {
                            ptr: self.lower_operand_scalar(a)?,
                            count: self.lower_operand_scalar(b)?,
                            stride,
                        },
                    }]);
                }
                let a_ty = self.op_ty(a)?;
                use mir::BinOp::*;
                if a_ty.is_floating_point() {
                    let is64 = match a_ty.kind() {
                        ty::Float(ty::FloatTy::F32) => false,
                        ty::Float(ty::FloatTy::F64) => true,
                        _ => return Err(format!("浮点宽度 {a_ty}（f16/f128，M4.1+）")),
                    };
                    let ao = self.lower_operand_scalar(a)?;
                    let bo = self.lower_operand_scalar(b)?;
                    use ir::FloatOp as F;
                    let fbin = |op| Rvalue::FloatBin { op, is64, a: ao.clone(), b: bo.clone() };
                    let fcmp = |cc| Rvalue::FloatCmp { cc, is64, a: ao.clone(), b: bo.clone() };
                    let rvalue = match binop {
                        Add | AddUnchecked => fbin(F::Add),
                        Sub | SubUnchecked => fbin(F::Sub),
                        Mul | MulUnchecked => fbin(F::Mul),
                        Div => fbin(F::Div),
                        Rem => fbin(F::Rem),
                        Eq => fcmp(IntCc::Eq),
                        Ne => fcmp(IntCc::Ne),
                        Lt => fcmp(IntCc::Lt),
                        Le => fcmp(IntCc::Le),
                        Gt => fcmp(IntCc::Gt),
                        Ge => fcmp(IntCc::Ge),
                        other => return Err(format!("浮点 BinOp {other:?}（M4.1+）")),
                    };
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("浮点运算目标非标量".into());
                    };
                    return Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv: rvalue }]);
                }
                let signed = frame::ty_signed(a_ty);
                let ao = self.lower_operand_scalar(a)?;
                let bo = self.lower_operand_scalar(b)?;
                let int = |op| Rvalue::IntBin { op, signed, a: ao.clone(), b: bo.clone() };
                let cmp = |cc| Rvalue::IntCmp { cc, signed, a: ao.clone(), b: bo.clone() };
                let rvalue = match binop {
                    Add | AddUnchecked => int(IntBinOp::Add),
                    Sub | SubUnchecked => int(IntBinOp::Sub),
                    Mul | MulUnchecked => int(IntBinOp::Mul),
                    Div => int(IntBinOp::Div),
                    Rem => int(IntBinOp::Rem),
                    BitAnd => int(IntBinOp::BitAnd),
                    BitOr => int(IntBinOp::BitOr),
                    BitXor => int(IntBinOp::BitXor),
                    Shl | ShlUnchecked => int(IntBinOp::Shl),
                    Shr | ShrUnchecked => int(IntBinOp::Shr),
                    Eq => cmp(IntCc::Eq),
                    Ne => cmp(IntCc::Ne),
                    Lt => cmp(IntCc::Lt),
                    Le => cmp(IntCc::Le),
                    Gt => cmp(IntCc::Gt),
                    Ge => cmp(IntCc::Ge),
                    Cmp => Rvalue::IntCmp3 { signed, a: ao.clone(), b: bo.clone() },
                    other => return Err(format!("BinOp {other:?}（M4.1+）")),
                };
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("整数运算目标非标量".into());
                };
                Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv: rvalue }])
            }
            mir::Rvalue::UnaryOp(unop, a) => {
                let a_ty = self.op_ty(a)?;
                match unop {
                    mir::UnOp::PtrMetadata => {
                        // 胖指针 → 取 meta 半；瘦指针 meta 是 ZST（dst_kind 已非 zst 才到这）
                        match self.lower_operand(a)? {
                            LoweredOp::Pair(_, h) => {
                                let ValKind::Scalar(w) = dst_kind else {
                                    return Err("PtrMetadata 目标非标量".into());
                                };
                                Ok(vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(h),
                                }])
                            }
                            _ => Err(format!("PtrMetadata 非胖指针（ty={a_ty}，M4.1+）")),
                        }
                    }
                    mir::UnOp::Not if a_ty.is_bool() => {
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("Not 目标非标量".into());
                        };
                        Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv: Rvalue::NotBool(ao) }])
                    }
                    mir::UnOp::Not => {
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("Not 目标非标量".into());
                        };
                        Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv: Rvalue::NotBits(ao) }])
                    }
                    mir::UnOp::Neg => {
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("Neg 目标非标量".into());
                        };
                        let rv = if a_ty.is_floating_point() {
                            let is64 = match a_ty.kind() {
                                ty::Float(ty::FloatTy::F32) => false,
                                ty::Float(ty::FloatTy::F64) => true,
                                _ => return Err(format!("浮点宽度 {a_ty}（M4.1+）")),
                            };
                            Rvalue::FloatNeg { is64, a: ao }
                        } else {
                            Rvalue::Neg(ao)
                        };
                        Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv }])
                    }
                }
            }
            mir::Rvalue::Cast(kind, a, to_ty) => self.lower_cast(&dst_p, dst_kind, *kind, a, *to_ty),
            mir::Rvalue::Repeat(op, n) => {
                let count = n
                    .try_to_target_usize(self.tcx)
                    .ok_or("Repeat 长度非常量")?;
                match self.lower_operand(op)? {
                    LoweredOp::Scalar(val) => {
                        let elem_size = val.width().bytes();
                        Ok(vec![Stmt::RepeatScalar { dst: dst_p.expr(), val, count, elem_size }])
                    }
                    LoweredOp::Zst => Ok(vec![Stmt::Nop]),
                    _ => Err("Repeat 非标量元素（M4.1+）".into()),
                }
            }
            mir::Rvalue::Discriminant(pl) => {
                let p = self.resolve_place(pl)?;
                let ValKind::Scalar(dw) = dst_kind else {
                    return Err("Discriminant 目标非标量".into());
                };
                let rv = match self.tag_info(p.ty)? {
                    TagInfo::Single { discr } => {
                        Rvalue::Use(Operand::Imm { bits: discr & dw.mask(), width: dw })
                    }
                    TagInfo::Direct { tag_off, tag_w, tag_signed } => Rvalue::Cast {
                        from: (tag_w, tag_signed),
                        to: dw,
                        a: p.half_operand(tag_off, tag_w),
                    },
                    TagInfo::Niche {
                        tag_off,
                        tag_w,
                        niche_start,
                        variants_start,
                        variants_len,
                        untagged,
                    } => Rvalue::NicheDiscr {
                        tag: p.half_operand(tag_off, tag_w),
                        niche_start,
                        variants_start,
                        variants_len,
                        untagged,
                    },
                };
                Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(dw), rv }])
            }
            mir::Rvalue::Aggregate(box kind, operands) => {
                use mir::AggregateKind as AK;
                match kind {
                    AK::Array(elem_ty) => {
                        let stride = self.layout_of(*elem_ty)?.size.bytes() as u32;
                        let mut stmts = Vec::new();
                        for (i, op) in operands.iter().enumerate() {
                            stmts.extend(self.write_at(&dst_p, i as u32 * stride, op)?);
                        }
                        Ok(stmts)
                    }
                    AK::RawPtr(..) => {
                        // (data, meta) → 胖指针；meta ZST → 瘦指针
                        let mut ops = operands.iter();
                        let (data, meta) =
                            (ops.next().ok_or("RawPtr 缺 data")?, ops.next().ok_or("RawPtr 缺 meta")?);
                        match dst_kind {
                            ValKind::Scalar(w) => {
                                let LoweredOp::Scalar(d) = self.lower_operand(data)? else {
                                    return Err("RawPtr data 非标量".into());
                                };
                                Ok(vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(d),
                                }])
                            }
                            ValKind::Pair((ao, aw), (bo, bw)) => {
                                let LoweredOp::Scalar(d) = self.lower_operand(data)? else {
                                    return Err("RawPtr data 非标量".into());
                                };
                                let LoweredOp::Scalar(m) = self.lower_operand(meta)? else {
                                    return Err("RawPtr meta 非标量".into());
                                };
                                Ok(vec![
                                    Stmt::Assign {
                                        dst: dst_p.half_place(ao, aw),
                                        rv: Rvalue::Use(d),
                                    },
                                    Stmt::Assign {
                                        dst: dst_p.half_place(bo, bw),
                                        rv: Rvalue::Use(m),
                                    },
                                ])
                            }
                            _ => Err("RawPtr 目标分类异常".into()),
                        }
                    }
                    AK::Adt(..) | AK::Tuple | AK::Closure(..) | AK::Coroutine(..)
                    | AK::CoroutineClosure(..) => {
                        // cg_ssa 同构：定 variant → 逐字段按 variant 布局落位 → 写判别式
                        let (vidx, active_field) = match kind {
                            AK::Adt(_, v, _, _, af) => (*v, *af),
                            _ => (VariantIdx::ZERO, None),
                        };
                        let layout = self.layout_of(dst_p.ty)?;
                        let variant_layout = if matches!(layout.variants, Variants::Single { .. } | Variants::Empty)
                        {
                            layout
                        } else {
                            layout.for_variant(&LayoutCxAt(self.tcx, self.typing_env), vidx)
                        };
                        let mut stmts = Vec::new();
                        for (i, op) in operands.iter().enumerate() {
                            let fi = active_field.map(|f| f.as_usize()).unwrap_or(i);
                            let off = variant_layout.fields.offset(fi).bytes() as u32;
                            stmts.extend(self.write_at(&dst_p, off, op)?);
                        }
                        if dst_p.ty.is_enum() {
                            stmts.extend(self.set_discr_stmts(&dst_p, dst_p.ty, vidx)?);
                        }
                        Ok(stmts)
                    }
                }
            }
            mir::Rvalue::ThreadLocalRef(_) => Err("ThreadLocalRef（M4.4）".into()),
            mir::Rvalue::WrapUnsafeBinder(op, _) => {
                let src = self.lower_operand(op)?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
        }
    }

    /// Cast 家族。
    fn lower_cast(
        &self,
        dst_p: &PlaceLow<'tcx>,
        dst_kind: ValKind,
        kind: mir::CastKind,
        a: &mir::Operand<'tcx>,
        to_ty: Ty<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        use mir::CastKind as CK;
        match kind {
            CK::IntToInt => {
                let a_ty = self.op_ty(a)?;
                let a_layout = self.layout_of(a_ty)?;
                let from_w = frame::scalar_width(&a_layout).ok_or("cast 源非标量")?;
                let to_layout = self.layout_of(to_ty)?;
                let to_w = frame::scalar_width(&to_layout).ok_or("cast 目标非标量（128 位，M4.1 第 3 步）")?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("IntToInt 目标非标量".into());
                };
                debug_assert_eq!(w.bytes(), to_w.bytes());
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Cast {
                        from: (from_w, frame::ty_signed(a_ty)),
                        to: to_w,
                        a: self.lower_operand_scalar(a)?,
                    },
                }])
            }
            // 真实地址模型下的位拷 cast 家族
            CK::PointerExposeProvenance | CK::PointerWithExposedProvenance | CK::FnPtrToPtr => {
                let src = self.lower_operand(a)?;
                self.assign_lowered(dst_p, dst_kind, src)
            }
            CK::PtrToPtr => {
                // 胖→瘦 = 取 data 半；同类 = 位拷
                let src = self.lower_operand(a)?;
                match (&dst_kind, src) {
                    (ValKind::Scalar(_), LoweredOp::Pair(l, _)) => {
                        self.assign_lowered(dst_p, dst_kind, LoweredOp::Scalar(l))
                    }
                    (_, src) => self.assign_lowered(dst_p, dst_kind, src),
                }
            }
            CK::Transmute => {
                // 位重解释 = 字节搬运。同宽标量直通（快路径）；src 是 place 时
                // 一律按字节拷（跨分类安全）；标量常量→同宽标量。
                match a {
                    mir::Operand::Copy(pl) | mir::Operand::Move(pl) => {
                        let src_p = self.resolve_place(pl)?;
                        let src_layout = self.layout_of(src_p.ty)?;
                        match (&dst_kind, frame::scalar_width(&src_layout)) {
                            (ValKind::Scalar(dw), Some(sw)) if sw == *dw => {
                                Ok(vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(*dw),
                                    rv: Rvalue::Use(src_p.scalar_operand(sw)),
                                }])
                            }
                            _ => {
                                let size = src_layout.size.bytes();
                                Ok(vec![Stmt::Copy {
                                    dst: dst_p.expr(),
                                    src: src_p.expr(),
                                    size: size as u32,
                                }])
                            }
                        }
                    }
                    _ => match (self.lower_operand(a)?, &dst_kind) {
                        (LoweredOp::Scalar(o), ValKind::Scalar(dw)) if o.width() == *dw => {
                            Ok(vec![Stmt::Assign {
                                dst: dst_p.scalar_place(*dw),
                                rv: Rvalue::Use(o),
                            }])
                        }
                        _ => Err(format!("Transmute 常量→{to_ty}（M4.1 第 4 步常量池）")),
                    },
                }
            }
            CK::PointerCoercion(pc, _) => {
                use ty::adjustment::PointerCoercion as PC;
                match pc {
                    PC::Unsize => {
                        // &[T;N] → &[T]：pair = (src 瘦指针, N)；dyn unsize = vtable（第 4 步）
                        let a_ty = self.op_ty(a)?;
                        let src_pointee =
                            a_ty.builtin_deref(true).ok_or("Unsize 源非指针")?;
                        let dst_pointee =
                            to_ty.builtin_deref(true).ok_or("Unsize 目标非指针")?;
                        match (src_pointee.kind(), dst_pointee.kind()) {
                            (ty::Array(_, n), ty::Slice(_)) => {
                                let n = n
                                    .try_to_target_usize(self.tcx)
                                    .ok_or("数组长度非常量")?;
                                let ValKind::Pair((ao, aw), (bo, bw)) = dst_kind else {
                                    return Err("Unsize 目标非 pair".into());
                                };
                                let LoweredOp::Scalar(data) = self.lower_operand(a)? else {
                                    return Err("Unsize 源非瘦指针".into());
                                };
                                Ok(vec![
                                    Stmt::Assign {
                                        dst: dst_p.half_place(ao, aw),
                                        rv: Rvalue::Use(data),
                                    },
                                    Stmt::Assign {
                                        dst: dst_p.half_place(bo, bw),
                                        rv: Rvalue::Use(Operand::Imm { bits: n, width: bw }),
                                    },
                                ])
                            }
                            (_, ty::Dynamic(..)) => {
                                Err("dyn unsize（vtable，M4.1 第 4 步）".into())
                            }
                            _ => Err(format!(
                                "Unsize {src_pointee} → {dst_pointee}（M4.1+）"
                            )),
                        }
                    }
                    PC::MutToConstPointer | PC::UnsafeFnPointer | PC::ArrayToPointer => {
                        // 位拷（胖→瘦经 PtrToPtr，这里同类位拷）
                        let src = self.lower_operand(a)?;
                        match (&dst_kind, src) {
                            (ValKind::Scalar(_), LoweredOp::Pair(l, _)) => {
                                self.assign_lowered(dst_p, dst_kind, LoweredOp::Scalar(l))
                            }
                            (_, src) => self.assign_lowered(dst_p, dst_kind, src),
                        }
                    }
                    PC::ReifyFnPointer(..) | PC::ClosureFnPointer(..) => {
                        Err("fn 指针物化（D4 条目表，M4.1 第 4 步）".into())
                    }
                }
            }
            CK::FloatToInt => {
                let a_ty = self.op_ty(a)?;
                let from64 = match a_ty.kind() {
                    ty::Float(ty::FloatTy::F32) => false,
                    ty::Float(ty::FloatTy::F64) => true,
                    _ => return Err(format!("FloatToInt 源 {a_ty}（M4.1+）")),
                };
                let to_layout = self.layout_of(to_ty)?;
                let to_w = frame::scalar_width(&to_layout).ok_or("FloatToInt 目标非标量")?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("FloatToInt 目标非标量".into());
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::FloatToInt {
                        from64,
                        to: to_w,
                        signed: frame::ty_signed(to_ty),
                        a: self.lower_operand_scalar(a)?,
                    },
                }])
            }
            CK::IntToFloat => {
                let a_ty = self.op_ty(a)?;
                let a_layout = self.layout_of(a_ty)?;
                let from_w = frame::scalar_width(&a_layout).ok_or("IntToFloat 源非标量（128 位，M4.1+）")?;
                let to64 = match to_ty.kind() {
                    ty::Float(ty::FloatTy::F32) => false,
                    ty::Float(ty::FloatTy::F64) => true,
                    _ => return Err(format!("IntToFloat 目标 {to_ty}（M4.1+）")),
                };
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("IntToFloat 目标非标量".into());
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntToFloat {
                        from: (from_w, frame::ty_signed(a_ty)),
                        to64,
                        a: self.lower_operand_scalar(a)?,
                    },
                }])
            }
            CK::FloatToFloat => {
                let a_ty = self.op_ty(a)?;
                let from64 = match a_ty.kind() {
                    ty::Float(ty::FloatTy::F32) => false,
                    ty::Float(ty::FloatTy::F64) => true,
                    _ => return Err(format!("FloatToFloat 源 {a_ty}（M4.1+）")),
                };
                let to64 = match to_ty.kind() {
                    ty::Float(ty::FloatTy::F32) => false,
                    ty::Float(ty::FloatTy::F64) => true,
                    _ => return Err(format!("FloatToFloat 目标 {to_ty}（M4.1+）")),
                };
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("FloatToFloat 目标非标量".into());
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::FloatCast { from64, to64, a: self.lower_operand_scalar(a)? },
                }])
            }
            CK::Subtype => {
                let src = self.lower_operand(a)?;
                self.assign_lowered(dst_p, dst_kind, src)
            }
        }
    }

    fn lower_unwind(&self, u: mir::UnwindAction) -> ir::UnwindAction {
        match u {
            mir::UnwindAction::Cleanup(bb) => ir::UnwindAction::Cleanup(bb.as_u32()),
            _ => ir::UnwindAction::Continue,
        }
    }

    /// 终止子 → (追加语句, ir 终止子)。
    fn lower_terminator(
        &mut self,
        term: &mir::Terminator<'tcx>,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        use mir::TerminatorKind as TK;
        Ok(match &term.kind {
            TK::Goto { target } => (vec![], Terminator::Goto(target.as_u32())),
            TK::SwitchInt { discr, targets } => {
                let d = self.lower_operand_scalar(discr)?;
                (
                    vec![],
                    Terminator::SwitchInt {
                        discr: d,
                        targets: targets.iter().map(|(v, b)| (v, b.as_u32())).collect(),
                        otherwise: targets.otherwise().as_u32(),
                    },
                )
            }
            TK::Return => (vec![], Terminator::Return),
            TK::Unreachable => (vec![], Terminator::Unreachable),
            TK::Assert { cond, expected, msg, target, unwind } => {
                let c = self.lower_operand_scalar(cond)?;
                (
                    vec![],
                    Terminator::Assert {
                        cond: c,
                        expected: *expected,
                        msg: assert_msg(msg).into_boxed_str(),
                        target: target.as_u32(),
                        unwind: self.lower_unwind(*unwind),
                    },
                )
            }
            TK::Drop { place, target, unwind, .. } => {
                let p = self.resolve_place(place)?;
                if p.ty.needs_drop(self.tcx, self.typing_env) {
                    // 正常路径 Drop = 普通 Call（F2）；glue 执行落 M4.1 第 5 步，
                    // 此前前置 Trap 防静默 + 保 Call 边（可达分析完整）
                    let glue = Instance::resolve_drop_glue(self.tcx, p.ty);
                    let callee = self.linker.func_id(glue);
                    return Ok((
                        vec![Stmt::Trap(
                            format!("Drop glue 执行（ty={}，M4.1 第 5 步）", p.ty).into_boxed_str(),
                        )],
                        Terminator::Call {
                            callee,
                            args: vec![],
                            ret: RetDest::Ignore,
                            target: target.as_u32(),
                            unwind: self.lower_unwind(*unwind),
                        },
                    ));
                }
                (vec![], Terminator::Goto(target.as_u32()))
            }
            TK::Call { func, args, destination, target, unwind, .. } => {
                // callee 解析：常量 FnDef → Instance
                let fn_ty = self.op_ty(func)?;
                let ty::FnDef(def_id, gargs) = fn_ty.kind() else {
                    return Err(format!("间接调用（fn ptr，ty={fn_ty}，M4.1+）"));
                };
                let inst = Instance::expect_resolve(
                    self.tcx,
                    self.typing_env,
                    *def_id,
                    gargs,
                    term.source_info.span,
                );
                // 纯值 intrinsic：就地展开为 IR 语句（无调用开销；D5 内建的语句形态）
                if let Some(res) = self.try_expand_intrinsic(&inst, args, destination, *target)? {
                    return Ok(res);
                }
                // Linker 三路解析（debt-map §2-B）：普通函数/intrinsic fallback →
                // worklist 扩集；foreign → ①引擎原语 ②链接仿真 ③Trap
                let callee = self.linker.resolve_call(inst)?;
                // 实参展平（ABI v2）：失败前置 Trap 保 Call 边
                let mut pre: Vec<Stmt> = Vec::new();
                let mut ir_args = Vec::new();
                for a in args {
                    match self.lower_operand(&a.node) {
                        Ok(LoweredOp::Zst) => {}
                        Ok(LoweredOp::Scalar(o)) => ir_args.push(o),
                        Ok(LoweredOp::Pair(l, h)) => {
                            ir_args.push(l);
                            ir_args.push(h);
                        }
                        Ok(LoweredOp::Bytes { place, .. }) => {
                            ir_args.push(Operand::AddrOf(place.expr()));
                        }
                        Err(e) => {
                            pre.push(Stmt::Trap(format!("调用实参: {e}").into_boxed_str()));
                            ir_args.clear();
                            break;
                        }
                    }
                }
                // 返回落点（ABI v2 四路）
                let ret = if pre.is_empty() {
                    match self.resolve_place(destination).and_then(|dp| {
                        let kind = self.classify(dp.ty)?;
                        Ok(match kind {
                            ValKind::Zst => RetDest::Ignore,
                            ValKind::Scalar(w) => RetDest::Scalar(dp.scalar_place(w)),
                            ValKind::Pair((ao, aw), (bo, bw)) => {
                                RetDest::Pair(dp.half_place(ao, aw), dp.half_place(bo, bw))
                            }
                            ValKind::Other { .. } => RetDest::Indirect(dp.expr()),
                        })
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            pre.push(Stmt::Trap(format!("调用返回落点: {e}").into_boxed_str()));
                            RetDest::Ignore
                        }
                    }
                } else {
                    RetDest::Ignore
                };
                // 发散调用（target=None）→ 合成 Unreachable 落点块
                let tgt = match target {
                    Some(b) => b.as_u32(),
                    None => {
                        let idx = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                        self.extra_blocks
                            .push(ir::Block { stmts: vec![], term: Terminator::Unreachable });
                        idx
                    }
                };
                let term = match callee {
                    Callee::Func(id) => Terminator::Call {
                        callee: id,
                        args: ir_args,
                        ret,
                        target: tgt,
                        unwind: self.lower_unwind(*unwind),
                    },
                    Callee::Builtin(b) => {
                        // 引擎侧未落地的原语（alloc 系 = 第 5 步堆内建）前置 Trap 防静默
                        if !matches!(b, ir::Builtin::NoAllocShim) {
                            pre.push(Stmt::Trap(
                                format!("引擎原语 {b:?}（堆内建，M4.1 第 5 步）").into_boxed_str(),
                            ));
                        }
                        Terminator::CallBuiltin {
                            builtin: b,
                            args: ir_args,
                            ret,
                            target: tgt,
                            unwind: self.lower_unwind(*unwind),
                        }
                    }
                };
                (pre, term)
            }
            other => return Err(format!("终止子 {other:?}（M4.1+）")),
        })
    }

    /// 纯值 intrinsic 的就地展开（返回 Some = 已展开为 语句+Goto）。
    /// 本步最小集：offset/arith_offset（ptr::add 的根，rawptr gate 必经）。
    /// 完整内建表是 M4.1 第 5 步（D5）。
    fn try_expand_intrinsic(
        &mut self,
        inst: &Instance<'tcx>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
    ) -> Result<Option<(Vec<Stmt>, Terminator)>, String> {
        let InstanceKind::Intrinsic(def_id) = inst.def else {
            return Ok(None);
        };
        let name = self.tcx.item_name(def_id);
        let stmts = match name.as_str() {
            "offset" | "arith_offset" => {
                // fn offset<Ptr, Delta>(ptr: Ptr, count: Delta) -> Ptr
                let ptr_ty = self.op_ty(&args[0].node)?;
                let pointee = ptr_ty
                    .builtin_deref(true)
                    .ok_or_else(|| format!("offset 非指针（ty={ptr_ty}）"))?;
                let stride = self.layout_of(pointee)?.size.bytes();
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::PtrOffset {
                        ptr: self.lower_operand_scalar(&args[0].node)?,
                        count: self.lower_operand_scalar(&args[1].node)?,
                        stride,
                    },
                }]
            }
            _ => return Ok(None),
        };
        let tgt = target.ok_or("intrinsic 展开：发散 intrinsic？")?.as_u32();
        Ok(Some((stmts, Terminator::Goto(tgt))))
    }
}

fn elem_of(ty: Ty<'_>) -> Option<Ty<'_>> {
    match ty.kind() {
        ty::Array(t, _) | ty::Slice(t) => Some(*t),
        _ => None,
    }
}

fn u128_to_u64(v: u128) -> Result<u64, String> {
    u64::try_from(v).map_err(|_| "128 位判别式（M4.1+）".to_string())
}

/// `TyAndLayout::for_variant` 需要一个 LayoutCx；用 (tcx, typing_env) 现造一个。
struct LayoutCxAt<'tcx>(TyCtxt<'tcx>, TypingEnv<'tcx>);

impl<'tcx> rustc_abi::HasDataLayout for LayoutCxAt<'tcx> {
    fn data_layout(&self) -> &rustc_abi::TargetDataLayout {
        self.0.data_layout()
    }
}
impl<'tcx> rustc_middle::ty::layout::HasTyCtxt<'tcx> for LayoutCxAt<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.0
    }
}
impl<'tcx> rustc_middle::ty::layout::HasTypingEnv<'tcx> for LayoutCxAt<'tcx> {
    fn typing_env(&self) -> TypingEnv<'tcx> {
        self.1
    }
}

fn assert_msg(msg: &mir::AssertMessage<'_>) -> String {
    use mir::AssertKind::*;
    match msg {
        Overflow(op, ..) => format!("算术溢出（{op:?}）"),
        OverflowNeg(_) => "取负溢出".into(),
        DivisionByZero(_) => "除以零".into(),
        RemainderByZero(_) => "取余以零".into(),
        BoundsCheck { .. } => "下标越界".into(),
        other => format!("{other:?}"),
    }
}

/// 语句 → ir 语句（Ok(None) = 无操作）。
fn lower_stmt<'tcx>(
    cx: &LowerCx<'tcx, '_>,
    stmt: &mir::Statement<'tcx>,
) -> Result<Vec<Stmt>, String> {
    use mir::StatementKind as SK;
    match &stmt.kind {
        SK::Assign(box (place, rv)) => cx.lower_assign(place, rv),
        SK::StorageLive(_) | SK::StorageDead(_) | SK::Nop | SK::PlaceMention(_)
        | SK::ConstEvalCounter | SK::Coverage(_) => Ok(vec![]),
        SK::Intrinsic(box mir::NonDivergingIntrinsic::Assume(_)) => Ok(vec![]),
        SK::SetDiscriminant { place, variant_index } => {
            let p = cx.resolve_place(place)?;
            let ty = p.ty;
            cx.set_discr_stmts(&p, ty, *variant_index)
        }
        other => Err(format!("语句 {other:?}（M4.1+）")),
    }
}

/// 一个 instance 的降低。Err = 整函数 Trap（layout 失败等）；
/// 语句级不支持 → 该块 Trap（细粒度）。
pub(crate) fn lower_instance<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    instance: Instance<'tcx>,
    linker: &mut Linker<'tcx>,
) -> Result<ir::FuncBody, String> {
    // intrinsic 无普通 MIR（fallback-body 型由 Linker 以 new_raw 补收为 Item）
    if let InstanceKind::Intrinsic(..) = instance.def {
        return Err("intrinsic 实例（M4.x 内建）".into());
    }
    let body_ref: &Body<'tcx> = tcx.instance_mir(instance.def);
    // 整体单态化（一次 clone + instantiate；Body: TypeFoldable）
    let body: Body<'tcx> = instance.instantiate_mir_and_normalize_erasing_regions(
        tcx,
        typing_env,
        EarlyBinder::bind(tcx, body_ref.clone()),
    );

    let mut frame = frame::freeze(tcx, typing_env, &body)?;

    // 返回通道（_0，ABI v2）：聚合 = indirect + sret 槽（帧尾追加 8 字节）
    let ret = {
        let ret_info = &frame.locals[0];
        match ret_info.kind {
            ValKind::Zst => RetAbi::Zst,
            ValKind::Scalar(w) => RetAbi::Scalar(Slot { off: ret_info.off, width: w }),
            ValKind::Pair((ao, aw), (bo, bw)) => RetAbi::Pair(
                Slot { off: ret_info.off + ao, width: aw },
                Slot { off: ret_info.off + bo, width: bw },
            ),
            ValKind::Other { size } => {
                let sret_off = (frame.size + 7) & !7;
                let ret_off = ret_info.off;
                frame.size = sret_off + 8;
                frame.align = frame.align.max(8);
                RetAbi::Indirect { ret_off, size: size as u32, sret_off }
            }
        }
    };

    // 参数落位（_1..=_argc，ABI v2）
    let mut params = Vec::new();
    for local in body.args_iter() {
        let info = &frame.locals[local.as_usize()];
        params.push(match info.kind {
            ValKind::Zst => ParamAbi::Zst,
            ValKind::Scalar(w) => ParamAbi::Scalar(Slot { off: info.off, width: w }),
            ValKind::Pair((ao, aw), (bo, bw)) => ParamAbi::Pair(
                Slot { off: info.off + ao, width: aw },
                Slot { off: info.off + bo, width: bw },
            ),
            ValKind::Other { size } => ParamAbi::Indirect { off: info.off, size: size as u32 },
        });
    }

    let name = tcx.symbol_name(instance).name.to_owned();
    let mir_block_count = body.basic_blocks.len();
    let mut cx =
        LowerCx { tcx, typing_env, frame, linker, extra_blocks: Vec::new(), mir_block_count };

    let mut blocks = Vec::with_capacity(mir_block_count);
    for bb_data in body.basic_blocks.iter() {
        let mut stmts = Vec::new();
        for stmt in &bb_data.statements {
            match lower_stmt(&cx, stmt) {
                Ok(mut s) => stmts.append(&mut s),
                Err(reason) => {
                    // 语句级 Trap：执行到此即诊断退出；终止子照常降低（保 Call 边）
                    stmts.push(Stmt::Trap(reason.into_boxed_str()));
                    break;
                }
            }
        }
        let term = match cx.lower_terminator(bb_data.terminator()) {
            Ok((mut extra, t)) => {
                stmts.append(&mut extra);
                t
            }
            Err(reason) => Terminator::Trap(reason.into_boxed_str()),
        };
        blocks.push(ir::Block { stmts, term });
    }
    blocks.append(&mut cx.extra_blocks);

    Ok(ir::FuncBody {
        frame_size: cx.frame.size,
        frame_align: cx.frame.align,
        ret,
        params,
        blocks,
        name: name.into_boxed_str(),
    })
}
