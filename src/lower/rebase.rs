//! Rebase: remap the ids a home contributes into the delta namespace while the home's module is
//! absorbed. Function/TLS/asm ids share one shape: a tagged `home_tag(h, j)` becomes `first[h] + j`,
//! an untagged id at or above the delta's first id is shifted by the total the homes contributed, and
//! a base-image id below that stays put.

use super::*;

/// Per-home rebase: the absolute base each home's ids start at, and the totals the delta shifts by.
pub(super) struct Rebase {
    /// Absolute first function/TLS/asm id of each home, indexed by home index.
    pub(super) first_fn: Vec<u32>,
    pub(super) first_tls: Vec<u32>,
    pub(super) first_asm: Vec<u32>,
    /// Totals the delta's ids shift by (every home that is below it).
    pub(super) total_fns: u32,
    pub(super) total_tls: u32,
    pub(super) total_asm: u32,
}

impl Rebase {
    pub(super) fn fn_id(&self, id: u32) -> u32 {
        match id_home(id) {
            Some(home) => self.first_fn[home] + id_ordinal(id),
            None if id >= self.delta_first_fn() => id + self.total_fns,
            None => id,
        }
    }
    pub(super) fn tls_id(&self, id: u32) -> u32 {
        match id_home(id) {
            Some(home) => self.first_tls[home] + id_ordinal(id),
            None if id >= self.delta_first_tls() => id + self.total_tls,
            None => id,
        }
    }
    pub(super) fn asm_id(&self, id: u32) -> u32 {
        match id_home(id) {
            Some(home) => self.first_asm[home] + id_ordinal(id),
            None if id >= self.delta_first_asm() => id + self.total_asm,
            None => id,
        }
    }

    /// The delta's own first ids: where its untagged ids start. They are derived from the totals, so
    /// the shift above and this threshold cannot disagree.
    fn delta_first_fn(&self) -> u32 {
        self.first_fn.first().copied().unwrap_or(0)
    }
    fn delta_first_tls(&self) -> u32 {
        self.first_tls.first().copied().unwrap_or(0)
    }
    fn delta_first_asm(&self) -> u32 {
        self.first_asm.first().copied().unwrap_or(0)
    }

    pub(super) fn guest_panic_cleanup(&self, plan: &mut ir::GuestPanicCleanup) {
        plan.cleanup = self.fn_id(plan.cleanup);
        plan.drop_payload = self.fn_id(plan.drop_payload);
    }

    /// Remap one function body. Only three places carry an id: `Call.callee`,
    /// `InlineAsm.stub` and `Rvalue::TlsRef`. Both matches below are exhaustive on
    /// purpose: a new statement or terminator variant must fail to compile rather than
    /// silently keep an unremapped id.
    pub(super) fn body(&self, b: &mut ir::FuncBody) {
        for block in &mut b.blocks {
            for stmt in &mut block.stmts {
                match stmt {
                    ir::Stmt::Assign { dst: _, rv } => {
                        if let ir::Rvalue::TlsRef(id) = rv {
                            *id = self.tls_id(*id);
                        }
                    }
                    ir::Stmt::AssignOverflow { .. }
                    | ir::Stmt::Copy { .. }
                    | ir::Stmt::RepeatScalar { .. }
                    | ir::Stmt::AtomicStore { .. }
                    | ir::Stmt::VolatileLoad { .. }
                    | ir::Stmt::VolatileStore { .. }
                    | ir::Stmt::AtomicCxchg { .. }
                    | ir::Stmt::AtomicRmw { .. }
                    | ir::Stmt::MemCopy { .. }
                    | ir::Stmt::MemSet { .. }
                    | ir::Stmt::SimdBin { .. }
                    | ir::Stmt::SimdUn { .. }
                    | ir::Stmt::SimdFma { .. }
                    | ir::Stmt::SimdFunnel { .. }
                    | ir::Stmt::SimdCast { .. }
                    | ir::Stmt::SimdSelect { .. }
                    | ir::Stmt::SimdSelectBitmask { .. }
                    | ir::Stmt::SimdGather { .. }
                    | ir::Stmt::SimdScatter { .. }
                    | ir::Stmt::SimdMaskedLoad { .. }
                    | ir::Stmt::SimdMaskedStore { .. }
                    | ir::Stmt::SimdExtractDyn { .. }
                    | ir::Stmt::SimdInsertDyn { .. }
                    | ir::Stmt::SimdArithOffset { .. }
                    | ir::Stmt::SimdSplat { .. }
                    | ir::Stmt::Bin128 { .. }
                    | ir::Stmt::Sat128 { .. }
                    | ir::Stmt::Wide128ToFloat { .. }
                    | ir::Stmt::FloatToWide128 { .. }
                    | ir::Stmt::Bit128 { .. }
                    | ir::Stmt::Bit128Count { .. }
                    | ir::Stmt::F128Bin { .. }
                    | ir::Stmt::F128MathBin { .. }
                    | ir::Stmt::F128Un { .. }
                    | ir::Stmt::F128Fma { .. }
                    | ir::Stmt::F128FromScalar { .. }
                    | ir::Stmt::F128ToScalar { .. }
                    | ir::Stmt::F128FromWideInt { .. }
                    | ir::Stmt::F128ToWideInt { .. }
                    | ir::Stmt::NicheDiscr128 { .. }
                    | ir::Stmt::Trap(_)
                    | ir::Stmt::Nop
                    | ir::Stmt::Fence { .. }
                    | ir::Stmt::RepeatBytes { .. } => {}
                }
            }
            match &mut block.term {
                ir::Terminator::Call { callee, .. } => *callee = self.fn_id(*callee),
                ir::Terminator::InlineAsm { stub, .. } => *stub = self.asm_id(*stub),
                ir::Terminator::Goto(_)
                | ir::Terminator::SwitchInt { .. }
                | ir::Terminator::CallBuiltin { .. }
                | ir::Terminator::CallForeign { .. }
                | ir::Terminator::CallIndirect { .. }
                | ir::Terminator::Return
                | ir::Terminator::Unreachable
                | ir::Terminator::Resume
                | ir::Terminator::TerminateAbort
                | ir::Terminator::Trap(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two homes: home 0 owns ids 10..12, home 1 owns 12..14, and the delta starts at 14.
    fn rebase() -> Rebase {
        Rebase {
            first_fn: vec![10, 12],
            first_tls: vec![20, 22],
            first_asm: vec![30, 31],
            total_fns: 4,
            total_tls: 3,
            total_asm: 2,
        }
    }

    #[test]
    fn home_ids_rebase_to_their_home_and_delta_ids_shift_past_every_home() {
        let rb = rebase();
        assert_eq!(rb.fn_id(home_tag(0, 0)), 10);
        assert_eq!(rb.fn_id(home_tag(0, 1)), 11);
        assert_eq!(rb.fn_id(home_tag(1, 0)), 12);
        assert_eq!(rb.fn_id(home_tag(1, 3)), 15);
        // A base id below the first home stays; the delta's own ids move past both homes.
        assert_eq!(rb.fn_id(3), 3);
        assert_eq!(rb.fn_id(14), 18);
    }

    #[test]
    fn tls_and_asm_use_their_own_home_bases() {
        let rb = rebase();
        assert_eq!(rb.tls_id(home_tag(1, 0)), 22);
        assert_eq!(rb.tls_id(20), 23);
        assert_eq!(rb.asm_id(home_tag(0, 0)), 30);
        assert_eq!(rb.asm_id(30), 32);
    }

    #[test]
    fn guest_panic_cleanup_rebases_home_and_delta_functions() {
        let rb = rebase();
        let mut plan = ir::GuestPanicCleanup {
            cleanup: home_tag(1, 1),
            drop_payload: 14,
        };

        rb.guest_panic_cleanup(&mut plan);

        assert_eq!(plan.cleanup, 13);
        assert_eq!(plan.drop_payload, 18);
    }
}
