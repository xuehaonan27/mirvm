//! cast family: the whole lower_cast family -- IntToInt/PtrToPtr/
//! PointerCoercion(Unsize/DynStar)/IntToFloat/FloatCast/Transmute etc.
//! Sole entry = the Cast arm of mod.rs lower_assign; unsize navigation lives in unsize.rs.

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    pub(super) fn lower_cast(
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
                // 128-bit direction: -> u128/i128 = cast the low half + sign-extend the high half; u128 -> smaller = take the low half
                if let ValKind::Other { size: 16 } = dst_kind {
                    // 128 -> 128 (equal-width int cast such as i128<->u128): same bits, copy all 16 bytes
                    if a_layout.size.bytes() == 16 {
                        return Ok(vec![Stmt::Copy {
                            dst: dst_p.expr(),
                            src: self.wide_place(a)?,
                            size: 16,
                        }]);
                    }
                    let from_w = frame::scalar_width(&a_layout)
                        .ok_or("128-bit cast source is not a scalar")?;
                    let ao = self.lower_operand_scalar(a)?;
                    let lo = Stmt::Assign {
                        dst: dst_p.half_place(0, Width::W64),
                        rv: Rvalue::Cast {
                            from: (from_w, signed),
                            to: Width::W64,
                            a: ao,
                        },
                    };
                    let hi_rv = if signed {
                        // Arithmetic shift right by 63 copies the sign
                        Rvalue::IntBin {
                            op: IntBinOp::Shr,
                            signed: true,
                            a: dst_p.half_operand(0, Width::W64),
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
                    };
                    let hi = Stmt::Assign {
                        dst: dst_p.half_place(8, Width::W64),
                        rv: hi_rv,
                    };
                    return Ok(vec![lo, hi]);
                }
                if a_layout.size.bytes() == 16 {
                    // u128/i128 -> <= 64: truncation = take the low half
                    let src_p = match a {
                        mir::Operand::Copy(pl) | mir::Operand::Move(pl) => {
                            self.resolve_place(pl)?
                        }
                        _ => return Err("128-bit constant cast".into()),
                    };
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("IntToInt target is not a scalar".into());
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
                let from_w = frame::scalar_width(&a_layout).ok_or("cast source is not a scalar")?;
                let to_layout = self.layout_of(to_ty)?;
                let to_w = frame::scalar_width(&to_layout).ok_or("cast target is not a scalar")?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("IntToInt target is not a scalar".into());
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
            // Bit-copy cast family under the real-address model
            CK::PointerExposeProvenance | CK::PointerWithExposedProvenance | CK::FnPtrToPtr => {
                let src = self.lower_operand(a)?;
                self.assign_lowered(dst_p, dst_kind, src)
            }
            CK::PtrToPtr => {
                // Fat -> thin = take the data half; same kind = bit copy
                let src = self.lower_operand(a)?;
                match (&dst_kind, src) {
                    (ValKind::Scalar(_), LoweredOp::Pair(l, _)) => {
                        self.assign_lowered(dst_p, dst_kind, LoweredOp::Scalar(l))
                    }
                    (_, src) => self.assign_lowered(dst_p, dst_kind, src),
                }
            }
            CK::Transmute => {
                // Bit reinterpretation = byte movement. An equal-width scalar passes straight
                // through (fast path); a place src is always byte-copied (cross-class safe); a
                // scalar constant becomes an equal-width scalar.
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
                        // Constant: lower_const_value already materialized all four classes
                        // (Slice/Indirect go to the frozen region); transmute is same-size bit
                        // reinterpretation, so a same-shape move suffices (&str -> &[u8] is an isomorphic pair).
                        let src = self.lower_operand(a)?;
                        self.assign_lowered(dst_p, dst_kind, src)
                            .map_err(|e| format!("Transmute constant: {e}"))
                    }
                }
            }
            CK::PointerCoercion(pc, _) => {
                use ty::adjustment::PointerCoercion as PC;
                match pc {
                    PC::Unsize => {
                        // Unsizing: data = the source thin scalar (one newtype wrapping a pointer),
                        // meta = derived by type recursion (unsize_meta_of).
                        // dyn tail pair (unified recursive criterion, dyn_unsize_tails --
                        // builtin_deref direct / lockstep direct / Pat shell / Adt single
                        // non-ZST field recursion, covering the whole
                        // Arc->NonNull->*const ArcInner->data chain). Same principal = pair bit copy
                        // (auto-trait diff); upcast = chase.
                        let a_ty = self.op_ty(a)?;
                        let dyn_tails: Option<(Ty<'tcx>, Ty<'tcx>)> =
                            self.dyn_unsize_tails(a_ty, to_ty);
                        if let Some((dsp, ddp)) = dyn_tails {
                            let (ty::Dynamic(src_preds, _), ty::Dynamic(dst_preds, _)) =
                                (dsp.kind(), ddp.kind())
                            else {
                                unreachable!("dyn_tails already filtered")
                            };
                            if src_preds.principal_def_id() == dst_preds.principal_def_id() {
                                // vtable unchanged (same criterion as cg_ssa unsized_info)
                                let src = self.lower_operand(a)?;
                                return self.assign_lowered(dst_p, dst_kind, src);
                            }
                            // dyn upcasting (isomorphic to cg_ssa base.rs unsized_info): the target vtable =
                            // *(source vtable + supertrait_vtable_slot x 8); None means an auto-trait
                            // difference (vtable unchanged, pair bit copy).
                            let Some(slot_idx) = self.tcx.supertrait_vtable_slot((dsp, ddp)) else {
                                let src = self.lower_operand(a)?;
                                return self.assign_lowered(dst_p, dst_kind, src);
                            };
                            let byte_off = (slot_idx as u64 * 8) as i32;
                            let ValKind::Pair((ao, aw), (bo, bw)) = dst_kind else {
                                return Err(format!("dyn upcast target is not a pair ({to_ty})"));
                            };
                            let ValKind::Pair((sao, saw), (sbo, _)) = self.classify(a_ty)? else {
                                return Err(format!(
                                    "dyn upcast source is not a pair ({a_ty}; nested tail-pair wrapper not handled)"
                                ));
                            };
                            // Chase the source meta half: <source meta half address> -> Deref -> +byte_off
                            // -> Mem read 8B = the target vtable pointer
                            let chase_of = |base: ir::PlaceBase,
                                            steps: &mut Vec<ir::PlaceStep>|
                             -> ir::Operand {
                                steps.push(ir::PlaceStep::Deref);
                                steps.push(ir::PlaceStep::Offset(byte_off));
                                ir::Operand::Mem {
                                    expr: ir::PlaceExpr {
                                        base,
                                        steps: steps.clone().into_boxed_slice(),
                                    },
                                    width: ir::Width::W64,
                                }
                            };
                            if let Some(pl) = a.place() {
                                let sp_pl = self.resolve_place(&pl)?;
                                let meta_expr = sp_pl.expr_plus(sbo);
                                let mut steps = meta_expr.steps.into_vec();
                                let new_meta = chase_of(meta_expr.base, &mut steps);
                                return Ok(vec![
                                    Stmt::Assign {
                                        dst: dst_p.half_place(ao, aw),
                                        rv: Rvalue::Use(sp_pl.half_operand(sao, saw)),
                                    },
                                    Stmt::Assign {
                                        dst: dst_p.half_place(bo, bw),
                                        rv: Rvalue::Use(new_meta),
                                    },
                                ]);
                            }
                            // Non-constant case (lower_operand's Slot meta half): same chase
                            let LoweredOp::Pair(l, h) = self.lower_operand(a)? else {
                                return Err(format!(
                                    "dyn upcast source is not a fat pointer ({a_ty}; constant fat-pointer upcast not handled)"
                                ));
                            };
                            let ir::Operand::Slot(meta_slot) = h else {
                                return Err(format!(
                                    "dyn upcast source meta is not a slot ({a_ty}; non-constant form not handled)"
                                ));
                            };
                            let mut steps = vec![];
                            let new_meta =
                                chase_of(ir::PlaceBase::Local(meta_slot.off), &mut steps);
                            return Ok(vec![
                                Stmt::Assign {
                                    dst: dst_p.half_place(ao, aw),
                                    rv: Rvalue::Use(l),
                                },
                                Stmt::Assign {
                                    dst: dst_p.half_place(bo, bw),
                                    rv: Rvalue::Use(new_meta),
                                },
                            ]);
                        }
                        let meta = self.unsize_meta_of(a_ty, to_ty)?;
                        let ValKind::Pair((ao, aw), (bo, bw)) = dst_kind else {
                            return Err(format!("Unsize target is not a pair ({to_ty})"));
                        };
                        let LoweredOp::Scalar(data) = self.lower_operand(a)? else {
                            return Err(format!(
                                "Unsize source is not a thin scalar ({a_ty}); custom CoerceUnsized with multiple non-ZST fields?"
                            ));
                        };
                        Ok(vec![
                            Stmt::Assign {
                                dst: dst_p.half_place(ao, aw),
                                rv: Rvalue::Use(data),
                            },
                            Stmt::Assign {
                                dst: dst_p.half_place(bo, bw),
                                rv: Rvalue::Use(meta),
                            },
                        ])
                    }
                    PC::MutToConstPointer | PC::UnsafeFnPointer | PC::ArrayToPointer => {
                        // Bit copy (fat -> thin goes through PtrToPtr; here it is a same-kind bit copy)
                        let src = self.lower_operand(a)?;
                        match (&dst_kind, src) {
                            (ValKind::Scalar(_), LoweredOp::Pair(l, _)) => {
                                self.assign_lowered(dst_p, dst_kind, LoweredOp::Scalar(l))
                            }
                            (_, src) => self.assign_lowered(dst_p, dst_kind, src),
                        }
                    }
                    PC::ReifyFnPointer(..) => {
                        // FnDef (ZST) -> fn ptr: must go through rustc's dedicated fn-ptr resolution.
                        // #[track_caller] cannot be encoded in the fn-ptr ABI; resolve_for_fn_ptr picks
                        // a Reify shim for it that takes ordinary fn-ptr ABI args and supplies caller location.
                        let a_ty = self.op_ty(a)?;
                        let ty::FnDef(def_id, gargs) = a_ty.kind() else {
                            return Err(format!("ReifyFnPointer source is not a FnDef ({a_ty})"));
                        };
                        let Some(inst) =
                            Instance::resolve_for_fn_ptr(self.tcx, self.typing_env, *def_id, gargs)
                        else {
                            return Err(format!(
                                "ReifyFnPointer instance resolution failed ({a_ty})"
                            ));
                        };
                        let addr = self.linker.fn_entry_addr(inst)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("ReifyFnPointer target is not a scalar".into());
                        };
                        // extern fn's fn-ptr value = the GOT slot content (host code address refilled at startup)
                        let op = match self.linker.foreign_fn_slot(inst) {
                            Some(slot) => Operand::Mem {
                                expr: PlaceExpr {
                                    base: PlaceBase::Static(ir::LinkAddr(slot)),
                                    steps: Box::new([]),
                                },
                                width: Width::W64,
                            },
                            None => Operand::AddrImm(ir::LinkAddr(addr)),
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::Use(op),
                        }])
                    }
                    PC::ClosureFnPointer(..) => {
                        // Captureless closure -> fn ptr (isomorphic to cg_ssa: resolve_closure FnOnce)
                        let a_ty = self.op_ty(a)?;
                        let ty::Closure(def_id, cargs) = a_ty.kind() else {
                            return Err(format!(
                                "ClosureFnPointer source is not a closure ({a_ty})"
                            ));
                        };
                        let inst = Instance::resolve_closure(
                            self.tcx,
                            *def_id,
                            cargs,
                            ty::ClosureKind::FnOnce,
                        );
                        let addr = self.linker.fn_entry_addr(inst)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("ClosureFnPointer target is not a scalar".into());
                        };
                        // extern fn's fn-ptr value = the GOT slot content (host code address refilled at startup)
                        let op = match self.linker.foreign_fn_slot(inst) {
                            Some(slot) => Operand::Mem {
                                expr: PlaceExpr {
                                    base: PlaceBase::Static(ir::LinkAddr(slot)),
                                    steps: Box::new([]),
                                },
                                width: Width::W64,
                            },
                            None => Operand::AddrImm(ir::LinkAddr(addr)),
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::Use(op),
                        }])
                    }
                }
            }
            CK::FloatToInt => {
                let a_ty = self.op_ty(a)?;
                let to_layout = self.layout_of(to_ty)?;
                let to_signed = frame::ty_signed(to_ty);
                // f128 source (wide channel): -> <= 64-bit integer via F128ToScalar; -> i128/u128 via F128ToWideInt
                if matches!(a_ty.kind(), ty::Float(ty::FloatTy::F128)) {
                    let pa = self.wide_place(a)?;
                    let Some(to_w) = frame::scalar_width(&to_layout) else {
                        return Ok(vec![Stmt::F128ToWideInt {
                            src: pa,
                            signed: to_signed,
                            dst: dst_p.expr(),
                        }]);
                    };
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err("FloatToInt target is not a scalar".into());
                    };
                    return Ok(vec![Stmt::F128ToScalar {
                        src: pa,
                        kind: ir::F128Scalar::Int { signed: to_signed },
                        w: to_w,
                        dst: dst_p.scalar_place(w),
                    }]);
                }
                let from = float_w(a_ty)?;
                // Scalar float -> i128/u128: a 16-byte-wide target goes through FloatToWide128
                let Some(to_w) = frame::scalar_width(&to_layout) else {
                    return Ok(vec![Stmt::FloatToWide128 {
                        src: self.lower_operand_scalar(a)?,
                        from,
                        signed: to_signed,
                        dst: dst_p.expr(),
                    }]);
                };
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("FloatToInt target is not a scalar".into());
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::FloatToInt {
                        from,
                        to: to_w,
                        signed: to_signed,
                        a: self.lower_operand_scalar(a)?,
                    },
                }])
            }
            CK::IntToFloat => {
                let a_ty = self.op_ty(a)?;
                let a_layout = self.layout_of(a_ty)?;
                // f128 target (wide channel)
                if matches!(to_ty.kind(), ty::Float(ty::FloatTy::F128)) {
                    return Ok(match frame::scalar_width(&a_layout) {
                        Some(_) => vec![Stmt::F128FromScalar {
                            src: self.lower_operand_scalar(a)?,
                            kind: ir::F128Scalar::Int {
                                signed: frame::ty_signed(a_ty),
                            },
                            dst: dst_p.expr(),
                        }],
                        // i128/u128 -> f128
                        None => vec![Stmt::F128FromWideInt {
                            src: self.wide_place(a)?,
                            signed: frame::ty_signed(a_ty),
                            dst: dst_p.expr(),
                        }],
                    });
                }
                let to = float_w(to_ty)?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("IntToFloat target is not a scalar".into());
                };
                // 128-bit source (u128/i128 as f): read 16 bytes and convert directly on the host
                let Some(from_w) = frame::scalar_width(&a_layout) else {
                    return Ok(vec![Stmt::Wide128ToFloat {
                        src: self.wide_place(a)?,
                        signed: frame::ty_signed(a_ty),
                        to,
                        dst: dst_p.scalar_place(w),
                    }]);
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntToFloat {
                        from: (from_w, frame::ty_signed(a_ty)),
                        to,
                        a: self.lower_operand_scalar(a)?,
                    },
                }])
            }
            CK::FloatToFloat => {
                let a_ty = self.op_ty(a)?;
                let from128 = matches!(a_ty.kind(), ty::Float(ty::FloatTy::F128));
                let to128 = matches!(to_ty.kind(), ty::Float(ty::FloatTy::F128));
                match (from128, to128) {
                    // f128 -> f128 (equal-width bit copy)
                    (true, true) => {
                        let pa = self.wide_place(a)?;
                        Ok(vec![Stmt::Copy {
                            dst: dst_p.expr(),
                            src: pa,
                            size: 16,
                        }])
                    }
                    (true, false) => {
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("FloatToFloat target is not a scalar".into());
                        };
                        Ok(vec![Stmt::F128ToScalar {
                            src: self.wide_place(a)?,
                            kind: ir::F128Scalar::F(float_w(to_ty)?),
                            w,
                            dst: dst_p.scalar_place(w),
                        }])
                    }
                    (false, true) => Ok(vec![Stmt::F128FromScalar {
                        src: self.lower_operand_scalar(a)?,
                        kind: ir::F128Scalar::F(float_w(a_ty)?),
                        dst: dst_p.expr(),
                    }]),
                    (false, false) => {
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("FloatToFloat target is not a scalar".into());
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::FloatCast {
                                from: float_w(a_ty)?,
                                to: float_w(to_ty)?,
                                a: self.lower_operand_scalar(a)?,
                            },
                        }])
                    }
                }
            }
            CK::Subtype => {
                let src = self.lower_operand(a)?;
                self.assign_lowered(dst_p, dst_kind, src)
            }
        }
    }
}
