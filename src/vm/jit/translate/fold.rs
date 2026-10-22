//! Constant folding for baseline translation, without a dataflow pass or memory speculation.

use super::*;
use cranelift_codegen::ir::{InstructionData, Opcode, ValueDef};

impl Translator<'_, '_> {
    pub(super) fn constant(&self, value: Value) -> Option<u64> {
        let dfg = &self.b.func.dfg;
        let ValueDef::Result(inst, _) = dfg.value_def(dfg.resolve_aliases(value)) else {
            return None;
        };
        match dfg.insts[inst] {
            InstructionData::UnaryImm {
                opcode: Opcode::Iconst,
                imm,
            } => Some(imm.bits() as u64),
            _ => None,
        }
    }

    fn constant_operand(&mut self, op: &Operand, depth: u8) -> Option<(u64, Width)> {
        if depth == 8 {
            return None;
        }
        let slot = match op {
            Operand::Imm { bits, width } => return Some((bits & width.mask(), *width)),
            Operand::Slot(slot) => *slot,
            Operand::Mem { expr, width } => Slot {
                off: expr.local_offset()?,
                width: *width,
            },
            Operand::SubImm { base, sub } => {
                let (bits, width) = self.constant_operand(base, depth + 1)?;
                return Some((bits.wrapping_sub(*sub) & width.mask(), width));
            }
            _ => return None,
        };
        if self.frame_offs.contains(slot.off) {
            return None;
        }
        let var = self.var(slot.off);
        let value = self.b.use_var(var);
        Some((self.constant(value)?, slot.width))
    }

    /// SSA copy propagation supplies exact constants. Unknown inputs, memory and faulting
    /// arithmetic keep the normal translator; no block is revisited to seek a fixed point.
    pub(super) fn fold(&mut self, rv: &ir::Rvalue) -> Option<u64> {
        use crate::vm::semantics::arith::{int_bin, int_cmp, sext};
        use ir::Rvalue as R;
        Some(match rv {
            R::Use(a) => self.constant_operand(a, 0)?.0,
            R::IntBin { op, signed, a, b } if !matches!(op, IntBinOp::Div | IntBinOp::Rem) => {
                let (a, width) = self.constant_operand(a, 0)?;
                let (b, _) = self.constant_operand(b, 0)?;
                int_bin(*op, *signed, a, b, width)
            }
            R::IntCmp { cc, signed, a, b } => {
                let (a, width) = self.constant_operand(a, 0)?;
                let (b, _) = self.constant_operand(b, 0)?;
                int_cmp(*cc, *signed, a, b, width)
            }
            R::NotBits(a) => {
                let (a, width) = self.constant_operand(a, 0)?;
                !a & width.mask()
            }
            R::Neg(a) => {
                let (a, width) = self.constant_operand(a, 0)?;
                a.wrapping_neg() & width.mask()
            }
            R::Cast { from, to, a } => {
                let (a, _) = self.constant_operand(a, 0)?;
                let value = if from.1 {
                    sext(a, from.0) as u64
                } else {
                    a & from.0.mask()
                };
                value & to.mask()
            }
            _ => return None,
        })
    }
}
