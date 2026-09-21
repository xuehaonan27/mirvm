//! Per-instance lowering: a monomorphized MIR body -> an engine FuncBody.
//!
//! Discipline: **Trap stubs everywhere**. When a statement, terminator or layout
//! hits an unrecognized construct, the current block lowers to `Trap(diagnostic)`
//! instead of aborting the whole lowering.
//!
//! Place compilation folds Field/Downcast projections into constant offsets and
//! leaves Deref/Index as runtime steps, yielding an address expression. Values fall
//! into four classes (Zst/Scalar/Pair/Bytes); calling convention v2 uses 2 slots for a
//! pair and indirect/sret for aggregates.

use crate::lower::Error;

use rustc_abi::{HasDataLayout, TagEncoding, VariantIdx, Variants};
use rustc_middle::mir::{self, Body};
use rustc_middle::ty::{self, EarlyBinder, Instance, InstanceKind, Ty, TyCtxt, TypingEnv};

use super::frame::{self, FrameLayout, ValKind};
use super::{Callee, Linker};
use crate::vm::ir::{
    self, Bb, IntBinOp, IntCc, Operand, OvfOp, ParamAbi, PlaceBase, PlaceExpr, PlaceStep, RetAbi,
    RetDest, Rvalue, ScalarPlace, Slot, Stmt, SwitchDiscr, Terminator, Width,
};

mod asm;
mod call;
mod cast;
mod intrinsic;
mod simd;
mod term;
mod unsize;

use intrinsic::*;

/// Placeholder body for a function that cannot be lowered (calling it Traps with the reason).
pub fn trap_body(name: &str, reason: &str) -> ir::FuncBody {
    ir::FuncBody {
        frame_size: 0,
        frame_align: 1,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![ir::Block {
            stmts: Vec::new(),
            term: Terminator::Trap(format!("function not lowered: {reason}").into_boxed_str()),
        }],
        name: name.into(),
    }
}

/// Intermediate result of place compilation: address expression + current type (+ fat-pointer meta source).
struct PlaceLow<'tcx> {
    base: PlaceBase,
    steps: Vec<PlaceStep>,
    ty: Ty<'tcx>,
    /// When the place derefs into an unsized pointee: where the meta (len/vtable ptr)
    /// is read from -- the second half of the fat pointer before the deref (used to rebuild a fat pointer for an unsized Ref place).
    meta: Option<Operand>,
}

impl<'tcx> PlaceLow<'tcx> {
    fn push_offset(&mut self, o: i64) {
        // Fold consecutive constant offsets (a Field chain becomes one Offset)
        if o == 0 {
            return;
        }
        if let Some(PlaceStep::Offset(prev)) = self.steps.last_mut() {
            *prev += o as i32;
        } else if self.steps.is_empty()
            && o >= 0
            && let PlaceBase::Local(off) = &mut self.base
        {
            // Purely in-frame and non-negative: fold directly into the base offset to keep the fast path
            *off += o as u32;
        } else {
            self.steps.push(PlaceStep::Offset(o as i32));
        }
    }

    /// Purely in-frame static offset (fast-path slot)
    fn frame_direct(&self) -> Option<u32> {
        match (&self.base, self.steps.is_empty()) {
            (PlaceBase::Local(off), true) => Some(*off),
            _ => None,
        }
    }

    fn expr(&self) -> PlaceExpr {
        PlaceExpr {
            base: self.base,
            steps: self.steps.clone().into_boxed_slice(),
        }
    }

    /// Expression with a constant offset appended (used to access either half of a pair; does not mutate self)
    fn expr_plus(&self, o: u32) -> PlaceExpr {
        let mut steps = self.steps.clone();
        if o != 0 {
            if let Some(PlaceStep::Offset(prev)) = steps.last_mut() {
                *prev += o as i32;
            } else {
                steps.push(PlaceStep::Offset(o as i32));
            }
        }
        PlaceExpr {
            base: self.base,
            steps: steps.into_boxed_slice(),
        }
    }

    /// Scalar place (fast path preferred)
    fn scalar_place(&self, w: Width) -> ScalarPlace {
        match self.frame_direct() {
            Some(off) => ScalarPlace::Slot(Slot { off, width: w }),
            None => ScalarPlace::Mem {
                expr: self.expr(),
                width: w,
            },
        }
    }

    /// Scalar operand (fast path preferred)
    fn scalar_operand(&self, w: Width) -> Operand {
        match self.frame_direct() {
            Some(off) => Operand::Slot(Slot { off, width: w }),
            None => Operand::Mem {
                expr: self.expr(),
                width: w,
            },
        }
    }

    /// Scalar place of one pair half
    fn half_place(&self, half_off: u32, w: Width) -> ScalarPlace {
        match self.frame_direct() {
            Some(off) => ScalarPlace::Slot(Slot {
                off: off + half_off,
                width: w,
            }),
            None => ScalarPlace::Mem {
                expr: self.expr_plus(half_off),
                width: w,
            },
        }
    }

    fn half_operand(&self, half_off: u32, w: Width) -> Operand {
        match self.frame_direct() {
            Some(off) => Operand::Slot(Slot {
                off: off + half_off,
                width: w,
            }),
            None => Operand::Mem {
                expr: self.expr_plus(half_off),
                width: w,
            },
        }
    }
}

/// Generalized operand (the four value classes).
enum LoweredOp<'tcx> {
    Zst,
    Scalar(Operand),
    Pair(Operand, Operand),
    /// Aggregate (memcpy channel): source place + size
    Bytes {
        place: PlaceLow<'tcx>,
        size: u64,
    },
}

/// Call target (shared by finish_call).
enum CallTarget {
    Direct(Callee),
    /// fn-ptr / vtable slot: evaluating the operand yields the real entry address.
    /// The Option is the frozen signature for an extern "C" fn-ptr (on a resolution
    /// miss, call the real native code directly via libffi; virtual dispatch / Rust ABI is always None)
    Indirect(Operand, Option<ir::ForeignSig>),
}

/// Frozen encoding of an enum discriminant (Direct dissolves into a Cast at lowering time; a niche uses the NicheDiscr rvalue).
enum TagInfo {
    /// Single variant / no variant: the discriminant is a constant
    Single { discr: u64 },
    Direct {
        tag_off: u32,
        tag_w: Width,
        tag_signed: bool,
    },
    Niche {
        tag_off: u32,
        tag_w: Width,
        niche_start: u64,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
    /// 128-bit niche (a large niche_start): the tag is 16 bytes and uses u128 arithmetic
    Niche128 {
        tag_off: u32,
        niche_start: u128,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
}

struct LowerCx<'tcx, 'a> {
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    /// The monomorphic instance currently being lowered; used to tag the standard main catch call site precisely.
    instance: Instance<'tcx>,
    /// This function's def_id (used for asm_target_features queries during inline asm register allocation)
    def_id: rustc_hir::def_id::DefId,
    frame: FrameLayout<'tcx>,
    linker: &'a mut Linker<'tcx>,
    /// &Location slot when this function is #[track_caller] (read by forwarding and the caller_location intrinsic)
    caller_loc_off: Option<u32>,
    /// Appended synthesized blocks (landings of diverging calls etc.), finally appended after the MIR blocks
    extra_blocks: Vec<ir::Block>,
    mir_block_count: usize,
}

impl<'tcx> LowerCx<'tcx, '_> {
    fn layout_of(
        &self,
        ty: Ty<'tcx>,
    ) -> Result<rustc_middle::ty::layout::TyAndLayout<'tcx>, Error> {
        frame::layout_of(self.tcx, self.typing_env, ty)
    }

    fn classify(&self, ty: Ty<'tcx>) -> Result<ValKind, Error> {
        Ok(frame::classify(self.tcx, &self.layout_of(ty)?))
    }

    /// Place compilation: projection chain -> address expression (Field/Downcast fold to offsets, Deref/Index stay as steps).
    fn resolve_place(&self, place: &mir::Place<'tcx>) -> Result<PlaceLow<'tcx>, Error> {
        let info = &self.frame.locals[place.local.as_usize()];
        let mut p = PlaceLow {
            base: PlaceBase::Local(info.off),
            steps: Vec::new(),
            ty: info.ty,
            meta: None,
        };
        // Downcast state: when Some(v), the next Field's offset is looked up in the variant layout
        let mut variant: Option<VariantIdx> = None;
        for elem in place.projection {
            match elem {
                mir::ProjectionElem::Field(f, fty) => {
                    let parent_ty = p.ty;
                    let layout = self.layout_of(p.ty)?;
                    let layout = match variant.take() {
                        Some(v) => layout.for_variant(&LayoutCxAt(self.tcx, self.typing_env), v),
                        None => layout,
                    };
                    let offset = layout.fields.offset(f.as_usize()).bytes();
                    let field_layout = self.layout_of(fty)?;
                    // Alignment handling for an unsized field at a non-zero offset:
                    // - tail is slice/str -> the field alignment is **statically known** (the element
                    //   alignment) and rustc's offset is already aligned to it, so use it directly
                    //   (including a struct nested under a slice tail, e.g. Outer{Packet<[u16]>});
                    // - tail is dyn -> the field alignment is **runtime** (vtable) and offset is a
                    //   lower bound that must be rounded up to the vtable alignment (VTableAlignOffset).
                    let needs_vtable_align = field_layout.is_unsized() && offset != 0 && {
                        let tail = self.tcx.struct_tail_for_codegen(fty, self.typing_env);
                        matches!(tail.kind(), ty::Dynamic(..))
                    };
                    if needs_vtable_align {
                        let meta = p.meta.clone().ok_or_else(|| {
                            Error::internal(format!("dyn tail field has no vtable meta (parent={parent_ty}, field={fty})"))
                        })?;
                        let packed = match parent_ty.kind() {
                            ty::Adt(def, _) => def.repr().pack.map(|align| align.bytes()),
                            _ => None,
                        };
                        p.steps.push(PlaceStep::VTableAlignOffset {
                            meta,
                            unaligned: offset,
                            packed,
                        });
                    } else {
                        p.push_offset(offset as i64);
                    }
                    p.ty = fty;
                }
                mir::ProjectionElem::Downcast(_, v) => {
                    variant = Some(v);
                }
                mir::ProjectionElem::Deref => {
                    let pointee = p.ty.builtin_deref(true).ok_or_else(|| {
                        Error::internal(format!("Deref of a non-pointer (ty={})", p.ty))
                    })?;
                    // Entering an unsized pointee: record where the fat pointer meta is read (body + 8)
                    let pointee_layout = self.layout_of(pointee);
                    let unsized_pointee = matches!(&pointee_layout, Ok(l) if l.is_unsized());
                    if unsized_pointee {
                        p.meta = Some(match p.frame_direct() {
                            Some(off) => Operand::Slot(Slot {
                                off: off + 8,
                                width: Width::W64,
                            }),
                            None => Operand::Mem {
                                expr: p.expr_plus(8),
                                width: Width::W64,
                            },
                        });
                    } else {
                        p.meta = None;
                    }
                    p.steps.push(PlaceStep::Deref);
                    p.ty = pointee;
                }
                mir::ProjectionElem::Index(idx_local) => {
                    let elem_ty = elem_of(p.ty).ok_or_else(|| {
                        Error::internal(format!("Index of a non-sequence (ty={})", p.ty))
                    })?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    let idx_info = &self.frame.locals[idx_local.as_usize()];
                    let Some(w) = idx_info.kind.scalar() else {
                        return Err(Error::internal("Index subscript is not a scalar"));
                    };
                    p.steps.push(PlaceStep::IndexScaled {
                        idx: Slot {
                            off: idx_info.off,
                            width: w,
                        },
                        stride,
                    });
                    p.ty = elem_ty;
                    p.meta = None;
                }
                mir::ProjectionElem::ConstantIndex {
                    offset,
                    min_length: _,
                    from_end,
                } => {
                    let elem_ty = elem_of(p.ty).ok_or_else(|| {
                        Error::internal(format!("ConstantIndex of a non-sequence (ty={})", p.ty))
                    })?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    if from_end {
                        if let ty::Array(_, n) = p.ty.kind() {
                            // Array length is known: fold to a constant
                            let n = n
                                .try_to_target_usize(self.tcx)
                                .ok_or(Error::internal("array length is not constant"))?;
                            p.push_offset(((n - offset) * stride) as i64);
                        } else {
                            // slice: addr += len x stride - offset x stride (len = the meta slot)
                            let Some(Operand::Slot(ms)) = p.meta else {
                                return Err(Error::internal(
                                    "ConstantIndex from_end: meta is not a frame slot",
                                ));
                            };
                            p.steps.push(PlaceStep::IndexScaled { idx: ms, stride });
                            p.push_offset(-((offset * stride) as i64));
                        }
                    } else {
                        p.push_offset((offset * stride) as i64);
                    }
                    p.ty = elem_ty;
                    p.meta = None;
                }
                mir::ProjectionElem::Subslice { from, to, from_end } => {
                    // `rest @ ..` slice pattern
                    let elem_ty = elem_of(p.ty).ok_or_else(|| {
                        Error::internal(format!("Subslice of a non-sequence (ty={})", p.ty))
                    })?;
                    let stride = self.layout_of(elem_ty)?.size.bytes();
                    if let ty::Array(_, n) = p.ty.kind() {
                        // Array: fold to a constant -- [from..to] / [from..N-to] still yields a fixed-length array
                        let n = n
                            .try_to_target_usize(self.tcx)
                            .ok_or(Error::internal("array length is not constant"))?;
                        let new_len = if from_end { n - from - to } else { to - from };
                        p.push_offset((from * stride) as i64);
                        p.ty = Ty::new_array(self.tcx, elem_ty, new_len);
                        p.meta = None;
                    } else {
                        // slice (from_end is always true; to counts from the tail): addr += from x stride;
                        // len' = len − (from + to) (subtract a constant from the meta value, Operand::SubImm)
                        if !from_end {
                            return Err(Error::internal(
                                "Subslice of a slice with from_end=false (MIR invariant)",
                            ));
                        }
                        let m = p.meta.clone().ok_or_else(|| {
                            Error::internal(format!(
                                "Subslice of a slice has no meta (ty={})",
                                p.ty
                            ))
                        })?;
                        p.push_offset((from * stride) as i64);
                        p.meta = Some(Operand::SubImm {
                            base: Box::new(m),
                            sub: from + to,
                        });
                    }
                }
                mir::ProjectionElem::OpaqueCast(t) | mir::ProjectionElem::UnwrapUnsafeBinder(t) => {
                    p.ty = t;
                }
                // All current variants are covered; guard against new projections in a future nightly (Trap-stub protocol)
                #[allow(unreachable_patterns)]
                other => return Err(Error::internal(format!("projection {other:?}"))),
            }
        }
        // A trailing Downcast (with no following Field) leaves the place type as the enum
        // and the offset unchanged; whole-value reads/writes use the enum layout.
        Ok(p)
    }

    /// Place -> scalar slot (the caller has established a scalar context).
    fn place_scalar(&self, place: &mir::Place<'tcx>) -> Result<(PlaceLow<'tcx>, Width), Error> {
        let p = self.resolve_place(place)?;
        let ValKind::Scalar(w) = self.classify(p.ty)? else {
            return Err(Error::internal(format!("non-scalar place (ty={})", p.ty)));
        };
        Ok((p, w))
    }

    /// Generalized operand (four-way value classification).
    fn lower_operand(&mut self, op: &mir::Operand<'tcx>) -> Result<LoweredOp<'tcx>, Error> {
        match op {
            mir::Operand::Copy(pl) | mir::Operand::Move(pl) => {
                let p = self.resolve_place(pl)?;
                Ok(match self.classify(p.ty)? {
                    ValKind::Zst => LoweredOp::Zst,
                    ValKind::Scalar(w) => LoweredOp::Scalar(p.scalar_operand(w)),
                    ValKind::Pair((ao, aw), (bo, bw)) => {
                        LoweredOp::Pair(p.half_operand(ao, aw), p.half_operand(bo, bw))
                    }
                    ValKind::Other { size } => LoweredOp::Bytes { place: p, size },
                })
            }
            mir::Operand::Constant(c) => {
                let ty = c.const_.ty();
                let layout = self.layout_of(ty)?;
                if layout.is_zst() {
                    return Ok(LoweredOp::Zst);
                }
                let kind = frame::classify(self.tcx, &layout);
                let val = c
                    .const_
                    .eval(self.tcx, self.typing_env, c.span)
                    .map_err(|e| Error::internal(format!("constant evaluation failed: {e:?}")))?;
                self.lower_const_value(val, ty, kind)
            }
            // Session flag query (UbChecks etc.): folded to a bool immediate at lowering time
            mir::Operand::RuntimeChecks(rc) => {
                use mir::RuntimeChecks as RC;
                let v = match rc {
                    RC::UbChecks => self.tcx.sess.ub_checks(),
                    RC::OverflowChecks => self.tcx.sess.overflow_checks(),
                    RC::ContractChecks => self.tcx.sess.contract_checks(),
                };
                Ok(LoweredOp::Scalar(Operand::Imm {
                    bits: v as u64,
                    width: Width::W8,
                }))
            }
        }
    }

    /// Constant value -> generalized operand (constant pool/statics are materialized into the frozen region and relocated to a real address).
    fn lower_const_value(
        &mut self,
        val: mir::ConstValue,
        ty: Ty<'tcx>,
        kind: ValKind,
    ) -> Result<LoweredOp<'tcx>, Error> {
        use mir::interpret::Scalar as S;
        Ok(match val {
            mir::ConstValue::Scalar(S::Int(si)) => {
                let bits = si.to_bits(si.size());
                match kind {
                    ValKind::Scalar(width) if bits <= u64::MAX as u128 => {
                        LoweredOp::Scalar(Operand::Imm {
                            bits: bits as u64,
                            width,
                        })
                    }
                    // 128-bit integer constant: materialize 16 bytes into the frozen region and use the memcpy channel
                    ValKind::Other { size: 16 } => {
                        let p = self.linker.frozen_alloc_bytes(&bits.to_le_bytes());
                        LoweredOp::Bytes {
                            place: PlaceLow {
                                base: PlaceBase::Static(ir::LinkAddr(p)),
                                steps: Vec::new(),
                                ty,
                                meta: None,
                            },
                            size: 16,
                        }
                    }
                    _ => {
                        return Err(Error::internal(format!(
                            "integer constant class drift (ty={ty})"
                        )));
                    }
                }
            }
            mir::ConstValue::Scalar(S::Ptr(ptr, _)) => {
                // Pointer constant (static reference / fn ptr / vtable): materialize the target -> real address immediate
                let (prov, off) = ptr.prov_and_relative_offset();
                let base = self.linker.ensure_alloc(prov.alloc_id())?;
                // foreign allocation (taking the address of an extern static/fn) -> a GOT slot
                // read operand (the slot is refilled at startup, so the bytecode never bakes in a host address)
                if let Some(op) =
                    self.linker
                        .foreign_const_operand(prov.alloc_id(), base, off.bytes())
                {
                    LoweredOp::Scalar(op)
                } else {
                    LoweredOp::Scalar(Operand::AddrImm(ir::LinkAddr(
                        base.wrapping_add(off.bytes()),
                    )))
                }
            }
            mir::ConstValue::Slice { alloc_id, meta } => {
                // &str/&[u8] literal: fat-pointer pair = (data real address, meta)
                let base = self.linker.ensure_alloc(alloc_id)?;
                LoweredOp::Pair(
                    Operand::AddrImm(ir::LinkAddr(base)),
                    Operand::Imm {
                        bits: meta,
                        width: Width::W64,
                    },
                )
            }
            mir::ConstValue::Indirect { alloc_id, offset } => {
                // Memory constant (Layout/empty-table template etc.): after materialization, access through the frozen-region address by class
                let base = self
                    .linker
                    .ensure_alloc(alloc_id)?
                    .wrapping_add(offset.bytes());
                let sexpr = |o: u64| PlaceExpr {
                    base: PlaceBase::Static(ir::LinkAddr(base.wrapping_add(o))),
                    steps: Box::new([]),
                };
                match kind {
                    ValKind::Zst => LoweredOp::Zst,
                    ValKind::Scalar(w) => LoweredOp::Scalar(Operand::Mem {
                        expr: sexpr(0),
                        width: w,
                    }),
                    ValKind::Pair((ao, aw), (bo, bw)) => LoweredOp::Pair(
                        Operand::Mem {
                            expr: sexpr(ao as u64),
                            width: aw,
                        },
                        Operand::Mem {
                            expr: sexpr(bo as u64),
                            width: bw,
                        },
                    ),
                    ValKind::Other { size } => LoweredOp::Bytes {
                        place: PlaceLow {
                            base: PlaceBase::Static(ir::LinkAddr(base)),
                            steps: Vec::new(),
                            ty,
                            meta: None,
                        },
                        size,
                    },
                }
            }
            mir::ConstValue::ZeroSized => LoweredOp::Zst,
        })
    }

    /// Carve an 8-byte scratch slot at the end of the frame (frame.size is frozen into
    /// the FuncBody only at the end of lower_instance; caller_loc/sret do the same). Used for multi-value intermediates such as size_of_val-dyn.
    fn scratch64(&mut self) -> Slot {
        let off = (self.frame.size + 7) & !7;
        self.frame.size = off + 8;
        self.frame.align = self.frame.align.max(8);
        Slot {
            off,
            width: Width::W64,
        }
    }

    /// Materialize a temporary place at the end of the frame, aligned to the guest
    /// layout. A constant / non-place operand of a volatile store is staged here as a
    /// full bit pattern so the executor can perform an opaque byte volatile write without interpreting padding as an integer.
    fn scratch_place(&mut self, ty: Ty<'tcx>) -> Result<PlaceLow<'tcx>, Error> {
        let layout = self.layout_of(ty)?;
        let size = u32::try_from(layout.size.bytes()).map_err(|_| {
            Error::internal(format!(
                "temporary value {ty} size exceeds a u32 frame offset"
            ))
        })?;
        let align = u32::try_from(layout.align.abi.bytes())
            .map_err(|_| Error::internal(format!("temporary value {ty} alignment exceeds u32")))?
            .max(1);
        let off = self
            .frame
            .size
            .checked_add(align - 1)
            .map(|n| n & !(align - 1))
            .ok_or_else(|| {
                Error::internal(format!("temporary value {ty} frame alignment overflow"))
            })?;
        self.frame.size = off
            .checked_add(size)
            .ok_or_else(|| Error::internal(format!("temporary value {ty} frame size overflow")))?;
        self.frame.align = self.frame.align.max(align);
        Ok(PlaceLow {
            base: PlaceBase::Local(off),
            steps: Vec::new(),
            ty,
            meta: None,
        })
    }

    /// Scalar operand (scalar context; any other class is a context error diagnostic).
    #[track_caller]
    fn lower_operand_scalar(&mut self, op: &mir::Operand<'tcx>) -> Result<Operand, Error> {
        let loc = std::panic::Location::caller();
        match self.lower_operand(op)? {
            LoweredOp::Scalar(o) => Ok(o),
            LoweredOp::Zst => Err(Error::internal("unexpected ZST operand")),
            LoweredOp::Pair(..) => Err(Error::internal(format!(
                "non-scalar operand (pair, ty={}, @{}:{})",
                self.op_ty_str(op),
                loc.file(),
                loc.line()
            ))),
            LoweredOp::Bytes { .. } => Err(Error::internal(format!(
                "non-scalar operand (aggregate, ty={})",
                self.op_ty_str(op)
            ))),
        }
    }

    fn op_ty(&self, op: &mir::Operand<'tcx>) -> Result<Ty<'tcx>, Error> {
        Ok(match op {
            mir::Operand::Copy(p) | mir::Operand::Move(p) => self.resolve_place(p)?.ty,
            mir::Operand::Constant(c) => c.const_.ty(),
            mir::Operand::RuntimeChecks(_) => self.tcx.types.bool,
        })
    }

    fn op_ty_str(&self, op: &mir::Operand<'tcx>) -> String {
        self.op_ty(op)
            .map(|t| t.to_string())
            .unwrap_or_else(|_| "?".into())
    }

    /// span -> &'static Location constant (materialized into the frozen region), shared
    /// by Assert expansion, track_caller argument synthesis and the caller_location intrinsic.
    fn caller_location_imm(&mut self, span: rustc_span::Span) -> Result<Operand, Error> {
        let cv = self.tcx.span_as_caller_location(span);
        let mir::ConstValue::Scalar(mir::interpret::Scalar::Ptr(ptr, _)) = cv else {
            return Err(Error::internal(
                "caller_location constant has an unexpected shape",
            ));
        };
        let (prov, off) = ptr.prov_and_relative_offset();
        let base = self.linker.ensure_alloc(prov.alloc_id())?;
        Ok(Operand::AddrImm(ir::LinkAddr(
            base.wrapping_add(off.bytes()),
        )))
    }

    /// Hidden trailing argument when the callee requires_caller_location: forward from this frame or synthesize at the call site.
    fn caller_loc_arg(
        &mut self,
        callee: &Instance<'tcx>,
        span: rustc_span::Span,
    ) -> Result<Option<Operand>, Error> {
        if !callee.def.requires_caller_location(self.tcx) {
            return Ok(None);
        }
        Ok(Some(match self.caller_loc_off {
            Some(off) => Operand::Slot(Slot {
                off,
                width: Width::W64,
            }), // forward
            None => self.caller_location_imm(span)?,
        }))
    }

    /// 128-bit operand -> place expression (Bytes channel; scalar constants are materialized).
    fn wide_place(&mut self, op: &mir::Operand<'tcx>) -> Result<PlaceExpr, Error> {
        match self.lower_operand(op)? {
            LoweredOp::Bytes { place, .. } => Ok(place.expr()),
            _ => Err(Error::internal("128-bit operand is not a place")),
        }
    }

    /// 128-bit immediate -> a 16-byte place in the frozen region (the 0 of Neg / all-ones of Not).
    fn wide_const(&mut self, v: u128) -> PlaceExpr {
        let base = self.linker.frozen_alloc_bytes(&v.to_le_bytes());
        PlaceExpr {
            base: PlaceBase::Static(ir::LinkAddr(base)),
            steps: Box::new([]),
        }
    }

    /// 128-bit binary op (Bin128): the shift-amount right operand may be a <= 64-bit scalar.
    fn lower_bin128(
        &mut self,
        op: IntBinOp,
        signed: bool,
        a: &mir::Operand<'tcx>,
        b: &mir::Operand<'tcx>,
        dst_p: &PlaceLow<'tcx>,
        with_overflow: bool,
    ) -> Result<Vec<Stmt>, Error> {
        use ir::Bin128Rhs;
        let pa = self.wide_place(a)?;
        let b_ty = self.op_ty(b)?;
        let rhs = if matches!(
            b_ty.kind(),
            ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
        ) {
            Bin128Rhs::Wide(self.wide_place(b)?)
        } else {
            Bin128Rhs::Scalar(self.lower_operand_scalar(b)?)
        };
        if with_overflow {
            // (u128, bool): the bool must be at +16 (hard-coded by the engine) -- layout verification
            let l = self.layout_of(dst_p.ty)?;
            if l.fields.offset(1).bytes() != 16 {
                return Err(Error::internal("128-bit overflow pair layout drift"));
            }
        }
        Ok(vec![Stmt::Bin128 {
            op,
            signed,
            a: pa,
            b: rhs,
            dst: dst_p.expr(),
            with_overflow,
        }])
    }

    /// Freeze the enum tag encoding (shared by Discriminant reads and SetDiscriminant writes).
    fn tag_info(&self, ty: Ty<'tcx>) -> Result<TagInfo, Error> {
        let layout = self.layout_of(ty)?;
        Ok(match &layout.variants {
            Variants::Empty => TagInfo::Single { discr: 0 }, // unreachable (reading it is guest UB)
            Variants::Single { index } => {
                let discr = ty
                    .discriminant_for_variant(self.tcx, *index)
                    .map(|d| d.val)
                    .unwrap_or(index.as_u32() as u128);
                TagInfo::Single {
                    discr: u128_to_u64(discr)?,
                }
            }
            Variants::Multiple {
                tag,
                tag_encoding,
                tag_field,
                ..
            } => {
                let dl = self.tcx.data_layout();
                let tag_off = layout.fields.offset(tag_field.as_usize()).bytes() as u32;
                let tag_bytes = tag.size(dl).bytes();
                // 128-bit tag (repr(u128) / large niche): narrow to the low 64 bits, which is
                // correct for values <= u64 on little-endian. Value-range checks guarantee safety
                // (compile-time discr in SetDiscr/SwitchInt goes through u128_to_u64; the high
                // bits of a Niche's niche_start are checked below) -- never truncate silently.
                let tag_w = Width::from_bytes(tag_bytes)
                    .or_else(|| (tag_bytes == 16).then_some(Width::W64))
                    .ok_or_else(|| Error::internal(format!("tag width {tag_bytes} bytes")))?;
                let tag_signed = matches!(tag.primitive(), rustc_abi::Primitive::Int(_, true));
                match tag_encoding {
                    TagEncoding::Direct => TagInfo::Direct {
                        tag_off,
                        tag_w,
                        tag_signed,
                    },
                    TagEncoding::Niche {
                        untagged_variant,
                        niche_variants,
                        niche_start,
                    } => {
                        let vstart = niche_variants.start.as_u32() as u64;
                        let vlen = (niche_variants.last.as_u32() - niche_variants.start.as_u32())
                            as u64
                            + 1;
                        let untagged = untagged_variant.as_u32() as u64;
                        // A 128-bit niche **always** uses the u128 arithmetic path (cg_ssa operand.rs
                        // reads the discriminant as rel = tag − niche_start with full-width wrapping and
                        // then compares with ule). Testing only the high bits of niche_start is not
                        // enough: NonZero<u128> has niche_start=0, so a truncated W64 read would
                        // misclassify a legal large value whose low 64 bits are 0 (2^64/2^66/2^127...)
                        // as the niche.
                        if tag_bytes == 16 {
                            return Ok(TagInfo::Niche128 {
                                tag_off,
                                niche_start: *niche_start,
                                variants_start: vstart,
                                variants_len: vlen,
                                untagged,
                            });
                        }
                        TagInfo::Niche {
                            tag_off,
                            tag_w,
                            niche_start: u128_to_u64(*niche_start & tag_w.mask() as u128)?,
                            variants_start: vstart,
                            variants_len: vlen,
                            untagged,
                        }
                    }
                }
            }
        })
    }

    /// SetDiscriminant: dissolves into a constant write to the tag slot at lowering time (an untagged niche is a no-op).
    fn set_discr_stmts(
        &self,
        dst_p: &PlaceLow<'tcx>,
        enum_ty: Ty<'tcx>,
        vidx: VariantIdx,
    ) -> Result<Vec<Stmt>, Error> {
        Ok(match self.tag_info(enum_ty)? {
            TagInfo::Single { .. } => vec![],
            TagInfo::Direct { tag_off, tag_w, .. } => {
                let discr = enum_ty
                    .discriminant_for_variant(self.tcx, vidx)
                    .map(|d| d.val)
                    .ok_or(Error::internal("Direct tag has no discr"))?;
                // Value-range guard before narrowing a 128-bit tag (prevents silent truncation; a discr always fits when tag_w <= W64)
                if tag_w == Width::W64 && discr > u64::MAX as u128 {
                    return Err(Error::internal(format!(
                        "128-bit discriminant {discr} exceeds 64 bits"
                    )));
                }
                let bits = (discr as u64) & tag_w.mask();
                vec![Stmt::Assign {
                    dst: dst_p.half_place(tag_off, tag_w),
                    rv: Rvalue::Use(Operand::Imm { bits, width: tag_w }),
                }]
            }
            TagInfo::Niche {
                tag_off,
                tag_w,
                niche_start,
                variants_start,
                untagged,
                ..
            } => {
                let vi = vidx.as_u32() as u64;
                if vi == untagged {
                    vec![]
                } else {
                    let bits =
                        vi.wrapping_sub(variants_start).wrapping_add(niche_start) & tag_w.mask();
                    vec![Stmt::Assign {
                        dst: dst_p.half_place(tag_off, tag_w),
                        rv: Rvalue::Use(Operand::Imm { bits, width: tag_w }),
                    }]
                }
            }
            // 128-bit niche write: tag_val = (vi − start + niche_start) as u128, written to the two 8-byte halves
            TagInfo::Niche128 {
                tag_off,
                niche_start,
                variants_start,
                untagged,
                ..
            } => {
                let vi = vidx.as_u32() as u64;
                if vi == untagged {
                    vec![]
                } else {
                    let tag_val =
                        (vi.wrapping_sub(variants_start) as u128).wrapping_add(niche_start);
                    let w = Width::W64;
                    vec![
                        Stmt::Assign {
                            dst: dst_p.half_place(tag_off, w),
                            rv: Rvalue::Use(Operand::Imm {
                                bits: tag_val as u64,
                                width: w,
                            }),
                        },
                        Stmt::Assign {
                            dst: dst_p.half_place(tag_off + 8, w),
                            rv: Rvalue::Use(Operand::Imm {
                                bits: (tag_val >> 64) as u64,
                                width: w,
                            }),
                        },
                    ]
                }
            }
        })
    }

    /// Write a MIR operand at byte offset off of the dst place (Aggregate field placement).
    fn write_at(
        &mut self,
        dst_p: &PlaceLow<'tcx>,
        off: u32,
        op: &mir::Operand<'tcx>,
    ) -> Result<Vec<Stmt>, Error> {
        Ok(match self.lower_operand(op)? {
            LoweredOp::Zst => vec![],
            LoweredOp::Scalar(o) => {
                let w = o.width();
                vec![Stmt::Assign {
                    dst: dst_p.half_place(off, w),
                    rv: Rvalue::Use(o),
                }]
            }
            LoweredOp::Pair(l, h) => {
                let ValKind::Pair((ao, aw), (bo, bw)) = self.classify(self.op_ty(op)?)? else {
                    return Err(Error::internal("pair operand class drift"));
                };
                vec![
                    Stmt::Assign {
                        dst: dst_p.half_place(off + ao, aw),
                        rv: Rvalue::Use(l),
                    },
                    Stmt::Assign {
                        dst: dst_p.half_place(off + bo, bw),
                        rv: Rvalue::Use(h),
                    },
                ]
            }
            LoweredOp::Bytes { place, size } => {
                vec![Stmt::Copy {
                    dst: dst_p.expr_plus(off),
                    src: place.expr(),
                    size: size as u32,
                }]
            }
        })
    }

    /// Write a generalized src operand into the dst place (a same-shape bit move, shared by Use/Transmute/bit-copy casts).
    fn assign_lowered(
        &self,
        dst: &PlaceLow<'tcx>,
        dst_kind: ValKind,
        src: LoweredOp<'tcx>,
    ) -> Result<Vec<Stmt>, Error> {
        Ok(match (dst_kind, src) {
            (ValKind::Zst, _) => vec![Stmt::Nop],
            (ValKind::Scalar(w), LoweredOp::Scalar(o)) => {
                vec![Stmt::Assign {
                    dst: dst.scalar_place(w),
                    rv: Rvalue::Use(o),
                }]
            }
            (ValKind::Pair((ao, aw), (bo, bw)), LoweredOp::Pair(l, h)) => vec![
                Stmt::Assign {
                    dst: dst.half_place(ao, aw),
                    rv: Rvalue::Use(l),
                },
                Stmt::Assign {
                    dst: dst.half_place(bo, bw),
                    rv: Rvalue::Use(h),
                },
            ],
            (ValKind::Other { size }, LoweredOp::Bytes { place, size: ssz }) => {
                debug_assert_eq!(size, ssz);
                vec![Stmt::Copy {
                    dst: dst.expr(),
                    src: place.expr(),
                    size: size as u32,
                }]
            }
            // Cross-class in a bit-copy context (Transmute pair<->aggregate etc.): a place src goes through a byte copy
            (ValKind::Pair(..) | ValKind::Scalar(_), LoweredOp::Bytes { place, size }) => {
                vec![Stmt::Copy {
                    dst: dst.expr(),
                    src: place.expr(),
                    size: size as u32,
                }]
            }
            // Scalar -> same-size small aggregate (transmute u32 -> [u8;4] etc.): write raw at the width
            (ValKind::Other { size }, LoweredOp::Scalar(o)) if o.width().bytes() as u64 == size => {
                vec![Stmt::Assign {
                    dst: ScalarPlace::Mem {
                        expr: dst.expr(),
                        width: o.width(),
                    },
                    rv: Rvalue::Use(o),
                }]
            }
            (ValKind::Other { .. }, LoweredOp::Pair(..)) => {
                // Writing a pair value into a place viewed as an aggregate writes two half-width
                // scalars (offset 0 / after alignment). This shows up in Transmute; the half
                // offsets cannot be derived from the src layout, and a compact 0/width-aligned
                // approximation is unreliable, so diagnose.
                return Err(Error::internal("Transmute pair -> aggregate"));
            }
            (k, s) => {
                return Err(Error::internal(format!(
                    "assignment class mismatch (dst={k:?}, src={})",
                    match s {
                        LoweredOp::Zst => "zst",
                        LoweredOp::Scalar(_) => "scalar",
                        LoweredOp::Pair(..) => "pair",
                        LoweredOp::Bytes { .. } => "bytes",
                    }
                )));
            }
        })
    }

    /// Assign statement -> ir statements (possibly several).
    fn lower_assign(
        &mut self,
        dst: &mir::Place<'tcx>,
        rv: &mir::Rvalue<'tcx>,
    ) -> Result<Vec<Stmt>, Error> {
        let dst_p = self.resolve_place(dst)?;
        let dst_kind = self.classify(dst_p.ty)?;

        // *WithOverflow: write the (value, flag) scalar pair
        if let mir::Rvalue::BinaryOp(binop, box (a, b)) = rv {
            let ovf = match binop {
                mir::BinOp::AddWithOverflow => Some(OvfOp::Add),
                mir::BinOp::SubWithOverflow => Some(OvfOp::Sub),
                mir::BinOp::MulWithOverflow => Some(OvfOp::Mul),
                _ => None,
            };
            if let Some(op) = ovf {
                let a_ty = self.op_ty(a)?;
                // 128-bit WithOverflow: a (u128, bool) aggregate with the flag at +16
                if matches!(
                    a_ty.kind(),
                    ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                ) {
                    let bop = match op {
                        OvfOp::Add => IntBinOp::Add,
                        OvfOp::Sub => IntBinOp::Sub,
                        OvfOp::Mul => IntBinOp::Mul,
                    };
                    return self.lower_bin128(bop, frame::ty_signed(a_ty), a, b, &dst_p, true);
                }
                let ValKind::Pair((vo, vw), (fo, fw)) = dst_kind else {
                    return Err(Error::internal("overflow arithmetic target is not a pair"));
                };
                let a_ty = self.op_ty(a)?;
                return Ok(vec![Stmt::AssignOverflow {
                    op,
                    signed: frame::ty_signed(a_ty),
                    a: self.lower_operand_scalar(a)?,
                    b: self.lower_operand_scalar(b)?,
                    dst_val: dst_p.half_place(vo, vw),
                    dst_flag: dst_p.half_place(fo, fw),
                }]);
            }
        }

        if dst_kind.is_zst() {
            return Ok(vec![Stmt::Nop]); // no rvalue in this set has side effects
        }

        match rv {
            // WithRetag: Tree Borrows retagging is checker semantics that the fast machine
            // ignores. CopyForDeref = Use; Reborrow = a same-shape bit copy (a user ADT reborrow has the same layout).
            mir::Rvalue::Use(op, _retag) => {
                let src = self.lower_operand(op)?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
            mir::Rvalue::CopyForDeref(pl) => {
                let src = self.lower_operand(&mir::Operand::Copy(*pl))?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
            mir::Rvalue::Reborrow(_, _, pl) => {
                let src = self.lower_operand(&mir::Operand::Copy(*pl))?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
            mir::Rvalue::Ref(_, _, pl) | mir::Rvalue::RawPtr(_, pl) => {
                let p = self.resolve_place(pl)?;
                let pointee_layout = self.layout_of(p.ty)?;
                if pointee_layout.is_unsized() {
                    // Fat pointer: dst pair = (place address, meta)
                    let ValKind::Pair((ao, aw), (bo, bw)) = dst_kind else {
                        return Err(Error::internal("unsized Ref target is not a pair"));
                    };
                    // &[T;N] projected to [T] does not occur; meta comes from the deref chain.
                    let meta = p.meta.clone().ok_or_else(|| {
                        Error::internal(format!("unsized Ref has no meta source (ty={})", p.ty))
                    })?;
                    Ok(vec![
                        Stmt::Assign {
                            dst: dst_p.half_place(ao, aw),
                            rv: Rvalue::Ref(p.expr()),
                        },
                        Stmt::Assign {
                            dst: dst_p.half_place(bo, bw),
                            rv: Rvalue::Use(meta),
                        },
                    ])
                } else {
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err(Error::internal("Ref target is not a scalar"));
                    };
                    Ok(vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::Ref(p.expr()),
                    }])
                }
            }
            mir::Rvalue::BinaryOp(binop, box (a, b)) => {
                // Pointer arithmetic
                if let mir::BinOp::Offset = binop {
                    let ptr_ty = self.op_ty(a)?;
                    let pointee = ptr_ty.builtin_deref(true).ok_or_else(|| {
                        Error::internal(format!("Offset of a non-pointer (ty={ptr_ty})"))
                    })?;
                    let stride = self.layout_of(pointee)?.size.bytes();
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err(Error::internal("Offset target is not a scalar"));
                    };
                    return Ok(vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: Rvalue::PtrOffset {
                            ptr: self.lower_operand_scalar(a)?,
                            count: self.lower_operand_scalar(b)?,
                            stride,
                        },
                    }]);
                }
                let a_ty = self.op_ty(a)?;
                use mir::BinOp::*;
                // Fat-pointer comparison (Eq/Ne of *const [T]/dyn compares both halves, which is
                // the raw-pointer == semantics): eq = (data==)&(meta==); ne = (data!=)|(meta!=).
                // Both half widths come from the operands (data pointer + usize/vtable meta, all W64).
                if matches!(binop, Eq | Ne) && matches!(self.classify(a_ty)?, ValKind::Pair(..)) {
                    let LoweredOp::Pair(al, ah) = self.lower_operand(a)? else {
                        return Err(Error::internal(
                            "fat-pointer comparison left side is not a pair",
                        ));
                    };
                    let LoweredOp::Pair(bl, bh) = self.lower_operand(b)? else {
                        return Err(Error::internal(
                            "fat-pointer comparison right side is not a pair",
                        ));
                    };
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err(Error::internal(
                            "fat-pointer comparison target is not a scalar",
                        ));
                    };
                    let (cc, comb) = if matches!(binop, Eq) {
                        (IntCc::Eq, IntBinOp::BitAnd)
                    } else {
                        (IntCc::Ne, IntBinOp::BitOr)
                    };
                    let s8 = Slot {
                        off: self.scratch64().off,
                        width: w,
                    };
                    return Ok(vec![
                        Stmt::Assign {
                            dst: ScalarPlace::Slot(s8),
                            rv: Rvalue::IntCmp {
                                cc,
                                signed: false,
                                a: al,
                                b: bl,
                            },
                        },
                        Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::IntCmp {
                                cc,
                                signed: false,
                                a: ah,
                                b: bh,
                            },
                        },
                        Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::IntBin {
                                op: comb,
                                signed: false,
                                a: dst_p.scalar_operand(w),
                                b: Operand::Slot(s8),
                            },
                        },
                    ]);
                }
                // 128-bit integer: comparison uses Cmp128; arithmetic/bit/shift use Bin128 (computed directly on host u128)
                if matches!(
                    a_ty.kind(),
                    ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                ) {
                    let signed = frame::ty_signed(a_ty);
                    let cc = match binop {
                        Eq => Some(IntCc::Eq),
                        Ne => Some(IntCc::Ne),
                        Lt => Some(IntCc::Lt),
                        Le => Some(IntCc::Le),
                        Gt => Some(IntCc::Gt),
                        Ge => Some(IntCc::Ge),
                        _ => None,
                    };
                    if let Some(cc) = cc {
                        let pa = self.wide_place(a)?;
                        let pb = self.wide_place(b)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err(Error::internal(
                                "128-bit comparison target is not a scalar",
                            ));
                        };
                        return Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::Cmp128 {
                                cc,
                                signed,
                                a: pa,
                                b: pb,
                            },
                        }]);
                    }
                    // Three-way comparison (three_way_compare -> Ord::cmp): dst(i8 Ordering) = (a>b) − (a<b)
                    if matches!(binop, Cmp) {
                        let pa = self.wide_place(a)?;
                        let pb = self.wide_place(b)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err(Error::internal("128-bit Cmp target is not a scalar"));
                        };
                        let gt = Slot {
                            width: Width::W8,
                            ..self.scratch64()
                        };
                        let lt = Slot {
                            width: Width::W8,
                            ..self.scratch64()
                        };
                        return Ok(vec![
                            Stmt::Assign {
                                dst: ScalarPlace::Slot(gt),
                                rv: Rvalue::Cmp128 {
                                    cc: IntCc::Gt,
                                    signed,
                                    a: pa.clone(),
                                    b: pb.clone(),
                                },
                            },
                            Stmt::Assign {
                                dst: ScalarPlace::Slot(lt),
                                rv: Rvalue::Cmp128 {
                                    cc: IntCc::Lt,
                                    signed,
                                    a: pa,
                                    b: pb,
                                },
                            },
                            Stmt::Assign {
                                dst: dst_p.scalar_place(w),
                                rv: Rvalue::IntBin {
                                    op: IntBinOp::Sub,
                                    signed: false,
                                    a: Operand::Slot(gt),
                                    b: Operand::Slot(lt),
                                },
                            },
                        ]);
                    }
                    let bop = match binop {
                        Add | AddUnchecked => IntBinOp::Add,
                        Sub | SubUnchecked => IntBinOp::Sub,
                        Mul | MulUnchecked => IntBinOp::Mul,
                        Div => IntBinOp::Div,
                        Rem => IntBinOp::Rem,
                        BitAnd => IntBinOp::BitAnd,
                        BitOr => IntBinOp::BitOr,
                        BitXor => IntBinOp::BitXor,
                        Shl | ShlUnchecked => IntBinOp::Shl,
                        Shr | ShrUnchecked => IntBinOp::Shr,
                        other => return Err(Error::internal(format!("128-bit BinOp {other:?}"))),
                    };
                    return self.lower_bin128(bop, signed, a, b, &dst_p, false);
                }
                if a_ty.is_floating_point() {
                    use ir::FloatOp as F;
                    // f128: 16-byte wide channel -- place operands, comparison produces a scalar bool
                    if matches!(a_ty.kind(), ty::Float(ty::FloatTy::F128)) {
                        let pa = self.wide_place(a)?;
                        let pb = self.wide_place(b)?;
                        let fop = match binop {
                            Add | AddUnchecked => Some(F::Add),
                            Sub | SubUnchecked => Some(F::Sub),
                            Mul | MulUnchecked => Some(F::Mul),
                            Div => Some(F::Div),
                            Rem => Some(F::Rem),
                            _ => None,
                        };
                        if let Some(op) = fop {
                            return Ok(vec![Stmt::F128Bin {
                                op,
                                a: pa,
                                b: pb,
                                dst: dst_p.expr(),
                            }]);
                        }
                        let cc = match binop {
                            Eq => IntCc::Eq,
                            Ne => IntCc::Ne,
                            Lt => IntCc::Lt,
                            Le => IntCc::Le,
                            Gt => IntCc::Gt,
                            Ge => IntCc::Ge,
                            other => {
                                return Err(Error::internal(format!("f128 BinOp {other:?}")));
                            }
                        };
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err(Error::internal("f128 comparison target is not a scalar"));
                        };
                        return Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::F128Cmp { cc, a: pa, b: pb },
                        }]);
                    }
                    let fw = float_w(a_ty)?;
                    let ao = self.lower_operand_scalar(a)?;
                    let bo = self.lower_operand_scalar(b)?;
                    let fbin = |op| Rvalue::FloatBin {
                        op,
                        fw,
                        a: ao.clone(),
                        b: bo.clone(),
                    };
                    let fcmp = |cc| Rvalue::FloatCmp {
                        cc,
                        fw,
                        a: ao.clone(),
                        b: bo.clone(),
                    };
                    let rvalue = match binop {
                        Add | AddUnchecked => fbin(F::Add),
                        Sub | SubUnchecked => fbin(F::Sub),
                        Mul | MulUnchecked => fbin(F::Mul),
                        Div => fbin(F::Div),
                        Rem => fbin(F::Rem),
                        Eq => fcmp(IntCc::Eq),
                        Ne => fcmp(IntCc::Ne),
                        Lt => fcmp(IntCc::Lt),
                        Le => fcmp(IntCc::Le),
                        Gt => fcmp(IntCc::Gt),
                        Ge => fcmp(IntCc::Ge),
                        other => return Err(Error::internal(format!("float BinOp {other:?}"))),
                    };
                    let ValKind::Scalar(w) = dst_kind else {
                        return Err(Error::internal("float arithmetic target is not a scalar"));
                    };
                    return Ok(vec![Stmt::Assign {
                        dst: dst_p.scalar_place(w),
                        rv: rvalue,
                    }]);
                }
                let signed = frame::ty_signed(a_ty);
                let ao = self.lower_operand_scalar(a)?;
                let bo = self.lower_operand_scalar(b)?;
                let int = |op| Rvalue::IntBin {
                    op,
                    signed,
                    a: ao.clone(),
                    b: bo.clone(),
                };
                let cmp = |cc| Rvalue::IntCmp {
                    cc,
                    signed,
                    a: ao.clone(),
                    b: bo.clone(),
                };
                let rvalue = match binop {
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
                    Cmp => Rvalue::IntCmp3 {
                        signed,
                        a: ao.clone(),
                        b: bo.clone(),
                    },
                    other => return Err(Error::internal(format!("BinOp {other:?}"))),
                };
                let ValKind::Scalar(w) = dst_kind else {
                    return Err(Error::internal("integer arithmetic target is not a scalar"));
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: rvalue,
                }])
            }
            mir::Rvalue::UnaryOp(unop, a) => {
                let a_ty = self.op_ty(a)?;
                match unop {
                    mir::UnOp::PtrMetadata => {
                        // Fat pointer -> take the meta half; a thin pointer's meta is a ZST (we only get here when dst_kind is not zst)
                        match self.lower_operand(a)? {
                            LoweredOp::Pair(_, h) => {
                                let ValKind::Scalar(w) = dst_kind else {
                                    return Err(Error::internal(
                                        "PtrMetadata target is not a scalar",
                                    ));
                                };
                                Ok(vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(h),
                                }])
                            }
                            _ => Err(Error::internal(format!(
                                "PtrMetadata of a non-fat-pointer (ty={a_ty})"
                            ))),
                        }
                    }
                    mir::UnOp::Not if a_ty.is_bool() => {
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err(Error::internal("Not target is not a scalar"));
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::NotBool(ao),
                        }])
                    }
                    mir::UnOp::Not => {
                        // 128-bit integer: x XOR all-ones (frozen-region constant edge, Bin128 channel)
                        if matches!(
                            a_ty.kind(),
                            ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                        ) {
                            let ones = self.wide_const(!0u128);
                            return Ok(vec![Stmt::Bin128 {
                                op: IntBinOp::BitXor,
                                signed: false,
                                a: self.wide_place(a)?,
                                b: ir::Bin128Rhs::Wide(ones),
                                dst: dst_p.expr(),
                                with_overflow: false,
                            }]);
                        }
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err(Error::internal("Not target is not a scalar"));
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv: Rvalue::NotBits(ao),
                        }])
                    }
                    mir::UnOp::Neg => {
                        if matches!(a_ty.kind(), ty::Float(ty::FloatTy::F128)) {
                            let pa = self.wide_place(a)?;
                            return Ok(vec![Stmt::F128Un {
                                op: ir::F128UnOp::Neg,
                                a: pa,
                                dst: dst_p.expr(),
                            }]);
                        }
                        // 128-bit integer: 0 − x (frozen-region zero constant; two's complement gives the same result either way)
                        if matches!(
                            a_ty.kind(),
                            ty::Int(ty::IntTy::I128) | ty::Uint(ty::UintTy::U128)
                        ) {
                            let zero = self.wide_const(0u128);
                            return Ok(vec![Stmt::Bin128 {
                                op: IntBinOp::Sub,
                                signed: false,
                                a: zero,
                                b: ir::Bin128Rhs::Wide(self.wide_place(a)?),
                                dst: dst_p.expr(),
                                with_overflow: false,
                            }]);
                        }
                        let ao = self.lower_operand_scalar(a)?;
                        let ValKind::Scalar(w) = dst_kind else {
                            return Err(Error::internal("Neg target is not a scalar"));
                        };
                        let rv = if a_ty.is_floating_point() {
                            Rvalue::FloatNeg {
                                fw: float_w(a_ty)?,
                                a: ao,
                            }
                        } else {
                            Rvalue::Neg(ao)
                        };
                        Ok(vec![Stmt::Assign {
                            dst: dst_p.scalar_place(w),
                            rv,
                        }])
                    }
                }
            }
            mir::Rvalue::Cast(kind, a, to_ty) => {
                self.lower_cast(&dst_p, dst_kind, *kind, a, *to_ty)
            }
            mir::Rvalue::Repeat(op, n) => {
                let count = n
                    .try_to_target_usize(self.tcx)
                    .ok_or(Error::internal("Repeat length is not constant"))?;
                match self.lower_operand(op)? {
                    LoweredOp::Scalar(val) => {
                        let elem_size = val.width().bytes();
                        Ok(vec![Stmt::RepeatScalar {
                            dst: dst_p.expr(),
                            val,
                            count,
                            elem_size,
                        }])
                    }
                    LoweredOp::Zst => Ok(vec![Stmt::Nop]),
                    // Aggregate element (pair/bytes): write one copy into dst[0]; the engine then
                    // replicates dst[0]'s bytes over the remaining count-1 copies.
                    _ if count == 0 => Ok(vec![Stmt::Nop]),
                    _ => {
                        let elem_size = self.layout_of(self.op_ty(op)?)?.size.bytes();
                        let mut v = self.write_at(&dst_p, 0, op)?;
                        v.push(Stmt::RepeatBytes {
                            first: dst_p.expr(),
                            count,
                            elem_size,
                        });
                        Ok(v)
                    }
                }
            }
            mir::Rvalue::Discriminant(pl) => {
                let p = self.resolve_place(pl)?;
                let ValKind::Scalar(dw) = dst_kind else {
                    return Err(Error::internal("Discriminant target is not a scalar"));
                };
                let rv = match self.tag_info(p.ty)? {
                    TagInfo::Single { discr } => Rvalue::Use(Operand::Imm {
                        bits: discr & dw.mask(),
                        width: dw,
                    }),
                    TagInfo::Direct {
                        tag_off,
                        tag_w,
                        tag_signed,
                    } => Rvalue::Cast {
                        from: (tag_w, tag_signed),
                        to: dw,
                        a: p.half_operand(tag_off, tag_w),
                    },
                    TagInfo::Niche {
                        tag_off,
                        tag_w,
                        niche_start,
                        variants_start,
                        variants_len,
                        untagged,
                    } => Rvalue::NicheDiscr {
                        tag: p.half_operand(tag_off, tag_w),
                        niche_start,
                        variants_start,
                        variants_len,
                        untagged,
                    },
                    // 128-bit niche: a separate stmt (read the 16-byte tag, u128 arithmetic)
                    TagInfo::Niche128 {
                        tag_off,
                        niche_start,
                        variants_start,
                        variants_len,
                        untagged,
                    } => {
                        return Ok(vec![Stmt::NicheDiscr128 {
                            tag: p.expr_plus(tag_off),
                            niche_start,
                            variants_start,
                            variants_len,
                            untagged,
                            dst: dst_p.scalar_place(dw),
                        }]);
                    }
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(dw),
                    rv,
                }])
            }
            mir::Rvalue::Aggregate(box kind, operands) => {
                use mir::AggregateKind as AK;
                match kind {
                    AK::Array(elem_ty) => {
                        let stride = self.layout_of(*elem_ty)?.size.bytes() as u32;
                        let mut stmts = Vec::new();
                        for (i, op) in operands.iter().enumerate() {
                            stmts.extend(self.write_at(&dst_p, i as u32 * stride, op)?);
                        }
                        Ok(stmts)
                    }
                    AK::RawPtr(..) => {
                        // (data, meta) -> fat pointer; a ZST meta -> thin pointer
                        let mut ops = operands.iter();
                        let (data, meta) = (
                            ops.next()
                                .ok_or(Error::internal("RawPtr is missing data"))?,
                            ops.next()
                                .ok_or(Error::internal("RawPtr is missing meta"))?,
                        );
                        match dst_kind {
                            ValKind::Scalar(w) => {
                                let LoweredOp::Scalar(d) = self.lower_operand(data)? else {
                                    return Err(Error::internal("RawPtr data is not a scalar"));
                                };
                                Ok(vec![Stmt::Assign {
                                    dst: dst_p.scalar_place(w),
                                    rv: Rvalue::Use(d),
                                }])
                            }
                            ValKind::Pair((ao, aw), (bo, bw)) => {
                                let LoweredOp::Scalar(d) = self.lower_operand(data)? else {
                                    return Err(Error::internal("RawPtr data is not a scalar"));
                                };
                                let LoweredOp::Scalar(m) = self.lower_operand(meta)? else {
                                    return Err(Error::internal("RawPtr meta is not a scalar"));
                                };
                                Ok(vec![
                                    Stmt::Assign {
                                        dst: dst_p.half_place(ao, aw),
                                        rv: Rvalue::Use(d),
                                    },
                                    Stmt::Assign {
                                        dst: dst_p.half_place(bo, bw),
                                        rv: Rvalue::Use(m),
                                    },
                                ])
                            }
                            _ => Err(Error::internal("RawPtr target has an unexpected class")),
                        }
                    }
                    AK::Adt(..)
                    | AK::Tuple
                    | AK::Closure(..)
                    | AK::Coroutine(..)
                    | AK::CoroutineClosure(..) => {
                        // Isomorphic to cg_ssa: **only Adt does a variant downcast** -- Coroutine/Closure/
                        // Tuple operands (upvars) land in top-level fields (a coroutine's variant fields
                        // are suspension-point saved locals, not upvars!); then write the discriminant.
                        let (vidx, active_field, use_variant) = match kind {
                            AK::Adt(_, v, _, _, af) => (*v, *af, true),
                            _ => (VariantIdx::ZERO, None, false),
                        };
                        let layout = self.layout_of(dst_p.ty)?;
                        let field_layout = if use_variant
                            && !matches!(layout.variants, Variants::Single { .. } | Variants::Empty)
                        {
                            layout.for_variant(&LayoutCxAt(self.tcx, self.typing_env), vidx)
                        } else {
                            layout
                        };
                        let mut stmts = Vec::new();
                        for (i, op) in operands.iter().enumerate() {
                            let fi = active_field.map(|f| f.as_usize()).unwrap_or(i);
                            let off = field_layout.fields.offset(fi).bytes() as u32;
                            stmts.extend(self.write_at(&dst_p, off, op)?);
                        }
                        // Discriminant: write it for every enum, and for the coroutine initial variant (Unresumed=0)
                        if dst_p.ty.is_enum() || dst_p.ty.is_coroutine() {
                            stmts.extend(self.set_discr_stmts(&dst_p, dst_p.ty, vidx)?);
                        }
                        Ok(stmts)
                    }
                }
            }
            mir::Rvalue::ThreadLocalRef(def_id) => {
                // Per-thread instance: a dense TlsId, lazily materialized into Ctx.tls at
                // execution time (heap allocation + a copy of the frozen template). Accounting note: the dtor does not run.
                let id = self.linker.tls_id(*def_id)?;
                let ValKind::Scalar(w) = dst_kind else {
                    return Err(Error::internal("ThreadLocalRef target is not a scalar"));
                };
                Ok(vec![Stmt::Assign {
                    dst: dst_p.scalar_place(w),
                    rv: Rvalue::TlsRef(id),
                }])
            }
            mir::Rvalue::WrapUnsafeBinder(op, _) => {
                let src = self.lower_operand(op)?;
                self.assign_lowered(&dst_p, dst_kind, src)
            }
        }
    }
}

/// Statement -> ir statements (an empty vec means no operation).
fn lower_stmt<'tcx>(
    cx: &mut LowerCx<'tcx, '_>,
    stmt: &mir::Statement<'tcx>,
) -> Result<Vec<Stmt>, Error> {
    use mir::StatementKind as SK;
    match &stmt.kind {
        SK::Assign(box (place, rv)) => cx.lower_assign(place, rv),
        SK::StorageLive(_)
        | SK::StorageDead(_)
        | SK::Nop
        | SK::PlaceMention(_)
        | SK::ConstEvalCounter
        | SK::Coverage(_) => Ok(vec![]),
        SK::Intrinsic(box mir::NonDivergingIntrinsic::Assume(_)) => Ok(vec![]),
        SK::Intrinsic(box mir::NonDivergingIntrinsic::CopyNonOverlapping(cp)) => {
            let ptr_ty = cx.op_ty(&cp.src)?;
            let pointee = ptr_ty.builtin_deref(true).ok_or(Error::internal(
                "CopyNonOverlapping source is not a pointer",
            ))?;
            let elem_size = cx.layout_of(pointee)?.size.bytes();
            Ok(vec![Stmt::MemCopy {
                src: cx.lower_operand_scalar(&cp.src)?,
                dst: cx.lower_operand_scalar(&cp.dst)?,
                count: cx.lower_operand_scalar(&cp.count)?,
                elem_size,
                overlap: false,
            }])
        }
        SK::SetDiscriminant {
            place,
            variant_index,
        } => {
            let p = cx.resolve_place(place)?;
            let ty = p.ty;
            cx.set_discr_stmts(&p, ty, *variant_index)
        }
        other => Err(Error::internal(format!("statement {other:?}"))),
    }
}

/// Lower one instance. Err = the whole function Traps (layout failure etc.);
/// an unsupported statement Traps only that block (finer-grained).
pub(crate) fn lower_instance<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    instance: Instance<'tcx>,
    linker: &mut Linker<'tcx>,
) -> Result<ir::FuncBody, Error> {
    // An intrinsic has no ordinary MIR (the fallback-body form is re-collected as an Item by the Linker via new_raw)
    if let InstanceKind::Intrinsic(..) = instance.def {
        return Err(Error::internal("intrinsic instance (engine builtin)"));
    }
    // A foreign item has no MIR (instance_mir panics the rustc query): taking a fn-ptr
    // must go through Linker::foreign_fn_entry_addr. Reaching this queue means an
    // upstream registration path missed a case; Err becomes a Trap body instead of taking down the rustc process.
    if tcx.is_foreign_item(instance.def_id()) {
        return Err(Error::internal(
            "foreign instance has no MIR to lower (fn-ptr address-taking must go through foreign_fn_entry_addr)",
        ));
    }
    let body_ref: &Body<'tcx> = tcx.instance_mir(instance.def);
    // Monomorphize the whole body at once (one clone + instantiate; Body: TypeFoldable)
    let body: Body<'tcx> = instance.instantiate_mir_and_normalize_erasing_regions(
        tcx,
        typing_env,
        EarlyBinder::bind(tcx, body_ref.clone()),
    );
    let mut frame = frame::freeze(tcx, typing_env, &body)?;

    // Return channel (_0, ABI v2): an aggregate uses indirect + an sret slot (8 bytes appended at the end of the frame)
    let ret = {
        let ret_info = &frame.locals[0];
        match ret_info.kind {
            ValKind::Zst => RetAbi::Zst,
            ValKind::Scalar(w) => RetAbi::Scalar(Slot {
                off: ret_info.off,
                width: w,
            }),
            ValKind::Pair((ao, aw), (bo, bw)) => RetAbi::Pair(
                Slot {
                    off: ret_info.off + ao,
                    width: aw,
                },
                Slot {
                    off: ret_info.off + bo,
                    width: bw,
                },
            ),
            ValKind::Other { size } => {
                let sret_off = (frame.size + 7) & !7;
                let ret_off = ret_info.off;
                frame.size = sret_off + 8;
                frame.align = frame.align.max(8);
                RetAbi::Indirect {
                    ret_off,
                    size: size as u32,
                    sret_off,
                }
            }
        }
    };

    // Parameter placement (_1..=_argc, ABI v2).
    //
    // The physical contract of the rust-call ABI is a field-flattened tuple (isomorphic
    // to cg_ssa/Miri): the closure body's MIR parameters are already split (env, a, b)
    // and the call site passes tuple operands field by field (untuple_rust_call_arg).
    // The shim body (ClosureOnce/VTable) marks the tuple local with spread_arg; here
    // it expands into separate parameters (placement = the tuple local's internal
    // offset, so the prologue write reassembles it).
    let mut params = Vec::new();
    for local in body.args_iter() {
        let info = &frame.locals[local.as_usize()];
        if body.spread_arg == Some(local) {
            let rustc_middle::ty::TyKind::Tuple(fields) = info.ty.kind() else {
                return Err(Error::internal(format!(
                    "spread_arg is not a tuple ({})",
                    info.ty
                )));
            };
            let layout = frame::layout_of(tcx, typing_env, info.ty)?;
            for (i, fty) in fields.iter().enumerate() {
                let foff = info.off + layout.fields.offset(i).bytes() as u32;
                let fl = frame::layout_of(tcx, typing_env, fty)?;
                params.push(match frame::classify(tcx, &fl) {
                    ValKind::Zst => ParamAbi::Zst,
                    ValKind::Scalar(w) => ParamAbi::Scalar(Slot {
                        off: foff,
                        width: w,
                    }),
                    ValKind::Pair((ao, aw), (bo, bw)) => ParamAbi::Pair(
                        Slot {
                            off: foff + ao,
                            width: aw,
                        },
                        Slot {
                            off: foff + bo,
                            width: bw,
                        },
                    ),
                    ValKind::Other { size } => ParamAbi::Indirect {
                        off: foff,
                        size: size as u32,
                    },
                });
            }
            continue;
        }
        params.push(match info.kind {
            ValKind::Zst => ParamAbi::Zst,
            ValKind::Scalar(w) => ParamAbi::Scalar(Slot {
                off: info.off,
                width: w,
            }),
            ValKind::Pair((ao, aw), (bo, bw)) => ParamAbi::Pair(
                Slot {
                    off: info.off + ao,
                    width: aw,
                },
                Slot {
                    off: info.off + bo,
                    width: bw,
                },
            ),
            ValKind::Other { size } => ParamAbi::Indirect {
                off: info.off,
                size: size as u32,
            },
        });
    }

    // #[track_caller]: hidden trailing &Location argument slot at the end of the frame (isomorphic to the cg_ssa ABI)
    let caller_loc_off = if instance.def.requires_caller_location(tcx) {
        let off = (frame.size + 7) & !7;
        frame.size = off + 8;
        frame.align = frame.align.max(8);
        Some(off)
    } else {
        None
    };

    let name = tcx.symbol_name(instance).name.to_owned();
    let mir_block_count = body.basic_blocks.len();
    let mut cx = LowerCx {
        tcx,
        typing_env,
        instance,
        def_id: instance.def_id(),
        frame,
        linker,
        caller_loc_off,
        extra_blocks: Vec::new(),
        mir_block_count,
    };

    // The panic surface of the rustc API is not controllable (layout corner cases
    // etc.), so the Trap-stub protocol catches it as a placeholder instead of aborting the whole lowering (the diagnostic carries the panic message).
    fn catch_lower<T>(
        f: impl FnOnce() -> Result<T, Error> + std::panic::UnwindSafe,
    ) -> Result<T, Error> {
        match std::panic::catch_unwind(f) {
            Ok(r) => r,
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("?");
                Err(Error::internal(format!("lower panic: {msg}")))
            }
        }
    }

    let mut blocks = Vec::with_capacity(mir_block_count);
    for bb_data in body.basic_blocks.iter() {
        let mut stmts = Vec::new();
        for stmt in &bb_data.statements {
            let r = catch_lower(std::panic::AssertUnwindSafe(|| lower_stmt(&mut cx, stmt)));
            match r {
                Ok(mut s) => stmts.append(&mut s),
                Err(reason) => {
                    // Statement-level Trap: execution stops with a diagnostic here; the terminator is still lowered (to preserve the Call edge)
                    stmts.push(Stmt::Trap(reason.to_string().into_boxed_str()));
                    break;
                }
            }
        }
        let term = match catch_lower(std::panic::AssertUnwindSafe(|| {
            cx.lower_terminator(bb_data.terminator())
        })) {
            Ok((mut extra, t)) => {
                stmts.append(&mut extra);
                t
            }
            Err(reason) => Terminator::Trap(reason.to_string().into_boxed_str()),
        };
        blocks.push(ir::Block { stmts, term });
    }
    blocks.append(&mut cx.extra_blocks);

    Ok(ir::FuncBody {
        frame_size: cx.frame.size,
        frame_align: cx.frame.align,
        ret,
        params,
        caller_loc_off,
        blocks,
        name: name.into_boxed_str(),
    })
}
