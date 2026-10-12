// The vocabulary the rest of the walk is written in: operands, places, slots, ids, and the
// unwind and frozen-range facts they rely on.

use super::*;

impl<'a> Verifier<'a> {
    pub(super) fn operand(&self, body: &FuncBody, op: &Operand) -> Result<(), crate::error::Error> {
        if let Some(shared) = body.shared.as_ref()
            && let Operand::Mem { expr, width } = op
            && matches!(expr.base, PlaceBase::Local(off) if off == shared.template.dictionary)
        {
            let [PlaceStep::Deref, PlaceStep::Offset(off)] = &expr.steps[..] else {
                return Err(crate::fail!(Engine, "invalid dictionary addressing"));
            };
            if *off < 0
                || !(*off as usize).is_multiple_of(8)
                || (*off as u64)
                    .checked_add(u64::from(width.bytes()))
                    .is_none_or(|end| end > shared.values.len() as u64 * 8)
            {
                return Err(crate::fail!(
                    Engine,
                    "dictionary read outside binding values"
                ));
            }
            return Ok(());
        }
        match op {
            Operand::Slot(slot) => self.slot(body, *slot),
            Operand::Mem { expr, .. } | Operand::AddrOf(expr) => self.place(body, expr),
            Operand::Imm { .. } => Ok(()),
            Operand::AddrImm(addr) => self.address(crate::vm::ir::LinkAddr(addr.0), true, true),
            Operand::SubImm { base, .. } => self.operand(body, base),
        }
    }

    pub(super) fn place(
        &self,
        body: &FuncBody,
        place: &PlaceExpr,
    ) -> Result<(), crate::error::Error> {
        match place.base {
            PlaceBase::Local(off) => {
                if body
                    .shared
                    .as_ref()
                    .is_some_and(|shared| shared.template.dictionary == off)
                {
                    return Err(crate::fail!(
                        Engine,
                        "dictionary address escapes a binding read"
                    ));
                }
                if off > body.frame_size {
                    return Err(crate::fail!(
                        Engine,
                        format!("local offset {off} exceeds frame size {}", body.frame_size)
                    ));
                }
            }
            PlaceBase::Static(addr) => self.address(
                crate::vm::ir::LinkAddr(addr.0),
                false,
                self.prefix.funcs != 0 || self.prefix.tls != 0 || self.prefix.asm != 0,
            )?,
        }
        for step in place.steps.iter() {
            match step {
                PlaceStep::Deref | PlaceStep::Offset(_) => {}
                PlaceStep::VTableAlignOffset { meta, packed, .. } => {
                    self.operand(body, meta)?;
                    if packed
                        .as_ref()
                        .is_some_and(|&n| n == 0 || !n.is_power_of_two())
                    {
                        return Err(crate::fail!(
                            Engine,
                            "vtable packed alignment is not a non-zero power of two"
                        ));
                    }
                }
                PlaceStep::IndexScaled { idx, .. } => self.slot(body, *idx)?,
            }
        }
        Ok(())
    }

    pub(super) fn scalar_place(
        &self,
        body: &FuncBody,
        place: &ScalarPlace,
    ) -> Result<(), crate::error::Error> {
        match place {
            ScalarPlace::Slot(slot) => self.slot(body, *slot),
            ScalarPlace::Mem { expr, .. } => self.place(body, expr),
        }
    }

    pub(super) fn ret_dest(
        &self,
        body: &FuncBody,
        ret: &RetDest,
    ) -> Result<(), crate::error::Error> {
        match ret {
            RetDest::Ignore => Ok(()),
            RetDest::Scalar(p) => self.scalar_place(body, p),
            RetDest::Pair(a, b) => self.scalar_places(body, [a, b]),
            RetDest::Indirect(p) => self.place(body, p),
        }
    }

    pub(super) fn switch_discr(
        &self,
        body: &FuncBody,
        discr: &SwitchDiscr,
    ) -> Result<(), crate::error::Error> {
        match discr {
            SwitchDiscr::Scalar(op) => self.operand(body, op),
            SwitchDiscr::Wide(place) => self.place(body, place),
        }
    }

    pub(super) fn bin128_rhs(
        &self,
        body: &FuncBody,
        rhs: &Bin128Rhs,
    ) -> Result<(), crate::error::Error> {
        match rhs {
            Bin128Rhs::Wide(p) => self.place(body, p),
            Bin128Rhs::Scalar(op) => self.operand(body, op),
        }
    }

    pub(super) fn operands<'b>(
        &self,
        body: &FuncBody,
        ops: impl IntoIterator<Item = &'b Operand>,
    ) -> Result<(), crate::error::Error> {
        for op in ops {
            self.operand(body, op)?;
        }
        Ok(())
    }

    pub(super) fn places<'b>(
        &self,
        body: &FuncBody,
        places: impl IntoIterator<Item = &'b PlaceExpr>,
    ) -> Result<(), crate::error::Error> {
        for place in places {
            self.place(body, place)?;
        }
        Ok(())
    }

    pub(super) fn scalar_places<'b>(
        &self,
        body: &FuncBody,
        places: impl IntoIterator<Item = &'b ScalarPlace>,
    ) -> Result<(), crate::error::Error> {
        for place in places {
            self.scalar_place(body, place)?;
        }
        Ok(())
    }

    pub(super) fn slot(&self, body: &FuncBody, slot: Slot) -> Result<(), crate::error::Error> {
        self.span(body, slot.off, slot.width.bytes(), "scalar slot")
    }

    pub(super) fn span(
        &self,
        body: &FuncBody,
        off: u32,
        size: u32,
        what: &str,
    ) -> Result<(), crate::error::Error> {
        let end = off
            .checked_add(size)
            .ok_or_else(|| crate::fail!(Engine, format!("{what} range overflows")))?;
        if end > body.frame_size {
            return Err(crate::fail!(
                Engine,
                format!("{what} {off}..{end} exceeds frame size {}", body.frame_size)
            ));
        }
        Ok(())
    }

    pub(super) fn func(&self, id: FuncId) -> Result<(), crate::error::Error> {
        if id as usize >= self.funcs {
            return Err(crate::fail!(
                Engine,
                format!("function id {id} is outside 0..{}", self.funcs)
            ));
        }
        Ok(())
    }

    pub(super) fn tls(&self, id: TlsId) -> Result<(), crate::error::Error> {
        if id as usize >= self.tls {
            return Err(crate::fail!(
                Engine,
                format!("TLS id {id} is outside 0..{}", self.tls)
            ));
        }
        Ok(())
    }

    pub(super) fn asm(&self, id: AsmStubId) -> Result<(), crate::error::Error> {
        if id as usize >= self.asm {
            return Err(crate::fail!(
                Engine,
                format!("inline-asm stub id {id} is outside 0..{}", self.asm)
            ));
        }
        Ok(())
    }

    pub(super) fn bb(&self, body: &FuncBody, bb: Bb) -> Result<(), crate::error::Error> {
        if bb as usize >= body.code().len() {
            return Err(crate::fail!(
                Engine,
                format!("basic block bb{bb} is outside 0..{}", body.code().len())
            ));
        }
        Ok(())
    }

    pub(super) fn unwind(
        &self,
        body: &FuncBody,
        unwind: UnwindAction,
    ) -> Result<(), crate::error::Error> {
        match unwind {
            UnwindAction::Cleanup(bb) => self.bb(body, bb),
            UnwindAction::Continue | UnwindAction::Terminate => Ok(()),
        }
    }
}
