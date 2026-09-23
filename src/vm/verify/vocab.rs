//! The vocabulary the rest of the walk is written in: operands, places, slots, ids, and the
//! unwind and frozen-range facts they rely on.

use super::*;

impl<'a> Verifier<'a> {
    pub(super) fn operand(&self, body: &FuncBody, op: &Operand) -> Result<(), String> {
        match op {
            Operand::Slot(slot) => self.slot(body, *slot),
            Operand::Mem { expr, .. } | Operand::AddrOf(expr) => self.place(body, expr),
            Operand::Imm { .. } => Ok(()),
            Operand::AddrImm(addr) => match self.module.try_resolve_link_addr(*addr) {
                Ok(runtime) => self.frozen_range(runtime, 0, true),
                Err(_) if self.module.link_fn_addrs.contains_key(addr) => Ok(()),
                Err(error) => Err(error),
            },
            Operand::SubImm { base, .. } => self.operand(body, base),
        }
    }

    pub(super) fn place(&self, body: &FuncBody, place: &PlaceExpr) -> Result<(), String> {
        match place.base {
            PlaceBase::Local(off) => {
                if off > body.frame_size {
                    return Err(format!(
                        "local offset {off} exceeds frame size {}",
                        body.frame_size
                    ));
                }
            }
            PlaceBase::Static(addr) => {
                let runtime = self.module.try_resolve_link_addr(addr)?;
                self.frozen_range(
                    runtime,
                    0,
                    self.prefix.funcs != 0 || self.prefix.tls != 0 || self.prefix.asm != 0,
                )?
            }
        }
        for step in &place.steps {
            match step {
                PlaceStep::Deref | PlaceStep::Offset(_) => {}
                PlaceStep::VTableAlignOffset { meta, packed, .. } => {
                    self.operand(body, meta)?;
                    if packed.is_some_and(|n| n == 0 || !n.is_power_of_two()) {
                        return Err("vtable packed alignment is not a non-zero power of two".into());
                    }
                }
                PlaceStep::IndexScaled { idx, .. } => self.slot(body, *idx)?,
            }
        }
        Ok(())
    }

    pub(super) fn scalar_place(&self, body: &FuncBody, place: &ScalarPlace) -> Result<(), String> {
        match place {
            ScalarPlace::Slot(slot) => self.slot(body, *slot),
            ScalarPlace::Mem { expr, .. } => self.place(body, expr),
        }
    }

    pub(super) fn ret_dest(&self, body: &FuncBody, ret: &RetDest) -> Result<(), String> {
        match ret {
            RetDest::Ignore => Ok(()),
            RetDest::Scalar(p) => self.scalar_place(body, p),
            RetDest::Pair(a, b) => self.scalar_places(body, [a, b]),
            RetDest::Indirect(p) => self.place(body, p),
        }
    }

    pub(super) fn switch_discr(&self, body: &FuncBody, discr: &SwitchDiscr) -> Result<(), String> {
        match discr {
            SwitchDiscr::Scalar(op) => self.operand(body, op),
            SwitchDiscr::Wide(place) => self.place(body, place),
        }
    }

    pub(super) fn bin128_rhs(&self, body: &FuncBody, rhs: &Bin128Rhs) -> Result<(), String> {
        match rhs {
            Bin128Rhs::Wide(p) => self.place(body, p),
            Bin128Rhs::Scalar(op) => self.operand(body, op),
        }
    }

    pub(super) fn operands<'b>(
        &self,
        body: &FuncBody,
        ops: impl IntoIterator<Item = &'b Operand>,
    ) -> Result<(), String> {
        for op in ops {
            self.operand(body, op)?;
        }
        Ok(())
    }

    pub(super) fn places<'b>(
        &self,
        body: &FuncBody,
        places: impl IntoIterator<Item = &'b PlaceExpr>,
    ) -> Result<(), String> {
        for place in places {
            self.place(body, place)?;
        }
        Ok(())
    }

    pub(super) fn scalar_places<'b>(
        &self,
        body: &FuncBody,
        places: impl IntoIterator<Item = &'b ScalarPlace>,
    ) -> Result<(), String> {
        for place in places {
            self.scalar_place(body, place)?;
        }
        Ok(())
    }

    pub(super) fn slot(&self, body: &FuncBody, slot: Slot) -> Result<(), String> {
        self.span(body, slot.off, slot.width.bytes(), "scalar slot")
    }

    pub(super) fn span(
        &self,
        body: &FuncBody,
        off: u32,
        size: u32,
        what: &str,
    ) -> Result<(), String> {
        let end = off
            .checked_add(size)
            .ok_or_else(|| format!("{what} range overflows"))?;
        if end > body.frame_size {
            return Err(format!(
                "{what} {off}..{end} exceeds frame size {}",
                body.frame_size
            ));
        }
        Ok(())
    }

    pub(super) fn func(&self, id: FuncId) -> Result<(), String> {
        if id as usize >= self.funcs {
            return Err(format!("function id {id} is outside 0..{}", self.funcs));
        }
        Ok(())
    }

    pub(super) fn tls(&self, id: TlsId) -> Result<(), String> {
        if id as usize >= self.tls {
            return Err(format!("TLS id {id} is outside 0..{}", self.tls));
        }
        Ok(())
    }

    pub(super) fn asm(&self, id: AsmStubId) -> Result<(), String> {
        if id as usize >= self.asm {
            return Err(format!(
                "inline-asm stub id {id} is outside 0..{}",
                self.asm
            ));
        }
        Ok(())
    }

    pub(super) fn bb(&self, body: &FuncBody, bb: Bb) -> Result<(), String> {
        if bb as usize >= body.blocks.len() {
            return Err(format!(
                "basic block bb{bb} is outside 0..{}",
                body.blocks.len()
            ));
        }
        Ok(())
    }

    pub(super) fn unwind(&self, body: &FuncBody, unwind: UnwindAction) -> Result<(), String> {
        match unwind {
            UnwindAction::Cleanup(bb) => self.bb(body, bb),
            UnwindAction::Continue | UnwindAction::Terminate => Ok(()),
        }
    }

    pub(super) fn frozen_range(
        &self,
        addr: u64,
        size: u64,
        allow_prefix: bool,
    ) -> Result<(), String> {
        if addr == 0 {
            return Err("contains a null frozen address".into());
        }
        let end = addr
            .checked_add(size)
            .ok_or("frozen address range overflows")?;
        let contains = self
            .module
            .frozen
            .iter()
            .chain(self.module.image_frozens.iter())
            .any(|arena| {
                let bytes = arena.snapshot();
                let start = bytes.as_ptr() as u64;
                let limit = start + bytes.len() as u64;
                addr >= start && end <= limit
            });
        if !contains && !allow_prefix {
            return Err(format!(
                "address range {addr:#x}..{end:#x} is outside frozen memory"
            ));
        }
        Ok(())
    }
}
