//! Bounded leaf inlining after concrete binding. Cache keys include every selected body.

use super::admit::callee_abi;
use super::frame::analyze_frame;
use super::*;

#[derive(Default)]
pub(super) struct Plan {
    pub calls: Vec<(u32, u32)>,
    pub dependencies: Vec<[u8; 32]>,
}

impl Plan {
    pub(super) fn for_body(shared: &Shared, body: &ir::FuncBody, enabled: bool) -> Self {
        let mut plan = Self::default();
        if !enabled {
            return plan;
        }
        let mut budget = 64usize;
        for (bb, block) in body.blocks.iter().enumerate() {
            let Terminator::Call {
                callee,
                args,
                ret,
                unwind: UnwindAction::Continue,
                role: ir::CallRole::Normal,
                ..
            } = &block.term
            else {
                continue;
            };
            if matches!(ret, RetDest::Indirect(_)) {
                continue;
            }
            let header = shared.module.funcs.header(*callee as usize).unwrap();
            let short = match header {
                ir::FuncHeader::Owned(body) => {
                    body.code().len() == 1 && body.code()[0].stmts.len() <= 16
                }
                ir::FuncHeader::Archived(body) => {
                    body.code().len() == 1 && body.code()[0].stmts.len() <= 16
                }
            };
            if !short || !callee_abi(header).is_some_and(|abi| abi.nparams == args.len()) {
                continue;
            }
            let callee_body = shared.module.funcs.get(*callee as usize).unwrap();
            let Some(block) = callee_body.blocks.first() else {
                continue;
            };
            if callee_body.blocks.len() != 1
                || block.stmts.is_empty()
                || block.stmts.len() > budget
                || !matches!(block.term, Terminator::Return)
                || callee_body
                    .params
                    .iter()
                    .any(|abi| matches!(abi, ParamAbi::Indirect { .. }))
                || matches!(callee_body.ret, RetAbi::Indirect { .. })
                || !block.stmts.iter().all(scalar_statement)
                || analyze_frame(callee_body).needs_frame()
            {
                continue;
            }
            let Some(identity) = artifact::Ordinals::of(callee_body) else {
                continue;
            };
            budget -= block.stmts.len();
            plan.calls.push((bb as u32, *callee));
            plan.dependencies.push(identity.fragment());
        }
        plan
    }

    pub(super) fn callee(&self, block: u32) -> Option<u32> {
        self.calls
            .binary_search_by_key(&block, |&(block, _)| block)
            .ok()
            .map(|index| self.calls[index].1)
    }
}

fn operand(value: &Operand) -> bool {
    match value {
        Operand::Slot(_) | Operand::Imm { .. } => true,
        Operand::SubImm { base, .. } => operand(base),
        Operand::Mem { expr, .. } => {
            matches!(expr.base, ir::PlaceBase::Local(_))
                && matches!(expr.steps.first(), Some(ir::PlaceStep::Deref))
                && expr.steps.iter().all(|step| {
                    matches!(
                        step,
                        ir::PlaceStep::Deref
                            | ir::PlaceStep::Offset(_)
                            | ir::PlaceStep::IndexScaled { .. }
                    )
                })
        }
        _ => false,
    }
}

fn scalar_statement(stmt: &Stmt) -> bool {
    let Stmt::Assign {
        dst: ScalarPlace::Slot(_),
        rv,
    } = stmt
    else {
        return false;
    };
    match rv {
        ir::Rvalue::Use(a)
        | ir::Rvalue::NotBits(a)
        | ir::Rvalue::NotBool(a)
        | ir::Rvalue::Neg(a)
        | ir::Rvalue::Cast { a, .. } => operand(a),
        ir::Rvalue::IntBin { op, a, b, .. } => {
            !matches!(op, IntBinOp::Div | IntBinOp::Rem) && operand(a) && operand(b)
        }
        ir::Rvalue::IntCmp { a, b, .. } => operand(a) && operand(b),
        _ => false,
    }
}
