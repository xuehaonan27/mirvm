//! SIMD intrinsic family: expand_simd with ~40 simd_* arms plus lane geometry
//! and element kind (LaneKind makes "forgot the kind" unrepresentable).
//! Sole entry = the simd_* dispatch in intrinsic.rs.

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    /// SIMD lane geometry + element kind. LaneKind makes "forgot the kind"
    /// unrepresentable: treating every lane as integer bits makes float lane add/cmp
    /// silently wrong. f16/f128 lanes are rejected here; f16 is allowed only through
    /// simd_geom_ext for the shuffle/cast families, where bit movement and lane
    /// conversion are exact -- per-lane arithmetic/comparison/reduction stay rejected.
    pub(super) fn simd_geom(
        &mut self,
        ty: Ty<'tcx>,
    ) -> Result<(u16, u8, ir::LaneKind, u64), String> {
        self.simd_geom_ext(ty, false)
    }

    /// f16-allowing form of simd_geom: with allow_f16 a Float::F16 lane returns Float
    /// plus 2 bytes. Only simd_shuffle (pure bit permutation) and simd_cast/simd_as
    /// (f16<->f32/f64 via exact host conversion) call it with the flag; stdarch's portable _mm_cvtph_ps expands to exactly these two families.
    pub(super) fn simd_geom_ext(
        &mut self,
        ty: Ty<'tcx>,
        allow_f16: bool,
    ) -> Result<(u16, u8, ir::LaneKind, u64), String> {
        let layout = self.layout_of(ty)?;
        let rustc_abi::BackendRepr::SimdVector { element, count } = layout.backend_repr else {
            return Err(format!("simd intrinsic argument is not a vector ({ty})"));
        };
        let dl = self.tcx.data_layout();
        let lane_bytes = element.size(dl).bytes() as u8;
        let lane = match element.primitive() {
            rustc_abi::Primitive::Int(_, s) => ir::LaneKind::Int { signed: s },
            rustc_abi::Primitive::Float(f) => {
                let ok = matches!(f, rustc_abi::Float::F32 | rustc_abi::Float::F64)
                    || (allow_f16 && matches!(f, rustc_abi::Float::F16));
                if !ok {
                    return Err(format!("simd float lane {f:?} (f16/f128)"));
                }
                ir::LaneKind::Float
            }
            // Pointer lane: under the real-address model, treat it as unsigned integer bits
            rustc_abi::Primitive::Pointer(_) => ir::LaneKind::Int { signed: false },
        };
        Ok((count as u16, lane_bytes, lane, layout.size.bytes()))
    }

    /// Expands the whole SIMD family (each operation is a per-lane host loop, with
    /// semantics dispatched by LaneKind). An unsupported simd_* returns Err (Trap placeholder).
    pub(super) fn expand_simd(
        &mut self,
        name: &str,
        inst: &Instance<'tcx>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
    ) -> Result<Vec<Stmt>, String> {
        use ir::{LaneKind, SimdBinOp as S, SimdReduceOp as R, SimdUnOp as U};
        // Vector operand -> place address expression (Bytes channel; constants are already materialized in the frozen region)
        let vplace = |cx: &mut Self, op: &mir::Operand<'tcx>| -> Result<PlaceExpr, String> {
            match cx.lower_operand(op)? {
                LoweredOp::Bytes { place, .. } => Ok(place.expr()),
                _ => Err("simd argument is not a vector".into()),
            }
        };
        // Special-shaped arm first: the first generic arg is not a vector (a scalar bitmask), so geometry comes from the data vector
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
        // Normal geometry: T = the first generic arg (the data vector for most arms; the mask vector for select/masked)
        let vec_ty = inst.args.type_at(0);
        // f16 lanes are allowed only for the shuffle/cast families (bit movement / lane conversion are exact; arithmetic stays rejected)
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
        // Unary (the float/integer lane kind is validated here; the executor keeps only defensive asserts)
        let un = |cx: &mut Self, op: U, need: Option<LaneKind>| -> Result<Vec<Stmt>, String> {
            if let Some(need) = need {
                let ok = match need {
                    LaneKind::Float => lane == LaneKind::Float,
                    LaneKind::Int { .. } => matches!(lane, LaneKind::Int { .. }),
                };
                if !ok {
                    return Err(format!("{name} requires a {need:?} lane, found {lane:?}"));
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
                    return Err(format!("{name} requires a float lane, found {lane:?}"));
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
                    return Err(format!("{name} requires a float lane, found {lane:?}"));
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
                    return Err(format!("{name} requires an integer lane, found {lane:?}"));
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
                // <T, U>(x: T) -> U: destination geometry comes from the destination place.
                // The pointer family forces the integer view (real-address model: provenance is the bits passed through).
                let ptr_family = name != "simd_cast" && name != "simd_as";
                let dst_p = self.resolve_place(destination)?;
                // f16 lanes allowed (vector form: f16<->f32/f64 via exact host conversion)
                let (dst_lanes, dst_bytes, dst_lane, _) = self.simd_geom_ext(dst_p.ty, true)?;
                if dst_lanes != lanes {
                    return Err(format!(
                        "{name} lane counts differ ({lanes} vs {dst_lanes})"
                    ));
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
                // <M, T>(mask: M, if_true: T, if_false: T): the geometry comes from the data vector
                let (d_lanes, d_bytes, _, _) = self.simd_geom(inst.args.type_at(1))?;
                if d_lanes != lanes {
                    return Err("simd_select mask/data lane counts differ".into());
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
                // <T, U, V>(val: T, ptr: U, mask: V): T = data vector (gather's passthru /
                // scatter's values), U = pointer vector (lane is always 8B), V = mask vector
                let (p_lanes, p_bytes, _, _) = self.simd_geom(inst.args.type_at(1))?;
                let (m_lanes, m_bytes, _, _) = self.simd_geom(inst.args.type_at(2))?;
                if p_lanes != lanes || m_lanes != lanes || p_bytes != 8 {
                    return Err(format!(
                        "{name} geometry mismatch (data={lanes} ptr={p_lanes}x{p_bytes}B mask={m_lanes})"
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
                // <V, U, T, ALIGN>(mask: V, ptr: U, val: T): the first generic arg is the mask
                // vector; ptr is a scalar element pointer and lane i is at ptr + i x lane.
                // ALIGN only affects the guest's UB contract (engine accesses are already unaligned-safe per lane).
                let (d_lanes, d_bytes, _, _) = self.simd_geom(inst.args.type_at(2))?;
                if d_lanes != lanes {
                    return Err(format!("{name} mask/data lane counts differ"));
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
                        "simd_extract_dyn lane width mismatch (vector={lane_bytes}, result={})",
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
                        "simd_insert_dyn lane width mismatch (vector={lane_bytes}, value={})",
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
                // <T, U>(ptr: T, offset: U): stride = the pointee size of the pointer lane
                let (_, elem_ty) = vec_ty.simd_size_and_type(self.tcx);
                let pointee = elem_ty.builtin_deref(true).ok_or_else(|| {
                    format!("simd_arith_offset lane is not a pointer ({elem_ty})")
                })?;
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
                // core::intrinsics::simd_insert's index contract requires a compile-time
                // constant that is in bounds. Dynamic indexing has its own simd_insert_dyn and must not be accepted here.
                let src = vplace(self, &args[0].node)?;
                let idx = match self.lower_operand_scalar(&args[1].node)? {
                    Operand::Imm {
                        bits,
                        width: Width::W32,
                    } => bits,
                    Operand::Imm { width, .. } => {
                        return Err(format!(
                            "simd_insert index type width should be u32, found {} bytes",
                            width.bytes()
                        ));
                    }
                    _ => return Err("simd_insert index is not constant".into()),
                };
                if idx >= count {
                    return Err(format!(
                        "simd_insert index {idx} out of bounds (lanes={count})"
                    ));
                }
                let (_, lane_ty) = vec_ty.simd_size_and_type(self.tcx);
                let val_ty = self.op_ty(&args[2].node)?;
                if val_ty != lane_ty {
                    return Err(format!(
                        "simd_insert lane type mismatch (vector={lane_ty}, value={val_ty})"
                    ));
                }
                let lane_width = Width::from_bytes(lane_bytes as u64)
                    .ok_or_else(|| format!("simd_insert lane width {lane_bytes} unsupported"))?;
                let val = self.lower_operand_scalar(&args[2].node)?;
                if val.width() != lane_width {
                    return Err(format!(
                        "simd_insert lane type width mismatch (vector={lane_bytes}, value={})",
                        val.width().bytes()
                    ));
                }
                let dst = self.resolve_place(destination)?;
                let size = u32::try_from(vec_size)
                    .map_err(|_| "simd_insert vector size exceeds u32".to_string())?;
                let lane_offset = idx
                    .checked_mul(u64::from(lane_bytes))
                    .and_then(|offset| i32::try_from(offset).ok())
                    .ok_or_else(|| "simd_insert lane offset exceeds i32".to_string())?;
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
                            "simd_extract index type width should be u32, found {} bytes",
                            width.bytes()
                        ));
                    }
                    _ => return Err("simd_extract index is not constant".into()),
                };
                if idx >= count {
                    return Err(format!(
                        "simd_extract index {idx} out of bounds (lanes={count})"
                    ));
                }
                let (_, lane_ty) = vec_ty.simd_size_and_type(self.tcx);
                let lane_width = Width::from_bytes(lane_bytes as u64)
                    .ok_or_else(|| format!("simd_extract lane width {lane_bytes} unsupported"))?;
                let (dst, dst_width) = self.place_scalar(destination)?;
                if dst.ty != lane_ty {
                    return Err(format!(
                        "simd_extract lane type mismatch (vector={lane_ty}, result={})",
                        dst.ty
                    ));
                }
                if dst_width != lane_width {
                    return Err(format!(
                        "simd_extract lane type width mismatch (vector={lane_bytes}, result={})",
                        dst_width.bytes()
                    ));
                }
                let mut lane = src;
                let lane_offset = idx
                    .checked_mul(u64::from(lane_bytes))
                    .and_then(|offset| i32::try_from(offset).ok())
                    .ok_or_else(|| "simd_extract lane offset exceeds i32".to_string())?;
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
                // (a, b, const idx array) -> shuffled vector: indices are known at lowering time, so expand into per-lane copies
                let mir::Operand::Constant(c) = &args[2].node else {
                    return Err("simd_shuffle index is not constant".into());
                };
                let val = c
                    .const_
                    .eval(self.tcx, self.typing_env, c.span)
                    .map_err(|e| format!("shuffle index evaluation failed: {e:?}"))?;
                let mir::ConstValue::Indirect { alloc_id, offset } = val else {
                    return Err(format!("shuffle index shape {val:?}"));
                };
                let alloc = self.tcx.global_alloc(alloc_id).unwrap_memory();
                let ai = alloc.inner();
                // Index elements are u32 (the stdarch simd_shuffle! macro emits [u32; N])
                let n_out = (ai.size().bytes() - offset.bytes()) / 4;
                let bytes = ai.inspect_with_uninit_and_ptr_outside_interpreter(
                    offset.bytes() as usize..ai.size().bytes() as usize,
                );
                let pa = vplace(self, &args[0].node)?;
                let pb = vplace(self, &args[1].node)?;
                let dst = self.resolve_place(destination)?;
                let lw = Width::from_bytes(lane_bytes as u64).ok_or("lane width")?;
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
                // splat(val: E) -> T: geometry comes from the return vector. The generic
                // order of splat is <T(vector), E>, so freezing it directly from the destination layout is safest.
                let dst_p = self.resolve_place(destination)?;
                let dst_layout = self.layout_of(dst_p.ty)?;
                let rustc_abi::BackendRepr::SimdVector { element, count } = dst_layout.backend_repr
                else {
                    return Err("simd_splat target is not a vector".into());
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
            other => Err(format!("intrinsic `{other}` (SIMD)")),
        }
    }
}
