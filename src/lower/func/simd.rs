//! SIMD intrinsic 一族（自 func.rs F15 独立 impl 块整搬）：expand_simd
//! ~40 个 simd_* 臂 + lane 几何/元素类别（LaneKind 使「忘带类别」不可表示）。
//! 唯一入口 = intrinsic.rs 的 simd_* 分派。

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    /// SIMD lane 几何 + 元素类别（M5.2 D8b）：LaneKind 使"忘带类别"不可表示——
    /// M4.1 曾对全部 lane 按整数位运算，float lane 的 add/cmp 是静默错值（当时仅因
    /// corpus 全为整数 lane 未爆雷）。f16/f128 lane 在此拒绝（D8c 接入点；f16 的
    /// 放行口在 simd_geom_ext，仅 shuffle/cast 两族——位搬运与 lane 转换可精确，
    /// 逐 lane 算术/比较/归约继续拒绝）。
    pub(super) fn simd_geom(
        &mut self,
        ty: Ty<'tcx>,
    ) -> Result<(u16, u8, ir::LaneKind, u64), String> {
        self.simd_geom_ext(ty, false)
    }

    /// simd_geom 的 f16 放行形态：allow_f16 时 Float::F16 lane 返回 Float + 2 字节。
    /// 只有 simd_shuffle（纯位重排）与 simd_cast/simd_as（f16↔f32/f64 经宿主精确
    /// 转换）调用放行——`_mm_cvtph_ps` 在晚近 stdarch 的 portable 展开正好是这两族。
    pub(super) fn simd_geom_ext(
        &mut self,
        ty: Ty<'tcx>,
        allow_f16: bool,
    ) -> Result<(u16, u8, ir::LaneKind, u64), String> {
        let layout = self.layout_of(ty)?;
        let rustc_abi::BackendRepr::SimdVector { element, count } = layout.backend_repr else {
            return Err(format!("simd intrinsic 非向量参（{ty}）"));
        };
        let dl = self.tcx.data_layout();
        let lane_bytes = element.size(dl).bytes() as u8;
        let lane = match element.primitive() {
            rustc_abi::Primitive::Int(_, s) => ir::LaneKind::Int { signed: s },
            rustc_abi::Primitive::Float(f) => {
                let ok = matches!(f, rustc_abi::Float::F32 | rustc_abi::Float::F64)
                    || (allow_f16 && matches!(f, rustc_abi::Float::F16));
                if !ok {
                    return Err(format!("simd 浮点 lane {f:?}（f16/f128，D8c）"));
                }
                ir::LaneKind::Float
            }
            // 指针 lane：真实地址模型下按无符号整数位处置
            rustc_abi::Primitive::Pointer(_) => ir::LaneKind::Int { signed: false },
        };
        Ok((count as u16, lane_bytes, lane, layout.size.bytes()))
    }

    /// SIMD 全家族展开（M5.2 D8b；每操作 = 逐 lane 宿主循环，语义按 LaneKind 分派）。
    /// 未支持的 simd_* = Err（Trap 占位）。
    pub(super) fn expand_simd(
        &mut self,
        name: &str,
        inst: &Instance<'tcx>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        use ir::{LaneKind, SimdBinOp as S, SimdReduceOp as R, SimdUnOp as U};
        // 向量 operand → place 地址表达式（Bytes 通道；常量已物化进冻结区）
        let vplace = |cx: &mut Self, op: &mir::Operand<'tcx>| -> Result<PlaceExpr, String> {
            match cx.lower_operand(op)? {
                LoweredOp::Bytes { place, .. } => Ok(place.expr()),
                _ => Err("simd 实参非向量（M4.1+）".into()),
            }
        };
        // 异形臂前置：第一泛参不是向量（标量位掩码），几何取自数据向量
        if name == "simd_select_bitmask" {
            let (lanes, lane_bytes, _, _) = self.simd_geom(inst.args.type_at(1))?;
            let mask = self.lower_operand_scalar(&args[0].node)?;
            let a = vplace(self, &args[1].node)?;
            let b = vplace(self, &args[2].node)?;
            let dst = self.resolve_place(destination)?.expr();
            return Ok(vec![Stmt::SimdSelectBitmask {
                mask,
                a,
                b,
                dst,
                lanes,
                lane_bytes,
            }]);
        }
        // 常规几何：T = 第一个泛型参（多数臂的数据向量；select/masked 的 mask 向量）
        let vec_ty = inst.args.type_at(0);
        // f16 lane 放行仅限 shuffle/cast 两族（位搬运/lane 转换精确；算术族继续拒）
        let f16_ok = matches!(name, "simd_shuffle" | "simd_cast" | "simd_as");
        let (lanes, lane_bytes, lane, vec_size) = self.simd_geom_ext(vec_ty, f16_ok)?;
        let count = lanes as u64;
        let bin = |cx: &mut Self, op: S| -> Result<Vec<Stmt>, String> {
            let a = vplace(cx, &args[0].node)?;
            let b = vplace(cx, &args[1].node)?;
            let dst = cx.resolve_place(destination)?.expr();
            Ok(vec![Stmt::SimdBin {
                op,
                lane,
                dst,
                a,
                b,
                lanes,
                lane_bytes,
            }])
        };
        // 单目（浮点族/位族的 lane 类别在此校验——执行器只留防御断言）
        let un = |cx: &mut Self, op: U, need: Option<LaneKind>| -> Result<Vec<Stmt>, String> {
            if let Some(need) = need {
                let ok = match need {
                    LaneKind::Float => lane == LaneKind::Float,
                    LaneKind::Int { .. } => matches!(lane, LaneKind::Int { .. }),
                };
                if !ok {
                    return Err(format!("{name} 要求 {need:?} lane，实为 {lane:?}"));
                }
            }
            let a = vplace(cx, &args[0].node)?;
            let dst = cx.resolve_place(destination)?.expr();
            Ok(vec![Stmt::SimdUn {
                op,
                lane,
                dst,
                a,
                lanes,
                lane_bytes,
            }])
        };
        let reduce = |cx: &mut Self, op: R| -> Result<Vec<Stmt>, String> {
            let a = vplace(cx, &args[0].node)?;
            let (dst_p, w) = cx.place_scalar(destination)?;
            Ok(vec![Stmt::Assign {
                dst: dst_p.scalar_place(w),
                rv: Rvalue::SimdReduceArith {
                    op,
                    lane,
                    a,
                    lanes,
                    lane_bytes,
                },
            }])
        };
        const FLOAT: Option<LaneKind> = Some(LaneKind::Float);
        const INT: Option<LaneKind> = Some(LaneKind::Int { signed: false });
        match name {
            "simd_eq" => bin(self, S::Eq),
            "simd_ne" => bin(self, S::Ne),
            "simd_lt" => bin(self, S::Lt),
            "simd_le" => bin(self, S::Le),
            "simd_gt" => bin(self, S::Gt),
            "simd_ge" => bin(self, S::Ge),
            "simd_and" => bin(self, S::And),
            "simd_or" => bin(self, S::Or),
            "simd_xor" => bin(self, S::Xor),
            "simd_add" => bin(self, S::Add),
            "simd_sub" => bin(self, S::Sub),
            "simd_mul" => bin(self, S::Mul),
            "simd_div" => bin(self, S::Div),
            "simd_rem" => bin(self, S::Rem),
            "simd_saturating_add" => bin(self, S::SatAdd),
            "simd_saturating_sub" => bin(self, S::SatSub),
            "simd_minimum_number_nsz" | "simd_maximum_number_nsz" => {
                if lane != LaneKind::Float {
                    return Err(format!("{name} 要求浮点 lane，实为 {lane:?}"));
                }
                bin(
                    self,
                    if name == "simd_minimum_number_nsz" {
                        S::MinNum
                    } else {
                        S::MaxNum
                    },
                )
            }
            "simd_shl" => bin(self, S::Shl),
            "simd_shr" => bin(self, S::Shr),
            "simd_neg" => un(self, U::Neg, None),
            "simd_fabs" => un(self, U::Fabs, FLOAT),
            "simd_fsqrt" => un(self, U::Fsqrt, FLOAT),
            "simd_ceil" => un(self, U::Ceil, FLOAT),
            "simd_floor" => un(self, U::Floor, FLOAT),
            "simd_round" => un(self, U::Round, FLOAT),
            "simd_round_ties_even" => un(self, U::RoundTiesEven, FLOAT),
            "simd_trunc" => un(self, U::Trunc, FLOAT),
            "simd_fsin" => un(self, U::Fsin, FLOAT),
            "simd_fcos" => un(self, U::Fcos, FLOAT),
            "simd_fexp" => un(self, U::Fexp, FLOAT),
            "simd_fexp2" => un(self, U::Fexp2, FLOAT),
            "simd_flog" => un(self, U::Flog, FLOAT),
            "simd_flog2" => un(self, U::Flog2, FLOAT),
            "simd_flog10" => un(self, U::Flog10, FLOAT),
            "simd_ctlz" => un(self, U::Ctlz, INT),
            "simd_cttz" => un(self, U::Cttz, INT),
            "simd_ctpop" => un(self, U::Ctpop, INT),
            "simd_bswap" => un(self, U::Bswap, INT),
            "simd_bitreverse" => un(self, U::Bitreverse, INT),
            "simd_fma" | "simd_relaxed_fma" => {
                if lane != LaneKind::Float {
                    return Err(format!("{name} 要求浮点 lane，实为 {lane:?}"));
                }
                let a = vplace(self, &args[0].node)?;
                let b = vplace(self, &args[1].node)?;
                let c = vplace(self, &args[2].node)?;
                let dst = self.resolve_place(destination)?.expr();
                Ok(vec![Stmt::SimdFma {
                    dst,
                    a,
                    b,
                    c,
                    lanes,
                    lane_bytes,
                }])
            }
            "simd_funnel_shl" | "simd_funnel_shr" => {
                if !matches!(lane, LaneKind::Int { .. }) {
                    return Err(format!("{name} 要求整数 lane，实为 {lane:?}"));
                }
                let a = vplace(self, &args[0].node)?;
                let b = vplace(self, &args[1].node)?;
                let shift = vplace(self, &args[2].node)?;
                let dst = self.resolve_place(destination)?.expr();
                Ok(vec![Stmt::SimdFunnel {
                    left: name == "simd_funnel_shl",
                    dst,
                    a,
                    b,
                    shift,
                    lanes,
                    lane_bytes,
                }])
            }
            "simd_cast"
            | "simd_as"
            | "simd_cast_ptr"
            | "simd_expose_provenance"
            | "simd_with_exposed_provenance" => {
                // <T, U>(x: T) -> U：目的几何从 destination place 取。
                // 指针族强制整数视角（真实地址模型：provenance 即位透传）。
                let ptr_family = name != "simd_cast" && name != "simd_as";
                let dst_p = self.resolve_place(destination)?;
                // f16 lane 放行（D8c 向量形态：f16↔f32/f64 经宿主精确转换）
                let (dst_lanes, dst_bytes, dst_lane, _) = self.simd_geom_ext(dst_p.ty, true)?;
                if dst_lanes != lanes {
                    return Err(format!("{name} 两侧 lanes 不等（{lanes} vs {dst_lanes}）"));
                }
                let (src_lane, dst_lane) = if ptr_family {
                    let i = LaneKind::Int { signed: false };
                    (i, i)
                } else {
                    (lane, dst_lane)
                };
                let src = vplace(self, &args[0].node)?;
                Ok(vec![Stmt::SimdCast {
                    dst: dst_p.expr(),
                    src,
                    lanes,
                    src_lane,
                    src_bytes: lane_bytes,
                    dst_lane,
                    dst_bytes,
                }])
            }
            "simd_select" => {
                // <M, T>(mask: M, if_true: T, if_false: T)：几何主体是数据向量
                let (d_lanes, d_bytes, _, _) = self.simd_geom(inst.args.type_at(1))?;
                if d_lanes != lanes {
                    return Err("simd_select mask/data lanes 不等".into());
                }
                let mask = vplace(self, &args[0].node)?;
                let a = vplace(self, &args[1].node)?;
                let b = vplace(self, &args[2].node)?;
                let dst = self.resolve_place(destination)?.expr();
                Ok(vec![Stmt::SimdSelect {
                    mask,
                    mask_bytes: lane_bytes,
                    a,
                    b,
                    dst,
                    lanes,
                    lane_bytes: d_bytes,
                }])
            }
            "simd_gather" | "simd_scatter" => {
                // <T, U, V>(val: T, ptr: U, mask: V)：T=数据向量（gather 的 passthru /
                // scatter 的 values），U=指针向量（lane 恒 8B），V=mask 向量
                let (p_lanes, p_bytes, _, _) = self.simd_geom(inst.args.type_at(1))?;
                let (m_lanes, m_bytes, _, _) = self.simd_geom(inst.args.type_at(2))?;
                if p_lanes != lanes || m_lanes != lanes || p_bytes != 8 {
                    return Err(format!(
                        "{name} 几何不一致（data={lanes} ptr={p_lanes}×{p_bytes}B mask={m_lanes}）"
                    ));
                }
                let val = vplace(self, &args[0].node)?;
                let ptrs = vplace(self, &args[1].node)?;
                let mask = vplace(self, &args[2].node)?;
                if name == "simd_gather" {
                    let dst = self.resolve_place(destination)?.expr();
                    Ok(vec![Stmt::SimdGather {
                        passthru: val,
                        ptrs,
                        mask,
                        mask_bytes: m_bytes,
                        dst,
                        lanes,
                        lane_bytes,
                    }])
                } else {
                    Ok(vec![Stmt::SimdScatter {
                        values: val,
                        ptrs,
                        mask,
                        mask_bytes: m_bytes,
                        lanes,
                        lane_bytes,
                    }])
                }
            }
            "simd_masked_load" | "simd_masked_store" => {
                // <V, U, T, ALIGN>(mask: V, ptr: U, val: T)：第一泛参是 mask 向量；
                // ptr 是标量元素指针，lane i 地址 = ptr + i×lane。ALIGN 只影响 guest
                // 的 UB 契约（引擎访存本就逐 lane 非对齐安全）。
                let (d_lanes, d_bytes, _, _) = self.simd_geom(inst.args.type_at(2))?;
                if d_lanes != lanes {
                    return Err(format!("{name} mask/data lanes 不等"));
                }
                let mask = vplace(self, &args[0].node)?;
                let base = self.lower_operand_scalar(&args[1].node)?;
                let val = vplace(self, &args[2].node)?;
                if name == "simd_masked_load" {
                    let dst = self.resolve_place(destination)?.expr();
                    Ok(vec![Stmt::SimdMaskedLoad {
                        mask,
                        mask_bytes: lane_bytes,
                        base,
                        passthru: val,
                        dst,
                        lanes,
                        lane_bytes: d_bytes,
                    }])
                } else {
                    Ok(vec![Stmt::SimdMaskedStore {
                        mask,
                        mask_bytes: lane_bytes,
                        base,
                        values: val,
                        lanes,
                        lane_bytes: d_bytes,
                    }])
                }
            }
            "simd_extract_dyn" => {
                let src = vplace(self, &args[0].node)?;
                let idx = self.lower_operand_scalar(&args[1].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                if w.bytes() as u8 != lane_bytes {
                    return Err(format!(
                        "simd_extract_dyn lane 宽不匹配（vector={lane_bytes}, result={}）",
                        w.bytes()
                    ));
                }
                Ok(vec![Stmt::SimdExtractDyn {
                    src,
                    idx,
                    dst: dst_p.scalar_place(w),
                    lanes,
                    lane_bytes,
                }])
            }
            "simd_insert_dyn" => {
                let src = vplace(self, &args[0].node)?;
                let idx = self.lower_operand_scalar(&args[1].node)?;
                let val = self.lower_operand_scalar(&args[2].node)?;
                if val.width().bytes() as u8 != lane_bytes {
                    return Err(format!(
                        "simd_insert_dyn lane 宽不匹配（vector={lane_bytes}, value={}）",
                        val.width().bytes()
                    ));
                }
                let dst = self.resolve_place(destination)?.expr();
                Ok(vec![Stmt::SimdInsertDyn {
                    src,
                    idx,
                    val,
                    dst,
                    lanes,
                    lane_bytes,
                }])
            }
            "simd_arith_offset" => {
                // <T, U>(ptr: T, offset: U)：stride = 指针 lane 的 pointee 尺寸
                let (_, elem_ty) = vec_ty.simd_size_and_type(self.tcx);
                let pointee = elem_ty
                    .builtin_deref(true)
                    .ok_or_else(|| format!("simd_arith_offset lane 非指针（{elem_ty}）"))?;
                let stride = self.layout_of(pointee)?.size.bytes();
                let ptrs = vplace(self, &args[0].node)?;
                let offsets = vplace(self, &args[1].node)?;
                let dst = self.resolve_place(destination)?.expr();
                Ok(vec![Stmt::SimdArithOffset {
                    ptrs,
                    offsets,
                    stride,
                    dst,
                    lanes,
                }])
            }
            "simd_reduce_add_ordered" | "simd_reduce_add_unordered" => reduce(self, R::Add),
            "simd_reduce_mul_ordered" | "simd_reduce_mul_unordered" => reduce(self, R::Mul),
            "simd_reduce_and" => reduce(self, R::And),
            "simd_reduce_or" => reduce(self, R::Or),
            "simd_reduce_xor" => reduce(self, R::Xor),
            "simd_reduce_min" => reduce(self, R::Min),
            "simd_reduce_max" => reduce(self, R::Max),
            "simd_bitmask" => {
                let a = vplace(self, &args[0].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::SimdBitmask {
                        a,
                        lanes,
                        lane_bytes,
                    },
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
            "simd_insert" => {
                // core::intrinsics::simd_insert 的索引契约是编译期常量且必须在界内。
                // 动态索引由独立的 simd_insert_dyn 表示，不能在这里悄悄接受。
                let src = vplace(self, &args[0].node)?;
                let idx = match self.lower_operand_scalar(&args[1].node)? {
                    Operand::Imm {
                        bits,
                        width: Width::W32,
                    } => bits,
                    Operand::Imm { width, .. } => {
                        return Err(format!(
                            "simd_insert 索引类型宽度应为 u32，实为 {} 字节",
                            width.bytes()
                        ));
                    }
                    _ => return Err("simd_insert 索引非常量".into()),
                };
                if idx >= count {
                    return Err(format!("simd_insert 索引 {idx} 越界（lanes={count}）"));
                }
                let (_, lane_ty) = vec_ty.simd_size_and_type(self.tcx);
                let val_ty = self.op_ty(&args[2].node)?;
                if val_ty != lane_ty {
                    return Err(format!(
                        "simd_insert lane 类型不匹配（vector={lane_ty}, value={val_ty}）"
                    ));
                }
                let lane_width = Width::from_bytes(lane_bytes as u64)
                    .ok_or_else(|| format!("simd_insert lane 宽度 {lane_bytes} 未支持"))?;
                let val = self.lower_operand_scalar(&args[2].node)?;
                if val.width() != lane_width {
                    return Err(format!(
                        "simd_insert lane 类型宽度不匹配（vector={lane_bytes}, value={}）",
                        val.width().bytes()
                    ));
                }
                let dst = self.resolve_place(destination)?;
                let size = u32::try_from(vec_size)
                    .map_err(|_| "simd_insert 向量尺寸超过 u32".to_string())?;
                let lane_offset = idx
                    .checked_mul(u64::from(lane_bytes))
                    .and_then(|offset| i32::try_from(offset).ok())
                    .ok_or_else(|| "simd_insert lane 偏移超过 i32".to_string())?;
                Ok(vec![
                    Stmt::Copy {
                        dst: dst.expr(),
                        src,
                        size,
                    },
                    Stmt::Assign {
                        dst: dst.half_place(lane_offset as u32, lane_width),
                        rv: Rvalue::Use(val),
                    },
                ])
            }
            "simd_extract" => {
                let src = vplace(self, &args[0].node)?;
                let idx = match self.lower_operand_scalar(&args[1].node)? {
                    Operand::Imm {
                        bits,
                        width: Width::W32,
                    } => bits,
                    Operand::Imm { width, .. } => {
                        return Err(format!(
                            "simd_extract 索引类型宽度应为 u32，实为 {} 字节",
                            width.bytes()
                        ));
                    }
                    _ => return Err("simd_extract 索引非常量".into()),
                };
                if idx >= count {
                    return Err(format!("simd_extract 索引 {idx} 越界（lanes={count}）"));
                }
                let (_, lane_ty) = vec_ty.simd_size_and_type(self.tcx);
                let lane_width = Width::from_bytes(lane_bytes as u64)
                    .ok_or_else(|| format!("simd_extract lane 宽度 {lane_bytes} 未支持"))?;
                let (dst, dst_width) = self.place_scalar(destination)?;
                if dst.ty != lane_ty {
                    return Err(format!(
                        "simd_extract lane 类型不匹配（vector={lane_ty}, result={}）",
                        dst.ty
                    ));
                }
                if dst_width != lane_width {
                    return Err(format!(
                        "simd_extract lane 类型宽度不匹配（vector={lane_bytes}, result={}）",
                        dst_width.bytes()
                    ));
                }
                let mut lane = src;
                let lane_offset = idx
                    .checked_mul(u64::from(lane_bytes))
                    .and_then(|offset| i32::try_from(offset).ok())
                    .ok_or_else(|| "simd_extract lane 偏移超过 i32".to_string())?;
                if lane_offset != 0 {
                    let mut steps = lane.steps.to_vec();
                    if let Some(PlaceStep::Offset(offset)) = steps.last_mut() {
                        *offset += lane_offset;
                    } else {
                        steps.push(PlaceStep::Offset(lane_offset));
                    }
                    lane.steps = steps.into_boxed_slice();
                }
                Ok(vec![Stmt::Assign {
                    dst: dst.scalar_place(dst_width),
                    rv: Rvalue::Use(Operand::Mem {
                        expr: lane,
                        width: lane_width,
                    }),
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
                        Operand::Mem {
                            expr: sexpr,
                            width: lw,
                        }
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
                let rustc_abi::BackendRepr::SimdVector { element, count } = dst_layout.backend_repr
                else {
                    return Err("simd_splat 目标非向量".into());
                };
                let lb = element.size(self.tcx.data_layout()).bytes() as u8;
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
