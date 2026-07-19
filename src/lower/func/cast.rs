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
                // 128 位方向：→u128/i128 = 低半 cast + 高半符号扩；u128→小 = 取低半
                if let ValKind::Other { size: 16 } = dst_kind {
                    // 128→128（i128↔u128 等宽 int cast，D8k）：位相同，16 字节整拷
                    if a_layout.size.bytes() == 16 {
                        return Ok(vec![Stmt::Copy {
                            dst: dst_p.expr(),
                            src: self.wide_place(a)?,
                            size: 16,
                        }]);
                    }
                    let from_w = frame::scalar_width(&a_layout).ok_or("128 cast 源非标量")?;
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
                        // 算术右移 63 位复制符号
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
                let to_w = frame::scalar_width(&to_layout).ok_or("cast 目标非标量（M4.3+）")?;
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
                        // 胖化：data = 源瘦标量（一路 newtype 包着一个指针），
                        // meta = 类型递归推导（unsize_meta_of）。
                        // dyn 尾对（统一递归判据，dyn_unsize_tails——
                        // builtin_deref 直达 / lockstep 直达 / Pat 壳 / Adt 唯一非
                        // ZST 字段递归，Arc→NonNull→*const ArcInner→data 全链覆盖）：
                        // 同 principal = pair 位拷（auto trait 差）；上溯 = C5 chase。
                        let a_ty = self.op_ty(a)?;
                        let dyn_tails: Option<(Ty<'tcx>, Ty<'tcx>)> =
                            self.dyn_unsize_tails(a_ty, to_ty);
                        if let Some((dsp, ddp)) = dyn_tails {
                            let (ty::Dynamic(src_preds, _), ty::Dynamic(dst_preds, _)) =
                                (dsp.kind(), ddp.kind())
                            else {
                                unreachable!("dyn_tails 已筛")
                            };
                            if src_preds.principal_def_id() == dst_preds.principal_def_id() {
                                // vtable 不变（cg_ssa unsized_info 同判据）
                                let src = self.lower_operand(a)?;
                                return self.assign_lowered(dst_p, dst_kind, src);
                            }
                            // C5 dyn 上溯（trait upcasting，批10 datafusion/typst
                            // 双供养；cg_ssa base.rs unsized_info 同构）：目标
                            // vtable = *(源 vtable + supertrait_vtable_slot×8)；
                            // None = auto trait 差（vtable 不变，pair 位拷）
                            let Some(slot_idx) = self.tcx.supertrait_vtable_slot((dsp, ddp))
                            else {
                                let src = self.lower_operand(a)?;
                                return self.assign_lowered(dst_p, dst_kind, src);
                            };
                            let byte_off = (slot_idx as u64 * 8) as i32;
                            let ValKind::Pair((ao, aw), (bo, bw)) = dst_kind else {
                                return Err(format!("dyn 上溯目标非 pair（{to_ty}）"));
                            };
                            let ValKind::Pair((sao, saw), (sbo, _)) =
                                self.classify(a_ty)?
                            else {
                                return Err(format!(
                                    "dyn 上溯源非 pair（{a_ty}；嵌套尾对包装未接）"
                                ));
                            };
                            // 源 meta 半 chase：<源 meta 半地址> → Deref → +byte_off
                            // → Mem 读 8B = 目标 vtable 指针
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
                            // 非常量位（lower_operand 的 Slot meta 半）同型 chase
                            let LoweredOp::Pair(l, h) = self.lower_operand(a)? else {
                                return Err(format!(
                                    "dyn 上溯源非胖指针（{a_ty}；常量胖指针上溯未接）"
                                ));
                            };
                            let ir::Operand::Slot(meta_slot) = h else {
                                return Err(format!(
                                    "dyn 上溯源 meta 非槽（{a_ty}；非常量形态未接）"
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
                            return Err(format!("Unsize 目标非 pair（{to_ty}）"));
                        };
                        let LoweredOp::Scalar(data) = self.lower_operand(a)? else {
                            return Err(format!(
                                "Unsize 源非瘦标量（{a_ty}，多非 ZST 字段的自定义 CoerceUnsized？）"
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
                        // FnDef（ZST）→ fn ptr：必须走 rustc 的 fn-ptr 专用解析。
                        // #[track_caller] 不能编码进 fn-ptr ABI；resolve_for_fn_ptr 会为它
                        // 选择 Reify shim，由 shim 以普通 fn-ptr ABI 接参并补 caller location。
                        let a_ty = self.op_ty(a)?;
                        let ty::FnDef(def_id, gargs) = a_ty.kind() else {
                            return Err(format!("ReifyFnPointer 源非 FnDef（{a_ty}）"));
                        };
                        let Some(inst) =
                            Instance::resolve_for_fn_ptr(self.tcx, self.typing_env, *def_id, gargs)
                        else {
                            return Err(format!("ReifyFnPointer 实例解析失败（{a_ty}）"));
                        };
                        let addr = self.linker.fn_entry_addr(inst)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("ReifyFnPointer 目标非标量".into());
                        };
                        // P2：extern fn 的 fn-ptr 值 = GOT 槽内容（宿主码址启动相重填）
                        let op = match self.linker.foreign_fn_slot(inst) {
                            Some(slot) => Operand::Mem {
                                expr: PlaceExpr {
                                    base: PlaceBase::Static(slot),
                                    steps: Box::new([]),
                                },
                                width: Width::W64,
                            },
                            None => Operand::Imm { bits: addr, width: w },
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::Use(op),
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
                        let addr = self.linker.fn_entry_addr(inst)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err("ClosureFnPointer 目标非标量".into());
                        };
                        // P2：extern fn 的 fn-ptr 值 = GOT 槽内容（宿主码址启动相重填）
                        let op = match self.linker.foreign_fn_slot(inst) {
                            Some(slot) => Operand::Mem {
                                expr: PlaceExpr {
                                    base: PlaceBase::Static(slot),
                                    steps: Box::new([]),
                                },
                                width: Width::W64,
                            },
                            None => Operand::Imm { bits: addr, width: w },
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
                // f128 源（宽通道）：→ ≤64 整数 F128ToScalar；→ i128/u128 F128ToWideInt
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
                        return Err("FloatToInt 目标非标量".into());
                    };
                    return Ok(vec![Stmt::F128ToScalar {
                        src: pa,
                        kind: ir::F128Scalar::Int { signed: to_signed },
                        w: to_w,
                        dst: dst_p.scalar_place(w),
                    }]);
                }
                let from = float_w(a_ty)?;
                // 标量浮点 → i128/u128（D8k）：16 字节宽目标走 FloatToWide128
                let Some(to_w) = frame::scalar_width(&to_layout) else {
                    return Ok(vec![Stmt::FloatToWide128 {
                        src: self.lower_operand_scalar(a)?,
                        from,
                        signed: to_signed,
                        dst: dst_p.expr(),
                    }]);
                };
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("FloatToInt 目标非标量".into());
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
                // f128 目标（宽通道）
                if matches!(to_ty.kind(), ty::Float(ty::FloatTy::F128)) {
                    return Ok(match frame::scalar_width(&a_layout) {
                        Some(_) => vec![Stmt::F128FromScalar {
                            src: self.lower_operand_scalar(a)?,
                            kind: ir::F128Scalar::Int {
                                signed: frame::ty_signed(a_ty),
                            },
                            dst: dst_p.expr(),
                        }],
                        // i128/u128 → f128
                        None => vec![Stmt::F128FromWideInt {
                            src: self.wide_place(a)?,
                            signed: frame::ty_signed(a_ty),
                            dst: dst_p.expr(),
                        }],
                    });
                }
                let to = float_w(to_ty)?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err("IntToFloat 目标非标量".into());
                };
                // 128 位源（u128/i128 as f，tokio 定时器逼出）：读 16 字节宿主直转
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
                    // f128 → f128（同宽位拷）
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
                            return Err("FloatToFloat 目标非标量".into());
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
                            return Err("FloatToFloat 目标非标量".into());
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
