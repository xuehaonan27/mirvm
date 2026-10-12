//! Structural generic MIR export with opaque local storage and concrete ABI binding. Unsupported
//! shapes return to the ordinary lowerer before any linker state has changed.

mod template;

use template::{Binding, CallAbi, Layout, LocalBinding, Template, reserve};

use super::frame::{self, ValKind};
use super::linker::Linker;
use crate::vm::ir::*;
use rustc_data_structures::fx::FxHashMap;
use rustc_hir::def_id::DefId;
use rustc_middle::{
    mir,
    ty::{self, EarlyBinder, Instance, InstanceKind, Ty, TyCtxt, TypingEnv},
};

/// This cache belongs to one compiler session and semantic profile. Shims are excluded, so the
/// definition is the complete template key. Negative entries avoid repeatedly probing a body.
#[derive(Default)]
pub(super) struct Templates<'tcx>(FxHashMap<DefId, Option<Export<'tcx>>>);

impl<'tcx> Templates<'tcx> {
    pub(super) fn lower(
        &mut self,
        tcx: TyCtxt<'tcx>,
        env: TypingEnv<'tcx>,
        instance: Instance<'tcx>,
    ) -> Option<Bound<'tcx>> {
        if !crate::options::generic_templates()
            || !matches!(instance.def, InstanceKind::Item(_))
            || !instance.args.iter().any(|arg| {
                matches!(
                    arg.kind(),
                    ty::GenericArgKind::Type(_) | ty::GenericArgKind::Const(_)
                )
            })
            || tcx.is_foreign_item(instance.def_id())
            || instance.def.requires_caller_location(tcx)
        {
            return None;
        }
        let exported = self
            .0
            .entry(instance.def_id())
            .or_insert_with(|| {
                let template = Exporter::export(tcx, instance);
                if crate::diag::prints(crate::diag::Severity::Debug) {
                    crate::diag_debug!(
                        Lower,
                        "generic template {}: {}",
                        if template.is_some() {
                            "exported"
                        } else {
                            "fallback"
                        },
                        tcx.def_path_str(instance.def_id())
                    );
                }
                template
            })
            .as_ref()?;
        let template = &exported.template;
        let layouts = exported
            .layouts
            .iter()
            .map(|&ty| {
                let ty = instance.instantiate_mir_and_normalize_erasing_regions(
                    tcx,
                    env,
                    EarlyBinder::bind(tcx, ty),
                );
                fixed_layout(tcx, env, ty)
            })
            .collect::<Option<Vec<_>>>()?;
        let values = template
            .bindings
            .iter()
            .map(|binding| match *binding {
                Binding::Size(index) => Some(u64::from(layouts[index as usize].size)),
                Binding::Const(index, width) => {
                    let value = instance
                        .args
                        .get(index as usize)?
                        .as_const()?
                        .try_to_leaf()?;
                    if value.size().bytes() != u64::from(width.bytes()) {
                        return None;
                    }
                    u64::try_from(value.to_bits(value.size())).ok()
                }
            })
            .collect::<Option<Vec<_>>>()?;
        // Resolve every witness before allocating any FuncId. Refusing one concrete binding
        // leaves the eager lowerer's worklist and native/constant state untouched.
        let callees = exported
            .calls
            .iter()
            .map(|call| {
                let fn_ty = instance.instantiate_mir_and_normalize_erasing_regions(
                    tcx,
                    env,
                    EarlyBinder::bind(tcx, call.ty),
                );
                let ty::FnDef(def, args) = *fn_ty.kind() else {
                    return None;
                };
                let mut callee = Instance::try_resolve(tcx, env, def, args).ok()??;
                if let InstanceKind::Intrinsic(def) = callee.def {
                    if tcx.intrinsic(def)?.must_be_overridden {
                        return None;
                    }
                    callee = Instance::new_raw(def, callee.args);
                }
                if tcx.is_foreign_item(callee.def_id())
                    || (callee.def.requires_caller_location(tcx) != call.location.is_some())
                    || !matches!(
                        callee.def,
                        InstanceKind::Item(_)
                            | InstanceKind::Shim(
                                ty::ShimKind::DropGlue(..) | ty::ShimKind::Clone(..)
                            )
                    )
                {
                    return None;
                }
                Some((callee, call.location))
            })
            .collect::<Option<Vec<_>>>()?;
        let body = template.bind(values, &layouts)?;
        if crate::diag::prints(crate::diag::Severity::Debug) {
            crate::diag_debug!(
                Lower,
                "generic template bound: {}",
                tcx.symbol_name(instance).name
            );
        }
        Some(Bound { body, callees })
    }
}

/// Compiler-owned layout and call recipes are separate from the owned instruction template.
struct Export<'tcx> {
    template: Template,
    calls: Vec<Call<'tcx>>,
    layouts: Vec<Ty<'tcx>>,
}

struct Call<'tcx> {
    ty: Ty<'tcx>,
    location: Option<rustc_span::Span>,
}

pub(super) struct Bound<'tcx> {
    body: FuncBody,
    callees: Vec<(Instance<'tcx>, Option<rustc_span::Span>)>,
}

impl<'tcx> Bound<'tcx> {
    pub(super) fn link(
        mut self,
        linker: &mut Linker<'tcx>,
        caller: Instance<'tcx>,
    ) -> Result<FuncBody, super::Error> {
        let ids: Vec<_> = self
            .callees
            .iter()
            .map(|&(inst, _)| linker.func_id(inst))
            .collect();
        for term in &mut self.body.shared.as_mut().unwrap().calls {
            if let Terminator::Call {
                callee, args, role, ..
            } = term
            {
                if let Some(span) = self.callees[*callee as usize].1 {
                    args.push(linker.caller_location_imm(span)?);
                }
                *callee = ids[*callee as usize];
                if linker.main_catch_site.is_some_and(|site| {
                    site.boundary_caller == caller && site.boundary_callee == *callee
                }) {
                    *role = CallRole::MainPanicBoundary;
                }
            }
        }
        self.body.name = linker.image_symbol(caller).into();
        Ok(self.body)
    }
}

struct ProjectedPlace<'tcx> {
    expr: PlaceExpr,
    ty: Ty<'tcx>,
    meta: Option<Operand>,
}

struct Local<'tcx> {
    off: u32,
    ty: Ty<'tcx>,
    kind: Option<ValKind>,
}

struct Exporter<'tcx> {
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    mir: &'tcx mir::Body<'tcx>,
    locals: Vec<Local<'tcx>>,
    frame_align: u32,
    local_bindings: Vec<LocalBinding>,
    call_abis: Vec<CallAbi>,
    temporary: Slot,
    dictionary: u32,
    bindings: Vec<Binding>,
    calls: Vec<Call<'tcx>>,
    layouts: Vec<Ty<'tcx>>,
    pending: Vec<Stmt>,
    next_scratch: u32,
    frame_size: u32,
    extra_blocks: Vec<Block>,
}

impl<'tcx> Exporter<'tcx> {
    fn export(tcx: TyCtxt<'tcx>, instance: Instance<'tcx>) -> Option<Export<'tcx>> {
        if tcx.data_layout.pointer_size().bytes() != 8 {
            return None;
        }
        let mir = tcx.instance_mir(instance.def);
        if mir.spread_arg.is_some() {
            return None;
        }
        let env = TypingEnv::post_analysis(tcx, instance.def_id());
        let mut size = 0;
        let mut frame_align = 8;
        let mut locals = Vec::new();
        let mut layouts = Vec::new();
        let mut local_bindings = Vec::new();
        let mut ret = RetAbi::Zst;
        let mut params = vec![ParamAbi::Zst; mir.arg_count];
        for (index, decl) in mir.local_decls.iter().enumerate() {
            let layout = fixed_layout(tcx, env, decl.ty);
            let off;
            if let Some(layout) = &layout {
                off = reserve(&mut size, layout.size, layout.align)?;
                frame_align = frame_align.max(layout.align);
                if index == 0 {
                    let sret = if matches!(layout.kind, ValKind::Other { .. }) {
                        reserve(&mut size, 8, 8)?
                    } else {
                        0
                    };
                    ret = layout.ret(off, sret);
                } else if index <= mir.arg_count {
                    params[index - 1] = layout.param(off);
                }
            } else {
                off = reserve(&mut size, 8, 8)?;
                local_bindings.push(LocalBinding {
                    local: index,
                    pointer: Slot {
                        off,
                        width: Width::W64,
                    },
                    layout: layout_index(&mut layouts, decl.ty)?,
                });
            }
            locals.push(Local {
                off,
                ty: decl.ty,
                kind: layout.map(|l| l.kind),
            });
        }
        let temporary = Slot {
            off: reserve(&mut size, 8, 8)?,
            width: Width::W64,
        };
        let dictionary = reserve(&mut size, 8, 8)?;
        params.push(ParamAbi::Scalar(Slot {
            off: dictionary,
            width: Width::W64,
        }));
        let mut cx = Self {
            tcx,
            env,
            mir,
            locals,
            frame_align,
            local_bindings,
            call_abis: Vec::new(),
            temporary,
            dictionary,
            bindings: Vec::new(),
            calls: Vec::new(),
            layouts,
            pending: Vec::new(),
            next_scratch: size,
            frame_size: size,
            extra_blocks: Vec::new(),
        };
        let mut blocks: Vec<_> = mir
            .basic_blocks
            .iter()
            .map(|block| cx.block(block))
            .collect::<Option<_>>()?;
        blocks.append(&mut cx.extra_blocks);
        Some(Export {
            template: Template {
                body: FuncBody {
                    shared: None,
                    frame_size: cx.frame_size,
                    frame_align: cx.frame_align,
                    ret,
                    params,
                    caller_loc_off: None,
                    blocks: Vec::new(),
                    name: tcx.def_path_str(instance.def_id()).into(),
                },
                instructions: std::sync::Arc::new(GenericTemplate {
                    dictionary,
                    binding_count: u32::try_from(cx.bindings.len()).ok()?,
                    blocks,
                }),
                bindings: cx.bindings,
                locals: cx.local_bindings,
                calls: cx.call_abis,
            },
            calls: cx.calls,
            layouts: cx.layouts,
        })
    }

    fn place(&mut self, place: mir::Place<'tcx>) -> Option<ProjectedPlace<'tcx>> {
        let local = &self.locals[place.local.as_usize()];
        let mut expr = PlaceExpr {
            base: PlaceBase::Local(local.off),
            steps: if local.kind.is_none() {
                vec![PlaceStep::Deref].into()
            } else {
                Box::new([])
            },
        };
        let mut ty = local.ty;
        let mut meta = None;
        for projection in place.projection {
            match projection {
                mir::ProjectionElem::Deref => {
                    let pointee = ty.builtin_deref(true)?;
                    meta = if matches!(pointee.kind(), ty::Slice(_) | ty::Str) {
                        let layout = frame::layout_of(self.tcx, self.env, ty).ok()?;
                        let ValKind::Pair(_, (off, width)) = frame::classify(self.tcx, &layout)
                        else {
                            return None;
                        };
                        Some(read(scalar_place(
                            offset(expr.clone(), u64::from(off))?,
                            width,
                        )))
                    } else {
                        None
                    };
                    ty = pointee;
                    expr.steps = expr
                        .steps
                        .into_vec()
                        .into_iter()
                        .chain([PlaceStep::Deref])
                        .collect();
                }
                mir::ProjectionElem::Field(field, field_ty) => {
                    let layout = frame::layout_of(self.tcx, self.env, ty).ok()?;
                    // Dynamic tails require an alignment binding as well as the field offset.
                    if frame::layout_of(self.tcx, self.env, field_ty)
                        .ok()?
                        .is_unsized()
                    {
                        return None;
                    }
                    expr = offset(expr, layout.fields.offset(field.as_usize()).bytes())?;
                    ty = field_ty;
                    meta = None;
                }
                mir::ProjectionElem::Index(index) => {
                    let (ty::Slice(elem) | ty::Array(elem, _)) = *ty.kind() else {
                        return None;
                    };
                    let info = &self.locals[index.as_usize()];
                    let idx = Slot {
                        off: info.off,
                        width: info.kind?.scalar()?,
                    };
                    expr = self.index(expr, Operand::Slot(idx), elem)?;
                    ty = elem;
                    meta = None;
                }
                mir::ProjectionElem::ConstantIndex {
                    offset, from_end, ..
                } => {
                    let (ty::Slice(elem) | ty::Array(elem, _)) = *ty.kind() else {
                        return None;
                    };
                    let index = if from_end {
                        let len = match *ty.kind() {
                            ty::Array(_, len) => self.array_len(len)?,
                            _ => meta.take()?,
                        };
                        self.subtract(len, offset)?
                    } else {
                        Operand::Imm {
                            bits: offset,
                            width: Width::W64,
                        }
                    };
                    expr = self.index(expr, index, elem)?;
                    ty = elem;
                    meta = None;
                }
                mir::ProjectionElem::Subslice {
                    from,
                    to,
                    from_end: true,
                } => {
                    let ty::Slice(elem) = *ty.kind() else {
                        return None;
                    };
                    expr = self.index(
                        expr,
                        Operand::Imm {
                            bits: from,
                            width: Width::W64,
                        },
                        elem,
                    )?;
                    meta = Some(self.subtract(meta.take()?, from.checked_add(to)?)?);
                }
                _ => return None,
            }
        }
        Some(ProjectedPlace { expr, ty, meta })
    }

    fn scalar_place(&mut self, place: mir::Place<'tcx>) -> Option<ScalarPlace> {
        let ProjectedPlace { expr, ty, .. } = self.place(place)?;
        Some(scalar_place(expr, self.width(ty)?))
    }

    fn pair(&mut self, place: mir::Place<'tcx>) -> Option<(ScalarPlace, ScalarPlace)> {
        let ProjectedPlace { expr, ty, .. } = self.place(place)?;
        let layout = frame::layout_of(self.tcx, self.env, ty).ok()?;
        let ValKind::Pair((a, aw), (b, bw)) = frame::classify(self.tcx, &layout) else {
            return None;
        };
        Some((
            scalar_place(offset(expr.clone(), u64::from(a))?, aw),
            scalar_place(offset(expr, u64::from(b))?, bw),
        ))
    }

    fn pair_operand(&mut self, operand: &mir::Operand<'tcx>) -> Option<(Operand, Operand)> {
        let (mir::Operand::Copy(place) | mir::Operand::Move(place)) = operand else {
            return None;
        };
        let (a, b) = self.pair(*place)?;
        Some((read(a), read(b)))
    }

    fn width(&self, ty: Ty<'tcx>) -> Option<Width> {
        frame::scalar_width(&frame::layout_of(self.tcx, self.env, ty).ok()?)
    }

    fn scratch64(&mut self) -> Option<Slot> {
        let slot = Slot {
            off: self.next_scratch,
            width: Width::W64,
        };
        self.next_scratch = self.next_scratch.checked_add(8)?;
        self.frame_size = self.frame_size.max(self.next_scratch);
        Some(slot)
    }

    fn index(&mut self, expr: PlaceExpr, index: Operand, elem: Ty<'tcx>) -> Option<PlaceExpr> {
        if matches!(index, Operand::Imm { bits: 0, .. }) {
            return Some(expr);
        }
        let stride = self.size(elem)?;
        let step = match (index, stride) {
            (Operand::Imm { bits: index, .. }, Operand::Imm { bits: stride, .. }) => {
                return offset(expr, index.checked_mul(stride)?);
            }
            (Operand::Slot(idx), Operand::Imm { bits: stride, .. }) => {
                PlaceStep::IndexScaled { idx, stride }
            }
            (index, stride) => {
                let bytes = self.scratch64()?;
                self.pending.push(Stmt::Assign {
                    dst: ScalarPlace::Slot(bytes),
                    rv: Rvalue::IntBin {
                        op: IntBinOp::Mul,
                        signed: false,
                        a: index,
                        b: stride,
                    },
                });
                PlaceStep::IndexScaled {
                    idx: bytes,
                    stride: 1,
                }
            }
        };
        let mut steps = expr.steps.into_vec();
        steps.push(step);
        Some(PlaceExpr {
            steps: steps.into(),
            ..expr
        })
    }

    fn subtract(&mut self, value: Operand, amount: u64) -> Option<Operand> {
        if amount == 0 {
            return Some(value);
        }
        if let Operand::Imm { bits, .. } = value {
            return Some(Operand::Imm {
                bits: bits.checked_sub(amount)?,
                width: Width::W64,
            });
        }
        let result = self.scratch64()?;
        self.pending.push(Stmt::Assign {
            dst: ScalarPlace::Slot(result),
            rv: Rvalue::IntBin {
                op: IntBinOp::Sub,
                signed: false,
                a: value,
                b: Operand::Imm {
                    bits: amount,
                    width: Width::W64,
                },
            },
        });
        Some(Operand::Slot(result))
    }

    fn array_len(&mut self, len: ty::Const<'tcx>) -> Option<Operand> {
        match len.kind() {
            ty::ConstKind::Param(param) => {
                self.binding(Binding::Const(param.index, Width::W64), Width::W64)
            }
            _ => Some(Operand::Imm {
                bits: len.try_to_target_usize(self.tcx)?,
                width: Width::W64,
            }),
        }
    }

    fn binding(&mut self, binding: Binding, width: Width) -> Option<Operand> {
        let index = match self.bindings.iter().position(|b| *b == binding) {
            Some(index) => index,
            None => {
                self.bindings.push(binding);
                self.bindings.len() - 1
            }
        };
        Some(Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Local(self.dictionary),
                steps: vec![
                    PlaceStep::Deref,
                    PlaceStep::Offset(i32::try_from(index.checked_mul(8)?).ok()?),
                ]
                .into(),
            },
            width,
        })
    }

    fn size(&mut self, ty: Ty<'tcx>) -> Option<Operand> {
        if let Ok(layout) = frame::layout_of(self.tcx, self.env, ty) {
            return (!layout.is_unsized()).then(|| Operand::Imm {
                bits: layout.size.bytes(),
                width: Width::W64,
            });
        }
        let index = layout_index(&mut self.layouts, ty)?;
        self.binding(Binding::Size(index), Width::W64)
    }

    fn call(&mut self, call: Call<'tcx>, abi: CallAbi) -> Option<u32> {
        let index = u32::try_from(self.calls.len()).ok()?;
        self.calls.push(call);
        self.call_abis.push(abi);
        Some(index)
    }

    fn operand_place(&mut self, operand: &mir::Operand<'tcx>) -> Option<ProjectedPlace<'tcx>> {
        let (mir::Operand::Copy(place) | mir::Operand::Move(place)) = operand else {
            return None;
        };
        self.place(*place)
    }

    fn operand(&mut self, operand: &mir::Operand<'tcx>) -> Option<Operand> {
        match operand {
            mir::Operand::Copy(place) | mir::Operand::Move(place) => {
                Some(read(self.scalar_place(*place)?))
            }
            mir::Operand::Constant(c) => {
                let width = self.width(c.const_.ty())?;
                if let mir::Const::Ty(_, value) = c.const_
                    && let ty::ConstKind::Param(param) = value.kind()
                {
                    return self.binding(Binding::Const(param.index, width), width);
                }
                let value = c.const_.try_to_scalar_int()?;
                Some(Operand::Imm {
                    bits: u64::try_from(value.to_bits(value.size())).ok()?,
                    width,
                })
            }
            _ => None,
        }
    }

    fn rvalue(&mut self, rv: &mir::Rvalue<'tcx>) -> Option<Rvalue> {
        Some(match rv {
            mir::Rvalue::Use(a, _) => Rvalue::Use(self.operand(a)?),
            mir::Rvalue::UnaryOp(mir::UnOp::PtrMetadata, a) => Rvalue::Use(self.pair_operand(a)?.1),
            mir::Rvalue::Ref(_, _, place) | mir::Rvalue::RawPtr(_, place) => {
                // A fat reference also needs metadata; the scalar destination check excludes it.
                Rvalue::Ref(self.place(*place)?.expr)
            }
            mir::Rvalue::Cast(kind, a, to) => {
                let from = a.ty(&self.mir.local_decls, self.tcx);
                match kind {
                    mir::CastKind::IntToInt => Rvalue::Cast {
                        from: (self.width(from)?, frame::ty_signed(from)),
                        to: self.width(*to)?,
                        a: self.operand(a)?,
                    },
                    mir::CastKind::PtrToPtr if self.width(*to)? == Width::W64 => Rvalue::Use(
                        self.operand(a)
                            .or_else(|| self.pair_operand(a).map(|v| v.0))?,
                    ),
                    _ => return None,
                }
            }
            mir::Rvalue::BinaryOp(op, box (a, b)) => {
                let ty = a.ty(&self.mir.local_decls, self.tcx);
                if !matches!(ty.kind(), ty::Int(_) | ty::Uint(_) | ty::Bool | ty::Char) {
                    return None;
                }
                let signed = frame::ty_signed(ty);
                let (a, b) = (self.operand(a)?, self.operand(b)?);
                use mir::BinOp as B;
                match op {
                    B::Add | B::Sub | B::Mul | B::BitAnd | B::BitOr | B::BitXor => Rvalue::IntBin {
                        op: match op {
                            B::Add => IntBinOp::Add,
                            B::Sub => IntBinOp::Sub,
                            B::Mul => IntBinOp::Mul,
                            B::BitAnd => IntBinOp::BitAnd,
                            B::BitOr => IntBinOp::BitOr,
                            _ => IntBinOp::BitXor,
                        },
                        signed,
                        a,
                        b,
                    },
                    B::Eq | B::Ne | B::Lt | B::Le | B::Gt | B::Ge => Rvalue::IntCmp {
                        cc: match op {
                            B::Eq => IntCc::Eq,
                            B::Ne => IntCc::Ne,
                            B::Lt => IntCc::Lt,
                            B::Le => IntCc::Le,
                            B::Gt => IntCc::Gt,
                            _ => IntCc::Ge,
                        },
                        signed,
                        a,
                        b,
                    },
                    _ => return None,
                }
            }
            _ => return None,
        })
    }

    fn scale(&mut self, count: &mir::Operand<'tcx>, pointee: Ty<'tcx>) -> Option<Stmt> {
        Some(Stmt::Assign {
            dst: ScalarPlace::Slot(self.temporary),
            rv: Rvalue::IntBin {
                op: IntBinOp::Mul,
                signed: false,
                a: self.operand(count)?,
                b: self.size(pointee)?,
            },
        })
    }

    fn assign(&mut self, place: mir::Place<'tcx>, rv: &mir::Rvalue<'tcx>) -> Option<Vec<Stmt>> {
        let ty = place.ty(&self.mir.local_decls, self.tcx).ty;
        let layout = fixed_layout(self.tcx, self.env, ty);
        if layout
            .as_ref()
            .is_none_or(|l| matches!(l.kind, ValKind::Other { .. }))
        {
            let mir::Rvalue::Use(op, _) = rv else {
                return None;
            };
            return Some(vec![Stmt::MemCopy {
                dst: Operand::AddrOf(self.place(place)?.expr),
                src: Operand::AddrOf(self.operand_place(op)?.expr),
                count: self.size(ty)?,
                elem_size: 1,
                overlap: true,
            }]);
        }
        let value = |dst, a| Stmt::Assign {
            dst,
            rv: Rvalue::Use(a),
        };
        match layout?.kind {
            ValKind::Zst => match rv {
                mir::Rvalue::Use(_, _) | mir::Rvalue::Aggregate(_, _) => Some(Vec::new()),
                _ => None,
            },
            ValKind::Scalar(_) => Some(vec![Stmt::Assign {
                dst: self.scalar_place(place)?,
                rv: self.rvalue(rv)?,
            }]),
            ValKind::Pair(..) => {
                let (a, b) = self.pair(place)?;
                let (lo, hi) = match rv {
                    mir::Rvalue::Use(op, _) => self.pair_operand(op)?,
                    mir::Rvalue::Ref(_, _, place) | mir::Rvalue::RawPtr(_, place) => {
                        let place = self.place(*place)?;
                        (Operand::AddrOf(place.expr), place.meta?)
                    }
                    mir::Rvalue::Aggregate(box mir::AggregateKind::RawPtr(..), ops)
                        if ops.len() == 2 =>
                    {
                        let mut ops = ops.iter();
                        (self.operand(ops.next()?)?, self.operand(ops.next()?)?)
                    }
                    _ => return None,
                };
                Some(vec![value(a, lo), value(b, hi)])
            }
            _ => None,
        }
    }

    fn block(&mut self, block: &mir::BasicBlockData<'tcx>) -> Option<Block> {
        let mut stmts = Vec::new();
        for stmt in &block.statements {
            // Projection temporaries live through one MIR statement, including all its operands.
            self.next_scratch = self.dictionary + 8;
            let mut lowered = Vec::new();
            match &stmt.kind {
                mir::StatementKind::Assign(box (place, rv)) => {
                    lowered.extend(self.assign(*place, rv)?)
                }
                mir::StatementKind::Intrinsic(
                    box mir::NonDivergingIntrinsic::CopyNonOverlapping(copy),
                ) => {
                    let ty::RawPtr(pointee, _) =
                        *copy.src.ty(&self.mir.local_decls, self.tcx).kind()
                    else {
                        return None;
                    };
                    lowered.push(self.scale(&copy.count, pointee)?);
                    lowered.push(Stmt::MemCopy {
                        dst: self.operand(&copy.dst)?,
                        src: self.operand(&copy.src)?,
                        count: Operand::Slot(self.temporary),
                        elem_size: 1,
                        overlap: false,
                    });
                }
                mir::StatementKind::StorageLive(_)
                | mir::StatementKind::StorageDead(_)
                | mir::StatementKind::Intrinsic(box mir::NonDivergingIntrinsic::Assume(_))
                | mir::StatementKind::Nop => {}
                _ => return None,
            }
            stmts.append(&mut self.pending);
            stmts.append(&mut lowered);
        }
        self.next_scratch = self.dictionary + 8;
        let mut lowered = Vec::new();
        let term = match &block.terminator().kind {
            mir::TerminatorKind::Goto { target } => Terminator::Goto(target.as_u32()),
            mir::TerminatorKind::Return => Terminator::Return,
            mir::TerminatorKind::Unreachable => Terminator::Unreachable,
            mir::TerminatorKind::UnwindResume => Terminator::Resume,
            mir::TerminatorKind::UnwindTerminate(_) => Terminator::TerminateAbort,
            mir::TerminatorKind::Drop {
                place,
                target,
                unwind,
                ..
            } => {
                let place = self.place(*place)?;
                if !place.ty.is_sized(self.tcx, self.env) {
                    return None;
                }
                let def = self.tcx.require_lang_item(
                    rustc_hir::LangItem::DropGlue,
                    block.terminator().source_info.span,
                );
                let callee = self.call(
                    Call {
                        ty: Ty::new_fn_def(self.tcx, def, [place.ty]),
                        location: None,
                    },
                    CallAbi::default(),
                )?;
                Terminator::Call {
                    callee,
                    args: vec![Operand::AddrOf(place.expr)],
                    ret: RetDest::Ignore,
                    target: target.as_u32(),
                    unwind: unwind_action(*unwind),
                    role: CallRole::Normal,
                }
            }
            mir::TerminatorKind::SwitchInt { discr, targets } => Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(self.operand(discr)?),
                targets: targets
                    .iter()
                    .map(|(value, target)| (value, target.as_u32()))
                    .collect(),
                otherwise: targets.otherwise().as_u32(),
            },
            mir::TerminatorKind::Assert {
                cond,
                expected,
                msg,
                target,
                unwind,
            } => {
                let mir::AssertKind::BoundsCheck { len, index } = &**msg else {
                    return None;
                };
                let span = block.terminator().source_info.span;
                let def = self
                    .tcx
                    .require_lang_item(rustc_hir::LangItem::PanicBoundsCheck, span);
                let args = vec![self.operand(index)?, self.operand(len)?];
                let unwind = unwind_action(*unwind);
                let callee = self.call(
                    Call {
                        ty: Ty::new_fn_def(self.tcx, def, ty::GenericArgs::empty()),
                        location: Some(span),
                    },
                    CallAbi::default(),
                )?;
                let unreach =
                    u32::try_from(self.mir.basic_blocks.len() + self.extra_blocks.len()).ok()?;
                let panic = unreach.checked_add(1)?;
                self.extra_blocks.push(Block {
                    stmts: Vec::new(),
                    term: Terminator::Unreachable,
                });
                self.extra_blocks.push(Block {
                    stmts: Vec::new(),
                    term: Terminator::Call {
                        callee,
                        args,
                        ret: RetDest::Ignore,
                        target: unreach,
                        unwind,
                        role: CallRole::Normal,
                    },
                });
                Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(self.operand(cond)?),
                    targets: vec![(*expected as u128, target.as_u32())],
                    otherwise: panic,
                }
            }
            mir::TerminatorKind::Call {
                func,
                args,
                destination,
                target: Some(target),
                unwind,
                ..
            } => {
                let fn_ty = func.ty(&self.mir.local_decls, self.tcx);
                let ty::FnDef(def, _) = *fn_ty.kind() else {
                    return None;
                };
                if !self
                    .tcx
                    .intrinsic(def)
                    .is_some_and(|i| i.name.as_str() == "arith_offset")
                {
                    if fn_ty.fn_sig(self.tcx).skip_binder().abi() == rustc_abi::ExternAbi::RustCall
                    {
                        return None;
                    }
                    let mut arguments = Vec::new();
                    let mut abi = CallAbi::default();
                    for arg in args {
                        let ty = arg.node.ty(&self.mir.local_decls, self.tcx);
                        match fixed_layout(self.tcx, self.env, ty).map(|l| l.kind) {
                            Some(ValKind::Zst) => {}
                            Some(ValKind::Scalar(_)) => arguments.push(self.operand(&arg.node)?),
                            Some(ValKind::Pair(..)) => {
                                let (a, b) = self.pair_operand(&arg.node)?;
                                arguments.extend([a, b]);
                            }
                            kind => {
                                if kind.is_none() {
                                    abi.args.push((
                                        arguments.len(),
                                        layout_index(&mut self.layouts, ty)?,
                                    ));
                                }
                                arguments
                                    .push(Operand::AddrOf(self.operand_place(&arg.node)?.expr));
                            }
                        }
                    }
                    let ty = destination.ty(&self.mir.local_decls, self.tcx).ty;
                    let ret = match fixed_layout(self.tcx, self.env, ty).map(|l| l.kind) {
                        Some(ValKind::Zst) => RetDest::Ignore,
                        Some(ValKind::Scalar(_)) => {
                            RetDest::Scalar(self.scalar_place(*destination)?)
                        }
                        Some(ValKind::Pair(..)) => {
                            let (a, b) = self.pair(*destination)?;
                            RetDest::Pair(a, b)
                        }
                        kind => {
                            if kind.is_none() {
                                abi.ret = Some(layout_index(&mut self.layouts, ty)?);
                            }
                            RetDest::Indirect(self.place(*destination)?.expr)
                        }
                    };
                    let unwind = unwind_action(*unwind);
                    let callee = self.call(
                        Call {
                            ty: fn_ty,
                            location: None,
                        },
                        abi,
                    )?;
                    stmts.append(&mut self.pending);
                    return Some(Block {
                        stmts,
                        term: Terminator::Call {
                            callee,
                            args: arguments,
                            ret,
                            target: target.as_u32(),
                            unwind,
                            role: CallRole::Normal,
                        },
                    });
                }
                if args.len() != 2 {
                    return None;
                }
                let ty::RawPtr(pointee, _) =
                    *args[0].node.ty(&self.mir.local_decls, self.tcx).kind()
                else {
                    return None;
                };
                lowered.push(self.scale(&args[1].node, pointee)?);
                lowered.push(Stmt::Assign {
                    dst: self.scalar_place(*destination)?,
                    rv: Rvalue::IntBin {
                        op: IntBinOp::Add,
                        signed: false,
                        a: self.operand(&args[0].node)?,
                        b: Operand::Slot(self.temporary),
                    },
                });
                Terminator::Goto(target.as_u32())
            }
            _ => return None,
        };
        stmts.append(&mut self.pending);
        stmts.append(&mut lowered);
        Some(Block { stmts, term })
    }
}

fn unwind_action(unwind: mir::UnwindAction) -> UnwindAction {
    match unwind {
        mir::UnwindAction::Continue | mir::UnwindAction::Unreachable => UnwindAction::Continue,
        mir::UnwindAction::Terminate(_) => UnwindAction::Terminate,
        mir::UnwindAction::Cleanup(block) => UnwindAction::Cleanup(block.as_u32()),
    }
}

fn fixed_layout<'tcx>(tcx: TyCtxt<'tcx>, env: TypingEnv<'tcx>, ty: Ty<'tcx>) -> Option<Layout> {
    let layout = tcx.layout_of(env.as_query_input(ty)).ok()?;
    if layout.is_unsized() {
        return None;
    }
    Some(Layout {
        size: u32::try_from(layout.size.bytes()).ok()?,
        align: u32::try_from(layout.align.abi.bytes()).ok()?,
        kind: frame::classify(tcx, &layout),
    })
}

fn layout_index<'tcx>(layouts: &mut Vec<Ty<'tcx>>, ty: Ty<'tcx>) -> Option<u32> {
    let index = match layouts.iter().position(|&layout| layout == ty) {
        Some(index) => index,
        None => {
            layouts.push(ty);
            layouts.len() - 1
        }
    };
    u32::try_from(index).ok()
}

fn offset(mut expr: PlaceExpr, offset: u64) -> Option<PlaceExpr> {
    if let PlaceBase::Local(off) = &mut expr.base
        && expr.steps.is_empty()
    {
        *off = off.checked_add(u32::try_from(offset).ok()?)?;
    } else if offset != 0 {
        let mut steps = expr.steps.into_vec();
        steps.push(PlaceStep::Offset(i32::try_from(offset).ok()?));
        expr.steps = steps.into();
    }
    Some(expr)
}

fn scalar_place(expr: PlaceExpr, width: Width) -> ScalarPlace {
    match expr.base {
        PlaceBase::Local(off) if expr.steps.is_empty() => ScalarPlace::Slot(Slot { off, width }),
        _ => ScalarPlace::Mem { expr, width },
    }
}

fn read(place: ScalarPlace) -> Operand {
    match place {
        ScalarPlace::Slot(slot) => Operand::Slot(slot),
        ScalarPlace::Mem { expr, width } => Operand::Mem { expr, width },
    }
}
