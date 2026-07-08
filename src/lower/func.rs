//! 逐 instance 降低：单态化 MIR body → 引擎 FuncBody。
//!
//! 纪律（M4.0 设计 §3）：**Trap-stub 全覆盖**——语句/终止子/布局遇不认识的构造，
//! 当前块降为 `Trap(诊断)`，绝不中止整个降低。诊断串标注"哪一期欠的账"。

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::{self, Body};
use rustc_middle::ty::{self, EarlyBinder, Instance, InstanceKind, Ty, TyCtxt, TypingEnv};

use super::frame::{self, FrameLayout};
use crate::vm::engine::ir::{
    self, Bb, FuncId, IntBinOp, IntCc, Operand, OvfOp, Rvalue, Slot, Stmt, Terminator, Width,
};

/// 整函数不可降低时的占位体（被调用即 Trap，诊断给出原因）。
pub fn trap_body(name: &str, reason: &str) -> ir::FuncBody {
    ir::FuncBody {
        frame_size: 0,
        frame_align: 1,
        ret: None,
        params: Vec::new(),
        blocks: vec![ir::Block {
            stmts: Vec::new(),
            term: Terminator::Trap(format!("整函数未降低: {reason}").into_boxed_str()),
        }],
        name: name.into(),
    }
}

struct LowerCx<'tcx, 'a> {
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    frame: FrameLayout<'tcx>,
    ids: &'a FxHashMap<Instance<'tcx>, FuncId>,
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

    /// place → (帧偏移, 最终类型)。仅支持 local + Field 链（M4.0）。
    fn resolve_place(&self, place: &mir::Place<'tcx>) -> Result<(u32, Ty<'tcx>), String> {
        let info = &self.frame.locals[place.local.as_usize()];
        let mut off = info.off;
        let mut ty = info.ty;
        for elem in place.projection {
            match elem {
                mir::ProjectionElem::Field(f, fty) => {
                    let layout = self.layout_of(ty)?;
                    off += layout.fields.offset(f.as_usize()).bytes() as u32;
                    ty = fty;
                }
                other => return Err(format!("投影 {other:?}（M4.1）")),
            }
        }
        Ok((off, ty))
    }

    /// place → 标量槽。
    fn place_slot(&self, place: &mir::Place<'tcx>) -> Result<Slot, String> {
        let (off, ty) = self.resolve_place(place)?;
        let layout = self.layout_of(ty)?;
        let width = frame::scalar_width(&layout)
            .ok_or_else(|| format!("非标量 place（ty={ty}，M4.1）"))?;
        Ok(Slot { off, width })
    }

    /// 操作数 → ir 操作数。Ok(None) = ZST（上层跳过/占位）。
    fn lower_operand(&self, op: &mir::Operand<'tcx>) -> Result<Option<Operand>, String> {
        match op {
            mir::Operand::Copy(p) | mir::Operand::Move(p) => {
                let (_, ty) = self.resolve_place(p)?;
                let layout = self.layout_of(ty)?;
                if layout.is_zst() {
                    return Ok(None);
                }
                Ok(Some(Operand::Slot(self.place_slot(p)?)))
            }
            mir::Operand::Constant(c) => {
                let ty = c.const_.ty();
                let layout = self.layout_of(ty)?;
                if layout.is_zst() {
                    return Ok(None);
                }
                let width = frame::scalar_width(&layout)
                    .ok_or_else(|| format!("非标量常量（ty={ty}，M4.1）"))?;
                let val = c
                    .const_
                    .eval(self.tcx, self.typing_env, c.span)
                    .map_err(|e| format!("常量求值失败: {e:?}"))?;
                match val {
                    mir::ConstValue::Scalar(mir::interpret::Scalar::Int(si)) => {
                        let bits = si.to_bits(si.size());
                        if bits > u64::MAX as u128 {
                            return Err("128 位常量（M4.1）".into());
                        }
                        Ok(Some(Operand::Imm { bits: bits as u64, width }))
                    }
                    mir::ConstValue::Scalar(mir::interpret::Scalar::Ptr(..)) => {
                        Err("指针常量（static/fn-ptr，M4.1）".into())
                    }
                    other => Err(format!("常量形态 {other:?}（M4.1）")),
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
                Ok(Some(Operand::Imm { bits: v as u64, width: Width::W8 }))
            }
        }
    }

    /// 非 ZST 操作数（ZST 视为错误——调用方已按语义处理 ZST）。
    fn lower_operand_scalar(&self, op: &mir::Operand<'tcx>) -> Result<Operand, String> {
        self.lower_operand(op)?.ok_or_else(|| "意外的 ZST 操作数".into())
    }

    fn op_ty(&self, op: &mir::Operand<'tcx>) -> Result<Ty<'tcx>, String> {
        Ok(match op {
            mir::Operand::Copy(p) | mir::Operand::Move(p) => self.resolve_place(p)?.1,
            mir::Operand::Constant(c) => c.const_.ty(),
            mir::Operand::RuntimeChecks(_) => self.tcx.types.bool,
        })
    }

    /// Assign 语句 → ir 语句（可能多条）。
    fn lower_assign(
        &self,
        dst: &mir::Place<'tcx>,
        rv: &mir::Rvalue<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        let (_, dst_ty) = self.resolve_place(dst)?;
        let dst_layout = self.layout_of(dst_ty)?;

        // *WithOverflow：写 (值, 旗标) 标量对
        if let mir::Rvalue::BinaryOp(binop, box (a, b)) = rv {
            let ovf = match binop {
                mir::BinOp::AddWithOverflow => Some(OvfOp::Add),
                mir::BinOp::SubWithOverflow => Some(OvfOp::Sub),
                mir::BinOp::MulWithOverflow => Some(OvfOp::Mul),
                _ => None,
            };
            if let Some(op) = ovf {
                let (dst_off, _) = self.resolve_place(dst)?;
                let f0 = dst_layout.fields.offset(0).bytes() as u32;
                let f1 = dst_layout.fields.offset(1).bytes() as u32;
                let a_ty = self.op_ty(a)?;
                let a_layout = self.layout_of(a_ty)?;
                let vw = frame::scalar_width(&a_layout).ok_or("溢出算术的非标量操作数")?;
                return Ok(vec![Stmt::AssignOverflow {
                    op,
                    signed: frame::ty_signed(a_ty),
                    a: self.lower_operand_scalar(a)?,
                    b: self.lower_operand_scalar(b)?,
                    dst_val: Slot { off: dst_off + f0, width: vw },
                    dst_flag: Slot { off: dst_off + f1, width: Width::W8 },
                }]);
            }
        }

        if dst_layout.is_zst() {
            return Ok(vec![Stmt::Nop]); // 本期 rvalue 集无副作用
        }
        let dst_slot = self.place_slot(dst)?;

        let rvalue = match rv {
            // WithRetag：Tree Borrows 的 retag 语义是检查器的事（P3 不检测别名）——fast machine 忽略
            mir::Rvalue::Use(op, _retag) => Rvalue::Use(self.lower_operand_scalar(op)?),
            mir::Rvalue::BinaryOp(binop, box (a, b)) => {
                let a_ty = self.op_ty(a)?;
                let signed = frame::ty_signed(a_ty);
                let ao = self.lower_operand_scalar(a)?;
                let bo = self.lower_operand_scalar(b)?;
                use mir::BinOp::*;
                let int = |op| Rvalue::IntBin { op, signed, a: ao, b: bo };
                let cmp = |cc| Rvalue::IntCmp { cc, signed, a: ao, b: bo };
                match binop {
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
                    other => return Err(format!("BinOp {other:?}（M4.1）")),
                }
            }
            mir::Rvalue::UnaryOp(unop, a) => {
                let a_ty = self.op_ty(a)?;
                let ao = self.lower_operand_scalar(a)?;
                match unop {
                    mir::UnOp::Not if a_ty.is_bool() => Rvalue::NotBool(ao),
                    mir::UnOp::Not => Rvalue::NotBits(ao),
                    mir::UnOp::Neg => Rvalue::Neg(ao),
                    other => return Err(format!("UnOp {other:?}（M4.1）")),
                }
            }
            mir::Rvalue::Cast(mir::CastKind::IntToInt, a, to_ty) => {
                let a_ty = self.op_ty(a)?;
                let a_layout = self.layout_of(a_ty)?;
                let from_w = frame::scalar_width(&a_layout).ok_or("cast 源非标量")?;
                let to_layout = self.layout_of(*to_ty)?;
                let to_w = frame::scalar_width(&to_layout).ok_or("cast 目标非标量")?;
                Rvalue::Cast {
                    from: (from_w, frame::ty_signed(a_ty)),
                    to: to_w,
                    a: self.lower_operand_scalar(a)?,
                }
            }
            other => return Err(format!("Rvalue {other:?}（M4.1+）")),
        };
        Ok(vec![Stmt::Assign { dst: dst_slot, rv: rvalue }])
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
                let (_, ty) = self.resolve_place(place)?;
                if ty.needs_drop(self.tcx, self.typing_env) {
                    // glue 执行是 M4.2；但解析 glue instance 并保留 Call 边（可达分析完整）
                    let glue = Instance::resolve_drop_glue(self.tcx, ty);
                    if let Some(&callee) = self.ids.get(&glue) {
                        return Ok((
                            vec![Stmt::Trap(
                                format!("Drop glue 执行（ty={ty}，M4.2）").into_boxed_str(),
                            )],
                            Terminator::Call {
                                callee,
                                args: vec![],
                                ret: None,
                                target: target.as_u32(),
                                unwind: self.lower_unwind(*unwind),
                            },
                        ));
                    }
                    return Err(format!("Drop glue（ty={ty}，M4.2）"));
                }
                (vec![], Terminator::Goto(target.as_u32()))
            }
            TK::Call { func, args, destination, target, unwind, .. } => {
                // callee 解析：常量 FnDef → Instance
                let fn_ty = self.op_ty(func)?;
                let ty::FnDef(def_id, gargs) = fn_ty.kind() else {
                    return Err(format!("间接调用（fn ptr，ty={fn_ty}，M4.1）"));
                };
                let inst = Instance::expect_resolve(
                    self.tcx,
                    self.typing_env,
                    *def_id,
                    gargs,
                    term.source_info.span,
                );
                if let InstanceKind::Intrinsic(..) = inst.def {
                    return Err(format!(
                        "intrinsic `{}`（D5：fallback body 走普通降低——须收集为 Item）",
                        self.tcx.item_name(*def_id)
                    ));
                }
                let Some(&callee) = self.ids.get(&inst) else {
                    // foreign（extern）单独归因：这份清单就是 os:: 注册表的种子
                    if self.tcx.is_foreign_item(inst.def_id()) {
                        return Err(format!(
                            "foreign `{}`（→内建/直通路由：alloc 系 M4.1，其余 os:: M4.3）",
                            self.tcx.item_name(inst.def_id())
                        ));
                    }
                    return Err(format!("调用目标未收集: {inst}"));
                };
                // callee 已解析：实参/返回落点失败不丢 Call 边——前置 Trap 语句 + 保留调用
                // （执行到 Trap 即停，Call 不会真跑；BFS 可达分析保持完整）
                let mut pre: Vec<Stmt> = Vec::new();
                let mut ir_args = Vec::with_capacity(args.len());
                for a in args {
                    match self.lower_operand(&a.node) {
                        Ok(Some(o)) => ir_args.push(o),
                        Ok(None) => ir_args.push(Operand::Imm { bits: 0, width: Width::W8 }),
                        Err(e) => {
                            pre.push(Stmt::Trap(format!("调用实参: {e}").into_boxed_str()));
                            ir_args.clear();
                            break;
                        }
                    }
                }
                // 返回落点
                let ret = if pre.is_empty() {
                    match self.resolve_place(destination).and_then(|(_, ret_ty)| {
                        let l = self.layout_of(ret_ty)?;
                        if l.is_zst() {
                            Ok(None)
                        } else {
                            self.place_slot(destination).map(Some)
                        }
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            pre.push(Stmt::Trap(
                                format!("调用返回落点: {e}").into_boxed_str(),
                            ));
                            None
                        }
                    }
                } else {
                    None
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
                (
                    pre,
                    Terminator::Call {
                        callee,
                        args: ir_args,
                        ret,
                        target: tgt,
                        unwind: self.lower_unwind(*unwind),
                    },
                )
            }
            other => return Err(format!("终止子 {other:?}（M4.1+）")),
        })
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
        other => Err(format!("语句 {other:?}（M4.1+）")),
    }
}

/// 一个 instance 的降低。Err = 整函数 Trap（layout 失败等）；
/// 语句级不支持 → 该块 Trap（细粒度）。
pub fn lower_instance<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    instance: Instance<'tcx>,
    ids: &FxHashMap<Instance<'tcx>, FuncId>,
) -> Result<ir::FuncBody, String> {
    // intrinsic 无普通 MIR（fallback-body 型由收集器按 Item 收集）
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

    let frame = frame::freeze(tcx, typing_env, &body)?;

    // 返回槽（_0）：ZST=None；非标量=记 None 且 Return 处 Trap（防静默错值）
    let ret_info = &frame.locals[0];
    let (ret, ret_unsupported) = if ret_info.zst {
        (None, false)
    } else {
        match ret_info.scalar {
            Some(w) => (Some(Slot { off: ret_info.off, width: w }), false),
            None => (None, true),
        }
    };

    // 参数槽：ZST=None（占位保序）；非标量参 → 体照常降低 + 入口 Trap 语句
    // （防静默错值不变，但保住整个下游调用图——可达分析准确性）
    let mut params = Vec::new();
    let mut param_trap: Option<String> = None;
    for local in body.args_iter() {
        let info = &frame.locals[local.as_usize()];
        if info.zst {
            params.push(None);
        } else {
            match info.scalar {
                Some(w) => params.push(Some(Slot { off: info.off, width: w })),
                None => {
                    params.push(None);
                    if param_trap.is_none() {
                        param_trap = Some(format!("非标量参数（ty={}，M4.1）", info.ty));
                    }
                }
            }
        }
    }

    let name = tcx.symbol_name(instance).name.to_owned();
    let mir_block_count = body.basic_blocks.len();
    let mut cx = LowerCx { tcx, typing_env, frame, ids, extra_blocks: Vec::new(), mir_block_count };

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
                match (&t, ret_unsupported) {
                    // 非标量返回：跑到 Return 即 Trap（防静默返回 0）
                    (Terminator::Return, true) => Terminator::Trap("非标量返回（M4.1）".into()),
                    _ => t,
                }
            }
            Err(reason) => Terminator::Trap(reason.into_boxed_str()),
        };
        blocks.push(ir::Block { stmts, term });
    }
    blocks.append(&mut cx.extra_blocks);
    if let Some(reason) = param_trap {
        blocks[0].stmts.insert(0, Stmt::Trap(reason.into_boxed_str()));
    }

    Ok(ir::FuncBody {
        frame_size: cx.frame.size,
        frame_align: cx.frame.align,
        ret,
        params,
        blocks,
        name: name.into_boxed_str(),
    })
}
