//! 调用一族（自 func.rs F12 整搬）：lower_virtual_call/finish_call/
//! untuple_rust_call_arg/finish_call_inner——Callee 三形态派发、FFI 聚合、
//! sret、track_caller。唯一入口 = term.rs 的 Call 臂。

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    /// dyn 虚派发（InstanceKind::Virtual）：receiver 胖指针 (data, vtable)，
    /// callee = *(vtable + idx×8)（vtable 已按第 4 步物化，槽存 D4 fn 条目真地址），
    /// receiver 实参换 data 半（&dyn → &Concrete 瘦化）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn lower_virtual_call(
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_call(
        &mut self,
        ct: CallTarget,
        loc_arg: Option<Operand>,
        rust_call: bool,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        self.finish_call_inner(
            ct,
            loc_arg,
            rust_call,
            None,
            args,
            destination,
            target,
            unwind,
        )
    }

    /// rust-call ABI 尾参 tuple 的调用点拆传（cg_ssa codegen_arguments_untupled 同构）：
    /// 物理约定 = 字段展平（闭包本体 MIR 参数天然已拆开；shim 的 spread_arg 对称展开）。
    pub(super) fn untuple_rust_call_arg(
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
    pub(super) fn finish_call_inner(
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
        // 变参 foreign：尾参 FfiKind 按调用点实参冻结（尾参位聚合 = C1 边界，响亮拒绝）
        let variadic_foreign = matches!(
            &ct,
            CallTarget::Direct(Callee::Foreign { variadic: true, .. })
        );
        let mut tail_kinds: Vec<ir::FfiKind> = Vec::new();
        let mut pre: Vec<Stmt> = Vec::new();
        let mut ir_args = Vec::new();
        // C1：foreign/native_sig 按位置查 FfiKind——聚合参数不按标量/pair 拆槽，
        // 改取 place 真地址（libffi avalue 直指连续字节）
        let ffi_arg_kinds: Option<&[ir::FfiKind]> = match &ct {
            CallTarget::Direct(Callee::Foreign { args: fixed, .. }) => Some(fixed),
            CallTarget::Indirect(_, Some(sig)) => Some(&sig.args),
            _ => None,
        };
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
            let agg_pos = matches!(
                ffi_arg_kinds.and_then(|ks| ks.get(i)),
                Some(ir::FfiKind::Agg(_))
            );
            match self.lower_operand(&a.node) {
                Ok(LoweredOp::Zst) => {}
                Ok(LoweredOp::Scalar(o)) if !agg_pos => {
                    if variadic_foreign {
                        let t = self.op_ty(&a.node)?;
                        let k = crate::lower::ffi_kind_of(self.tcx, self.typing_env, t)
                            .map_err(|e| format!("变参实参 {t}: {e}"))?;
                        if matches!(k, ir::FfiKind::Agg(_)) {
                            return Err(format!("变参尾参按值聚合（{t}，C1 边界）"));
                        }
                        tail_kinds.push(k);
                    }
                    ir_args.push(o);
                }
                Ok(LoweredOp::Pair(l, h)) if !agg_pos => {
                    ir_args.push(l);
                    ir_args.push(h);
                }
                Ok(LoweredOp::Bytes { place, .. }) => {
                    // F-07：变参固定聚合也须占位 tail_kinds——末端按 nfixed
                    // 位置对齐 drain，缺位会把真实尾参类型错切
                    if variadic_foreign {
                        let k = ffi_arg_kinds
                            .and_then(|ks| ks.get(i))
                            .expect("聚合实参位必有 FfiKind")
                            .clone();
                        tail_kinds.push(k);
                    }
                    ir_args.push(Operand::AddrOf(place.expr()));
                }
                Ok(LoweredOp::Scalar(_) | LoweredOp::Pair(..)) => {
                    // C1 按值聚合实参（≤16B 经 lower_operand 拆成 scalar/pair 槽）：
                    // 该 MIR 实参是个 place（move/copy），整个值在 guest 帧连续内存——
                    // 直接取 place 真地址交给 libffi avalue
                    if variadic_foreign {
                        // F-07：同上的 tail_kinds 位置占位
                        let k = ffi_arg_kinds
                            .and_then(|ks| ks.get(i))
                            .expect("聚合实参位必有 FfiKind")
                            .clone();
                        tail_kinds.push(k);
                    }
                    let Some(pl) = a.node.place() else {
                        pre.push(Stmt::Trap(
                            "C1 按值聚合实参非 place（常量展开未接）"
                                .to_string()
                                .into_boxed_str(),
                        ));
                        ir_args.clear();
                        break;
                    };
                    let dp = match self.resolve_place(&pl) {
                        Ok(dp) => dp,
                        Err(e) => {
                            pre.push(Stmt::Trap(format!("C1 实参落点: {e}").into_boxed_str()));
                            ir_args.clear();
                            break;
                        }
                    };
                    ir_args.push(Operand::AddrOf(dp.expr()));
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
        // C1：foreign/native_sig 的按值聚合返回强制 Indirect 落点（libffi 依结构
        // 类型把结构体字节写进结果缓冲，interp 层 memcpy 至 dst）——≤16B pair 档
        // 与 >16B sret 档统一此路
        let ret = if matches!(
            &ct,
            CallTarget::Direct(Callee::Foreign {
                ret: ir::FfiKind::Agg(_),
                ..
            }) | CallTarget::Indirect(
                _,
                Some(ir::ForeignSig {
                    ret: ir::FfiKind::Agg(_),
                    ..
                })
            )
        ) {
            match self.resolve_place(destination) {
                Ok(dp) => RetDest::Indirect(dp.expr()),
                Err(e) => {
                    pre.push(Stmt::Trap(format!("C1 返回落点: {e}").into_boxed_str()));
                    RetDest::Ignore
                }
            }
        } else {
            ret
        };
        // 发散调用（target=None）→ 合成 Unreachable 落点块
        let tgt = match target {
            Some(b) => b.as_u32(),
            None => {
                let idx = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks.push(ir::Block {
                    stmts: vec![],
                    term: Terminator::Unreachable,
                });
                idx
            }
        };
        let unwind = self.lower_unwind(unwind);
        let term = match ct {
            CallTarget::Direct(Callee::Func(id)) => Terminator::Call {
                callee: id,
                args: ir_args,
                ret,
                target: tgt,
                unwind,
            },
            CallTarget::Direct(Callee::Builtin(b)) => Terminator::CallBuiltin {
                builtin: b,
                args: ir_args,
                ret,
                target: tgt,
                unwind,
            },
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
                    ir::ForeignSig {
                        args: all,
                        ret: fret,
                        fixed: Some(nfixed),
                        thunk_args,
                        // 出向 unwind 未建模（R18②；libffi 边界天然不可传播）
                        unwind: false,
                    }
                } else {
                    ir::ForeignSig {
                        args: fixed,
                        ret: fret,
                        fixed: None,
                        thunk_args,
                        unwind: false,
                    }
                };
                Terminator::CallForeign {
                    sym,
                    sig,
                    args: ir_args,
                    ret,
                    target: tgt,
                    unwind,
                }
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
}
