//! Inline asm site lowering: pairs MIR operands with asm-stub wrapper slot
//! offsets; register allocation and GAS generation are delegated to
//! crate::lower::asm (isomorphic to cg_clif). Sole entry = term.rs's InlineAsm arm.

use crate::lower::Error;

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    /// Lowers one inline asm site. Register allocation and wrapper text come from
    /// `super::asm` (isomorphic to cg_clif); values and destinations are paired with
    /// wrapper slot offsets here. Supported surface: in/out/inout x explicit
    /// register/reg class; noreturn has two faces (resume/ud2, with an Unreachable
    /// landing that faults on violation). sym/label/const/naked/may_unwind/att_syntax/non-x86_64 stay Trap stubs.
    pub(super) fn lower_inline_asm(
        &mut self,
        asm_macro: mir::InlineAsmMacro,
        template: &[rustc_ast::ast::InlineAsmTemplatePiece],
        operands: &[mir::InlineAsmOperand<'tcx>],
        options: rustc_ast::ast::InlineAsmOptions,
        targets: &[mir::BasicBlock],
        unwind: mir::UnwindAction,
    ) -> Result<(Vec<Stmt>, Terminator), Error> {
        use rustc_ast::ast::InlineAsmOptions as Opt;
        use rustc_target::asm::InlineAsmArch;

        if matches!(asm_macro, mir::InlineAsmMacro::NakedAsm) {
            return Err(Error::internal("naked_asm!"));
        }
        if options.contains(Opt::MAY_UNWIND) {
            return Err(Error::internal("inline asm may_unwind"));
        }
        // noreturn has two faces. ud2/int3 terminate: fully supported (the asm body is
        // machine code, so the process dies on a host signal exactly as native does).
        // resume/longjmp transfer: the asm body executes faithfully (cranelift emits
        // machine code and trap propagation works). Known boundary: the synthesized
        // protocol that reuses the captured frame's host stack memory between capture
        // and resume can crash the interpreted frame; eliminating it needs real JIT
        // frame identity. rustc forces noreturn asm to have no outputs (outs is always
        // empty); the landing is Unreachable -- returning anyway is native UB.
        let noreturn = options.contains(Opt::NORETURN);
        if noreturn
            && operands
                .iter()
                .any(|op| !matches!(op, mir::InlineAsmOperand::In { .. }))
        {
            return Err(Error::internal(
                "inline asm noreturn with a non-In operand (rustc invariant broken)",
            ));
        }
        if options.contains(Opt::ATT_SYNTAX) {
            // Guard against silently wrong values: the wrapper forces intel syntax, so an
            // att-syntax template would be misassembled.
            return Err(Error::internal("inline asm att_syntax"));
        }
        if !matches!(
            unwind,
            mir::UnwindAction::Unreachable | mir::UnwindAction::Continue
        ) {
            return Err(Error::internal("inline asm with cleanup unwind"));
        }
        let arch = self
            .tcx
            .sess
            .asm_arch
            .ok_or(Error::unsupported("target does not support asm"))?;
        if !matches!(arch, InlineAsmArch::X86_64) {
            return Err(Error::internal(format!(
                "inline asm is not x86_64 (arch={arch:?})"
            )));
        }

        // MIR operands -> wrapper constraints (super::asm; only the reg constraint and role are needed, not values/destinations).
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
                // const/sym: render as literal text into the template (isomorphic to cg_clif), no register
                mir::InlineAsmOperand::Const { value } => {
                    gen_ops.push(crate::lower::asm::AsmOperand::Inline {
                        text: self.asm_const_text(value)?,
                    });
                }
                mir::InlineAsmOperand::SymFn { value } => {
                    let rustc_middle::ty::TyKind::FnDef(def_id, args) = value.const_.ty().kind()
                    else {
                        return Err(Error::internal("inline asm sym fn is not a FnDef"));
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
                    return Err(Error::internal("inline asm label (asm goto)"));
                }
            }
        }

        let (stub_id, name) = self.linker.reserve_asm_stub();
        let g = crate::lower::asm::generate(self.tcx, self.def_id, arch, template, &gen_ops, &name);
        self.linker.set_asm_stub(stub_id, g.text);

        // Pair the already-lowered values/destinations with wrapper slot offsets; both
        // sides come from the same generation, which is the correctness basis. The second
        // pass re-matches operands[i] (it borrows the parameter, not self). Two value
        // channels: layout <= 8B uses an 8B scalar slot; > 8B (__m128i/__m256/__m512
        // vectors) uses the vector byte channel (copy the full width from the place's real address).
        let mut ins: Vec<(u32, ir::AsmIoVal)> = Vec::new();
        let mut outs: Vec<(u32, ir::AsmIoDst)> = Vec::new();
        for (i, op) in operands.iter().enumerate() {
            let val_of =
                |this: &mut Self, value: &mir::Operand<'tcx>| -> Result<ir::AsmIoVal, Error> {
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
                            _ => Err(Error::internal(format!(
                                "asm vector input is not a place (ty={ty})"
                            ))),
                        }
                    }
                };
            let dst_of =
                |this: &mut Self, place: &mir::Place<'tcx>| -> Result<ir::AsmIoDst, Error> {
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
                    ins.push((g.input_slot[i].expect("In always has an input slot"), v));
                }
                mir::InlineAsmOperand::Out {
                    place: Some(place), ..
                } => {
                    let d = dst_of(self, place)?;
                    outs.push((
                        g.output_slot[i].expect("Out with a place always has an output slot"),
                        d,
                    ));
                }
                mir::InlineAsmOperand::InOut {
                    in_value,
                    out_place,
                    ..
                } => {
                    let v = val_of(self, in_value)?;
                    ins.push((g.input_slot[i].expect("InOut always has an input slot"), v));
                    if let Some(place) = out_place {
                        let d = dst_of(self, place)?;
                        outs.push((
                            g.output_slot[i]
                                .expect("InOut with an out_place always has an output slot"),
                            d,
                        ));
                    }
                }
                // Out{place:None} is clobber-only (no destination); Const/Sym/Label were rejected above
                _ => {}
            }
        }

        let target = if noreturn {
            // noreturn: synthesize an Unreachable landing (the stub actually returning is a violation)
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
                .ok_or(Error::internal("inline asm has no fallthrough target"))?
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

    /// Inline asm const operand -> literal text (isomorphic to cg_ssa asm_const_to_str).
    fn asm_const_text(&self, value: &mir::ConstOperand<'tcx>) -> Result<String, Error> {
        let cv = value
            .const_
            .eval(self.tcx, self.typing_env, value.span)
            .map_err(|e| Error::internal(format!("inline asm const evaluation failed: {e:?}")))?;
        let layout = self
            .tcx
            .layout_of(self.typing_env.as_query_input(value.const_.ty()))
            .map_err(|e| Error::internal(format!("inline asm const layout: {e:?}")))?;
        Ok(rustc_codegen_ssa::common::asm_const_to_str(
            self.tcx, value.span, cv, layout,
        ))
    }
}
