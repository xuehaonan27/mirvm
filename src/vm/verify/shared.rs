// The admitted shared vocabulary has no relocations in its reusable instructions.
// All external identities live in the concrete call adapters.

use super::*;

impl Verifier<'_> {
    pub(super) fn shared(&self, body: &FuncBody) -> Result<(), crate::error::Error> {
        let Some(shared) = body.shared.as_ref() else {
            return Ok(());
        };
        if shared.values.len() != shared.template.binding_count as usize {
            return Err(crate::fail!(
                Engine,
                "dictionary arity differs from template schema"
            ));
        }
        if !body.blocks.is_empty() {
            return Err(crate::fail!(
                Engine,
                "shared function also stores concrete blocks"
            ));
        }
        self.span(body, shared.template.dictionary, 8, "dictionary pointer")?;
        for local in shared.locals.iter() {
            self.slot(body, local.pointer)?;
            self.span(body, local.off, local.size, "shared local storage")?;
            if !matches!(local.pointer.width, Width::W64)
                || local.pointer.off == shared.template.dictionary
                || local.align == 0
                || !local.align.is_power_of_two()
                || local.align > body.frame_align
                || !local.off.is_multiple_of(local.align)
            {
                return Err(crate::fail!(Engine, "invalid shared local layout"));
            }
        }
        let mut calls = vec![false; shared.calls.len()];
        for block in shared.template.blocks.iter() {
            for stmt in block.stmts.iter() {
                match stmt {
                    Stmt::Assign { dst, rv } => {
                        self.shared_destination(body, dst)?;
                        match rv {
                            Rvalue::Use(a) | Rvalue::Cast { a, .. } => {
                                self.shared_operand(body, a)?
                            }
                            Rvalue::IntBin { a, b, .. } | Rvalue::IntCmp { a, b, .. } => {
                                self.shared_operand(body, a)?;
                                self.shared_operand(body, b)?;
                            }
                            Rvalue::Ref(expr) => self.shared_place(body, expr)?,
                            _ => return Err(crate::fail!(Engine, "unsupported shared rvalue")),
                        }
                    }
                    Stmt::AssignOverflow {
                        a,
                        b,
                        dst_val,
                        dst_flag,
                        ..
                    } => {
                        self.shared_operand(body, a)?;
                        self.shared_operand(body, b)?;
                        self.shared_destination(body, dst_val)?;
                        self.shared_destination(body, dst_flag)?;
                    }
                    Stmt::MemCopy {
                        src, dst, count, ..
                    } => {
                        for op in [src, dst, count] {
                            self.shared_operand(body, op)?;
                        }
                    }
                    _ => return Err(crate::fail!(Engine, "unsupported shared statement")),
                }
            }
            match &block.term {
                Terminator::Call {
                    callee,
                    args,
                    ret,
                    target,
                    unwind,
                    role,
                } => {
                    let Some(seen) = calls.get_mut(*callee as usize) else {
                        return Err(crate::fail!(Engine, "shared call binding is out of range"));
                    };
                    if *seen {
                        return Err(crate::fail!(Engine, "duplicate shared call ordinal"));
                    }
                    *seen = true;
                    for op in args.iter() {
                        self.shared_operand(body, op)?;
                    }
                    self.shared_return(body, ret)?;
                    self.bb(body, *target)?;
                    let _ = (unwind, role);
                    let Terminator::Call { args, ret, .. } = &shared.calls[*callee as usize] else {
                        return Err(crate::fail!(
                            Engine,
                            "shared call adapter is not a direct call"
                        ));
                    };
                    for op in args.iter() {
                        if !matches!(op, Operand::AddrImm(_)) {
                            self.shared_operand(body, op)?;
                        }
                    }
                    self.shared_return(body, ret)?;
                }
                Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(op),
                    ..
                } => self.shared_operand(body, op)?,
                Terminator::Goto(_)
                | Terminator::Return
                | Terminator::Unreachable
                | Terminator::Resume
                | Terminator::TerminateAbort => {}
                _ => return Err(crate::fail!(Engine, "unsupported shared terminator")),
            }
        }
        if calls.contains(&false) {
            return Err(crate::fail!(Engine, "unused shared call binding"));
        }
        Ok(())
    }

    fn shared_operand(&self, body: &FuncBody, op: &Operand) -> Result<(), crate::error::Error> {
        match op {
            Operand::Slot(slot) => {
                if body
                    .shared
                    .as_ref()
                    .is_some_and(|shared| slot.off == shared.template.dictionary)
                {
                    return Err(crate::fail!(
                        Engine,
                        "shared instruction exposes dictionary pointer"
                    ));
                }
            }
            Operand::Mem { expr, .. } => {
                if !body.shared.as_ref().is_some_and(
                    |shared| matches!(expr.base, PlaceBase::Local(off) if off == shared.template.dictionary),
                ) {
                    self.shared_place(body, expr)?;
                }
            }
            Operand::AddrOf(expr) => self.shared_place(body, expr)?,
            Operand::Imm { .. } => {}
            _ => {
                return Err(crate::fail!(
                    Engine,
                    "shared instruction contains a relocation"
                ));
            }
        }
        self.operand(body, op)
    }

    fn shared_place(&self, body: &FuncBody, expr: &PlaceExpr) -> Result<(), crate::error::Error> {
        if !matches!(expr.base, PlaceBase::Local(_))
            || expr.steps.iter().any(|step| {
                !matches!(
                    step,
                    PlaceStep::Deref | PlaceStep::Offset(_) | PlaceStep::IndexScaled { .. }
                )
            })
        {
            return Err(crate::fail!(
                Engine,
                "shared place contains a relocation or dynamic metadata"
            ));
        }
        self.place(body, expr)
    }

    fn shared_destination(
        &self,
        body: &FuncBody,
        dst: &ScalarPlace,
    ) -> Result<(), crate::error::Error> {
        match dst {
            ScalarPlace::Slot(slot) => {
                if body
                    .shared
                    .as_ref()
                    .is_some_and(|shared| slot.off == shared.template.dictionary)
                {
                    return Err(crate::fail!(
                        Engine,
                        "shared instruction overwrites dictionary pointer"
                    ));
                }
                self.slot(body, *slot)
            }
            ScalarPlace::Mem { expr, .. } => self.shared_place(body, expr),
        }
    }
    fn shared_return(&self, body: &FuncBody, ret: &RetDest) -> Result<(), crate::error::Error> {
        match ret {
            RetDest::Ignore => Ok(()),
            RetDest::Scalar(dst) => self.shared_destination(body, dst),
            RetDest::Pair(a, b) => {
                self.shared_destination(body, a)?;
                self.shared_destination(body, b)
            }
            RetDest::Indirect(expr) => self.shared_place(body, expr),
        }
    }
}
