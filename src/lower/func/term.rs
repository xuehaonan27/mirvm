//! terminator and unwind lowering (moved wholesale from func.rs F10): full lower_terminator family
//! (Goto/SwitchInt/Assert→panic block synthesis/Call→call.rs/InlineAsm→asm.rs)
//! + lower_unwind. Single entry point = mod.rs lower_instance.

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    pub(super) fn lower_unwind(&self, u: mir::UnwindAction) -> ir::UnwindAction {
        match u {
            mir::UnwindAction::Cleanup(bb) => ir::UnwindAction::Cleanup(bb.as_u32()),
            mir::UnwindAction::Terminate(_) => ir::UnwindAction::Terminate,
            // Unreachable: unwinding here = UB (fast does not check) — treated as Continue
            mir::UnwindAction::Continue | mir::UnwindAction::Unreachable => {
                ir::UnwindAction::Continue
            }
        }
    }

    /// terminator → (extra statements, ir terminator).
    pub(super) fn lower_terminator(
        &mut self,
        term: &mir::Terminator<'tcx>,
    ) -> Result<(Vec<Stmt>, Terminator), String> {
        use mir::TerminatorKind as TK;
        Ok(match &term.kind {
            TK::Goto { target } => (vec![], Terminator::Goto(target.as_u32())),
            TK::SwitchInt { discr, targets } => {
                let d = match self.lower_operand(discr)? {
                    LoweredOp::Scalar(value) => SwitchDiscr::Scalar(value),
                    LoweredOp::Bytes { place, size: 16 }
                        if matches!(
                            self.op_ty(discr)?.kind(),
                            ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                        ) =>
                    {
                        SwitchDiscr::Wide(place.expr())
                    }
                    _ => {
                        return Err(format!(
                            "SwitchInt discriminant is not an integer scalar (ty={})",
                            self.op_ty(discr)?
                        ));
                    }
                };
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
            // cleanup chain tail: return guard.drop to let host unwind continue (spike3 protocol)
            TK::UnwindResume => (vec![], Terminator::Resume),
            TK::UnwindTerminate(_) => (vec![], Terminator::TerminateAbort),
            // analysis-only fake edges: codegen semantics = jump straight to real target
            TK::FalseEdge { real_target, .. } | TK::FalseUnwind { real_target, .. } => {
                (vec![], Terminator::Goto(real_target.as_u32()))
            }
            TK::Assert {
                cond,
                expected,
                msg,
                target,
                unwind,
            } => {
                // isomorphic to cg_ssa codegen_assert_terminator: conditional branch + synthesized panic block
                // (Call panic lang item, args + location tail arg—all panic fns are track_caller)
                let c = self.lower_operand_scalar(cond)?;
                use rustc_hir::LangItem;
                let mut pre: Vec<Stmt> = Vec::new();
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
                    mir::AssertKind::InvalidEnumConstruction(op) => {
                        // cg_ssa: panic_invalid_enum_construction(source: u128)
                        // (core panicking.rs lang fn signature)—u128 arg uses
                        // Indirect ABI (pass 16-byte source address, interp.rs:2313
                        // copy_nonoverlapping path, isomorphic to finish_call Bytes
                        // arg `Operand::AddrOf`; proven by polars batch 6)
                        let v = match self.lower_operand(op)? {
                            LoweredOp::Bytes { place, .. } => Operand::AddrOf(place.expr()),
                            LoweredOp::Scalar(o) => {
                                // narrow tag: sign-extend into two adjacent scratch slots
                                // (low 64 = extended value, high 64 = sign broadcast / 0), first slot address
                                // is the 16-byte Indirect arg
                                let ty = self.op_ty(op)?;
                                let signed = frame::ty_signed(ty);
                                let src_w = match o {
                                    Operand::Slot(s) => s.width,
                                    _ => Width::W64,
                                };
                                let lo = self.scratch64();
                                let hi = self.scratch64();
                                pre.push(Stmt::Assign {
                                    dst: ScalarPlace::Slot(lo),
                                    rv: Rvalue::Cast {
                                        from: (src_w, signed),
                                        to: Width::W64,
                                        a: o,
                                    },
                                });
                                pre.push(Stmt::Assign {
                                    dst: ScalarPlace::Slot(hi),
                                    rv: if signed {
                                        Rvalue::IntBin {
                                            op: IntBinOp::Shr,
                                            signed: true,
                                            a: Operand::Slot(lo),
                                            b: Operand::Imm {
                                                bits: 63,
                                                width: Width::W64,
                                            },
                                        }
                                    } else {
                                        Rvalue::Use(Operand::Imm {
                                            bits: 0,
                                            width: Width::W64,
                                        })
                                    },
                                });
                                Operand::AddrOf(ir::PlaceExpr {
                                    base: ir::PlaceBase::Local(lo.off),
                                    steps: Box::from([]),
                                })
                            }
                            LoweredOp::Zst => {
                                return Err("InvalidEnumConstruction arg is Zst".into());
                            }
                            LoweredOp::Pair(..) => {
                                return Err("InvalidEnumConstruction arg is pair (M4.2+)".into());
                            }
                        };
                        pargs.push(v);
                        LangItem::PanicInvalidEnumConstruction
                    }
                    other => other.panic_function(),
                };
                pargs.push(match self.caller_loc_off {
                    Some(off) => Operand::Slot(Slot {
                        off,
                        width: Width::W64,
                    }),
                    None => self.caller_location_imm(term.source_info.span)?,
                });
                let def_id = self.tcx.require_lang_item(lang_item, term.source_info.span);
                let callee = self.linker.func_id(Instance::mono(self.tcx, def_id));
                // synthesize: panic block (diverging Call → Unreachable landing)
                let unreach = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks.push(ir::Block {
                    stmts: vec![],
                    term: Terminator::Unreachable,
                });
                let panic_blk = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks.push(ir::Block {
                    stmts: vec![],
                    term: Terminator::Call {
                        callee,
                        args: pargs,
                        ret: RetDest::Ignore,
                        target: unreach,
                        unwind: self.lower_unwind(*unwind),
                        role: ir::CallRole::Normal,
                    },
                });
                (
                    pre,
                    Terminator::SwitchInt {
                        discr: SwitchDiscr::Scalar(c),
                        targets: vec![(*expected as u128, target.as_u32())],
                        otherwise: panic_blk,
                    },
                )
            }
            TK::Drop {
                place,
                target,
                unwind,
                ..
            } => {
                let p = self.resolve_place(place)?;
                if p.ty.needs_drop(self.tcx, self.typing_env) {
                    // dyn place: virtual drop = indirect call via vtable slot 0 (isomorphic to cg_ssa;
                    // resolve_drop_glue would resolve back to itself → infinite recursion)
                    if let ty::Dynamic(..) = p.ty.kind() {
                        let meta = p
                            .meta
                            .clone()
                            .ok_or_else(|| format!("dyn Drop has no vtable meta (ty={})", p.ty))?;
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
                    // normal Drop path = ordinary Call (F2): drop_in_place synthesized shim,
                    // arg = place real address (*mut T); unsized place (Box<[T]> contents, etc.)
                    // glue argument is a fat pointer → add meta half (resolve_place meta tracking).
                    let glue = Instance::resolve_drop_glue(self.tcx, p.ty);
                    let callee = self.linker.func_id(glue);
                    let mut glue_args = vec![Operand::AddrOf(p.expr())];
                    if self.layout_of(p.ty)?.is_unsized() {
                        let meta = p
                            .meta
                            .clone()
                            .ok_or_else(|| format!("unsized Drop has no meta (ty={})", p.ty))?;
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
                            role: ir::CallRole::Normal,
                        },
                    ));
                }
                (vec![], Terminator::Goto(target.as_u32()))
            }
            TK::Call {
                func,
                args,
                destination,
                target,
                unwind,
                fn_span,
                ..
            } => {
                // callee resolution: constant FnDef → Instance; FnPtr → indirect call
                let fn_ty = self.op_ty(func)?;
                let ty::FnDef(def_id, gargs) = fn_ty.kind() else {
                    if fn_ty.is_fn_ptr() {
                        // fn-ptr indirect call: value = D4 entry real address, engine resolves and dispatches;
                        // extern "C" family additionally freezes native signature (resolution miss = real code
                        // obtained at runtime via dlsym → libffi direct call, second direction of M4.4 FFI)
                        let callee_op = self.lower_operand_scalar(func)?;
                        let native_sig =
                            crate::lower::freeze_c_fnptr_sig(self.tcx, self.typing_env, fn_ty);
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
                    return Err(format!("indirect call (ty={fn_ty}, M4.1+)"));
                };
                let mut inst = Instance::expect_resolve(
                    self.tcx,
                    self.typing_env,
                    *def_id,
                    gargs,
                    term.source_info.span,
                );
                // dyn virtual dispatch: receiver fat pointer split into (data, vtable),
                // callee = *(vtable + idx*8), receiver arg replaced with data half.
                // track_caller methods still pass location (vtable side receives via VTable shim).
                if let InstanceKind::Virtual(_, idx) = inst.def {
                    // #[track_caller] 的 Location 取 fn_span（被调名段）——
                    // cg_ssa `SourceInfo { span: fn_span, ..terminator.source_info }`
                    // 同构；用整个调用表达式 span 会让行列全偏（corpus 批3 redb 实锤）
                    let loc_arg = self.caller_loc_arg(&inst, *fn_span)?;
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
                    let intrinsic = self
                        .tcx
                        .intrinsic(idef)
                        .expect("Intrinsic 必有 IntrinsicDef");
                    if intrinsic.must_be_overridden {
                        return Err(format!(
                            "intrinsic `{}` 无 fallback（引擎内建表，M4.2+）",
                            intrinsic.name
                        ));
                    }
                    inst = Instance::new_raw(idef, inst.args);
                }
                // #[track_caller] 的隐藏尾实参（转发或按调用点合成）；span 取
                // fn_span（被调名段，cg_ssa 同构——见上方 Virtual 分支注）
                let loc_arg = self.caller_loc_arg(&inst, *fn_span)?;
                // Linker 三路解析（debt-map §2-B）：普通函数/intrinsic fallback →
                // worklist 扩集；foreign → ①引擎原语 ②链接仿真 ③Trap
                let callee = self.linker.resolve_call(inst)?;
                let rust_call =
                    fn_ty.fn_sig(self.tcx).skip_binder().abi() == rustc_abi::ExternAbi::RustCall;
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
            TK::InlineAsm {
                asm_macro,
                template,
                operands,
                options,
                targets,
                unwind,
                ..
            } => {
                self.lower_inline_asm(*asm_macro, template, operands, *options, targets, *unwind)?
            }
            other => return Err(format!("终止子 {other:?}（M4.1+）")),
        })
    }
}
