//! Structural generic MIR export. Only layout-independent scalar frames are admitted; every
//! unsupported shape returns to the ordinary lowerer before any linker state has changed.

use super::frame::{self, FrameLayout, ValKind};
use crate::vm::ir::*;
use rustc_data_structures::fx::FxHashMap;
use rustc_hir::def_id::DefId;
use rustc_middle::{
    mir,
    ty::{self, Instance, InstanceKind, Ty, TyCtxt, TypingEnv},
};

/// This cache belongs to one compiler session and semantic profile. Shims are excluded, so the
/// definition is the complete template key. Negative entries avoid repeatedly probing a body.
#[derive(Default)]
pub(super) struct Templates(FxHashMap<DefId, Option<Template>>);

impl Templates {
    pub(super) fn lower<'tcx>(
        &mut self,
        tcx: TyCtxt<'tcx>,
        env: TypingEnv<'tcx>,
        instance: Instance<'tcx>,
    ) -> Option<FuncBody> {
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
        let template = self
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
        let values = template
            .bindings
            .iter()
            .map(|binding| match *binding {
                Binding::Size(index) => {
                    let ty = instance.args.get(index as usize)?.as_type()?;
                    Some(frame::layout_of(tcx, env, ty).ok()?.size.bytes())
                }
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
        if crate::diag::prints(crate::diag::Severity::Debug) {
            crate::diag_debug!(
                Lower,
                "generic template bound: {}",
                tcx.symbol_name(instance).name
            );
        }
        Some(template.specialize(&values))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Binding {
    Size(u32),
    Const(u32, Width),
}

/// Owned instructions and a positional binding schema; neither retains compiler data. Memory
/// operands read the hidden immutable dictionary. Only specialization leaves this module today.
struct Template {
    body: FuncBody,
    bindings: Vec<Binding>,
    dictionary: u32,
}

impl Template {
    fn specialize(&self, values: &[u64]) -> FuncBody {
        assert_eq!(values.len(), self.bindings.len());
        let mut body = self.body.clone();
        let bind = |operand: &mut Operand| {
            if let Operand::Mem { expr, width } = operand {
                assert!(matches!(expr.base, PlaceBase::Local(off) if off == self.dictionary));
                let [PlaceStep::Deref, PlaceStep::Offset(off)] = &*expr.steps else {
                    unreachable!("template dictionary addressing")
                };
                *operand = Operand::Imm {
                    bits: values[*off as usize / 8],
                    width: *width,
                };
            }
        };
        for block in &mut body.blocks {
            for stmt in &mut block.stmts {
                match stmt {
                    Stmt::Assign { rv, .. } => match rv {
                        Rvalue::Use(a) | Rvalue::Cast { a, .. } => bind(a),
                        Rvalue::IntBin { a, b, .. } | Rvalue::IntCmp { a, b, .. } => {
                            bind(a);
                            bind(b);
                        }
                        _ => unreachable!("template rvalue"),
                    },
                    Stmt::MemCopy {
                        src, dst, count, ..
                    } => {
                        bind(src);
                        bind(dst);
                        bind(count);
                    }
                    _ => unreachable!("template statement"),
                }
            }
            match &mut block.term {
                Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(a),
                    ..
                } => bind(a),
                Terminator::Goto(_) | Terminator::Return | Terminator::Unreachable => {}
                _ => unreachable!("template terminator"),
            }
        }
        body.params.pop();
        body.frame_size = self.dictionary;
        body
    }
}

struct Exporter<'tcx> {
    tcx: TyCtxt<'tcx>,
    env: TypingEnv<'tcx>,
    mir: &'tcx mir::Body<'tcx>,
    frame: FrameLayout<'tcx>,
    temporary: Slot,
    dictionary: u32,
    bindings: Vec<Binding>,
}

impl<'tcx> Exporter<'tcx> {
    fn export(tcx: TyCtxt<'tcx>, instance: Instance<'tcx>) -> Option<Template> {
        if tcx.data_layout.pointer_size().bytes() != 8 {
            return None;
        }
        let mir = tcx.instance_mir(instance.def);
        if mir.spread_arg.is_some() || mir.basic_blocks.iter().any(|b| b.is_cleanup) {
            return None;
        }
        // Restrict local types before querying layout: admitting a currently scalar generic ADT
        // would confuse one concrete layout with a universally fixed frame.
        for local in &mir.local_decls {
            if !matches!(
                local.ty.kind(),
                ty::Bool | ty::Char | ty::Int(_) | ty::Uint(_) | ty::RawPtr(..) | ty::Ref(..)
            ) && !local.ty.is_unit()
            {
                return None;
            }
        }
        let env = TypingEnv::post_analysis(tcx, instance.def_id());
        let frame = frame::freeze(tcx, env, mir).ok()?;
        if frame
            .locals
            .iter()
            .any(|local| !matches!(local.kind, ValKind::Zst | ValKind::Scalar(_)))
        {
            return None;
        }
        let temporary = Slot {
            off: frame.size.checked_add(7)? & !7,
            width: Width::W64,
        };
        let dictionary = temporary.off.checked_add(8)?;
        let frame_size = dictionary.checked_add(8)?;
        let ret = match frame.locals[0].kind {
            ValKind::Zst => RetAbi::Zst,
            ValKind::Scalar(width) => RetAbi::Scalar(Slot {
                off: frame.locals[0].off,
                width,
            }),
            _ => return None,
        };
        let mut params = mir
            .args_iter()
            .map(|local| {
                let info = &frame.locals[local.as_usize()];
                match info.kind {
                    ValKind::Zst => ParamAbi::Zst,
                    ValKind::Scalar(width) => ParamAbi::Scalar(Slot {
                        off: info.off,
                        width,
                    }),
                    _ => unreachable!("scalar template frame"),
                }
            })
            .collect::<Vec<_>>();
        params.push(ParamAbi::Scalar(Slot {
            off: dictionary,
            width: Width::W64,
        }));
        let mut cx = Self {
            tcx,
            env,
            mir,
            frame,
            temporary,
            dictionary,
            bindings: Vec::new(),
        };
        let blocks = mir
            .basic_blocks
            .iter()
            .map(|block| cx.block(block))
            .collect::<Option<_>>()?;
        Some(Template {
            body: FuncBody {
                frame_size,
                frame_align: cx.frame.align.max(8),
                ret,
                params,
                caller_loc_off: None,
                blocks,
                name: tcx.def_path_str(instance.def_id()).into(),
            },
            dictionary,
            bindings: cx.bindings,
        })
    }

    fn slot(&self, place: mir::Place<'tcx>) -> Option<Slot> {
        if !place.projection.is_empty() {
            return None;
        }
        let local = &self.frame.locals[place.local.as_usize()];
        Some(Slot {
            off: local.off,
            width: local.kind.scalar()?,
        })
    }

    fn width(&self, ty: Ty<'tcx>) -> Option<Width> {
        frame::scalar_width(&frame::layout_of(self.tcx, self.env, ty).ok()?)
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
        if let ty::Param(param) = ty.kind() {
            self.binding(Binding::Size(param.index), Width::W64)
        } else {
            Some(Operand::Imm {
                bits: frame::layout_of(self.tcx, self.env, ty).ok()?.size.bytes(),
                width: Width::W64,
            })
        }
    }

    fn operand(&mut self, operand: &mir::Operand<'tcx>) -> Option<Operand> {
        match operand {
            mir::Operand::Copy(place) | mir::Operand::Move(place) => {
                Some(Operand::Slot(self.slot(*place)?))
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
            mir::Rvalue::Cast(kind, a, to) => {
                let from = a.ty(&self.mir.local_decls, self.tcx);
                match kind {
                    mir::CastKind::IntToInt => Rvalue::Cast {
                        from: (self.width(from)?, frame::ty_signed(from)),
                        to: self.width(*to)?,
                        a: self.operand(a)?,
                    },
                    mir::CastKind::PtrToPtr if self.width(from)? == self.width(*to)? => {
                        Rvalue::Use(self.operand(a)?)
                    }
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

    fn block(&mut self, block: &mir::BasicBlockData<'tcx>) -> Option<Block> {
        let mut stmts = Vec::new();
        for stmt in &block.statements {
            match &stmt.kind {
                mir::StatementKind::Assign(box (place, rv)) => stmts.push(Stmt::Assign {
                    dst: ScalarPlace::Slot(self.slot(*place)?),
                    rv: self.rvalue(rv)?,
                }),
                mir::StatementKind::Intrinsic(
                    box mir::NonDivergingIntrinsic::CopyNonOverlapping(copy),
                ) => {
                    let ty::RawPtr(pointee, _) =
                        *copy.src.ty(&self.mir.local_decls, self.tcx).kind()
                    else {
                        return None;
                    };
                    stmts.push(self.scale(&copy.count, pointee)?);
                    stmts.push(Stmt::MemCopy {
                        dst: self.operand(&copy.dst)?,
                        src: self.operand(&copy.src)?,
                        count: Operand::Slot(self.temporary),
                        elem_size: 1,
                        overlap: false,
                    });
                }
                mir::StatementKind::StorageLive(_)
                | mir::StatementKind::StorageDead(_)
                | mir::StatementKind::Nop => {}
                _ => return None,
            }
        }
        let term = match &block.terminator().kind {
            mir::TerminatorKind::Goto { target } => Terminator::Goto(target.as_u32()),
            mir::TerminatorKind::Return => Terminator::Return,
            mir::TerminatorKind::Unreachable => Terminator::Unreachable,
            mir::TerminatorKind::SwitchInt { discr, targets } => Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(self.operand(discr)?),
                targets: targets
                    .iter()
                    .map(|(value, target)| (value, target.as_u32()))
                    .collect(),
                otherwise: targets.otherwise().as_u32(),
            },
            mir::TerminatorKind::Call {
                func,
                args,
                destination,
                target: Some(target),
                ..
            } => {
                let ty::FnDef(def, _) = *func.ty(&self.mir.local_decls, self.tcx).kind() else {
                    return None;
                };
                let intrinsic = self.tcx.intrinsic(def)?;
                if intrinsic.name.as_str() != "arith_offset" || args.len() != 2 {
                    return None;
                }
                let ty::RawPtr(pointee, _) =
                    *args[0].node.ty(&self.mir.local_decls, self.tcx).kind()
                else {
                    return None;
                };
                stmts.push(self.scale(&args[1].node, pointee)?);
                stmts.push(Stmt::Assign {
                    dst: ScalarPlace::Slot(self.slot(*destination)?),
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
        Some(Block { stmts, term })
    }
}
