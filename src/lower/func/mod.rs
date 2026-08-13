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
    RetDest, Rvalue, ScalarPlace, Slot, Stmt, SwitchDiscr, Terminator, Width,
};

mod asm;
mod call;
mod cast;
mod intrinsic;
mod simd;
mod term;
mod unsize;

use intrinsic::*;

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
        PlaceExpr {
            base: self.base,
            steps: self.steps.clone().into_boxed_slice(),
        }
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
        PlaceExpr {
            base: self.base,
            steps: steps.into_boxed_slice(),
        }
    }

    /// 标量位置（快路径优先）
    fn scalar_place(&self, w: Width) -> ScalarPlace {
        match self.frame_direct() {
            Some(off) => ScalarPlace::Slot(Slot { off, width: w }),
            None => ScalarPlace::Mem {
                expr: self.expr(),
                width: w,
            },
        }
    }

    /// 标量 operand（快路径优先）
    fn scalar_operand(&self, w: Width) -> Operand {
        match self.frame_direct() {
            Some(off) => Operand::Slot(Slot { off, width: w }),
            None => Operand::Mem {
                expr: self.expr(),
                width: w,
            },
        }
    }

    /// pair 半的标量位置
    fn half_place(&self, half_off: u32, w: Width) -> ScalarPlace {
        match self.frame_direct() {
            Some(off) => ScalarPlace::Slot(Slot {
                off: off + half_off,
                width: w,
            }),
            None => ScalarPlace::Mem {
                expr: self.expr_plus(half_off),
                width: w,
            },
        }
    }

    fn half_operand(&self, half_off: u32, w: Width) -> Operand {
        match self.frame_direct() {
            Some(off) => Operand::Slot(Slot {
                off: off + half_off,
                width: w,
            }),
            None => Operand::Mem {
                expr: self.expr_plus(half_off),
                width: w,
            },
        }
    }
}

/// 泛化 operand（值分类四路）。
enum LoweredOp<'tcx> {
    Zst,
    Scalar(Operand),
    Pair(Operand, Operand),
    /// 聚合（memcpy 通道）：源 place + 尺寸
    Bytes {
        place: PlaceLow<'tcx>,
        size: u64,
    },
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
    Direct {
        tag_off: u32,
        tag_w: Width,
        tag_signed: bool,
    },
    Niche {
        tag_off: u32,
        tag_w: Width,
        niche_start: u64,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
    /// 128 位 niche（大 niche_start，regex_automata 逼出）：tag 16 字节，u128 算术
    Niche128 {
        tag_off: u32,
        niche_start: u128,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
}

struct LowerCx<'tcx, 'a> {
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    /// 当前正在降低的单态实例；用于精确标记标准 main 捕获调用点。
    instance: Instance<'tcx>,
    /// 本函数的 def_id（asm_target_features 查询用，M5.0 inline asm 寄存器分配）
    def_id: rustc_hir::def_id::DefId,
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
                    let parent_ty = p.ty;
                    let layout = self.layout_of(p.ty)?;
                    let layout = match variant.take() {
                        Some(v) => layout.for_variant(&LayoutCxAt(self.tcx, self.typing_env), v),
                        None => layout,
                    };
                    let offset = layout.fields.offset(f.as_usize()).bytes();
                    let field_layout = self.layout_of(fty)?;
                    // unsized 字段在非零偏移的对齐处置（M5.2 D8k：嵌套 DST）：
                    // - 尾是 slice/str → 字段对齐**静态已知**（元素对齐），rustc 给的
                    //   offset 已按之对齐，直接用（含尾是 slice 的嵌套结构体，如
                    //   Outer{Packet<[u16]>}）；
                    // - 尾是 dyn → 字段对齐**运行期**（vtable），offset 是下界，须按
                    //   vtable alignment 向上取整（VTableAlignOffset）。
                    let needs_vtable_align = field_layout.is_unsized() && offset != 0 && {
                        let tail = self.tcx.struct_tail_for_codegen(fty, self.typing_env);
                        matches!(tail.kind(), ty::Dynamic(..))
                    };
                    if needs_vtable_align {
                        let meta = p.meta.clone().ok_or_else(|| {
                            format!("dyn 尾字段无 vtable meta（parent={parent_ty}, field={fty}）")
                        })?;
                        let packed = match parent_ty.kind() {
                            ty::Adt(def, _) => def.repr().pack.map(|align| align.bytes()),
                            _ => None,
                        };
                        p.steps.push(PlaceStep::VTableAlignOffset {
                            meta,
                            unaligned: offset,
                            packed,
                        });
                    } else {
                        p.push_offset(offset as i64);
                    }
                    p.ty = fty;
                }
                mir::ProjectionElem::Downcast(_, v) => {
                    variant = Some(v);
                }
                mir::ProjectionElem::Deref => {
                    let pointee =
                        p.ty.builtin_deref(true)
                            .ok_or_else(|| format!("Deref 非指针（ty={}）", p.ty))?;
                    // 进入 unsized pointee：记录胖指针 meta 的读取位置（本体 +8）
                    let pointee_layout = self.layout_of(pointee);
                    let unsized_pointee = matches!(&pointee_layout, Ok(l) if l.is_unsized());
                    if unsized_pointee {
                        p.meta = Some(match p.frame_direct() {
                            Some(off) => Operand::Slot(Slot {
                                off: off + 8,
                                width: Width::W64,
                            }),
                            None => Operand::Mem {
                                expr: p.expr_plus(8),
                                width: Width::W64,
                            },
                        });
                    } else {
                        p.meta = None;
                    }
                    p.steps.push(PlaceStep::Deref);
                    p.ty = pointee;
                }
                mir::ProjectionElem::Index(idx_local) => {
                    let elem_ty =
                        elem_of(p.ty).ok_or_else(|| format!("Index 非序列（ty={}）", p.ty))?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    let idx_info = &self.frame.locals[idx_local.as_usize()];
                    let Some(w) = idx_info.kind.scalar() else {
                        return Err("Index 下标非标量".into());
                    };
                    p.steps.push(PlaceStep::IndexScaled {
                        idx: Slot {
                            off: idx_info.off,
                            width: w,
                        },
                        stride,
                    });
                    p.ty = elem_ty;
                    p.meta = None;
                }
                mir::ProjectionElem::ConstantIndex {
                    offset,
                    min_length: _,
                    from_end,
                } => {
                    let elem_ty = elem_of(p.ty)
                        .ok_or_else(|| format!("ConstantIndex 非序列（ty={}）", p.ty))?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    if from_end {
                        if let ty::Array(_, n) = p.ty.kind() {
                            // 数组长度已知：折常量
                            let n = n.try_to_target_usize(self.tcx).ok_or("数组长度非常量")?;
                            p.push_offset(((n - offset) * stride) as i64);
                        } else {
                            // slice：addr += len×stride - offset×stride（len = meta 槽）
                            let Some(Operand::Slot(ms)) = p.meta else {
                                return Err("ConstantIndex from_end：meta 非帧槽（M4.3+）".into());
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
                mir::ProjectionElem::Subslice { from, to, from_end } => {
                    // `rest @ ..` 型切片模式（M4.4 补，真线程把 std 内路径逼可达）
                    let elem_ty =
                        elem_of(p.ty).ok_or_else(|| format!("Subslice 非序列（ty={}）", p.ty))?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    if let ty::Array(_, n) = p.ty.kind() {
                        // 数组：折常量——[from..to] / [from..N-to]，结果仍是定长数组
                        let n = n.try_to_target_usize(self.tcx).ok_or("数组长度非常量")?;
                        let new_len = if from_end { n - from - to } else { to - from };
                        p.push_offset((from * stride) as i64);
                        p.ty = Ty::new_array(self.tcx, elem_ty, new_len);
                        p.meta = None;
                    } else {
                        // slice（from_end 恒真，to 自尾计）：addr += from×stride；
                        // len' = len − (from+to)（meta 值减常量，Operand::SubImm）
                        if !from_end {
                            return Err("Subslice slice 而 from_end=false（MIR 不变量）".into());
                        }
                        let m = p
                            .meta
                            .clone()
                            .ok_or_else(|| format!("Subslice slice 无 meta（ty={}）", p.ty))?;
                        p.push_offset((from * stride) as i64);
                        p.meta = Some(Operand::SubImm {
                            base: Box::new(m),
                            sub: from + to,
                        });
                    }
                }
                mir::ProjectionElem::OpaqueCast(t) | mir::ProjectionElem::UnwrapUnsafeBinder(t) => {
                    p.ty = t;
                }
                // 当前全变体已覆盖；防未来 nightly 新增投影（Trap-stub 协议）
                #[allow(unreachable_patterns)]
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
                Ok(LoweredOp::Scalar(Operand::Imm {
                    bits: v as u64,
                    width: Width::W8,
                }))
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
                        LoweredOp::Scalar(Operand::Imm {
                            bits: bits as u64,
                            width,
                        })
                    }
                    // 128 位整数常量：物化 16 字节进冻结区，走 memcpy 通道
                    ValKind::Other { size: 16 } => {
                        let p = self.linker.frozen_alloc_bytes(&bits.to_le_bytes());
                        LoweredOp::Bytes {
                            place: PlaceLow {
                                base: PlaceBase::Static(ir::LinkAddr(p)),
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
                // P2：foreign 分配（extern static/fn 取址）→ GOT 槽读操作数
                //（decision-history §7.5c；启动相重填槽内容，字节码不烤宿主地址）
                if let Some(op) =
                    self.linker
                        .foreign_const_operand(prov.alloc_id(), base, off.bytes())
                {
                    LoweredOp::Scalar(op)
                } else {
                    LoweredOp::Scalar(Operand::AddrImm(ir::LinkAddr(
                        base.wrapping_add(off.bytes()),
                    )))
                }
            }
            mir::ConstValue::Slice { alloc_id, meta } => {
                // &str/&[u8] 字面量：胖指针 pair =（数据真地址, meta）
                let base = self.linker.ensure_alloc(alloc_id)?;
                LoweredOp::Pair(
                    Operand::AddrImm(ir::LinkAddr(base)),
                    Operand::Imm {
                        bits: meta,
                        width: Width::W64,
                    },
                )
            }
            mir::ConstValue::Indirect { alloc_id, offset } => {
                // 内存常量（Layout/空表模板等）：物化后按分类经冻结区地址访问
                let base = self
                    .linker
                    .ensure_alloc(alloc_id)?
                    .wrapping_add(offset.bytes());
                let sexpr = |o: u64| PlaceExpr {
                    base: PlaceBase::Static(ir::LinkAddr(base.wrapping_add(o))),
                    steps: Box::new([]),
                };
                match kind {
                    ValKind::Zst => LoweredOp::Zst,
                    ValKind::Scalar(w) => LoweredOp::Scalar(Operand::Mem {
                        expr: sexpr(0),
                        width: w,
                    }),
                    ValKind::Pair((ao, aw), (bo, bw)) => LoweredOp::Pair(
                        Operand::Mem {
                            expr: sexpr(ao as u64),
                            width: aw,
                        },
                        Operand::Mem {
                            expr: sexpr(bo as u64),
                            width: bw,
                        },
                    ),
                    ValKind::Other { size } => LoweredOp::Bytes {
                        place: PlaceLow {
                            base: PlaceBase::Static(ir::LinkAddr(base)),
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

    /// 帧尾划一个 8 字节暂存槽（frame.size 在 lower_instance 收尾才冻结进 FuncBody
    /// ——caller_loc/sret 同法）。size_of_val-dyn 等多值中间量用。
    fn scratch64(&mut self) -> Slot {
        let off = (self.frame.size + 7) & !7;
        self.frame.size = off + 8;
        self.frame.align = self.frame.align.max(8);
        Slot {
            off,
            width: Width::W64,
        }
    }

    /// 帧尾物化一个按 guest layout 对齐的临时 place。volatile store
    /// 的常量/非 place 操作数先在这里保留为完整位型，再由执行器
    /// 作 opaque 字节 volatile 写，避免把 padding 解释为整数。
    fn scratch_place(&mut self, ty: Ty<'tcx>) -> Result<PlaceLow<'tcx>, String> {
        let layout = self.layout_of(ty)?;
        let size = u32::try_from(layout.size.bytes())
            .map_err(|_| format!("临时值 {ty} 大小超出 u32 帧偏移"))?;
        let align = u32::try_from(layout.align.abi.bytes())
            .map_err(|_| format!("临时值 {ty} 对齐超出 u32"))?
            .max(1);
        let off = self
            .frame
            .size
            .checked_add(align - 1)
            .map(|n| n & !(align - 1))
            .ok_or_else(|| format!("临时值 {ty} 帧对齐溢出"))?;
        self.frame.size = off
            .checked_add(size)
            .ok_or_else(|| format!("临时值 {ty} 帧大小溢出"))?;
        self.frame.align = self.frame.align.max(align);
        Ok(PlaceLow {
            base: PlaceBase::Local(off),
            steps: Vec::new(),
            ty,
            meta: None,
        })
    }

    /// 标量操作数（标量语境；其他分类 = 语境错误诊断）。
    #[track_caller]
    fn lower_operand_scalar(&mut self, op: &mir::Operand<'tcx>) -> Result<Operand, String> {
        let loc = std::panic::Location::caller();
        match self.lower_operand(op)? {
            LoweredOp::Scalar(o) => Ok(o),
            LoweredOp::Zst => Err("意外的 ZST 操作数".into()),
            LoweredOp::Pair(..) => Err(format!(
                "非标量操作数（pair，ty={}，@{}:{}）",
                self.op_ty_str(op),
                loc.file(),
                loc.line()
            )),
            LoweredOp::Bytes { .. } => Err(format!(
                "非标量操作数（聚合，ty={}，M4.1+）",
                self.op_ty_str(op)
            )),
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
        self.op_ty(op)
            .map(|t| t.to_string())
            .unwrap_or_else(|_| "?".into())
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
        Ok(Operand::AddrImm(ir::LinkAddr(
            base.wrapping_add(off.bytes()),
        )))
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
            Some(off) => Operand::Slot(Slot {
                off,
                width: Width::W64,
            }), // 转发
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

    /// 128 位立即数 → 冻结区 16 字节 place（Neg 的 0 / Not 的全 1 常量边）。
    fn wide_const(&mut self, v: u128) -> PlaceExpr {
        let base = self.linker.frozen_alloc_bytes(&v.to_le_bytes());
        PlaceExpr {
            base: PlaceBase::Static(ir::LinkAddr(base)),
            steps: Box::new([]),
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
        let rhs = if matches!(
            b_ty.kind(),
            ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
        ) {
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
                TagInfo::Single {
                    discr: u128_to_u64(discr)?,
                }
            }
            Variants::Multiple {
                tag,
                tag_encoding,
                tag_field,
                ..
            } => {
                let dl = self.tcx.data_layout();
                let tag_off = layout.fields.offset(tag_field.as_usize()).bytes() as u32;
                let tag_bytes = tag.size(dl).bytes();
                // 128 位 tag（repr(u128) / 大 niche）：窄化读低 64 位——小端下值 ≤ u64
                // 时正确。安全由值域检查保证（SetDiscr/SwitchInt 的编译期 discr 走
                // u128_to_u64；Niche 的 niche_start 高位下面查）——绝不静默截断。
                let tag_w = Width::from_bytes(tag_bytes)
                    .or_else(|| (tag_bytes == 16).then_some(Width::W64))
                    .ok_or_else(|| format!("tag 宽 {tag_bytes} 字节（M4.5+）"))?;
                let tag_signed = matches!(tag.primitive(), rustc_abi::Primitive::Int(_, true));
                match tag_encoding {
                    TagEncoding::Direct => TagInfo::Direct {
                        tag_off,
                        tag_w,
                        tag_signed,
                    },
                    TagEncoding::Niche {
                        untagged_variant,
                        niche_variants,
                        niche_start,
                    } => {
                        let vstart = niche_variants.start.as_u32() as u64;
                        let vlen = (niche_variants.last.as_u32() - niche_variants.start.as_u32())
                            as u64
                            + 1;
                        let untagged = untagged_variant.as_u32() as u64;
                        // 128 位 niche【一律】走 u128 算术路径（cg_ssa operand.rs
                        // 读判别式：rel=tag−niche_start 全宽 wrapping 后 ule 比较）。
                        // 不能只按 niche_start 高位判——NonZero<u128> 的
                        // niche_start=0，截断 W64 读会把 lo=0 的合法大值
                        // （2^64/2^66/2^127…）误判进 niche（corpus 批5
                        // fixed_point 的 U64F64::sqrt 静默产 0 即此实锤）。
                        if tag_bytes == 16 {
                            return Ok(TagInfo::Niche128 {
                                tag_off,
                                niche_start: *niche_start,
                                variants_start: vstart,
                                variants_len: vlen,
                                untagged,
                            });
                        }
                        TagInfo::Niche {
                            tag_off,
                            tag_w,
                            niche_start: u128_to_u64(*niche_start & tag_w.mask() as u128)?,
                            variants_start: vstart,
                            variants_len: vlen,
                            untagged,
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
                // 128 位 tag 窄化前的值域守卫（防静默截断；tag_w ≤ W64 的判别式恒 fit）
                if tag_w == Width::W64 && discr > u64::MAX as u128 {
                    return Err(format!("128 位判别式 {discr} 超 64 位（M4.5+）"));
                }
                let bits = (discr as u64) & tag_w.mask();
                vec![Stmt::Assign {
                    dst: dst_p.half_place(tag_off, tag_w),
                    rv: Rvalue::Use(Operand::Imm { bits, width: tag_w }),
                }]
            }
            TagInfo::Niche {
                tag_off,
                tag_w,
                niche_start,
                variants_start,
                untagged,
                ..
            } => {
                let vi = vidx.as_u32() as u64;
                if vi == untagged {
                    vec![]
                } else {
                    let bits =
                        vi.wrapping_sub(variants_start).wrapping_add(niche_start) & tag_w.mask();
                    vec![Stmt::Assign {
                        dst: dst_p.half_place(tag_off, tag_w),
                        rv: Rvalue::Use(Operand::Imm { bits, width: tag_w }),
                    }]
                }
            }
            // 128 位 niche 写：tag_val = (vi − start + niche_start) 的 u128，落 16 字节两半
            TagInfo::Niche128 {
                tag_off,
                niche_start,
                variants_start,
                untagged,
                ..
            } => {
                let vi = vidx.as_u32() as u64;
                if vi == untagged {
                    vec![]
                } else {
                    let tag_val =
                        (vi.wrapping_sub(variants_start) as u128).wrapping_add(niche_start);
                    let w = Width::W64;
                    vec![
                        Stmt::Assign {
                            dst: dst_p.half_place(tag_off, w),
                            rv: Rvalue::Use(Operand::Imm {
                                bits: tag_val as u64,
                                width: w,
                            }),
                        },
                        Stmt::Assign {
                            dst: dst_p.half_place(tag_off + 8, w),
                            rv: Rvalue::Use(Operand::Imm {
                                bits: (tag_val >> 64) as u64,
                                width: w,
                            }),
                        },
                    ]
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
                vec![Stmt::Assign {
                    dst: dst_p.half_place(off, w),
                    rv: Rvalue::Use(o),
                }]
            }
            LoweredOp::Pair(l, h) => {
                let ValKind::Pair((ao, aw), (bo, bw)) = self.classify(self.op_ty(op)?)? else {
                    return Err("pair operand 分类漂移".into());
                };
                vec![
                    Stmt::Assign {
                        dst: dst_p.half_place(off + ao, aw),
                        rv: Rvalue::Use(l),
                    },
                    Stmt::Assign {
                        dst: dst_p.half_place(off + bo, bw),
                        rv: Rvalue::Use(h),
                    },
                ]
            }
            LoweredOp::Bytes { place, size } => {
                vec![Stmt::Copy {
                    dst: dst_p.expr_plus(off),
                    src: place.expr(),
                    size: size as u32,
                }]
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
                vec![Stmt::Assign {
                    dst: dst.scalar_place(w),
                    rv: Rvalue::Use(o),
                }]
            }
            (ValKind::Pair((ao, aw), (bo, bw)), LoweredOp::Pair(l, h)) => vec![
                Stmt::Assign {
                    dst: dst.half_place(ao, aw),
                    rv: Rvalue::Use(l),
                },
                Stmt::Assign {
                    dst: dst.half_place(bo, bw),
                    rv: Rvalue::Use(h),
                },
            ],
            (ValKind::Other { size }, LoweredOp::Bytes { place, size: ssz }) => {
                debug_assert_eq!(size, ssz);
                vec![Stmt::Copy {
                    dst: dst.expr(),
                    src: place.expr(),
                    size: size as u32,
                }]
            }
            // 位拷语境的跨分类（Transmute pair↔聚合等）：src 是 place 时走字节拷
            (ValKind::Pair(..) | ValKind::Scalar(_), LoweredOp::Bytes { place, size }) => {
                vec![Stmt::Copy {
                    dst: dst.expr(),
                    src: place.expr(),
                    size: size as u32,
                }]
            }
            // 标量 → 同尺寸小聚合（transmute u32→[u8;4] 等）：按宽度裸写
            (ValKind::Other { size }, LoweredOp::Scalar(o)) if o.width().bytes() as u64 == size => {
                vec![Stmt::Assign {
                    dst: ScalarPlace::Mem {
                        expr: dst.expr(),
                        width: o.width(),
                    },
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
                if matches!(
                    a_ty.kind(),
                    ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                ) {
                    let bop = match op {
                        OvfOp::Add => IntBinOp::Add,
                        OvfOp::Sub => IntBinOp::Sub,
                        OvfOp::Mul => IntBinOp::Mul,
                    };
                    return self.lower_bin128(bop, frame::ty_signed(a_ty), a, b, &dst_p, true);
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
                    // &[T;N] 经投影到 [T] 不会出现；meta 来自 deref 链。
                    let meta = p
                        .meta
                        .clone()
                        .ok_or_else(|| format!("unsized Ref 无 meta 来源（ty={}，M4.1+）", p.ty))?;
                    Ok(vec![
                        Stmt::Assign {
                            dst: dst_p.half_place(ao, aw),
                            rv: Rvalue::Ref(p.expr()),
                        },
                        Stmt::Assign {
                            dst: dst_p.half_place(bo, bw),
                            rv: Rvalue::Use(meta),
                        },
                    ])
                } else {
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("Ref 目标非标量".into());
                    };
                    Ok(vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::Ref(p.expr()),
                    }])
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
                // 胖指针比较（*const [T]/dyn 的 Eq/Ne：两半都比——裸指针 == 语义，
                // Arc::ptr_eq 等逼出）：eq = (data==)&(meta==)；ne = (data!=)|(meta!=)。
                // 两半宽度取自操作数自带（data 指针 + usize/vtable meta，均 W64）。
                if matches!(binop, Eq | Ne) && matches!(self.classify(a_ty)?, ValKind::Pair(..)) {
                    let LoweredOp::Pair(al, ah) = self.lower_operand(a)? else {
                        return Err("胖指针比较左非 pair".into());
                    };
                    let LoweredOp::Pair(bl, bh) = self.lower_operand(b)? else {
                        return Err("胖指针比较右非 pair".into());
                    };
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("胖指针比较目标非标量".into());
                    };
                    let (cc, comb) = if matches!(binop, Eq) {
                        (IntCc::Eq, IntBinOp::BitAnd)
                    } else {
                        (IntCc::Ne, IntBinOp::BitOr)
                    };
                    let s8 = Slot {
                        off: self.scratch64().off,
                        width: w,
                    };
                    return Ok(vec![
                        Stmt::Assign {
                            dst: ScalarPlace::Slot(s8),
                            rv: Rvalue::IntCmp {
                                cc,
                                signed: false,
                                a: al,
                                b: bl,
                            },
                        },
                        Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::IntCmp {
                                cc,
                                signed: false,
                                a: ah,
                                b: bh,
                            },
                        },
                        Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::IntBin {
                                op: comb,
                                signed: false,
                                a: dst_p.scalar_operand(w),
                                b: Operand::Slot(s8),
                            },
                        },
                    ]);
                }
                // 128 位整数：比较走 Cmp128；算术/位/移位走 Bin128（宿主 u128 直算）
                if matches!(
                    a_ty.kind(),
                    ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                ) {
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
                            rv: Rvalue::Cmp128 {
                                cc,
                                signed,
                                a: pa,
                                b: pb,
                            },
                        }]);
                    }
                    // 三向比较（three_way_compare → Ord::cmp）：dst(i8 Ordering) = (a>b) − (a<b)
                    if matches!(binop, Cmp) {
                        let pa = self.wide_place(a)?;
                        let pb = self.wide_place(b)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("128 位 Cmp 目标非标量".into());
                        };
                        let gt = Slot {
                            width: Width::W8,
                            ..self.scratch64()
                        };
                        let lt = Slot {
                            width: Width::W8,
                            ..self.scratch64()
                        };
                        return Ok(vec![
                            Stmt::Assign {
                                dst: ScalarPlace::Slot(gt),
                                rv: Rvalue::Cmp128 {
                                    cc: IntCc::Gt,
                                    signed,
                                    a: pa.clone(),
                                    b: pb.clone(),
                                },
                            },
                            Stmt::Assign {
                                dst: ScalarPlace::Slot(lt),
                                rv: Rvalue::Cmp128 {
                                    cc: IntCc::Lt,
                                    signed,
                                    a: pa,
                                    b: pb,
                                },
                            },
                            Stmt::Assign {
                                dst: dst_p.scalar_place(w),
                                rv: Rvalue::IntBin {
                                    op: IntBinOp::Sub,
                                    signed: false,
                                    a: Operand::Slot(gt),
                                    b: Operand::Slot(lt),
                                },
                            },
                        ]);
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
                    use ir::FloatOp as F;
                    // f128：16 字节宽通道（D8c）——place 操作数，比较产标量 bool
                    if matches!(a_ty.kind(), ty::Float(ty::FloatTy::F128)) {
                        let pa = self.wide_place(a)?;
                        let pb = self.wide_place(b)?;
                        let fop = match binop {
                            Add | AddUnchecked => Some(F::Add),
                            Sub | SubUnchecked => Some(F::Sub),
                            Mul | MulUnchecked => Some(F::Mul),
                            Div => Some(F::Div),
                            Rem => Some(F::Rem),
                            _ => None,
                        };
                        if let Some(op) = fop {
                            return Ok(vec![Stmt::F128Bin {
                                op,
                                a: pa,
                                b: pb,
                                dst: dst_p.expr(),
                            }]);
                        }
                        let cc = match binop {
                            Eq => IntCc::Eq,
                            Ne => IntCc::Ne,
                            Lt => IntCc::Lt,
                            Le => IntCc::Le,
                            Gt => IntCc::Gt,
                            Ge => IntCc::Ge,
                            other => {
                                return Err(format!("f128 BinOp {other:?}"));
                            }
                        };
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("f128 比较目标非标量".into());
                        };
                        return Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::F128Cmp { cc, a: pa, b: pb },
                        }]);
                    }
                    let fw = float_w(a_ty)?;
                    let ao = self.lower_operand_scalar(a)?;
                    let bo = self.lower_operand_scalar(b)?;
                    let fbin = |op| Rvalue::FloatBin {
                        op,
                        fw,
                        a: ao.clone(),
                        b: bo.clone(),
                    };
                    let fcmp = |cc| Rvalue::FloatCmp {
                        cc,
                        fw,
                        a: ao.clone(),
                        b: bo.clone(),
                    };
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
                    return Ok(vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: rvalue,
                    }]);
                }
                let signed = frame::ty_signed(a_ty);
                let ao = self.lower_operand_scalar(a)?;
                let bo = self.lower_operand_scalar(b)?;
                let int = |op| Rvalue::IntBin {
                    op,
                    signed,
                    a: ao.clone(),
                    b: bo.clone(),
                };
                let cmp = |cc| Rvalue::IntCmp {
                    cc,
                    signed,
                    a: ao.clone(),
                    b: bo.clone(),
                };
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
                    Cmp => Rvalue::IntCmp3 {
                        signed,
                        a: ao.clone(),
                        b: bo.clone(),
                    },
                    other => return Err(format!("BinOp {other:?}（M4.1+）")),
                };
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("整数运算目标非标量".into());
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: rvalue,
                }])
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
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::NotBool(ao),
                        }])
                    }
                    mir::UnOp::Not => {
                        // 128 位整数：x XOR 全 1（冻结区常量边，Bin128 通道）
                        if matches!(
                            a_ty.kind(),
                            ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                        ) {
                            let ones = self.wide_const(!0u128);
                            return Ok(vec![Stmt::Bin128 {
                                op: IntBinOp::BitXor,
                                signed: false,
                                a: self.wide_place(a)?,
                                b: ir::Bin128Rhs::Wide(ones),
                                dst: dst_p.expr(),
                                with_overflow: false,
                            }]);
                        }
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("Not 目标非标量".into());
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::NotBits(ao),
                        }])
                    }
                    mir::UnOp::Neg => {
                        if matches!(a_ty.kind(), ty::Float(ty::FloatTy::F128)) {
                            let pa = self.wide_place(a)?;
                            return Ok(vec![Stmt::F128Un {
                                op: ir::F128UnOp::Neg,
                                a: pa,
                                dst: dst_p.expr(),
                            }]);
                        }
                        // 128 位整数：0 − x（冻结区零常量；补码下 signed 与否同结果）
                        if matches!(
                            a_ty.kind(),
                            ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                        ) {
                            let zero = self.wide_const(0u128);
                            return Ok(vec![Stmt::Bin128 {
                                op: IntBinOp::Sub,
                                signed: false,
                                a: zero,
                                b: ir::Bin128Rhs::Wide(self.wide_place(a)?),
                                dst: dst_p.expr(),
                                with_overflow: false,
                            }]);
                        }
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("Neg 目标非标量".into());
                        };
                        let rv = if a_ty.is_floating_point() {
                            Rvalue::FloatNeg {
                                fw: float_w(a_ty)?,
                                a: ao,
                            }
                        } else {
                            Rvalue::Neg(ao)
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv,
                        }])
                    }
                }
            }
            mir::Rvalue::Cast(kind, a, to_ty) => {
                self.lower_cast(&dst_p, dst_kind, *kind, a, *to_ty)
            }
            mir::Rvalue::Repeat(op, n) => {
                let count = n.try_to_target_usize(self.tcx).ok_or("Repeat 长度非常量")?;
                match self.lower_operand(op)? {
                    LoweredOp::Scalar(val) => {
                        let elem_size = val.width().bytes();
                        Ok(vec![Stmt::RepeatScalar {
                            dst: dst_p.expr(),
                            val,
                            count,
                            elem_size,
                        }])
                    }
                    LoweredOp::Zst => Ok(vec![Stmt::Nop]),
                    // 聚合元素（pair/bytes，M4.4 rayon 逼出）：写一份进 dst[0]，
                    // 引擎从 dst[0] 字节复制铺满其余 count-1 份
                    _ if count == 0 => Ok(vec![Stmt::Nop]),
                    _ => {
                        let elem_size = self.layout_of(self.op_ty(op)?)?.size.bytes();
                        let mut v = self.write_at(&dst_p, 0, op)?;
                        v.push(Stmt::RepeatBytes {
                            first: dst_p.expr(),
                            count,
                            elem_size,
                        });
                        Ok(v)
                    }
                }
            }
            mir::Rvalue::Discriminant(pl) => {
                let p = self.resolve_place(pl)?;
                let ValKind::Scalar(dw) = dst_kind else {
                    return Err("Discriminant 目标非标量".into());
                };
                let rv = match self.tag_info(p.ty)? {
                    TagInfo::Single { discr } => Rvalue::Use(Operand::Imm {
                        bits: discr & dw.mask(),
                        width: dw,
                    }),
                    TagInfo::Direct {
                        tag_off,
                        tag_w,
                        tag_signed,
                    } => Rvalue::Cast {
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
                    // 128 位 niche：独立 stmt（读 16 字节 tag，u128 算术）
                    TagInfo::Niche128 {
                        tag_off,
                        niche_start,
                        variants_start,
                        variants_len,
                        untagged,
                    } => {
                        return Ok(vec![Stmt::NicheDiscr128 {
                            tag: p.expr_plus(tag_off),
                            niche_start,
                            variants_start,
                            variants_len,
                            untagged,
                            dst: dst_p.scalar_place(dw),
                        }]);
                    }
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(dw),
                    rv,
                }])
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
                        let (data, meta) = (
                            ops.next().ok_or("RawPtr 缺 data")?,
                            ops.next().ok_or("RawPtr 缺 meta")?,
                        );
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
                    AK::Adt(..)
                    | AK::Tuple
                    | AK::Closure(..)
                    | AK::Coroutine(..)
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
                            && !matches!(layout.variants, Variants::Single { .. } | Variants::Empty)
                        {
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
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::TlsRef(id),
                }])
            }
            mir::Rvalue::WrapUnsafeBinder(op, _) => {
                let src = self.lower_operand(op)?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
        }
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
        SK::StorageLive(_)
        | SK::StorageDead(_)
        | SK::Nop
        | SK::PlaceMention(_)
        | SK::ConstEvalCounter
        | SK::Coverage(_) => Ok(vec![]),
        SK::Intrinsic(box mir::NonDivergingIntrinsic::Assume(_)) => Ok(vec![]),
        SK::Intrinsic(box mir::NonDivergingIntrinsic::CopyNonOverlapping(cp)) => {
            let ptr_ty = cx.op_ty(&cp.src)?;
            let pointee = ptr_ty
                .builtin_deref(true)
                .ok_or("CopyNonOverlapping 源非指针")?;
            let elem_size = cx.layout_of(pointee)?.size.bytes();
            Ok(vec![Stmt::MemCopy {
                src: cx.lower_operand_scalar(&cp.src)?,
                dst: cx.lower_operand_scalar(&cp.dst)?,
                count: cx.lower_operand_scalar(&cp.count)?,
                elem_size,
                overlap: false,
            }])
        }
        SK::SetDiscriminant {
            place,
            variant_index,
        } => {
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
    // foreign item 无 MIR（instance_mir 即 rustc query panic）：fn-ptr 取址必须走
    // Linker::foreign_fn_entry_addr；入队到这里 = 上游登记路径漏判，Err 落 Trap
    // 体而非拖垮整个 rustc 进程。
    if tcx.is_foreign_item(instance.def_id()) {
        return Err("foreign 实例无 MIR 可降（fn-ptr 取址应走 foreign_fn_entry_addr）".into());
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
            ValKind::Scalar(w) => RetAbi::Scalar(Slot {
                off: ret_info.off,
                width: w,
            }),
            ValKind::Pair((ao, aw), (bo, bw)) => RetAbi::Pair(
                Slot {
                    off: ret_info.off + ao,
                    width: aw,
                },
                Slot {
                    off: ret_info.off + bo,
                    width: bw,
                },
            ),
            ValKind::Other { size } => {
                let sret_off = (frame.size + 7) & !7;
                let ret_off = ret_info.off;
                frame.size = sret_off + 8;
                frame.align = frame.align.max(8);
                RetAbi::Indirect {
                    ret_off,
                    size: size as u32,
                    sret_off,
                }
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
                    ValKind::Scalar(w) => ParamAbi::Scalar(Slot {
                        off: foff,
                        width: w,
                    }),
                    ValKind::Pair((ao, aw), (bo, bw)) => ParamAbi::Pair(
                        Slot {
                            off: foff + ao,
                            width: aw,
                        },
                        Slot {
                            off: foff + bo,
                            width: bw,
                        },
                    ),
                    ValKind::Other { size } => ParamAbi::Indirect {
                        off: foff,
                        size: size as u32,
                    },
                });
            }
            continue;
        }
        params.push(match info.kind {
            ValKind::Zst => ParamAbi::Zst,
            ValKind::Scalar(w) => ParamAbi::Scalar(Slot {
                off: info.off,
                width: w,
            }),
            ValKind::Pair((ao, aw), (bo, bw)) => ParamAbi::Pair(
                Slot {
                    off: info.off + ao,
                    width: aw,
                },
                Slot {
                    off: info.off + bo,
                    width: bw,
                },
            ),
            ValKind::Other { size } => ParamAbi::Indirect {
                off: info.off,
                size: size as u32,
            },
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
        instance,
        def_id: instance.def_id(),
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
