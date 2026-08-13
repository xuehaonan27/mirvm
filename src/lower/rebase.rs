//! Rebase（自 lower/mod.rs M10 整搬）：A2 id 重映射（编译期穷尽枚举——
//! fn/TLS/asm 同构 TAG|j → first+j、untagged d → d+image_count、底座 id
//! 不动），absorb 时装配 image 模块进 delta 命名空间。

use super::*;

/// fn/TLS/asm 同构：`TAG|j` → `first + j`；untagged d（≥ first）→ `d + image_count`；
/// 底座 id（< first）不动。触及字段 = 设计 §9 盘点的 6 处 + ids/tls_ids 两表。
pub(super) struct Rebase {
    pub(super) first_fn: u32,
    pub(super) image_fns: u32,
    pub(super) first_tls: u32,
    pub(super) image_tls: u32,
    pub(super) first_asm: u32,
    pub(super) image_asm: u32,
}

impl Rebase {
    pub(super) fn fn_id(&self, id: u32) -> u32 {
        if id & IMAGE_TAG != 0 {
            self.first_fn + (id & !IMAGE_TAG)
        } else if id >= self.first_fn {
            id + self.image_fns
        } else {
            id
        }
    }
    pub(super) fn tls_id(&self, id: u32) -> u32 {
        if id & IMAGE_TAG != 0 {
            self.first_tls + (id & !IMAGE_TAG)
        } else if id >= self.first_tls {
            id + self.image_tls
        } else {
            id
        }
    }
    pub(super) fn asm_id(&self, id: u32) -> u32 {
        if id & IMAGE_TAG != 0 {
            self.first_asm + (id & !IMAGE_TAG)
        } else if id >= self.first_asm {
            id + self.image_asm
        } else {
            id
        }
    }

    pub(super) fn guest_panic_cleanup(&self, plan: &mut ir::GuestPanicCleanup) {
        plan.cleanup = self.fn_id(plan.cleanup);
        plan.drop_payload = self.fn_id(plan.drop_payload);
    }

    /// 单函数体重映射。op 级字段只有 3 处（设计 §9 实证）：Call.callee /
    /// InlineAsm.stub / Rvalue::TlsRef。**编译期穷尽**（or-pattern 全枚举，新变体
    /// = 非穷尽编译错误——防"新增携带 id 的 op 被遗忘"的静默错值）。
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

    #[test]
    fn guest_panic_cleanup_rebases_image_and_delta_functions() {
        let rb = Rebase {
            first_fn: 10,
            image_fns: 3,
            first_tls: 0,
            image_tls: 0,
            first_asm: 0,
            image_asm: 0,
        };
        let mut plan = ir::GuestPanicCleanup {
            cleanup: IMAGE_TAG | 1,
            drop_payload: 10,
        };

        rb.guest_panic_cleanup(&mut plan);

        assert_eq!(plan.cleanup, 11);
        assert_eq!(plan.drop_payload, 13);
    }
}
