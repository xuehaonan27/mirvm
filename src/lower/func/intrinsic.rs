//! In-place intrinsic expansion: try_expand_intrinsic dispatches ~60 names
//! (atomic/volatile/copy/ct*/math/fma/fast-float/ptr/size_of/saturating/
//! caller_location/compare_bytes families) plus float routing / atomic_ord and
//! free helpers (elem_of/float_w/LayoutCxAt etc., pub(super) for the whole tree).

use crate::lower::Error;

use super::*;

impl<'tcx> LowerCx<'tcx, '_> {
    pub(super) fn resolve_float_route(
        &self,
        suffix: Option<FloatSuffix>,
        inst: &Instance<'tcx>,
        n: &str,
    ) -> Result<FloatRoute, Error> {
        let suffix = match suffix {
            Some(sfx) => sfx,
            None => {
                let t = inst.args.type_at(0);
                match t.kind() {
                    ty::Float(ty::FloatTy::F16) => FloatSuffix::F16,
                    ty::Float(ty::FloatTy::F32) => FloatSuffix::F32,
                    ty::Float(ty::FloatTy::F64) => FloatSuffix::F64,
                    ty::Float(ty::FloatTy::F128) => FloatSuffix::F128,
                    _ => {
                        return Err(Error::internal(format!(
                            "intrinsic `{n}` generic argument is not a float ({t})"
                        )));
                    }
                }
            }
        };
        Ok(match suffix {
            FloatSuffix::F16 => FloatRoute::Scalar(ir::FloatW::F16),
            FloatSuffix::F32 => FloatRoute::Scalar(ir::FloatW::F32),
            FloatSuffix::F64 => FloatRoute::Scalar(ir::FloatW::F64),
            FloatSuffix::F128 => FloatRoute::Wide128,
        })
    }

    /// atomic intrinsic const generic ordering -> frozen MemOrd (isomorphic to cg_ssa
    /// parse_atomic_ordering: valtree branch[0] is the discriminant leaf). Collect
    /// **all** const generic args by position rather than hard-coding an index: the
    /// type-arg counts differ per atomic intrinsic (xadd<T,U,ORD> vs load<T,ORD>), and
    /// the ordering arg is the only const among them (cxchg has two: succ, fail).
    pub(super) fn atomic_ord(
        &self,
        inst: &Instance<'tcx>,
        nth: usize,
    ) -> Result<ir::MemOrd, Error> {
        use rustc_middle::ty::AtomicOrdering as A;
        let c = inst
            .args
            .iter()
            .filter_map(|a| a.as_const())
            .nth(nth)
            .ok_or_else(|| {
                Error::internal(format!(
                    "atomic intrinsic is missing const ordering arg #{nth}"
                ))
            })?;
        let discr = c.to_value().to_branch()[0].to_leaf();
        Ok(match discr.to_atomic_ordering() {
            A::Relaxed => ir::MemOrd::Relaxed,
            A::Acquire => ir::MemOrd::Acquire,
            A::Release => ir::MemOrd::Release,
            A::AcqRel => ir::MemOrd::AcqRel,
            A::SeqCst => ir::MemOrd::SeqCst,
        })
    }

    pub(super) fn try_expand_intrinsic(
        &mut self,
        inst: &Instance<'tcx>,
        args: &[rustc_span::Spanned<mir::Operand<'tcx>>],
        destination: &mir::Place<'tcx>,
        target: Option<mir::BasicBlock>,
        unwind: mir::UnwindAction,
        span: rustc_span::Span,
    ) -> Result<Option<(Vec<Stmt>, Terminator)>, Error> {
        let InstanceKind::Intrinsic(def_id) = inst.def else {
            return Ok(None);
        };
        let name = self.tcx.item_name(def_id);
        // Engine-level intrinsic (not a pure-value expansion): catch_unwind goes through the Builtin channel
        if name.as_str() == "catch_unwind" {
            let (dst_p, w) = self.place_scalar(destination)?;
            let tgt = target
                .ok_or(Error::internal("catch_unwind diverges?"))?
                .as_u32();
            let role = self
                .linker
                .main_catch_site
                .filter(|site| {
                    site.catcher_caller == self.instance && site.catcher_intrinsic == *inst
                })
                .map_or(ir::BuiltinCallRole::Normal, |_| {
                    ir::BuiltinCallRole::MainPanicCatcher
                });
            return Ok(Some((
                vec![],
                Terminator::CallBuiltin {
                    builtin: ir::Builtin::CatchUnwind,
                    args: vec![
                        self.lower_operand_scalar(&args[0].node)?,
                        self.lower_operand_scalar(&args[1].node)?,
                        self.lower_operand_scalar(&args[2].node)?,
                    ],
                    ret: RetDest::Scalar(dst_p.scalar_place(w)),
                    target: tgt,
                    unwind: self.lower_unwind(unwind),
                    role,
                },
            )));
        }
        // Generic element size (T of copy/write_bytes/offset)
        let elem_size = |cx: &Self| -> Result<u64, Error> {
            let t = inst.args.type_at(0);
            Ok(cx.layout_of(t)?.size.bytes())
        };
        let stmts = match name.as_str() {
            "offset" | "arith_offset" => {
                // fn offset<Ptr, Delta>(ptr: Ptr, count: Delta) -> Ptr
                let ptr_ty = self.op_ty(&args[0].node)?;
                let pointee = ptr_ty.builtin_deref(true).ok_or_else(|| {
                    Error::internal(format!("offset is not a pointer (ty={ptr_ty})"))
                })?;
                let stride = self.layout_of(pointee)?.size.bytes();
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::PtrOffset {
                        ptr: self.lower_operand_scalar(&args[0].node)?,
                        count: self.lower_operand_scalar(&args[1].node)?,
                        stride,
                    },
                }]
            }
            "ctpop" | "ctlz" | "cttz" | "ctlz_nonzero" | "cttz_nonzero" | "bswap"
            | "bitreverse" => {
                use ir::BitUnOp as B;
                let op = match name.as_str() {
                    "ctpop" => B::Popcount,
                    "ctlz" | "ctlz_nonzero" => B::Ctlz,
                    "cttz" | "cttz_nonzero" => B::Cttz,
                    "bswap" => B::Bswap,
                    _ => B::Bitreverse,
                };
                // 128-bit: ctpop/ctlz/cttz results are u32 (<= 128 fits u32); bswap/
                // bitreverse results remain 128-bit. Both use the wide Bit128 channel (a 16-byte source place).
                let a_ty = self.op_ty(&args[0].node)?;
                if self.layout_of(a_ty)?.size.bytes() == 16 {
                    let src = self.wide_place(&args[0].node)?;
                    return Ok(Some((
                        if matches!(op, B::Bswap | B::Bitreverse) {
                            vec![Stmt::Bit128 {
                                op,
                                src,
                                dst: self.resolve_place(destination)?.expr(),
                            }]
                        } else {
                            // Counting: the result is a scalar (u32), so use Bit128Count
                            let (dst_p, w) = self.place_scalar(destination)?;
                            vec![Stmt::Bit128Count {
                                op,
                                src,
                                dst: dst_p.scalar_place(w),
                            }]
                        },
                        Terminator::Goto(
                            target
                                .ok_or(Error::internal("bit intrinsic diverges?"))?
                                .as_u32(),
                        ),
                    )));
                }
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::BitUn {
                        op,
                        a: self.lower_operand_scalar(&args[0].node)?,
                    },
                }]
            }
            "exact_div" => {
                let a_ty = self.op_ty(&args[0].node)?;
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntBin {
                        op: IntBinOp::Div,
                        signed: frame::ty_signed(a_ty),
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                    },
                }]
            }
            "atomic_load" => {
                let order = self.atomic_ord(inst, 0)?;
                if matches!(order, ir::MemOrd::Release | ir::MemOrd::AcqRel) {
                    return Err(Error::internal(format!(
                        "atomic_load has an invalid ordering {order:?}"
                    )));
                }
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::AtomicLoad {
                        addr: self.lower_operand_scalar(&args[0].node)?,
                        width: w,
                        order,
                    },
                }]
            }
            "atomic_store" => {
                let order = self.atomic_ord(inst, 0)?;
                if matches!(order, ir::MemOrd::Acquire | ir::MemOrd::AcqRel) {
                    return Err(Error::internal(format!(
                        "atomic_store has an invalid ordering {order:?}"
                    )));
                }
                vec![Stmt::AtomicStore {
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    order,
                }]
            }
            "atomic_cxchg" | "atomic_cxchgweak" => {
                // (ptr, expected, new) -> (T, bool)
                let dst_p = self.resolve_place(destination)?;
                let ValKind::Pair((vo, vw), (fo, fw)) = self.classify(dst_p.ty)? else {
                    return Err(Error::internal("cxchg target is not a pair"));
                };
                let succ = self.atomic_ord(inst, 0)?;
                let fail = self.atomic_ord(inst, 1)?;
                if matches!(fail, ir::MemOrd::Release | ir::MemOrd::AcqRel) {
                    return Err(Error::internal(format!(
                        "cxchg has an invalid failure ordering {fail:?}"
                    )));
                }
                vec![Stmt::AtomicCxchg {
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    expected: self.lower_operand_scalar(&args[1].node)?,
                    new: self.lower_operand_scalar(&args[2].node)?,
                    dst_val: dst_p.half_place(vo, vw),
                    dst_ok: dst_p.half_place(fo, fw),
                    weak: name.as_str() == "atomic_cxchgweak",
                    succ,
                    fail,
                }]
            }
            "atomic_xchg" | "atomic_xadd" | "atomic_xsub" | "atomic_and" | "atomic_or"
            | "atomic_xor" | "atomic_nand" | "atomic_max" | "atomic_min" | "atomic_umax"
            | "atomic_umin" => {
                use ir::RmwOp as R;
                let op = match name.as_str() {
                    "atomic_xchg" => R::Xchg,
                    "atomic_xadd" => R::Add,
                    "atomic_xsub" => R::Sub,
                    "atomic_and" => R::And,
                    "atomic_or" => R::Or,
                    "atomic_xor" => R::Xor,
                    // fetch_max/min: signedness is frozen into the variant by the intrinsic name
                    "atomic_max" => R::Max,
                    "atomic_min" => R::Min,
                    "atomic_umax" => R::UMax,
                    "atomic_umin" => R::UMin,
                    _ => R::Nand,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::AtomicRmw {
                    op,
                    addr: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    dst: dst_p.scalar_place(w),
                    order: self.atomic_ord(inst, 0)?,
                }]
            }
            // fence
            "atomic_fence" => vec![Stmt::Fence {
                single_thread: false,
                order: self.atomic_ord(inst, 0)?,
            }],
            "atomic_singlethreadfence" => vec![Stmt::Fence {
                single_thread: true,
                order: self.atomic_ord(inst, 0)?,
            }],
            // volatile is RAM-observable behavior and must be preserved all the way to
            // the executor. The value always uses an alignment=1 opaque MaybeUninit byte
            // carrier: this neither imposes a wrong integer alignment on `[u8; N]` nor
            // reads an aggregate's uninitialized padding as a host value. rustc emits a
            // volatile memcpy for memory-repr stores; the executor splits wide values into
            // backend-supported blocks, while common scalar widths stay one volatile event.
            "volatile_load" | "unaligned_volatile_load" => {
                let t = inst.args.type_at(0);
                let size = u32::try_from(self.layout_of(t)?.size.bytes())
                    .map_err(|_| Error::internal(format!("{name} type {t} size exceeds u32")))?;
                if size == 0 {
                    vec![Stmt::Nop]
                } else {
                    vec![Stmt::VolatileLoad {
                        addr: self.lower_operand_scalar(&args[0].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                        size,
                    }]
                }
            }
            "volatile_store" | "unaligned_volatile_store" => {
                let t = inst.args.type_at(0);
                let size = u32::try_from(self.layout_of(t)?.size.bytes())
                    .map_err(|_| Error::internal(format!("{name} type {t} size exceeds u32")))?;
                if size == 0 {
                    vec![Stmt::Nop]
                } else {
                    let addr = self.lower_operand_scalar(&args[0].node)?;
                    let mut stmts = Vec::new();
                    let src = if let Some(place) = args[1].node.place() {
                        self.resolve_place(&place)?.expr()
                    } else {
                        let value = self.lower_operand(&args[1].node)?;
                        let kind = self.classify(t)?;
                        let scratch = self.scratch_place(t)?;
                        stmts.extend(self.assign_lowered(&scratch, kind, value)?);
                        scratch.expr()
                    };
                    stmts.push(Stmt::VolatileStore { addr, src, size });
                    stmts
                }
            }
            "copy_nonoverlapping" | "copy" => {
                // (src, dst, count) -- note the order is the reverse of C memcpy
                vec![Stmt::MemCopy {
                    src: self.lower_operand_scalar(&args[0].node)?,
                    dst: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                    overlap: name.as_str() == "copy",
                }]
            }
            // Volatile bulk memory access: LLVM volatile memcpy/memset make no promise
            // about access width or count; volatile only guarantees "not omitted, not
            // reordered/merged". The interpreter executes every statement and never
            // omits one, so sharing the copy/write_bytes channel is a faithful
            // implementation. Note the argument order: this family is (dst, src, count),
            // the reverse of copy's (src, dst, count).
            "volatile_copy_memory" | "volatile_copy_nonoverlapping_memory" => {
                vec![Stmt::MemCopy {
                    dst: self.lower_operand_scalar(&args[0].node)?,
                    src: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                    overlap: name.as_str() == "volatile_copy_memory",
                }]
            }
            "volatile_set_memory" => {
                vec![Stmt::MemSet {
                    dst: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                }]
            }
            // Nontemporal store (stdarch _mm_stream_* funnels here): NT is a cache-bypassing
            // performance hint whose value semantics are an ordinary store; its weak-ordering
            // obligations relative to fence belong to the guest. Use the volatile store
            // channel (a superset guarantee against omission; wide-value blocking rules match).
            "nontemporal_store" => {
                let t = inst.args.type_at(0);
                let size = u32::try_from(self.layout_of(t)?.size.bytes())
                    .map_err(|_| Error::internal(format!("{name} type {t} size exceeds u32")))?;
                if size == 0 {
                    vec![Stmt::Nop]
                } else {
                    let addr = self.lower_operand_scalar(&args[0].node)?;
                    let mut stmts = Vec::new();
                    let src = if let Some(place) = args[1].node.place() {
                        self.resolve_place(&place)?.expr()
                    } else {
                        let value = self.lower_operand(&args[1].node)?;
                        let kind = self.classify(t)?;
                        let scratch = self.scratch_place(t)?;
                        stmts.extend(self.assign_lowered(&scratch, kind, value)?);
                        scratch.expr()
                    };
                    stmts.push(Stmt::VolatileStore { addr, src, size });
                    stmts
                }
            }
            // ptr.mask(m): address bitwise-AND; provenance is unchanged (under real addresses the AND is the semantics)
            "ptr_mask" => {
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::IntBin {
                        op: IntBinOp::BitAnd,
                        signed: false,
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                    },
                }]
            }
            // Direct vtable slot read: the argument is a bare vtable pointer (DynMetadata::size_of/align_of).
            // The slot layout [drop, size, align, ...] comes from the same source as size_of_val's dyn arm.
            "vtable_size" | "vtable_align" => {
                let vt = self.lower_operand_scalar(&args[0].node)?;
                let slot_off = if name.as_str() == "vtable_size" {
                    8
                } else {
                    16
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(operand_deref_at(vt, slot_off)?),
                }]
            }
            // Nullary type query safety arm: normally GVN constant-folds it away; when it
            // survives, fold it at lowering time via tcx (isomorphic to rustc
            // eval_nullary_intrinsic). type_id/type_name/offset_of/field_offset are not
            // included: on this nightly TypeId is a vtable identity struct and type_name
            // needs string materialization, so a survivor still Traps loudly. Reopen this if a
            // real program hits it under non-default mir-opt.
            "size_of" | "align_of" | "min_align_of" => {
                let t = inst.args.type_at(0);
                let l = self.layout_of(t)?;
                let v = if name.as_str() == "size_of" {
                    l.size.bytes()
                } else {
                    l.align.abi.bytes()
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                }]
            }
            "variant_count" => {
                // Isomorphic to rustc eval_nullary_intrinsic: peel Pat down to the base;
                // an Adt yields the variant count and any other concrete type yields 0
                let mut t = inst.args.type_at(0);
                while let ty::Pat(base, _) = t.kind() {
                    t = *base;
                }
                let v = match t.kind() {
                    ty::Adt(adt, _) => adt.variants().len() as u64,
                    _ => 0,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                }]
            }
            "needs_drop" => {
                let t = inst.args.type_at(0);
                let v = t.needs_drop(self.tcx, self.typing_env) as u64;
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                }]
            }
            "write_bytes" => {
                vec![Stmt::MemSet {
                    dst: self.lower_operand_scalar(&args[0].node)?,
                    val: self.lower_operand_scalar(&args[1].node)?,
                    count: self.lower_operand_scalar(&args[2].node)?,
                    elem_size: elem_size(self)?,
                }]
            }
            "black_box" | "transmute" => {
                // Value pass-through (the transmute call form is a bit reinterpretation; black_box is a copy)
                let dst_p = self.resolve_place(destination)?;
                let dst_kind = self.classify(dst_p.ty)?;
                let src = self.lower_operand(&args[0].node)?;
                self.assign_lowered(&dst_p, dst_kind, src)?
            }
            "assume" => vec![Stmt::Nop],
            "abort" => {
                // core::intrinsics::abort: process-level abort (native = SIGILL trap,
                // engine = SIGABRT; the signal difference is recorded and aligned later if differentials compare signals)
                let tgt = (self.mir_block_count + self.extra_blocks.len()) as Bb;
                self.extra_blocks.push(ir::Block {
                    stmts: vec![],
                    term: Terminator::Unreachable,
                });
                return Ok(Some((
                    vec![],
                    Terminator::CallBuiltin {
                        builtin: ir::Builtin::HostAbort,
                        args: vec![],
                        ret: RetDest::Ignore,
                        target: tgt,
                        unwind: ir::UnwindAction::Continue,
                        role: ir::BuiltinCallRole::Normal,
                    },
                )));
            }
            "breakpoint" => {
                // Real int3: observably SIGTRAP as in native; normally continues to target
                let tgt = target
                    .ok_or(Error::internal("breakpoint diverges?"))?
                    .as_u32();
                return Ok(Some((
                    vec![],
                    Terminator::CallBuiltin {
                        builtin: ir::Builtin::Breakpoint,
                        args: vec![],
                        ret: RetDest::Ignore,
                        target: tgt,
                        unwind: ir::UnwindAction::Continue,
                        role: ir::BuiltinCallRole::Normal,
                    },
                )));
            }
            "assert_inhabited" | "assert_zero_valid" | "assert_mem_uninitialized_valid" => {
                // Decided at lowering time (isomorphic to the collector): valid -> nop;
                // violated -> placeholder (native expands to panic_nounwind; this path is a defensive dead end)
                let req = rustc_middle::ty::layout::ValidityRequirement::from_intrinsic(name)
                    .expect("validity intrinsic name");
                let t = inst.args.type_at(0);
                let ok = self
                    .tcx
                    .check_validity_requirement((req, self.typing_env.as_query_input(t)))
                    .map_err(|e| Error::internal(format!("validity check failed: {e}")))?;
                if ok {
                    vec![Stmt::Nop]
                } else {
                    vec![Stmt::Trap(
                        format!("{name} violated (ty={t} -- native panic_nounwind)")
                            .into_boxed_str(),
                    )]
                }
            }
            "saturating_add" | "saturating_sub" => {
                let a_ty = self.op_ty(&args[0].node)?;
                let op = if name.as_str() == "saturating_add" {
                    OvfOp::Add
                } else {
                    OvfOp::Sub
                };
                // 128-bit: the wide form uses Sat128 (computed on host u128/i128; scalar IntSat only reaches 64 bits)
                if matches!(
                    a_ty.kind(),
                    ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                ) {
                    vec![Stmt::Sat128 {
                        op,
                        signed: frame::ty_signed(a_ty),
                        a: self.wide_place(&args[0].node)?,
                        b: self.wide_place(&args[1].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                    }]
                } else {
                    let (dst_p, w) = self.place_scalar(destination)?;
                    vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::IntSat {
                            op,
                            signed: frame::ty_signed(a_ty),
                            a: self.lower_operand_scalar(&args[0].node)?,
                            b: self.lower_operand_scalar(&args[1].node)?,
                        },
                    }]
                }
            }
            "caller_location" => {
                // Location::caller(): when this function is track_caller, read the hidden
                // trailing arg slot; otherwise synthesize from the intrinsic call site (rare; the caller chain usually tracks to the bottom)
                let (dst_p, w) = self.place_scalar(destination)?;
                let op = match self.caller_loc_off {
                    Some(off) => Operand::Slot(Slot {
                        off,
                        width: Width::W64,
                    }),
                    None => self.caller_location_imm(span)?,
                };
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::Use(op),
                }]
            }
            "compare_bytes" => {
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::MemCmp {
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        n: self.lower_operand_scalar(&args[2].node)?,
                    },
                }]
            }
            // Byte-level equality (the spec path of <[u8;N]>::eq): memcmp == 0
            "raw_eq" => {
                let t = inst.args.type_at(0);
                let size = self.layout_of(t)?.size.bytes();
                let (dst_p, w) = self.place_scalar(destination)?;
                let s = self.scratch64();
                let s32 = Slot {
                    off: s.off,
                    width: Width::W32,
                };
                vec![
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(s32),
                        rv: Rvalue::MemCmp {
                            a: self.lower_operand_scalar(&args[0].node)?,
                            b: self.lower_operand_scalar(&args[1].node)?,
                            n: Operand::Imm {
                                bits: size,
                                width: Width::W64,
                            },
                        },
                    },
                    Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::IntCmp {
                            cc: IntCc::Eq,
                            signed: false,
                            a: Operand::Slot(s32),
                            b: Operand::Imm {
                                bits: 0,
                                width: Width::W32,
                            },
                        },
                    },
                ]
            }
            // Unchecked float -> integer (fast mode does not check UB; same implementation as `as`)
            "float_to_int_unchecked" => {
                let fty = inst.args.type_at(0);
                let ity = inst.args.type_at(1);
                let signed = frame::ty_signed(ity);
                // f128 source: <= 64-bit integer via F128ToScalar; i128/u128 via F128ToWideInt
                if matches!(fty.kind(), ty::Float(ty::FloatTy::F128)) {
                    let pa = self.wide_place(&args[0].node)?;
                    let dst = self.resolve_place(destination)?;
                    match frame::scalar_width(&self.layout_of(ity)?) {
                        Some(to_w) => {
                            vec![Stmt::F128ToScalar {
                                src: pa,
                                kind: ir::F128Scalar::Int { signed },
                                w: to_w,
                                dst: dst.scalar_place(to_w),
                            }]
                        }
                        None => vec![Stmt::F128ToWideInt {
                            src: pa,
                            signed,
                            dst: dst.expr(),
                        }],
                    }
                } else if let Some(to_w) = frame::scalar_width(&self.layout_of(ity)?) {
                    let (dst_p, w) = self.place_scalar(destination)?;
                    vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::FloatToInt {
                            from: float_w(fty)?,
                            to: to_w,
                            signed,
                            a: self.lower_operand_scalar(&args[0].node)?,
                        },
                    }]
                } else {
                    // Scalar float -> i128/u128
                    vec![Stmt::FloatToWide128 {
                        src: self.lower_operand_scalar(&args[0].node)?,
                        from: float_w(fty)?,
                        signed,
                        dst: self.resolve_place(destination)?.expr(),
                    }]
                }
            }
            // Math surface (must_be_overridden float intrinsics): computed directly on the
            // host. Width comes from the suffix (sqrtf64) when present; a bare generic name
            // such as fabs<T> takes its width from the type argument.
            n if math_un_of(n).is_some() => {
                let (op, sfx) = math_un_of(n).unwrap();
                match self.resolve_float_route(sfx, inst, n)? {
                    FloatRoute::Scalar(fw) => {
                        let (dst_p, w) = self.place_scalar(destination)?;
                        vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::MathUn {
                                op,
                                fw,
                                a: self.lower_operand_scalar(&args[0].node)?,
                            },
                        }]
                    }
                    FloatRoute::Wide128 => vec![Stmt::F128Un {
                        op: ir::F128UnOp::Math(op),
                        a: self.wide_place(&args[0].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                    }],
                }
            }
            n if math_bin_of(n).is_some() => {
                let (op, sfx) = math_bin_of(n).unwrap();
                match self.resolve_float_route(sfx, inst, n)? {
                    FloatRoute::Scalar(fw) => {
                        let (dst_p, w) = self.place_scalar(destination)?;
                        vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::MathBin {
                                op,
                                fw,
                                a: self.lower_operand_scalar(&args[0].node)?,
                                b: self.lower_operand_scalar(&args[1].node)?,
                            },
                        }]
                    }
                    FloatRoute::Wide128 => {
                        // powi's rhs is an i32 scalar; every other f128 rhs is wide
                        let b = if matches!(op, ir::MathBinOp::Powi) {
                            ir::F128Rhs::Scalar(self.lower_operand_scalar(&args[1].node)?)
                        } else {
                            ir::F128Rhs::Wide(self.wide_place(&args[1].node)?)
                        };
                        vec![Stmt::F128MathBin {
                            op,
                            a: self.wide_place(&args[0].node)?,
                            b,
                            dst: self.resolve_place(destination)?.expr(),
                        }]
                    }
                }
            }
            // Fused multiply-add: a*b+c with a single rounding. fmuladd permits both
            // fused and unfused, and fusion is always in the allowed set. f16 goes through host f16::mul_add (mathematically correct rounding).
            "fmaf16" | "fmaf32" | "fmaf64" | "fmuladdf16" | "fmuladdf32" | "fmuladdf64" => {
                let fw = match split_float_suffix(name.as_str()).1 {
                    Some(FloatSuffix::F16) => ir::FloatW::F16,
                    Some(FloatSuffix::F32) => ir::FloatW::F32,
                    _ => ir::FloatW::F64,
                };
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::MathFma {
                        fw,
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        c: self.lower_operand_scalar(&args[2].node)?,
                    },
                }]
            }
            "fmaf128" | "fmuladdf128" => vec![Stmt::F128Fma {
                a: self.wide_place(&args[0].node)?,
                b: self.wide_place(&args[1].node)?,
                c: self.wide_place(&args[2].node)?,
                dst: self.resolve_place(destination)?.expr(),
            }],
            // fast/algebraic float: the fast-math flag licenses reassociation/contraction,
            // so a result computed with exact IEEE semantics is always in the allowed set (same value as native without fast-math).
            "fadd_fast" | "fsub_fast" | "fmul_fast" | "fdiv_fast" | "frem_fast"
            | "fadd_algebraic" | "fsub_algebraic" | "fmul_algebraic" | "fdiv_algebraic"
            | "frem_algebraic" => {
                use ir::FloatOp as F;
                let op = match name.as_str().split('_').next().unwrap() {
                    "fadd" => F::Add,
                    "fsub" => F::Sub,
                    "fmul" => F::Mul,
                    "fdiv" => F::Div,
                    _ => F::Rem,
                };
                match self.resolve_float_route(None, inst, name.as_str())? {
                    FloatRoute::Scalar(fw) => {
                        let (dst_p, w) = self.place_scalar(destination)?;
                        vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::FloatBin {
                                op,
                                fw,
                                a: self.lower_operand_scalar(&args[0].node)?,
                                b: self.lower_operand_scalar(&args[1].node)?,
                            },
                        }]
                    }
                    FloatRoute::Wide128 => vec![Stmt::F128Bin {
                        op,
                        a: self.wide_place(&args[0].node)?,
                        b: self.wide_place(&args[1].node)?,
                        dst: self.resolve_place(destination)?.expr(),
                    }],
                }
            }
            n if n.starts_with("simd_") => self.expand_simd(n, inst, args, destination)?,
            "ptr_offset_from" | "ptr_offset_from_unsigned" => {
                let ptr_ty = self.op_ty(&args[0].node)?;
                let pointee = ptr_ty
                    .builtin_deref(true)
                    .ok_or(Error::internal("ptr_offset_from is not a pointer"))?;
                let stride = self.layout_of(pointee)?.size.bytes().max(1);
                let (dst_p, w) = self.place_scalar(destination)?;
                vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::PtrDiff {
                        a: self.lower_operand_scalar(&args[0].node)?,
                        b: self.lower_operand_scalar(&args[1].node)?,
                        stride,
                    },
                }]
            }
            "size_of_val" | "min_align_of_val" | "align_of_val" => {
                // *const T argument; a sized T is a constant; [E]/str folds through meta; dyn reads the vtable
                let t = inst.args.type_at(0);
                let (dst_p, w) = self.place_scalar(destination)?;
                let is_size = name.as_str() == "size_of_val";
                let t_layout = self.layout_of(t)?;
                if t_layout.is_sized() {
                    let v = if is_size {
                        t_layout.size.bytes()
                    } else {
                        t_layout.align.abi.bytes()
                    };
                    vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::Use(Operand::Imm { bits: v, width: w }),
                    }]
                } else {
                    match t.kind() {
                        ty::Slice(e) | ty::Array(e, _) => {
                            let el = self.layout_of(*e)?;
                            if is_size {
                                // meta (element count) x elem size
                                let LoweredOp::Pair(_, meta) = self.lower_operand(&args[0].node)?
                                else {
                                    return Err(Error::internal(
                                        "size_of_val argument is not a fat pointer",
                                    ));
                                };
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::IntBin {
                                        op: IntBinOp::Mul,
                                        signed: false,
                                        a: meta,
                                        b: Operand::Imm {
                                            bits: el.size.bytes(),
                                            width: Width::W64,
                                        },
                                    },
                                }]
                            } else {
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(Operand::Imm {
                                        bits: el.align.abi.bytes(),
                                        width: w,
                                    }),
                                }]
                            }
                        }
                        ty::Str => {
                            if is_size {
                                let LoweredOp::Pair(_, meta) = self.lower_operand(&args[0].node)?
                                else {
                                    return Err(Error::internal(
                                        "size_of_val argument is not a fat pointer",
                                    ));
                                };
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(meta),
                                }]
                            } else {
                                vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(Operand::Imm { bits: 1, width: w }),
                                }]
                            }
                        }
                        ty::Dynamic(..) => {
                            // vtable layout: [drop, size, align, ...] (COMMON_VTABLE_ENTRIES)
                            let LoweredOp::Pair(_, vt) = self.lower_operand(&args[0].node)? else {
                                return Err(Error::internal(format!(
                                    "{name} argument is not a dyn fat pointer"
                                )));
                            };
                            let slot_off = if is_size { 8 } else { 16 };
                            vec![Stmt::Assign {
                                dst: dst_p.scalar_place(w),
                                rv: Rvalue::Use(operand_deref_at(vt, slot_off)?),
                            }]
                        }
                        // Struct with an unsized tail field (Path/OsStr/RcInner<dyn>):
                        // isomorphic to cg_ssa glue size_and_align_of_dst --
                        // full_align = max(sized_align, tail_align);
                        // full_size = align_to(sized_size + tail_size, full_align).
                        ty::Adt(..) | ty::Tuple(..) => {
                            let LoweredOp::Pair(_, meta) = self.lower_operand(&args[0].node)?
                            else {
                                return Err(Error::internal(format!(
                                    "{name} argument is not a fat pointer"
                                )));
                            };
                            let tail = self.tcx.struct_tail_for_codegen(t, self.typing_env);
                            let sized_size = t_layout.size.bytes();
                            let sized_align = t_layout.align.abi.bytes();
                            let dst = dst_p.scalar_place(w);
                            let assign = |rv| Stmt::Assign {
                                dst: dst.clone(),
                                rv,
                            };
                            let imm = |v: u64| Operand::Imm {
                                bits: v,
                                width: Width::W64,
                            };
                            let dst_op = dst_p.scalar_operand(w);
                            match tail.kind() {
                                ty::Slice(..) | ty::Str => {
                                    let (esz, eal) = match tail.kind() {
                                        ty::Slice(e) => {
                                            let l = self.layout_of(*e)?;
                                            (l.size.bytes(), l.align.abi.bytes())
                                        }
                                        _ => (1, 1),
                                    };
                                    let fa = sized_align.max(eal); // compile-time constant
                                    if !is_size {
                                        vec![assign(Rvalue::Use(imm(fa)))]
                                    } else {
                                        let mut v = vec![
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Mul,
                                                signed: false,
                                                a: meta,
                                                b: imm(esz),
                                            }),
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: imm(sized_size),
                                            }),
                                        ];
                                        if fa > 1 {
                                            v.push(assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: imm(fa - 1),
                                            }));
                                            v.push(assign(Rvalue::IntBin {
                                                op: IntBinOp::BitAnd,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: imm(!(fa - 1)),
                                            }));
                                        }
                                        v
                                    }
                                }
                                ty::Dynamic(..) => {
                                    // meta = vtable: tail size@+8 / align@+16 (read at runtime).
                                    // full_align = max(sized_align, tail_align);
                                    // full_size = align_to(sized_size + tail_size, full_align)
                                    //          = (s + a − 1) & !(a − 1) (same formula as cg_ssa).
                                    let sl = |s: Slot| Operand::Slot(s);
                                    let sp = |s: Slot| ScalarPlace::Slot(s);
                                    let umax_align = Rvalue::UMax {
                                        a: operand_deref_at(meta.clone(), 16)?,
                                        b: imm(sized_align),
                                    };
                                    if !is_size {
                                        vec![assign(umax_align)]
                                    } else {
                                        let fa = self.scratch64();
                                        let m = self.scratch64();
                                        vec![
                                            Stmt::Assign {
                                                dst: sp(fa),
                                                rv: umax_align,
                                            },
                                            // dst = sized_size + tail_size
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: operand_deref_at(meta, 8)?,
                                                b: imm(sized_size),
                                            }),
                                            // m = fa − 1
                                            Stmt::Assign {
                                                dst: sp(m),
                                                rv: Rvalue::IntBin {
                                                    op: IntBinOp::Sub,
                                                    signed: false,
                                                    a: sl(fa),
                                                    b: imm(1),
                                                },
                                            },
                                            // dst += m
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::Add,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: sl(m),
                                            }),
                                            // m = !m
                                            Stmt::Assign {
                                                dst: sp(m),
                                                rv: Rvalue::NotBits(sl(m)),
                                            },
                                            // dst &= m
                                            assign(Rvalue::IntBin {
                                                op: IntBinOp::BitAnd,
                                                signed: false,
                                                a: dst_op.clone(),
                                                b: sl(m),
                                            }),
                                        ]
                                    }
                                }
                                _ => {
                                    return Err(Error::unsupported(format!(
                                        "{name} tail type {tail} unsupported"
                                    )));
                                }
                            }
                        }
                        _ => return Err(Error::internal(format!("{name} on unsized {t}"))),
                    }
                }
            }
            _ => return Ok(None),
        };
        let tgt = target
            .ok_or(Error::internal("intrinsic expansion: diverging intrinsic?"))?
            .as_u32();
        Ok(Some((stmts, Terminator::Goto(tgt))))
    }
}

pub(super) fn elem_of(ty: Ty<'_>) -> Option<Ty<'_>> {
    match ty.kind() {
        ty::Array(t, _) | ty::Slice(t) => Some(*t),
        _ => None,
    }
}

pub(super) fn u128_to_u64(v: u128) -> Result<u64, Error> {
    u64::try_from(v).map_err(|_| Error::internal("128-bit discriminant"))
}

/// Math intrinsic name -> (op, width). Width `Some(is64)` comes from the f32/f64
/// suffix; `None` means a bare generic name (on this nightly `fabs<T: FloatPrimitive>`
/// has dropped the suffix), resolved at the call site from type arguments. **Every
/// entry has a generic fallback**: suffix stripping is fragile against nightly drift,
/// so a name that loses its suffix takes the generic path instead of Trapping. f16/f128
/// are handled at the call site.
pub(super) fn math_un_of(n: &str) -> Option<(ir::MathUnOp, Option<FloatSuffix>)> {
    let (stem, sfx) = split_float_suffix(n);
    math_un_stem(stem).map(|op| (op, sfx))
}

fn math_un_stem(stem: &str) -> Option<ir::MathUnOp> {
    use ir::MathUnOp as M;
    Some(match stem {
        "sqrt" => M::Sqrt,
        "sin" => M::Sin,
        "cos" => M::Cos,
        "exp" => M::Exp,
        "exp2" => M::Exp2,
        "log" => M::Ln,
        "log2" => M::Log2,
        "log10" => M::Log10,
        "fabs" => M::Fabs,
        "floor" => M::Floor,
        "ceil" => M::Ceil,
        "trunc" => M::Trunc,
        "round" => M::Round,
        "round_ties_even" => M::RoundTiesEven,
        _ => return None,
    })
}

pub(super) fn math_bin_of(n: &str) -> Option<(ir::MathBinOp, Option<FloatSuffix>)> {
    use ir::MathBinOp as M;
    let (stem, sfx) = split_float_suffix(n);
    Some((
        match stem {
            "pow" => M::Pow,
            "powi" => M::Powi,
            "copysign" => M::Copysign,
            "minnum" => M::Minnum,
            "maxnum" => M::Maxnum,
            _ => return None,
        },
        sfx,
    ))
}

/// Float intrinsic name suffixes (sqrtf16/f32/f64/f128).
#[derive(Clone, Copy)]
pub(super) enum FloatSuffix {
    F16,
    F32,
    F64,
    F128,
}

/// Scalar/wide-channel routing.
#[derive(Clone, Copy)]
pub(super) enum FloatRoute {
    Scalar(ir::FloatW),
    Wide128,
}

/// Scalar float width (f128 is excluded: 16 bytes use the wide channel and the call site routes first).
pub(super) fn float_w(t: Ty<'_>) -> Result<ir::FloatW, Error> {
    match t.kind() {
        ty::Float(ty::FloatTy::F16) => Ok(ir::FloatW::F16),
        ty::Float(ty::FloatTy::F32) => Ok(ir::FloatW::F32),
        ty::Float(ty::FloatTy::F64) => Ok(ir::FloatW::F64),
        _ => Err(Error::internal(format!("non-scalar float width {t}"))),
    }
}

/// Strip the f16/f32/f64/f128 suffix; without one, return the original name + None (generic intrinsic, width from type args).
pub(super) fn split_float_suffix(n: &str) -> (&str, Option<FloatSuffix>) {
    for (sfx, tag) in [
        ("f128", FloatSuffix::F128),
        ("f64", FloatSuffix::F64),
        ("f32", FloatSuffix::F32),
        ("f16", FloatSuffix::F16),
    ] {
        if let Some(stem) = n.strip_suffix(sfx) {
            return (stem.trim_end_matches('_'), Some(tag));
        }
    }
    (n, None)
}

/// Add one more indirection on the operand's value (a pointer): *(op + off). Used to read vtable slots.
pub(super) fn operand_deref_at(op: Operand, off: u32) -> Result<Operand, Error> {
    let deref_steps = |mut steps: Vec<PlaceStep>| {
        steps.push(PlaceStep::Deref);
        if off != 0 {
            steps.push(PlaceStep::Offset(off as i32));
        }
        steps.into_boxed_slice()
    };
    Ok(match op {
        Operand::Slot(s) => Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Local(s.off),
                steps: deref_steps(Vec::new()),
            },
            width: Width::W64,
        },
        Operand::Mem { expr, .. } => Operand::Mem {
            expr: PlaceExpr {
                base: expr.base,
                steps: deref_steps(expr.steps.into_vec()),
            },
            width: Width::W64,
        },
        // Constant vtable address (a constant dyn reference): read the slot from the frozen region at run time
        Operand::Imm { bits, .. } => Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Static(ir::LinkAddr(bits.wrapping_add(off as u64))),
                steps: Box::new([]),
            },
            width: Width::W64,
        },
        Operand::AddrImm(addr) => Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Static(ir::LinkAddr(addr.0.wrapping_add(off as u64))),
                steps: Box::new([]),
            },
            width: Width::W64,
        },
        Operand::AddrOf(_) | Operand::SubImm { .. } => {
            return Err(Error::internal("vtable operand has an unexpected shape"));
        }
    })
}

/// `TyAndLayout::for_variant` needs a LayoutCx; build one on the spot from (tcx, typing_env).
pub(super) struct LayoutCxAt<'tcx>(pub(super) TyCtxt<'tcx>, pub(super) TypingEnv<'tcx>);

impl<'tcx> rustc_abi::HasDataLayout for LayoutCxAt<'tcx> {
    fn data_layout(&self) -> &rustc_abi::TargetDataLayout {
        self.0.data_layout()
    }
}
impl<'tcx> rustc_middle::ty::layout::HasTyCtxt<'tcx> for LayoutCxAt<'tcx> {
    fn tcx(&self) -> TyCtxt<'tcx> {
        self.0
    }
}
impl<'tcx> rustc_middle::ty::layout::HasTypingEnv<'tcx> for LayoutCxAt<'tcx> {
    fn typing_env(&self) -> TypingEnv<'tcx> {
        self.1
    }
}
