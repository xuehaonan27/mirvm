//! Compact interpreter operations derived from verified semantic bytecode.

use super::*;

pub(crate) const IMMEDIATE: u32 = 1 << 31;
pub(crate) const DICTIONARY: u32 = 1 << 30;

#[derive(
    Clone,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct Prepared {
    pub offsets: Vec<u32>,
    pub ops: Vec<Operation>,
    pub constants: Vec<u64>,
}

/// A fixed-size operation. Operand tags select a frame offset, a literal or a bound dictionary word.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct Operation {
    pub code: u8,
    pub flags: u8,
    pub aux: u8,
    pub dst: u32,
    pub a: u32,
    pub b: u32,
}

trait Literals {
    fn literal(&mut self, value: u64) -> Option<u32>;
}

impl Literals for Vec<u64> {
    fn literal(&mut self, value: u64) -> Option<u32> {
        let index = u32::try_from(self.len())
            .ok()
            .filter(|index| *index < DICTIONARY)?;
        self.push(value);
        Some(IMMEDIATE | index)
    }
}

struct CheckLiterals<'a> {
    constants: &'a [u64],
    next: usize,
}

impl Literals for CheckLiterals<'_> {
    fn literal(&mut self, value: u64) -> Option<u32> {
        if self.constants.get(self.next) != Some(&value) {
            return None;
        }
        let index = self.next as u32;
        self.next += 1;
        Some(IMMEDIATE | index)
    }
}

mod owned {
    use super::*;
    include!("prepared/lower.rs");
}

impl Prepared {
    pub(crate) fn new(blocks: &[Block], dictionary: Option<u32>) -> Self {
        let mut plan = Self::default();
        plan.offsets.push(0);
        for block in blocks {
            for (index, stmt) in block.stmts.iter().enumerate() {
                let mark = plan.constants.len();
                let op = owned::lower(stmt, dictionary, &mut plan.constants).unwrap_or_else(|| {
                    plan.constants.truncate(mark);
                    Operation::fallback(index as u32)
                });
                plan.ops.push(op);
            }
            plan.offsets.push(plan.ops.len() as u32);
        }
        plan
    }

    pub(crate) fn block(&self, block: usize) -> Option<&[Operation]> {
        if self.offsets.is_empty() {
            return None;
        }
        Some(&self.ops[self.offsets[block] as usize..self.offsets[block + 1] as usize])
    }

    pub(crate) fn matches(&self, blocks: &[Block], dictionary: Option<u32>) -> bool {
        use owned::lower;
        include!("prepared/validate.rs")
    }
}

mod archived {
    use super::{CheckLiterals, DICTIONARY, Literals, Operation};
    use crate::vm::ir::{
        ArchivedIntBinOp as IntBinOp, ArchivedIntCc as IntCc, ArchivedOperand as Operand,
        ArchivedPlaceBase as PlaceBase, ArchivedPlaceExpr as PlaceExpr,
        ArchivedPlaceStep as PlaceStep, ArchivedRvalue as Rvalue,
        ArchivedScalarPlace as ScalarPlace, ArchivedStmt as Stmt, ArchivedWidth as Width,
    };
    include!("prepared/lower.rs");

    impl super::ArchivedPrepared {
        pub(crate) fn matches(
            &self,
            blocks: &[crate::vm::ir::ArchivedBlock],
            dictionary: Option<u32>,
        ) -> bool {
            include!("prepared/validate.rs")
        }
    }
}

impl Operation {
    fn fallback(index: u32) -> Self {
        Self {
            code: 0,
            flags: 0,
            aux: 0,
            dst: 0,
            a: index,
            b: 0,
        }
    }
}
