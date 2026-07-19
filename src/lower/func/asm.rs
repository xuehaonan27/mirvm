//! inline asm 站点降低（自 func.rs F11 整搬）：MIR 操作数 ↔ asm-stub
//! wrapper 槽偏移配对；寄存器分配与 GAS 生成委托 crate::lower::asm
//! （cg_clif 同构）。唯一入口 = term.rs 的 InlineAsm 臂。

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    /// inline asm 站点降低（M5.0 asm-stub 工厂，corpus §2.2 三面孔归宿）。
    /// 寄存器分配 + wrapper 文本经 `super::asm`（cg_clif 同构），值/落点在此配对槽偏移。
    /// M5.0 支持面：in/out/inout × 显式寄存器/reg 类；noreturn 两面孔（C3：
    /// resume/ud2，Unreachable 落点兜底违约）；sym/label/const/naked/may_unwind/
    /// att_syntax/非 x86_64 保留 Trap-stub（诊断留痕；按需再补）。
    pub(super) fn lower_inline_asm(
        &mut self,
        asm_macro: mir::InlineAsmMacro,
        template: &[rustc_ast::ast::InlineAsmTemplatePiece],
        operands: &[mir::InlineAsmOperand<'tcx>],
        options: rustc_ast::ast::InlineAsmOptions,
        targets: &[mir::BasicBlock],
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        use rustc_ast::ast::InlineAsmOptions as Opt;
        use rustc_target::asm::InlineAsmArch;

        if matches!(asm_macro, mir::InlineAsmMacro::NakedAsm) {
            return Err("naked_asm!（M5.x）".into());
        }
        if options.contains(Opt::MAY_UNWIND) {
            return Err("inline asm may_unwind（M5.x；三面孔无）".into());
        }
        // noreturn 两面孔（C3 定稿，2026-07-18）：ud2/int3 终止形 = 完全支持
        // （asm 本体即机器码，进程以宿主信号死 = native 同）；resume/longjmp
        // 转移形 = asm 本体忠实执行（c_wasmtime_wat 全 trap 面三维确定性绿——
        // cranelift 发机器码 + trap 上抛链全通）。如实边界：解释帧在捕获与
        // 恢复之间复用捕获帧宿主栈内存的合成协议可撞死（v2 spike 实锤，
        // open-issues 引擎边界记档；消除 = JIT 真帧身份，不宣称全形态闭合）。
        // rustc 强制 noreturn 无输出操作数（outs 恒空）；落点合成
        // Unreachable（asm 若违约返回 = native UB，响亮诊断）。
        let noreturn = options.contains(Opt::NORETURN);
        if noreturn
            && operands
                .iter()
                .any(|op| !matches!(op, mir::InlineAsmOperand::In { .. }))
        {
            return Err("inline asm noreturn 带非 In 操作数（rustc 不变量破坏）".into());
        }
        if options.contains(Opt::ATT_SYNTAX) {
            // 防静默错值：wrapper 强制 intel 语法，att 语法模板会被误汇编
            return Err("inline asm att_syntax（M5.x；三面孔无）".into());
        }
        if !matches!(
            unwind,
            mir::UnwindAction::Unreachable | mir::UnwindAction::Continue
        ) {
            return Err("inline asm 带 cleanup unwind（M5.x）".into());
        }
        let arch = self.tcx.sess.asm_arch.ok_or("目标不支持 asm")?;
        if !matches!(arch, InlineAsmArch::X86_64) {
            return Err(format!("inline asm 非 x86_64（arch={arch:?}，M5.x）"));
        }

        // MIR 操作数 → wrapper 约束（super::asm；只需 reg 约束 + 角色，不需值/落点）。
        let mut gen_ops: Vec<crate::lower::asm::AsmOperand> = Vec::with_capacity(operands.len());
        for op in operands {
            match op {
                mir::InlineAsmOperand::In { reg, .. } => {
                    gen_ops.push(crate::lower::asm::AsmOperand::In { reg: *reg });
                }
                mir::InlineAsmOperand::Out { reg, late, place } => {
                    gen_ops.push(crate::lower::asm::AsmOperand::Out {
                        reg: *reg,
                        late: *late,
                        has_place: place.is_some(),
                    });
                }
                mir::InlineAsmOperand::InOut { reg, out_place, .. } => {
                    gen_ops.push(crate::lower::asm::AsmOperand::InOut {
                        reg: *reg,
                        has_out_place: out_place.is_some(),
                    });
                }
                // const/sym（D8h）：渲染成字面文本进模板（cg_clif 同构），无寄存器
                mir::InlineAsmOperand::Const { value } => {
                    gen_ops.push(crate::lower::asm::AsmOperand::Inline {
                        text: self.asm_const_text(value)?,
                    });
                }
                mir::InlineAsmOperand::SymFn { value } => {
                    let rustc_middle::ty::TyKind::FnDef(def_id, args) = value.const_.ty().kind()
                    else {
                        return Err("inline asm sym fn 非 FnDef".into());
                    };
                    let callee = Instance::expect_resolve(
                        self.tcx,
                        self.typing_env,
                        *def_id,
                        args,
                        rustc_span::DUMMY_SP,
                    );
                    gen_ops.push(crate::lower::asm::AsmOperand::Inline {
                        text: self.tcx.symbol_name(callee).name.to_owned(),
                    });
                }
                mir::InlineAsmOperand::SymStatic { def_id } => {
                    gen_ops.push(crate::lower::asm::AsmOperand::Inline {
                        text: self
                            .tcx
                            .symbol_name(Instance::mono(self.tcx, *def_id))
                            .name
                            .to_owned(),
                    });
                }
                mir::InlineAsmOperand::Label { .. } => {
                    return Err("inline asm label（asm goto，M5.x）".into());
                }
            }
        }

        let (stub_id, name) = self.linker.reserve_asm_stub();
        let g = crate::lower::asm::generate(self.tcx, self.def_id, arch, template, &gen_ops, &name);
        self.linker.set_asm_stub(stub_id, g.text);

        // 配对已降低的值/落点与 wrapper 槽偏移（同源一致——正确性地基）。
        // 第二遍重匹配 operands[i]（借的是参数非 self，与 lower_* 的 &mut self 不冲突）。
        // 值通道双形态（批10，c_typst_pdf 供养）：layout ≤8B 走 8B 标量槽（今路径）；
        // >8B（__m128i/__m256/__m512 向量）走向量字节通道（place 真地址拷全宽）。
        let mut ins: Vec<(u32, ir::AsmIoVal)> = Vec::new();
        let mut outs: Vec<(u32, ir::AsmIoDst)> = Vec::new();
        for (i, op) in operands.iter().enumerate() {
            let val_of = |this: &mut Self, value: &mir::Operand<'tcx>| -> Result<ir::AsmIoVal, String> {
                let ty = this.op_ty(value)?;
                let layout = frame::layout_of(this.tcx, this.typing_env, ty)?;
                let size = layout.layout.size().bytes() as u32;
                if size <= 8 {
                    Ok(ir::AsmIoVal::Scalar(this.lower_operand_scalar(value)?))
                } else if let Some(pl) = value.place() {
                    let dp = this.resolve_place(&pl)?;
                    Ok(ir::AsmIoVal::VecBytes(dp.expr(), size))
                } else {
                    match this.lower_operand(value)? {
                        LoweredOp::Bytes { place, .. } => {
                            Ok(ir::AsmIoVal::VecBytes(place.expr(), size))
                        }
                        _ => Err(format!(
                            "asm 向量输入非 place（ty={ty}，批10 xmm 通道）"
                        )),
                    }
                }
            };
            let dst_of = |this: &mut Self, place: &mir::Place<'tcx>| -> Result<ir::AsmIoDst, String> {
                let dp = this.resolve_place(place)?;
                let layout = frame::layout_of(this.tcx, this.typing_env, dp.ty)?;
                let size = layout.layout.size().bytes() as u32;
                if size <= 8 {
                    let (pl, w) = this.place_scalar(place)?;
                    Ok(ir::AsmIoDst::Scalar(pl.scalar_place(w)))
                } else {
                    Ok(ir::AsmIoDst::VecBytes(dp.expr(), size))
                }
            };
            match op {
                mir::InlineAsmOperand::In { value, .. } => {
                    let v = val_of(self, value)?;
                    ins.push((g.input_slot[i].expect("In 必有输入槽"), v));
                }
                mir::InlineAsmOperand::Out {
                    place: Some(place), ..
                } => {
                    let d = dst_of(self, place)?;
                    outs.push((g.output_slot[i].expect("Out 有 place 必有输出槽"), d));
                }
                mir::InlineAsmOperand::InOut {
                    in_value,
                    out_place,
                    ..
                } => {
                    let v = val_of(self, in_value)?;
                    ins.push((g.input_slot[i].expect("InOut 必有输入槽"), v));
                    if let Some(place) = out_place {
                        let d = dst_of(self, place)?;
                        outs.push((
                            g.output_slot[i].expect("InOut 有 out_place 必有输出槽"),
                            d,
                        ));
                    }
                }
                // Out{place:None} = clobber-only（无落点）；Const/Sym/Label 已在上拒
                _ => {}
            }
        }

        let target = if noreturn {
            // noreturn（C3 复验窗口）：合成 Unreachable 落点（stub 真返 = 违约）
            let idx = (self.mir_block_count + self.extra_blocks.len()) as Bb;
            self.extra_blocks.push(ir::Block {
                stmts: vec![],
                term: Terminator::Unreachable,
            });
            idx
        } else {
            targets
                .first()
                .map(|b| b.as_u32())
                .ok_or("inline asm 无 fallthrough 目标")?
        };
        Ok((
            vec![],
            Terminator::InlineAsm {
                stub: stub_id,
                buf_size: g.buf_size,
                ins,
                outs,
                target,
            },
        ))
    }

    /// inline asm const 操作数 → 字面文本（cg_ssa asm_const_to_str 同构）。
    fn asm_const_text(&self, value: &mir::ConstOperand<'tcx>) -> Result<String, String> {
        let cv = value
            .const_
            .eval(self.tcx, self.typing_env, value.span)
            .map_err(|e| format!("inline asm const 求值失败: {e:?}"))?;
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(value.const_.ty()))
            .map_err(|e| format!("inline asm const layout: {e:?}"))?;
        Ok(rustc_codegen_ssa::common::asm_const_to_str(
            self.tcx, value.span, cv, layout,
        ))
    }
}
