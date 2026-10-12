//! Shared instructions and the concrete dictionary, frame and call bindings of one instance.

use std::sync::Arc;

use crate::vm::ir::*;

#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct SharedBody {
    pub template: Arc<GenericTemplate>,
    pub values: Vec<u64>,
    /// Pointer slot and concrete storage offset, both relative to the activation's frame.
    pub locals: Vec<FrameLocal>,
    /// Concrete call adapters, indexed by the template's Call.callee ordinal.
    pub calls: Vec<Terminator>,
}

#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct GenericTemplate {
    pub dictionary: u32,
    pub binding_count: u32,
    pub blocks: Vec<Block>,
}

#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct FrameLocal {
    pub pointer: Slot,
    pub off: u32,
    pub size: u32,
    pub align: u32,
}

impl FuncBody {
    pub(crate) fn code(&self) -> &[Block] {
        self.shared
            .as_ref()
            .map_or(&self.blocks, |shared| &shared.template.blocks)
    }

    pub(crate) fn term<'a>(&'a self, block: &'a Block) -> &'a Terminator {
        if let Some(shared) = &self.shared
            && let Terminator::Call { callee, .. } = &block.term
        {
            &shared.calls[*callee as usize]
        } else {
            &block.term
        }
    }

    /// Generate concrete instructions only when a consumer requests that execution form.
    /// The ABI and frame remain identical to the shared activation, including pointer slots.
    pub(crate) fn specialize(&self) -> Self {
        let Some(shared) = &self.shared else {
            return self.clone();
        };
        let mut body = self.clone();
        body.shared = None;
        body.blocks = shared.template.blocks.clone();
        let bind = |operand: &mut Operand| {
            if let Operand::Mem { expr, width } = operand
                && matches!(expr.base, PlaceBase::Local(off) if off == shared.template.dictionary)
            {
                let [PlaceStep::Deref, PlaceStep::Offset(off)] = &*expr.steps else {
                    unreachable!("verified dictionary addressing")
                };
                *operand = Operand::Imm {
                    bits: shared.values[*off as usize / 8],
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
                        Rvalue::Ref(_) => {}
                        _ => unreachable!("verified shared rvalue"),
                    },
                    Stmt::MemCopy {
                        src, dst, count, ..
                    } => {
                        bind(src);
                        bind(dst);
                        bind(count);
                    }
                    _ => unreachable!("verified shared statement"),
                }
            }
            if let Terminator::Call { callee, .. } = &block.term {
                block.term = shared.calls[*callee as usize].clone();
            }
            match &mut block.term {
                Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(a),
                    ..
                } => bind(a),
                Terminator::Call { args, .. } => args.iter_mut().for_each(&bind),
                _ => {}
            }
        }
        let mut initialize: Vec<_> = shared
            .locals
            .iter()
            .map(|local| Stmt::Assign {
                dst: ScalarPlace::Slot(local.pointer),
                rv: Rvalue::Ref(PlaceExpr {
                    base: PlaceBase::Local(local.off),
                    steps: Box::new([]),
                }),
            })
            .collect();
        initialize.append(&mut body.blocks[0].stmts);
        body.blocks[0].stmts = initialize;
        body
    }
}

impl ArchivedFuncBody {
    pub(crate) fn code(&self) -> &[ArchivedBlock] {
        self.shared
            .as_ref()
            .map_or(self.blocks.as_slice(), |shared| {
                shared.template.blocks.as_slice()
            })
    }

    pub(crate) fn term<'a>(&'a self, block: &'a ArchivedBlock) -> &'a ArchivedTerminator {
        if let Some(shared) = self.shared.as_ref()
            && let ArchivedTerminator::Call { callee, .. } = &block.term
        {
            &shared.calls[*callee as usize]
        } else {
            &block.term
        }
    }
}

#[cfg(test)]
pub(crate) fn test_generic_body(value: u64) -> FuncBody {
    let slot = |off| Slot {
        off,
        width: Width::W64,
    };
    FuncBody {
        frame_size: 24,
        frame_align: 8,
        ret: RetAbi::Scalar(slot(8)),
        params: vec![ParamAbi::Scalar(slot(0))],
        caller_loc_off: None,
        name: format!("generic{value}").into(),
        blocks: Vec::new(),
        shared: Some(Box::new(SharedBody {
            template: Arc::new(GenericTemplate {
                dictionary: 16,
                binding_count: 1,
                blocks: vec![Block {
                    stmts: vec![Stmt::Assign {
                        dst: ScalarPlace::Slot(slot(8)),
                        rv: Rvalue::IntBin {
                            op: IntBinOp::Add,
                            signed: false,
                            a: Operand::Slot(slot(0)),
                            b: Operand::Mem {
                                expr: PlaceExpr {
                                    base: PlaceBase::Local(16),
                                    steps: vec![PlaceStep::Deref, PlaceStep::Offset(0)].into(),
                                },
                                width: Width::W64,
                            },
                        },
                    }],
                    term: Terminator::Return,
                }],
            }),
            values: vec![value],
            locals: Vec::new(),
            calls: Vec::new(),
        })),
    }
}
