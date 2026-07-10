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
        caller_loc_off: None,
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
    fn push_offset(&mut self, o: i64) {
        // 连续常量偏移折叠（Field 链一个 Offset）
        if o == 0 {
            return;
        }
        if let Some(PlaceStep::Offset(prev)) = self.steps.last_mut() {
            *prev += o as i32;
        } else if self.steps.is_empty()
            && o >= 0
            && let PlaceBase::Local(off) = &mut self.base
        {
            // 纯帧内非负：直接折进基址偏移（保住快路径）
            *off += o as u32;
        } else {
            self.steps.push(PlaceStep::Offset(o as i32));
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
                *prev += o as i32;
            } else {
                steps.push(PlaceStep::Offset(o as i32));
            }
        }
        PlaceExpr { base: self.base, steps: steps.into_boxed_slice() }
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

/// 调用目标三形态（finish_call 共用道）。
enum CallTarget {
    Direct(Callee),
    /// fn-ptr / vtable 槽：operand 求值 = D4 条目真地址。
    /// Option = extern "C" 系 fn-ptr 的冻结签名（M4.4 FFI 反方向之二：
    /// 反查未命中 → native 真码 libffi 直调；虚派发/Rust ABI 恒 None）
    Indirect(Operand, Option<ir::ForeignSig>),
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
    /// 本函数 #[track_caller] 时的 &Location 槽（转发/caller_location intrinsic 读取）
    caller_loc_off: Option<u32>,
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
                    p.push_offset(layout.fields.offset(f.as_usize()).bytes() as i64);
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
                        if let ty::Array(_, n) = p.ty.kind() {
                            // 数组长度已知：折常量
                            let n = n
                                .try_to_target_usize(self.tcx)
                                .ok_or("数组长度非常量")?;
                            p.push_offset(((n - offset) * stride) as i64);
                        } else {
                            // slice：addr += len×stride - offset×stride（len = meta 槽）
                            let Some(Operand::Slot(ms)) = p.meta else {
                                return Err(
                                    "ConstantIndex from_end：meta 非帧槽（M4.3+）".into()
                                );
                            };
                            p.steps.push(PlaceStep::IndexScaled { idx: ms, stride });
                            p.push_offset(-((offset * stride) as i64));
                        }
                    } else {
                        p.push_offset((offset * stride) as i64);
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
    fn lower_operand(&mut self, op: &mir::Operand<'tcx>) -> Result<LoweredOp<'tcx>, String> {
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
                let kind = frame::classify(self.tcx, &layout);
                let val = c
                    .const_
                    .eval(self.tcx, self.typing_env, c.span)
                    .map_err(|e| format!("常量求值失败: {e:?}"))?;
                self.lower_const_value(val, ty, kind)
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

    /// 常量值 → 泛化操作数（常量池/statics：物化进冻结区，重定位成真地址——F5）。
    fn lower_const_value(
        &mut self,
        val: mir::ConstValue,
        ty: Ty<'tcx>,
        kind: ValKind,
    ) -> Result<LoweredOp<'tcx>, String> {
        use mir::interpret::Scalar as S;
        Ok(match val {
            mir::ConstValue::Scalar(S::Int(si)) => {
                let bits = si.to_bits(si.size());
                match kind {
                    ValKind::Scalar(width) if bits <= u64::MAX as u128 => {
                        LoweredOp::Scalar(Operand::Imm { bits: bits as u64, width })
                    }
                    // 128 位整数常量：物化 16 字节进冻结区，走 memcpy 通道
                    ValKind::Other { size: 16 } => {
                        let p = self.linker.frozen_alloc_bytes(&bits.to_le_bytes());
                        LoweredOp::Bytes {
                            place: PlaceLow {
                                base: PlaceBase::Static(p),
                                steps: Vec::new(),
                                ty,
                                meta: None,
                            },
                            size: 16,
                        }
                    }
                    _ => return Err(format!("整数常量分类漂移（ty={ty}）")),
                }
            }
            mir::ConstValue::Scalar(S::Ptr(ptr, _)) => {
                // 指针常量（static 引用 / fn ptr / vtable）：物化目标 → 真地址立即数
                let (prov, off) = ptr.prov_and_relative_offset();
                let base = self.linker.ensure_alloc(prov.alloc_id())?;
                LoweredOp::Scalar(Operand::Imm {
                    bits: base.wrapping_add(off.bytes()),
                    width: Width::W64,
                })
            }
            mir::ConstValue::Slice { alloc_id, meta } => {
                // &str/&[u8] 字面量：胖指针 pair =（数据真地址, meta）
                let base = self.linker.ensure_alloc(alloc_id)?;
                LoweredOp::Pair(
                    Operand::Imm { bits: base, width: Width::W64 },
                    Operand::Imm { bits: meta, width: Width::W64 },
                )
            }
            mir::ConstValue::Indirect { alloc_id, offset } => {
                // 内存常量（Layout/空表模板等）：物化后按分类经冻结区地址访问
                let base = self.linker.ensure_alloc(alloc_id)?.wrapping_add(offset.bytes());
                let sexpr = |o: u64| PlaceExpr {
                    base: PlaceBase::Static(base.wrapping_add(o)),
                    steps: Box::new([]),
                };
                match kind {
                    ValKind::Zst => LoweredOp::Zst,
                    ValKind::Scalar(w) => {
                        LoweredOp::Scalar(Operand::Mem { expr: sexpr(0), width: w })
                    }
                    ValKind::Pair((ao, aw), (bo, bw)) => LoweredOp::Pair(
                        Operand::Mem { expr: sexpr(ao as u64), width: aw },
                        Operand::Mem { expr: sexpr(bo as u64), width: bw },
                    ),
                    ValKind::Other { size } => LoweredOp::Bytes {
                        place: PlaceLow {
                            base: PlaceBase::Static(base),
                            steps: Vec::new(),
                            ty,
                            meta: None,
                        },
                        size,
                    },
                }
            }
            mir::ConstValue::ZeroSized => LoweredOp::Zst,
        })
    }

    /// 标量操作数（标量语境；其他分类 = 语境错误诊断）。
    fn lower_operand_scalar(&mut self, op: &mir::Operand<'tcx>) -> Result<Operand, String> {
        match self.lower_operand(op)? {
            LoweredOp::Scalar(o) => Ok(o),
            LoweredOp::Zst => Err("意外的 ZST 操作数".into()),
            LoweredOp::Pair(..) => {
                Err(format!("非标量操作数（pair，ty={}，M4.1+）", self.op_ty_str(op)))
            }
            LoweredOp::Bytes { .. } => {
                Err(format!("非标量操作数（聚合，ty={}，M4.1+）", self.op_ty_str(op)))
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

    /// span → &'static Location 常量（物化进冻结区）——Assert 展开 / track_caller
    /// 实参合成 / caller_location intrinsic 共用（cg_ssa get_caller_location 同构）。
    fn caller_location_imm(&mut self, span: rustc_span::Span) -> Result<Operand, String> {
        let cv = self.tcx.span_as_caller_location(span);
        let mir::ConstValue::Scalar(mir::interpret::Scalar::Ptr(ptr, _)) = cv else {
            return Err("caller_location 常量形态异常".into());
        };
        let (prov, off) = ptr.prov_and_relative_offset();
        let base = self.linker.ensure_alloc(prov.alloc_id())?;
        Ok(Operand::Imm { bits: base.wrapping_add(off.bytes()), width: Width::W64 })
    }

    /// 被调方 requires_caller_location 时的隐藏尾实参：本帧转发或按调用点合成。
    fn caller_loc_arg(
        &mut self,
        callee: &Instance<'tcx>,
        span: rustc_span::Span,
    ) -> Result<Option<Operand>, String> {
        if !callee.def.requires_caller_location(self.tcx) {
            return Ok(None);
        }
        Ok(Some(match self.caller_loc_off {
            Some(off) => Operand::Slot(Slot { off, width: Width::W64 }), // 转发
            None => self.caller_location_imm(span)?,
        }))
    }

    /// 128 位操作数 → place 表达式（Bytes 通道；标量常量物化）。
    fn wide_place(&mut self, op: &mir::Operand<'tcx>) -> Result<PlaceExpr, String> {
        match self.lower_operand(op)? {
            LoweredOp::Bytes { place, .. } => Ok(place.expr()),
            _ => Err("128 位操作数非 place".into()),
        }
    }

    /// 128 位双目（Bin128）：移位量右操作数可为 ≤64 标量。
    fn lower_bin128(
        &mut self,
        op: IntBinOp,
        signed: bool,
        a: &mir::Operand<'tcx>,
        b: &mir::Operand<'tcx>,
        dst_p: &PlaceLow<'tcx>,
        with_overflow: bool,
    ) -> Result<Vec<Stmt>, String> {
        use ir::Bin128Rhs;
        let pa = self.wide_place(a)?;
        let b_ty = self.op_ty(b)?;
        let rhs = if matches!(b_ty.kind(), ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128))
        {
            Bin128Rhs::Wide(self.wide_place(b)?)
        } else {
            Bin128Rhs::Scalar(self.lower_operand_scalar(b)?)
        };
        if with_overflow {
            // (u128, bool)：bool 必须在 +16（engine 写死）——布局核验
            let l = self.layout_of(dst_p.ty)?;
            if l.fields.offset(1).bytes() != 16 {
                return Err("128 位溢出对布局漂移".into());
            }
        }
        Ok(vec![Stmt::Bin128 {
            op,
            signed,
            a: pa,
            b: rhs,
            dst: dst_p.expr(),
            with_overflow,
        }])
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
        &mut self,
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
            // 标量 → 同尺寸小聚合（transmute u32→[u8;4] 等）：按宽度裸写
            (ValKind::Other { size }, LoweredOp::Scalar(o))
                if o.width().bytes() as u64 == size =>
            {
                vec![Stmt::Assign {
                    dst: ScalarPlace::Mem { expr: dst.expr(), width: o.width() },
                    rv: Rvalue::Use(o),
                }]
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
        &mut self,
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
                let a_ty = self.op_ty(a)?;
                // 128 位 WithOverflow：(u128, bool) 聚合，旗标 @+16
                if matches!(a_ty.kind(), ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)) {
                    let bop = match op {
                        OvfOp::Add => IntBinOp::Add,
                        OvfOp::Sub => IntBinOp::Sub,
                        OvfOp::Mul => IntBinOp::Mul,
                    };
                    return self.lower_bin128(
                        bop,
                        frame::ty_signed(a_ty),
                        a,
                        b,
                        &dst_p,
                        true,
                    );
                }
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
                // 128 位整数：比较走 Cmp128；算术/位/移位走 Bin128（宿主 u128 直算）
                if matches!(a_ty.kind(), ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)) {
                    let signed = frame::ty_signed(a_ty);
                    let cc = match binop {
                        Eq => Some(IntCc::Eq),
                        Ne => Some(IntCc::Ne),
                        Lt => Some(IntCc::Lt),
                        Le => Some(IntCc::Le),
                        Gt => Some(IntCc::Gt),
                        Ge => Some(IntCc::Ge),
                        _ => None,
                    };
                    if let Some(cc) = cc {
                        let pa = self.wide_place(a)?;
                        let pb = self.wide_place(b)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("128 位比较目标非标量".into());
                        };
                        return Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::Cmp128 { cc, signed, a: pa, b: pb },
                        }]);
                    }
                    let bop = match binop {
                        Add | AddUnchecked => IntBinOp::Add,
                        Sub | SubUnchecked => IntBinOp::Sub,
                        Mul | MulUnchecked => IntBinOp::Mul,
                        Div => IntBinOp::Div,
                        Rem => IntBinOp::Rem,
                        BitAnd => IntBinOp::BitAnd,
                        BitOr => IntBinOp::BitOr,
                        BitXor => IntBinOp::BitXor,
                        Shl | ShlUnchecked => IntBinOp::Shl,
                        Shr | ShrUnchecked => IntBinOp::Shr,
                        other => return Err(format!("128 位 BinOp {other:?}（M4.3+）")),
                    };
                    return self.lower_bin128(bop, signed, a, b, &dst_p, false);
                }
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
                        // cg_ssa 同构：**仅 Adt 做 variant downcast**——Coroutine/Closure/
                        // Tuple 的 operands（upvars）落顶层 fields（coroutine 的 variant
                        // fields 是暂停点 saved locals，不是 upvars！）；随后写判别式。
                        let (vidx, active_field, use_variant) = match kind {
                            AK::Adt(_, v, _, _, af) => (*v, *af, true),
                            _ => (VariantIdx::ZERO, None, false),
                        };
                        let layout = self.layout_of(dst_p.ty)?;
                        let field_layout = if use_variant
                            && !matches!(
                                layout.variants,
                                Variants::Single { .. } | Variants::Empty
                            ) {
                            layout.for_variant(&LayoutCxAt(self.tcx, self.typing_env), vidx)
                        } else {
                            layout
                        };
                        let mut stmts = Vec::new();
                        for (i, op) in operands.iter().enumerate() {
                            let fi = active_field.map(|f| f.as_usize()).unwrap_or(i);
                            let off = field_layout.fields.offset(fi).bytes() as u32;
                            stmts.extend(self.write_at(&dst_p, off, op)?);
                        }
                        // 判别式：enum 全部要写；coroutine 初始 variant（Unresumed=0）
                        if dst_p.ty.is_enum() || dst_p.ty.is_coroutine() {
                            stmts.extend(self.set_discr_stmts(&dst_p, dst_p.ty, vidx)?);
                        }
                        Ok(stmts)
                    }
                }
            }
            mir::Rvalue::ThreadLocalRef(def_id) => {
                // M4.4 D3：per-thread 实例——稠密 TlsId，执行期 Ctx.tls 惰性物化
                // （heap 分配 + 冻结模板拷贝）。v1 记账：dtor 不跑（设计 D3）。
                let id = self.linker.tls_id(*def_id)?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("ThreadLocalRef 目标非标量".into());
                };
                Ok(vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv: Rvalue::TlsRef(id) }])
            }
            mir::Rvalue::WrapUnsafeBinder(op, _) => {
                let src = self.lower_operand(op)?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
        }
    }

    /// Cast 家族。
    fn lower_cast(
        &mut self,
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
                let signed = frame::ty_signed(a_ty);
                // 128 位方向：→u128/i128 = 低半 cast + 高半符号扩；u128→小 = 取低半
                if let ValKind::Other { size: 16 } = dst_kind {
                    let from_w = frame::scalar_width(&a_layout).ok_or("128 cast 源非标量")?;
                    let ao = self.lower_operand_scalar(a)?;
                    let lo = Stmt::Assign {
                        dst: dst_p.half_place(0, Width::W64),
                        rv: Rvalue::Cast { from: (from_w, signed), to: Width::W64, a: ao },
                    };
                    let hi_rv = if signed {
                        // 算术右移 63 位复制符号
                        Rvalue::IntBin {
                            op: IntBinOp::Shr,
                            signed: true,
                            a: dst_p.half_operand(0, Width::W64),
                            b: Operand::Imm { bits: 63, width: Width::W64 },
                        }
                    } else {
                        Rvalue::Use(Operand::Imm { bits: 0, width: Width::W64 })
                    };
                    let hi = Stmt::Assign { dst: dst_p.half_place(8, Width::W64), rv: hi_rv };
                    return Ok(vec![lo, hi]);
                }
                if a_layout.size.bytes() == 16 {
                    // u128/i128 → ≤64：截断 = 取低半
                    let src_p = match a {
                        mir::Operand::Copy(pl) | mir::Operand::Move(pl) => {
                            self.resolve_place(pl)?
                        }
                        _ => return Err("128 位常量 cast（M4.3+）".into()),
                    };
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("IntToInt 目标非标量".into());
                    };
                    return Ok(vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::Cast {
                            from: (Width::W64, false),
                            to: w,
                            a: src_p.half_operand(0, Width::W64),
                        },
                    }]);
                }
                let from_w = frame::scalar_width(&a_layout).ok_or("cast 源非标量")?;
                let to_layout = self.layout_of(to_ty)?;
                let to_w =
                    frame::scalar_width(&to_layout).ok_or("cast 目标非标量（M4.3+）")?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("IntToInt 目标非标量".into());
                };
                debug_assert_eq!(w.bytes(), to_w.bytes());
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Cast {
                        from: (from_w, signed),
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
                    _ => {
                        // 常量：lower_const_value 已物化四路（Slice/Indirect 进冻结区），
                        // transmute = 同尺寸位重解释 → 同型搬运即可（&str→&[u8] 同构 pair）
                        let src = self.lower_operand(a)?;
                        self.assign_lowered(dst_p, dst_kind, src)
                            .map_err(|e| format!("Transmute 常量: {e}"))
                    }
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
                            (_, ty::Dynamic(dyn_preds, _)) => {
                                // dyn→dyn 同 principal（仅剥 auto trait，如 Any+Send→Any）：
                                // vtable 不变 = pair 位拷（cg_ssa unsized_info 同判据）
                                if let ty::Dynamic(src_preds, _) = src_pointee.kind() {
                                    if src_preds.principal_def_id()
                                        == dyn_preds.principal_def_id()
                                    {
                                        let src = self.lower_operand(a)?;
                                        return self.assign_lowered(dst_p, dst_kind, src);
                                    }
                                    return Err(format!(
                                        "dyn 上溯 vtable 变换（{src_pointee} → {dst_pointee}，M4.2+）"
                                    ));
                                }
                                // dyn unsize：pair =（data 瘦指针, 物化的 vtable 真地址）
                                let src_layout = self.layout_of(src_pointee)?;
                                if src_layout.is_unsized() {
                                    return Err(format!(
                                        "unsized→dyn（{src_pointee} → {dst_pointee}，M4.1+）"
                                    ));
                                }
                                let principal = dyn_preds.principal().map(|b| {
                                    self.tcx.instantiate_bound_regions_with_erased(b)
                                });
                                let vt_id =
                                    self.tcx.vtable_allocation((src_pointee, principal));
                                let vt_addr = self.linker.ensure_alloc(vt_id)?;
                                let ValKind::Pair((ao, aw), (bo, bw)) = dst_kind else {
                                    return Err("dyn Unsize 目标非 pair".into());
                                };
                                let LoweredOp::Scalar(data) = self.lower_operand(a)? else {
                                    return Err("dyn Unsize 源非瘦指针（M4.1+）".into());
                                };
                                Ok(vec![
                                    Stmt::Assign {
                                        dst: dst_p.half_place(ao, aw),
                                        rv: Rvalue::Use(data),
                                    },
                                    Stmt::Assign {
                                        dst: dst_p.half_place(bo, bw),
                                        rv: Rvalue::Use(Operand::Imm {
                                            bits: vt_addr,
                                            width: bw,
                                        }),
                                    },
                                ])
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
                    PC::ReifyFnPointer(..) => {
                        // FnDef（ZST）→ fn ptr：D4 条目表给真地址身份
                        let a_ty = self.op_ty(a)?;
                        let ty::FnDef(def_id, gargs) = a_ty.kind() else {
                            return Err(format!("ReifyFnPointer 源非 FnDef（{a_ty}）"));
                        };
                        let inst = Instance::expect_resolve(
                            self.tcx,
                            self.typing_env,
                            *def_id,
                            gargs,
                            rustc_span::DUMMY_SP,
                        );
                        let addr = self.linker.fn_entry_addr(inst);
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("ReifyFnPointer 目标非标量".into());
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::Use(Operand::Imm { bits: addr, width: w }),
                        }])
                    }
                    PC::ClosureFnPointer(..) => {
                        // 无捕获闭包 → fn ptr（cg_ssa 同构：resolve_closure FnOnce）
                        let a_ty = self.op_ty(a)?;
                        let ty::Closure(def_id, cargs) = a_ty.kind() else {
                            return Err(format!("ClosureFnPointer 源非闭包（{a_ty}）"));
                        };
                        let inst = Instance::resolve_closure(
                            self.tcx,
                            *def_id,
                            cargs,
                            ty::ClosureKind::FnOnce,
                        );
                        let addr = self.linker.fn_entry_addr(inst);
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("ClosureFnPointer 目标非标量".into());
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::Use(Operand::Imm { bits: addr, width: w }),
                        }])
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
            mir::UnwindAction::Terminate(_) => ir::UnwindAction::Terminate,
            // Unreachable：unwind 到此 = UB（fast 不检测）——当 Continue
            mir::UnwindAction::Continue | mir::UnwindAction::Unreachable => {
                ir::UnwindAction::Continue
            }
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
            // cleanup 链尾：返回 guard.drop 让宿主 unwind 续传（spike3 协议）
            TK::UnwindResume => (vec![], Terminator::Resume),
            TK::UnwindTerminate(_) => (vec![], Terminator::TerminateAbort),
            // 分析用假边：codegen 语义 = 直跳真目标
            TK::FalseEdge { real_target, .. } | TK::FalseUnwind { real_target, .. } => {
                (vec![], Terminator::Goto(real_target.as_u32()))
            }
            TK::Assert { cond, expected, msg, target, unwind } => {
                // cg_ssa codegen_assert_terminator 同构：条件分支 + 合成 panic 块
                //（Call panic lang item，实参 + location 尾参——panic fn 全 track_caller）
                let c = self.lower_operand_scalar(cond)?;
                use rustc_hir::LangItem;
                let mut pargs: Vec<Operand> = Vec::new();
                let lang_item = match &**msg {
                    mir::AssertKind::BoundsCheck { len, index } => {
                        pargs.push(self.lower_operand_scalar(index)?);
                        pargs.push(self.lower_operand_scalar(len)?);
                        LangItem::PanicBoundsCheck
                    }
                    mir::AssertKind::MisalignedPointerDereference { required, found } => {
                        pargs.push(self.lower_operand_scalar(required)?);
                        pargs.push(self.lower_operand_scalar(found)?);
                        LangItem::PanicMisalignedPointerDereference
                    }
                    mir::AssertKind::InvalidEnumConstruction(_) => {
                        return Err("InvalidEnumConstruction assert（u128 实参，M4.2+）".into());
                    }
                    other => other.panic_function(),
                };
                pargs.push(match self.caller_loc_off {
                    Some(off) => Operand::Slot(Slot { off, width: Width::W64 }),
                    None => self.caller_location_imm(term.source_info.span)?,
                });
                let def_id = self.tcx.require_lang_item(lang_item, term.source_info.span);
                let callee = self.linker.func_id(Instance::mono(self.tcx, def_id));
                // 合成：panic 块（发散 Call → Unreachable 落点）
                let unreach = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks
                    .push(ir::Block { stmts: vec![], term: Terminator::Unreachable });
                let panic_blk = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks.push(ir::Block {
                    stmts: vec![],
                    term: Terminator::Call {
                        callee,
                        args: pargs,
                        ret: RetDest::Ignore,
                        target: unreach,
                        unwind: self.lower_unwind(*unwind),
                    },
                });
                (
                    vec![],
                    Terminator::SwitchInt {
                        discr: c,
                        targets: vec![(*expected as u128, target.as_u32())],
                        otherwise: panic_blk,
                    },
                )
            }
            TK::Drop { place, target, unwind, .. } => {
                let p = self.resolve_place(place)?;
                if p.ty.needs_drop(self.tcx, self.typing_env) {
                    // dyn place：虚 drop = vtable 槽 0 间接调（cg_ssa 同构；
                    // resolve_drop_glue 会解析回自身 → 无限递归）
                    if let ty::Dynamic(..) = p.ty.kind() {
                        let meta = p
                            .meta
                            .clone()
                            .ok_or_else(|| format!("dyn Drop 无 vtable meta（ty={}）", p.ty))?;
                        let callee = operand_deref_at(meta, 0)?;
                        return Ok((
                            vec![],
                            Terminator::CallIndirect {
                                callee,
                                args: vec![Operand::AddrOf(p.expr())],
                                ret: RetDest::Ignore,
                                target: target.as_u32(),
                                unwind: self.lower_unwind(*unwind),
                                null_ok: true,
                                native_sig: None,
                            },
                        ));
                    }
                    // 正常路径 Drop = 普通 Call（F2）：drop_in_place 合成 shim，
                    // 实参 = place 真地址（*mut T）；unsized place（Box<[T]> 内容等）
                    // 的 glue 参数是胖指针 → 补 meta 半（resolve_place 的 meta 跟踪）。
                    let glue = Instance::resolve_drop_glue(self.tcx, p.ty);
                    let callee = self.linker.func_id(glue);
                    let mut glue_args = vec![Operand::AddrOf(p.expr())];
                    if self.layout_of(p.ty)?.is_unsized() {
                        let meta = p
                            .meta
                            .clone()
                            .ok_or_else(|| format!("unsized Drop 无 meta（ty={}）", p.ty))?;
                        glue_args.push(meta);
                    }
                    return Ok((
                        vec![],
                        Terminator::Call {
                            callee,
                            args: glue_args,
                            ret: RetDest::Ignore,
                            target: target.as_u32(),
                            unwind: self.lower_unwind(*unwind),
                        },
                    ));
                }
                (vec![], Terminator::Goto(target.as_u32()))
            }
            TK::Call { func, args, destination, target, unwind, .. } => {
                // callee 解析：常量 FnDef → Instance；FnPtr → 间接调用
                let fn_ty = self.op_ty(func)?;
                let ty::FnDef(def_id, gargs) = fn_ty.kind() else {
                    if fn_ty.is_fn_ptr() {
                        // fn-ptr 间接调用：值 = D4 条目真地址，引擎反查派发；
                        // extern "C" 系另冻结 native 签名（反查未命中 = 运行期 dlsym
                        // 所得真码 → libffi 直调，M4.4 FFI 反方向之二）
                        let callee_op = self.lower_operand_scalar(func)?;
                        let native_sig =
                            super::freeze_c_fnptr_sig(self.tcx, self.typing_env, fn_ty);
                        let rust_call = fn_ty.fn_sig(self.tcx).skip_binder().abi()
                            == rustc_abi::ExternAbi::RustCall;
                        return self.finish_call(
                            CallTarget::Indirect(callee_op, native_sig),
                            None,
                            rust_call,
                            args,
                            destination,
                            *target,
                            *unwind,
                        );
                    }
                    return Err(format!("间接调用（ty={fn_ty}，M4.1+）"));
                };
                let mut inst = Instance::expect_resolve(
                    self.tcx,
                    self.typing_env,
                    *def_id,
                    gargs,
                    term.source_info.span,
                );
                // dyn 虚派发：receiver 胖指针拆 (data, vtable)，
                // callee = *(vtable + idx*8)，receiver 实参换 data 半。
                // track_caller 方法照传 location（vtable 侧是 VTable shim 接收）。
                if let InstanceKind::Virtual(_, idx) = inst.def {
                    let loc_arg = self.caller_loc_arg(&inst, term.source_info.span)?;
                    let rust_call = fn_ty.fn_sig(self.tcx).skip_binder().abi()
                        == rustc_abi::ExternAbi::RustCall;
                    return self.lower_virtual_call(
                        idx,
                        loc_arg,
                        rust_call,
                        args,
                        destination,
                        *target,
                        *unwind,
                    );
                }
                // 纯值 intrinsic：就地展开为 IR 语句（无调用开销；D5 内建的语句形态）
                if let Some(res) = self.try_expand_intrinsic(
                    &inst,
                    args,
                    destination,
                    *target,
                    *unwind,
                    term.source_info.span,
                )? {
                    return Ok(res);
                }
                // fallback-body intrinsic：调用点即换 new_raw（Item），caller ABI 与
                // callee 一致——track_caller attr 生效（cg_ssa IntrinsicResult::Fallback 同构）
                if let InstanceKind::Intrinsic(idef) = inst.def {
                    let intrinsic =
                        self.tcx.intrinsic(idef).expect("Intrinsic 必有 IntrinsicDef");
                    if intrinsic.must_be_overridden {
                        return Err(format!(
                            "intrinsic `{}` 无 fallback（引擎内建表，M4.2+）",
                            intrinsic.name
                        ));
                    }
                    inst = Instance::new_raw(idef, inst.args);
                }
                // #[track_caller] 的隐藏尾实参（转发或按调用点合成）
                let loc_arg = self.caller_loc_arg(&inst, term.source_info.span)?;
                // Linker 三路解析（debt-map §2-B）：普通函数/intrinsic fallback →
                // worklist 扩集；foreign → ①引擎原语 ②链接仿真 ③Trap
                let callee = self.linker.resolve_call(inst)?;
                let rust_call = fn_ty.fn_sig(self.tcx).skip_binder().abi()
                    == rustc_abi::ExternAbi::RustCall;
                return self.finish_call(
                    CallTarget::Direct(callee),
                    loc_arg,
                    rust_call,
                    args,
                    destination,
                    *target,
                    *unwind,
                );
            }
            other => return Err(format!("终止子 {other:?}（M4.1+）")),
        })
    }

    /// dyn 虚派发（InstanceKind::Virtual）：receiver 胖指针 (data, vtable)，
    /// callee = *(vtable + idx×8)（vtable 已按第 4 步物化，槽存 D4 fn 条目真地址），
    /// receiver 实参换 data 半（&dyn → &Concrete 瘦化）。
    #[allow(clippy::too_many_arguments)]
    fn lower_virtual_call(
        &mut self,
        idx: usize,
        loc_arg: Option<Operand>,
        rust_call: bool,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        let (data, vt) = match self.lower_operand(&args[0].node) {
            Ok(LoweredOp::Pair(data, vt)) => (data, vt),
            // by-value dyn 派发（cg_ssa Ref(PlaceValue{llextra:Some(meta)}) 臂同构）：
            // receiver 是 unsized dyn place 的 move（Box<dyn FnOnce>::call_once 内
            // `F::call_once(move (*self))`）——data = place 真地址、callee 经 place meta
            // 查 vtable 槽（槽内是 ShimKind::VTable shim：收 *mut Self 瘦指针再 move 出，
            // 本 nightly instance.rs resolve_for_vtable）。与 dyn Drop 同一形状。
            _ => {
                let Some(pl) = args[0].node.place() else {
                    return Err("dyn receiver 非胖指针亦非 place".into());
                };
                let p = self.resolve_place(&pl)?;
                if !matches!(p.ty.kind(), ty::Dynamic(..)) {
                    return Err(format!("dyn receiver 形态未知（ty={}）", p.ty));
                }
                let meta = p
                    .meta
                    .clone()
                    .ok_or_else(|| format!("by-value dyn receiver 无 meta（ty={}）", p.ty))?;
                (Operand::AddrOf(p.expr()), meta)
            }
        };
        let callee = operand_deref_at(vt, (idx * 8) as u32)?;
        self.finish_call_inner(
            CallTarget::Indirect(callee, None),
            loc_arg,
            rust_call,
            Some(data),
            args,
            destination,
            target,
            unwind,
        )
    }

    fn finish_call(
        &mut self,
        ct: CallTarget,
        loc_arg: Option<Operand>,
        rust_call: bool,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        self.finish_call_inner(ct, loc_arg, rust_call, None, args, destination, target, unwind)
    }

    /// rust-call ABI 尾参 tuple 的调用点拆传（cg_ssa codegen_arguments_untupled 同构）：
    /// 物理约定 = 字段展平（闭包本体 MIR 参数天然已拆开；shim 的 spread_arg 对称展开）。
    fn untuple_rust_call_arg(
        &mut self,
        op: &mir::Operand<'tcx>,
        out: &mut Vec<Operand>,
    ) -> Result<(), String> {
        let ty = self.op_ty(op)?;
        let ty::Tuple(fields) = ty.kind() else {
            return Err(format!("rust-call 尾参非 tuple（{ty}）"));
        };
        let layout = self.layout_of(ty)?;
        // 空/全 ZST tuple（含常量形态，如 `(fn项,)`）：无字段可传
        if fields.is_empty() || layout.is_zst() {
            return Ok(());
        }
        // 可字段投影的 place；常量 tuple（如 `(None,)` 提升常量）按整体分类映射字段
        let p = if let Some(pl) = op.place() {
            self.resolve_place(&pl)?
        } else {
            match self.lower_operand(op)? {
                LoweredOp::Zst => return Ok(()),
                // Indirect 常量已物化冻结区 → 有 place，照常字段投影
                LoweredOp::Bytes { place, .. } => place,
                // 整体标量 = 唯一非 ZST 字段即整体（ZST 字段两侧都跳过）
                LoweredOp::Scalar(o) => {
                    out.push(o);
                    return Ok(());
                }
                // 整体 pair：两半按**偏移**认领到字段（tuple 字段可重排），按字段序发出
                LoweredOp::Pair(a, b) => {
                    let ValKind::Pair((ao, _), (bo, _)) = self.classify(ty)? else {
                        return Err(format!("常量 tuple 分类漂移（{ty}）"));
                    };
                    let (mut ha, mut hb) = (Some(a), Some(b));
                    for (i, fty) in fields.iter().enumerate() {
                        if matches!(self.classify(fty)?, ValKind::Zst) {
                            continue;
                        }
                        let off = layout.fields.offset(i).bytes() as u32;
                        if off == ao
                            && let Some(x) = ha.take()
                        {
                            out.push(x);
                        } else if off == bo
                            && let Some(x) = hb.take()
                        {
                            out.push(x);
                        }
                    }
                    if ha.is_some() || hb.is_some() {
                        return Err(format!("常量 pair tuple 字段认领失败（{ty}）"));
                    }
                    return Ok(());
                }
            }
        };
        for (i, fty) in fields.iter().enumerate() {
            let off = layout.fields.offset(i).bytes() as u32;
            match self.classify(fty)? {
                ValKind::Zst => {}
                ValKind::Scalar(w) => out.push(p.half_operand(off, w)),
                ValKind::Pair((ao, aw), (bo, bw)) => {
                    out.push(p.half_operand(off + ao, aw));
                    out.push(p.half_operand(off + bo, bw));
                }
                // 聚合字段：传字段真地址（callee 侧 ParamAbi::Indirect memcpy 重组）
                ValKind::Other { .. } => out.push(Operand::AddrOf(p.expr_plus(off))),
            }
        }
        Ok(())
    }

    /// 调用收尾共用道：实参展平（ABI v2，失败前置 Trap 保 Call 边）→ 返回落点四路
    /// → 发散落点合成 → 按 CallTarget 发终止子。loc_arg = track_caller 隐藏尾实参。
    #[allow(clippy::too_many_arguments)]
    fn finish_call_inner(
        &mut self,
        ct: CallTarget,
        loc_arg: Option<Operand>,
        rust_call: bool,
        first_override: Option<Operand>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        // 变参 foreign：尾参 FfiKind 按调用点实参冻结
        let variadic_foreign =
            matches!(&ct, CallTarget::Direct(Callee::Foreign { variadic: true, .. }));
        let mut tail_kinds: Vec<ir::FfiKind> = Vec::new();
        let mut pre: Vec<Stmt> = Vec::new();
        let mut ir_args = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if i == 0
                && let Some(o) = &first_override
            {
                ir_args.push(o.clone());
                continue;
            }
            // rust-call ABI：尾参 tuple 逐字段拆传（物理约定 = 字段展平）
            if rust_call && i == args.len() - 1 {
                if let Err(e) = self.untuple_rust_call_arg(&a.node, &mut ir_args) {
                    pre.push(Stmt::Trap(format!("rust-call 尾参: {e}").into_boxed_str()));
                    ir_args.clear();
                }
                continue;
            }
            match self.lower_operand(&a.node) {
                Ok(LoweredOp::Zst) => {}
                Ok(LoweredOp::Scalar(o)) => {
                    if variadic_foreign {
                        let t = self.op_ty(&a.node)?;
                        tail_kinds.push(
                            super::ffi_kind_of(self.tcx, self.typing_env, t)
                                .map_err(|e| format!("变参实参 {t}: {e}"))?,
                        );
                    }
                    ir_args.push(o);
                }
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
        // #[track_caller]：&Location 隐藏尾实参（cg_ssa 同构，排展平序最后）
        if pre.is_empty()
            && let Some(loc) = loc_arg
        {
            ir_args.push(loc);
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
        let unwind = self.lower_unwind(unwind);
        let term = match ct {
            CallTarget::Direct(Callee::Func(id)) => {
                Terminator::Call { callee: id, args: ir_args, ret, target: tgt, unwind }
            }
            CallTarget::Direct(Callee::Builtin(b)) => {
                Terminator::CallBuiltin { builtin: b, args: ir_args, ret, target: tgt, unwind }
            }
            CallTarget::Direct(Callee::Foreign {
                sym,
                args: fixed,
                ret: fret,
                variadic,
                thunk_args,
            }) => {
                let sig = if variadic {
                    let nfixed = fixed.len();
                    let mut all = fixed;
                    all.extend(tail_kinds.drain(nfixed.min(tail_kinds.len())..));
                    ir::ForeignSig { args: all, ret: fret, fixed: Some(nfixed), thunk_args }
                } else {
                    ir::ForeignSig { args: fixed, ret: fret, fixed: None, thunk_args }
                };
                Terminator::CallForeign { sym, sig, args: ir_args, ret, target: tgt, unwind }
            }
            CallTarget::Indirect(callee, native_sig) => Terminator::CallIndirect {
                callee,
                args: ir_args,
                ret,
                target: tgt,
                unwind,
                null_ok: false,
                native_sig,
            },
        };
        Ok((pre, term))
    }

    /// 纯值 intrinsic 的就地展开（返回 Some = 已展开为 语句+Goto）。
    /// 本步最小集：offset/arith_offset（ptr::add 的根，rawptr gate 必经）。
    /// 完整内建表是 M4.1 第 5 步（D5）。
    #[allow(clippy::too_many_arguments)]
    fn try_expand_intrinsic(
        &mut self,
        inst: &Instance<'tcx>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
        span: rustc_span::Span,
    ) -> Result<Option<(Vec<Stmt>, Terminator)>, String> {
        let InstanceKind::Intrinsic(def_id) = inst.def else {
            return Ok(None);
        };
        let name = self.tcx.item_name(def_id);
        // 引擎级 intrinsic（非纯值展开）：catch_unwind 走 Builtin 通道
        if name.as_str() == "catch_unwind" {
            let (dst_p, w) = self.place_scalar(destination)?;
            let tgt = target.ok_or("catch_unwind 发散？")?.as_u32();
            return Ok(Some((
                vec![],
                Terminator::CallBuiltin {
                    builtin: ir::Builtin::CatchUnwind,
                    args: vec![
                        self.lower_operand_scalar(&args[0].node)?,
                        self.lower_operand_scalar(&args[1].node)?,
                        self.lower_operand_scalar(&args[2].node)?,
                    ],
                    ret: RetDest::Scalar(dst_p.scalar_place(w)),
                    target: tgt,
                    unwind: self.lower_unwind(unwind),
                },
            )));
        }
        // 泛型元素尺寸（copy/write_bytes/offset 的 T）
        let elem_size = |cx: &Self| -> Result<u64, String> {
            let t = inst.args.type_at(0);
            Ok(cx.layout_of(t)?.size.bytes())
        };
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
            "ctpop" | "ctlz" | "cttz" | "ctlz_nonzero" | "cttz_nonzero" | "bswap"
            | "bitreverse" => {
                use ir::BitUnOp as B;
                let op = match name.as_str() {
                    "ctpop" => B::Popcount,
                    "ctlz" | "ctlz_nonzero" => B::Ctlz,
                    "cttz" | "cttz_nonzero" => B::Cttz,
                    "bswap" => B::Bswap,
                    _ => B::Bitreverse,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::BitUn { op, a: self.lower_operand_scalar(&args[0].node)? },
                }]
            }
            "exact_div" => {
                let a_ty = self.op_ty(&args[0].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntBin {
                        op: IntBinOp::Div,
                        signed: frame::ty_signed(a_ty),
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                    },
                }]
            }
            "atomic_load" => {
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::AtomicLoad {
                        addr: self.lower_operand_scalar(&args[0].node)?,
                        width: w,
                    },
                }]
            }
            "atomic_store" => {
                vec![Stmt::AtomicStore {
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                }]
            }
            "atomic_cxchg" | "atomic_cxchgweak" => {
                // (ptr, expected, new) -> (T, bool)
                let dst_p = self.resolve_place(destination)?;
                let ValKind::Pair((vo, vw), (fo, fw)) = self.classify(dst_p.ty)? else {
                    return Err("cxchg 目标非 pair".into());
                };
                vec![Stmt::AtomicCxchg {
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    expected: self.lower_operand_scalar(&args[1].node)?,
                    new: self.lower_operand_scalar(&args[2].node)?,
                    dst_val: dst_p.half_place(vo, vw),
                    dst_ok: dst_p.half_place(fo, fw),
                    weak: name.as_str() == "atomic_cxchgweak",
                }]
            }
            "atomic_xchg" | "atomic_xadd" | "atomic_xsub" | "atomic_and" | "atomic_or"
            | "atomic_xor" | "atomic_nand" => {
                use ir::RmwOp as R;
                let op = match name.as_str() {
                    "atomic_xchg" => R::Xchg,
                    "atomic_xadd" => R::Add,
                    "atomic_xsub" => R::Sub,
                    "atomic_and" => R::And,
                    "atomic_or" => R::Or,
                    "atomic_xor" => R::Xor,
                    _ => R::Nand,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::AtomicRmw {
                    op,
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    dst: dst_p.scalar_place(w),
                }]
            }
            // fence：单线程语义下宿主原子已全 SeqCst；M4.4 起补真 fence（此处 nop 会
            // 在真线程期复查——spike4 义务只关乎数据原子性，fence 弱序细化挂 M4.4）
            "atomic_fence" | "atomic_singlethreadfence" => {
                vec![Stmt::Nop]
            }
            "copy_nonoverlapping" | "copy" => {
                // (src, dst, count)——注意顺序与 C memcpy 相反
                vec![Stmt::MemCopy {
                    src: self.lower_operand_scalar(&args[0].node)?,
                    dst: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                    overlap: name.as_str() == "copy",
                }]
            }
            "write_bytes" => {
                vec![Stmt::MemSet {
                    dst: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                }]
            }
            "black_box" | "transmute" => {
                // 值透传（transmute 调用形态 = 位重解释；black_box = copy）
                let dst_p = self.resolve_place(destination)?;
                let dst_kind = self.classify(dst_p.ty)?;
                let src = self.lower_operand(&args[0].node)?;
                self.assign_lowered(&dst_p, dst_kind, src)?
            }
            "assume" => vec![Stmt::Nop],
            "abort" => {
                // core::intrinsics::abort：进程级中止（native=SIGILL trap，引擎=SIGABRT
                // ——信号差异记 m4-log，差分若较信号级再对齐）
                let tgt = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks
                    .push(ir::Block { stmts: vec![], term: Terminator::Unreachable });
                return Ok(Some((
                    vec![],
                    Terminator::CallBuiltin {
                        builtin: ir::Builtin::HostAbort,
                        args: vec![],
                        ret: RetDest::Ignore,
                        target: tgt,
                        unwind: ir::UnwindAction::Continue,
                    },
                )));
            }
            "assert_inhabited" | "assert_zero_valid" | "assert_mem_uninitialized_valid" => {
                // lower 期判定（collector 同构）：合法 → nop；违反 → 占位
                //（native 展开为 panic_nounwind；此路径本就是防御性死路）
                let req = rustc_middle::ty::layout::ValidityRequirement::from_intrinsic(name)
                    .expect("validity intrinsic 名");
                let t = inst.args.type_at(0);
                let ok = self
                    .tcx
                    .check_validity_requirement((req, self.typing_env.as_query_input(t)))
                    .map_err(|e| format!("validity 判定失败: {e}"))?;
                if ok {
                    vec![Stmt::Nop]
                } else {
                    vec![Stmt::Trap(
                        format!("{name} 违反（ty={t}——native panic_nounwind）").into_boxed_str(),
                    )]
                }
            }
            "saturating_add" | "saturating_sub" => {
                let a_ty = self.op_ty(&args[0].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntSat {
                        op: if name.as_str() == "saturating_add" { OvfOp::Add } else { OvfOp::Sub },
                        signed: frame::ty_signed(a_ty),
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                    },
                }]
            }
            "caller_location" => {
                // Location::caller()：本函数 track_caller → 读隐藏尾实参槽；
                // 否则按 intrinsic 调用点合成（罕见——caller 链通常 track 到底）
                let (dst_p, w) = self.place_scalar(destination)?;
                let op = match self.caller_loc_off {
                    Some(off) => Operand::Slot(Slot { off, width: Width::W64 }),
                    None => self.caller_location_imm(span)?,
                };
                vec![Stmt::Assign { dst: dst_p.scalar_place(w), rv: Rvalue::Use(op) }]
            }
            "compare_bytes" => {
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::MemCmp {
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        n: self.lower_operand_scalar(&args[2].node)?,
                    },
                }]
            }
            n if n.starts_with("simd_") => self.expand_simd(n, inst, args, destination)?,
            "ptr_offset_from" | "ptr_offset_from_unsigned" => {
                let ptr_ty = self.op_ty(&args[0].node)?;
                let pointee =
                    ptr_ty.builtin_deref(true).ok_or("ptr_offset_from 非指针")?;
                let stride = self.layout_of(pointee)?.size.bytes().max(1);
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::PtrDiff {
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        stride,
                    },
                }]
            }
            "size_of_val" | "min_align_of_val" | "align_of_val" => {
                // *const T 实参；T sized → 常量；[E]/str → meta 折算；dyn → vtable（M4.1+）
                let t = inst.args.type_at(0);
                let (dst_p, w) = self.place_scalar(destination)?;
                let is_size = name.as_str() == "size_of_val";
                let t_layout = self.layout_of(t)?;
                if t_layout.is_sized() {
                    let v =
                        if is_size { t_layout.size.bytes() } else { t_layout.align.abi.bytes() };
                    vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                    }]
                } else {
                    match t.kind() {
                        ty::Slice(e) | ty::Array(e, _) => {
                            let el = self.layout_of(*e)?;
                            if is_size {
                                // meta（元素数）× elem size
                                let LoweredOp::Pair(_, meta) =
                                    self.lower_operand(&args[0].node)?
                                else {
                                    return Err("size_of_val 实参非胖指针".into());
                                };
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::IntBin {
                                        op: IntBinOp::Mul,
                                        signed: false,
                                        a: meta,
                                        b: Operand::Imm {
                                            bits: el.size.bytes(),
                                            width: Width::W64,
                                        },
                                    },
                                }]
                            } else {
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(Operand::Imm {
                                        bits: el.align.abi.bytes(),
                                        width: w,
                                    }),
                                }]
                            }
                        }
                        ty::Str => {
                            if is_size {
                                let LoweredOp::Pair(_, meta) =
                                    self.lower_operand(&args[0].node)?
                                else {
                                    return Err("size_of_val 实参非胖指针".into());
                                };
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(meta),
                                }]
                            } else {
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(Operand::Imm { bits: 1, width: w }),
                                }]
                            }
                        }
                        ty::Dynamic(..) => {
                            // vtable 布局：[drop, size, align, ...]（COMMON_VTABLE_ENTRIES）
                            let LoweredOp::Pair(_, vt) = self.lower_operand(&args[0].node)?
                            else {
                                return Err(format!("{name} 实参非 dyn 胖指针").into());
                            };
                            let slot_off = if is_size { 8 } else { 16 };
                            vec![Stmt::Assign {
                                dst: dst_p.scalar_place(w),
                                rv: Rvalue::Use(operand_deref_at(vt, slot_off)?),
                            }]
                        }
                        _ => return Err(format!("{name} on unsized {t}（M4.1+）")),
                    }
                }
            }
            _ => return Ok(None),
        };
        let tgt = target.ok_or("intrinsic 展开：发散 intrinsic？")?.as_u32();
        Ok(Some((stmts, Terminator::Goto(tgt))))
    }
}

impl<'tcx> LowerCx<'tcx, '_> {
    /// SIMD 最小集（m4.1-design §4.1：hashbrown SSE2 group 探测；lane 几何冻结自
    /// SimdVector layout，每操作 = 逐 lane 宿主循环）。未支持的 simd_* = Err（Trap 占位）。
    fn expand_simd(
        &mut self,
        name: &str,
        inst: &Instance<'tcx>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        use ir::SimdBinOp as S;
        // lane 几何：T = 第一个泛型参（向量类型）
        let vec_ty = inst.args.type_at(0);
        let layout = self.layout_of(vec_ty)?;
        let rustc_abi::BackendRepr::SimdVector { element, count } = layout.backend_repr else {
            return Err(format!("simd intrinsic 非向量参（{vec_ty}）"));
        };
        let dl = self.tcx.data_layout();
        let lane_bytes = element.size(dl).bytes() as u8;
        let lanes = count as u16;
        let signed = matches!(element.primitive(), rustc_abi::Primitive::Int(_, true));
        // 向量 operand → place 地址表达式（Bytes 通道；常量已物化进冻结区）
        let vplace = |cx: &mut Self, op: &mir::Operand<'tcx>| -> Result<PlaceExpr, String> {
            match cx.lower_operand(op)? {
                LoweredOp::Bytes { place, .. } => Ok(place.expr()),
                _ => Err("simd 实参非向量（M4.1+）".into()),
            }
        };
        let bin = |cx: &mut Self, op: S| -> Result<Vec<Stmt>, String> {
            let a = vplace(cx, &args[0].node)?;
            let b = vplace(cx, &args[1].node)?;
            let dst = cx.resolve_place(destination)?.expr();
            Ok(vec![Stmt::SimdBin { op, dst, a, b, lanes, lane_bytes }])
        };
        match name {
            "simd_eq" => bin(self, S::Eq),
            "simd_ne" => bin(self, S::Ne),
            "simd_lt" => bin(self, S::Lt { signed }),
            "simd_le" => bin(self, S::Le { signed }),
            "simd_gt" => bin(self, S::Gt { signed }),
            "simd_ge" => bin(self, S::Ge { signed }),
            "simd_and" => bin(self, S::And),
            "simd_or" => bin(self, S::Or),
            "simd_xor" => bin(self, S::Xor),
            "simd_add" => bin(self, S::Add),
            "simd_sub" => bin(self, S::Sub),
            "simd_bitmask" => {
                let a = vplace(self, &args[0].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::SimdBitmask { a, lanes, lane_bytes },
                }])
            }
            "simd_reduce_all" | "simd_reduce_any" => {
                let a = vplace(self, &args[0].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::SimdReduce {
                        all: name == "simd_reduce_all",
                        a,
                        lanes,
                        lane_bytes,
                    },
                }])
            }
            "simd_shuffle" => {
                // (a, b, const idx 数组) -> 重排向量：索引 lower 期已知 → 展开为逐 lane 拷
                let mir::Operand::Constant(c) = &args[2].node else {
                    return Err("simd_shuffle 索引非常量".into());
                };
                let val = c
                    .const_
                    .eval(self.tcx, self.typing_env, c.span)
                    .map_err(|e| format!("shuffle 索引求值失败: {e:?}"))?;
                let mir::ConstValue::Indirect { alloc_id, offset } = val else {
                    return Err(format!("shuffle 索引形态 {val:?}（M4.2+）"));
                };
                let alloc = self.tcx.global_alloc(alloc_id).unwrap_memory();
                let ai = alloc.inner();
                // 索引元素是 u32（stdarch simd_shuffle! 宏产出 [u32; N]）
                let n_out = (ai.size().bytes() - offset.bytes()) / 4;
                let bytes = ai.inspect_with_uninit_and_ptr_outside_interpreter(
                    offset.bytes() as usize..ai.size().bytes() as usize,
                );
                let pa = vplace(self, &args[0].node)?;
                let pb = vplace(self, &args[1].node)?;
                let dst = self.resolve_place(destination)?;
                let lw = Width::from_bytes(lane_bytes as u64).ok_or("lane 宽度")?;
                let mut stmts = Vec::new();
                for i in 0..n_out as usize {
                    let idx = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
                    let (src, si) = if (idx as u64) < count {
                        (&pa, idx as u64)
                    } else {
                        (&pb, idx as u64 - count)
                    };
                    let src_off = (si * lane_bytes as u64) as u32;
                    let mut sexpr = src.clone();
                    let src_op = {
                        let mut steps = sexpr.steps.to_vec();
                        if src_off != 0 {
                            if let Some(PlaceStep::Offset(o)) = steps.last_mut() {
                                *o += src_off as i32;
                            } else {
                                steps.push(PlaceStep::Offset(src_off as i32));
                            }
                        }
                        sexpr.steps = steps.into_boxed_slice();
                        Operand::Mem { expr: sexpr, width: lw }
                    };
                    stmts.push(Stmt::Assign {
                        dst: dst.half_place(i as u32 * lane_bytes as u32, lw),
                        rv: Rvalue::Use(src_op),
                    });
                }
                Ok(stmts)
            }
            "simd_splat" => {
                // splat(val: E) -> T：几何从返回向量取（T 是第一个泛型参？splat 的
                // 泛型序是 <T(向量), E>？——此处从 destination 的 layout 直接冻结，最稳）
                let dst_p = self.resolve_place(destination)?;
                let dst_layout = self.layout_of(dst_p.ty)?;
                let rustc_abi::BackendRepr::SimdVector { element, count } =
                    dst_layout.backend_repr
                else {
                    return Err("simd_splat 目标非向量".into());
                };
                let lb = element.size(dl).bytes() as u8;
                let val = self.lower_operand_scalar(&args[0].node)?;
                Ok(vec![Stmt::SimdSplat {
                    dst: dst_p.expr(),
                    val,
                    lanes: count as u16,
                    lane_bytes: lb,
                }])
            }
            other => Err(format!("intrinsic `{other}`（SIMD，M4.1+）")),
        }
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

/// 在 operand 的值（指针）上再间接一层：*(op + off)。vtable 槽读取用。
fn operand_deref_at(op: Operand, off: u32) -> Result<Operand, String> {
    let deref_steps = |mut steps: Vec<PlaceStep>| {
        steps.push(PlaceStep::Deref);
        if off != 0 {
            steps.push(PlaceStep::Offset(off as i32));
        }
        steps.into_boxed_slice()
    };
    Ok(match op {
        Operand::Slot(s) => Operand::Mem {
            expr: PlaceExpr { base: PlaceBase::Local(s.off), steps: deref_steps(Vec::new()) },
            width: Width::W64,
        },
        Operand::Mem { expr, .. } => Operand::Mem {
            expr: PlaceExpr { base: expr.base, steps: deref_steps(expr.steps.into_vec()) },
            width: Width::W64,
        },
        // 常量 vtable 地址（常量 dyn 引用）：运行期从冻结区读槽
        Operand::Imm { bits, .. } => Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Static(bits.wrapping_add(off as u64)),
                steps: Box::new([]),
            },
            width: Width::W64,
        },
        Operand::AddrOf(_) => return Err("vtable operand 形态异常".into()),
    })
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

/// 语句 → ir 语句（Ok(None) = 无操作）。
fn lower_stmt<'tcx>(
    cx: &mut LowerCx<'tcx, '_>,
    stmt: &mir::Statement<'tcx>,
) -> Result<Vec<Stmt>, String> {
    use mir::StatementKind as SK;
    match &stmt.kind {
        SK::Assign(box (place, rv)) => cx.lower_assign(place, rv),
        SK::StorageLive(_) | SK::StorageDead(_) | SK::Nop | SK::PlaceMention(_)
        | SK::ConstEvalCounter | SK::Coverage(_) => Ok(vec![]),
        SK::Intrinsic(box mir::NonDivergingIntrinsic::Assume(_)) => Ok(vec![]),
        SK::Intrinsic(box mir::NonDivergingIntrinsic::CopyNonOverlapping(cp)) => {
            let ptr_ty = cx.op_ty(&cp.src)?;
            let pointee =
                ptr_ty.builtin_deref(true).ok_or("CopyNonOverlapping 源非指针")?;
            let elem_size = cx.layout_of(pointee)?.size.bytes();
            Ok(vec![Stmt::MemCopy {
                src: cx.lower_operand_scalar(&cp.src)?,
                dst: cx.lower_operand_scalar(&cp.dst)?,
                count: cx.lower_operand_scalar(&cp.count)?,
                elem_size,
                overlap: false,
            }])
        }
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

    // 参数落位（_1..=_argc，ABI v2）。
    //
    // rust-call ABI 的物理约定 = tuple **按字段展平**（cg_ssa/Miri 同构）：闭包本体的
    // MIR 参数天然已拆开（env, a, b），调用点把 tuple operand 逐字段拆传
    // （untuple_rust_call_arg）；shim body（ClosureOnce/VTable）的 spread_arg 标记
    // tuple local——此处按字段展开为多个参数（落位 = tuple local 内部偏移，prologue
    // 写入即重组）。单参闭包曾靠 (A,) 与 A 布局巧合蒙混（M4.1-4.3），双参闭包
    // （thread spawn 链的 map_try_fold/LocalKey::set）逼出真协议。
    let mut params = Vec::new();
    for local in body.args_iter() {
        let info = &frame.locals[local.as_usize()];
        if body.spread_arg == Some(local) {
            let rustc_middle::ty::TyKind::Tuple(fields) = info.ty.kind() else {
                return Err(format!("spread_arg 非 tuple（{}）", info.ty));
            };
            let layout = frame::layout_of(tcx, typing_env, info.ty)?;
            for (i, fty) in fields.iter().enumerate() {
                let foff = info.off + layout.fields.offset(i).bytes() as u32;
                let fl = frame::layout_of(tcx, typing_env, fty)?;
                params.push(match frame::classify(tcx, &fl) {
                    ValKind::Zst => ParamAbi::Zst,
                    ValKind::Scalar(w) => ParamAbi::Scalar(Slot { off: foff, width: w }),
                    ValKind::Pair((ao, aw), (bo, bw)) => ParamAbi::Pair(
                        Slot { off: foff + ao, width: aw },
                        Slot { off: foff + bo, width: bw },
                    ),
                    ValKind::Other { size } => {
                        ParamAbi::Indirect { off: foff, size: size as u32 }
                    }
                });
            }
            continue;
        }
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

    // #[track_caller]：帧尾 &Location 隐藏尾实参槽（cg_ssa ABI 同构）
    let caller_loc_off = if instance.def.requires_caller_location(tcx) {
        let off = (frame.size + 7) & !7;
        frame.size = off + 8;
        frame.align = frame.align.max(8);
        Some(off)
    } else {
        None
    };

    let name = tcx.symbol_name(instance).name.to_owned();
    let mir_block_count = body.basic_blocks.len();
    let mut cx = LowerCx {
        tcx,
        typing_env,
        frame,
        linker,
        caller_loc_off,
        extra_blocks: Vec::new(),
        mir_block_count,
    };

    // rustc API 的 panic 面不可控（布局角例等）——按 Trap-stub 协议兜成占位，
    // 绝不中止整个降低（诊断带 panic 消息，指认待修的构造）
    fn catch_lower<T>(
        f: impl FnOnce() -> Result<T, String> + std::panic::UnwindSafe,
    ) -> Result<T, String> {
        match std::panic::catch_unwind(f) {
            Ok(r) => r,
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("?");
                Err(format!("lower panic: {msg}（M4.x 待修）"))
            }
        }
    }

    let mut blocks = Vec::with_capacity(mir_block_count);
    for bb_data in body.basic_blocks.iter() {
        let mut stmts = Vec::new();
        for stmt in &bb_data.statements {
            let r = catch_lower(std::panic::AssertUnwindSafe(|| lower_stmt(&mut cx, stmt)));
            match r {
                Ok(mut s) => stmts.append(&mut s),
                Err(reason) => {
                    // 语句级 Trap：执行到此即诊断退出；终止子照常降低（保 Call 边）
                    stmts.push(Stmt::Trap(reason.into_boxed_str()));
                    break;
                }
            }
        }
        let term = match catch_lower(std::panic::AssertUnwindSafe(|| {
            cx.lower_terminator(bb_data.terminator())
        })) {
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
        caller_loc_off,
        blocks,
        name: name.into_boxed_str(),
    })
}
