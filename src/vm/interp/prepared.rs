//! Execute compact operations through the same arithmetic and memory semantics as the reference walk.

use crate::vm::ir::prepared::{DICTIONARY, IMMEDIATE};
use crate::vm::ir::{IntBinOp, IntCc, Operation, Width};

impl Operation {
    #[inline]
    fn width(bits: u8) -> Width {
        match bits & 3 {
            0 => Width::W8,
            1 => Width::W16,
            2 => Width::W32,
            _ => Width::W64,
        }
    }

    #[inline]
    fn value(word: u32, width: Width, base: usize, constants: &[u64], dictionary: &[u64]) -> u64 {
        if word & IMMEDIATE != 0 {
            constants[(word & !IMMEDIATE) as usize]
        } else if word & DICTIONARY != 0 {
            dictionary[(word & !DICTIONARY) as usize] & width.mask()
        } else {
            crate::vm::semantics::memory::mem_read(base as u64 + u64::from(word), width)
        }
    }

    /// The fallback remains the semantic interpreter's instruction at the same block position.
    #[inline]
    pub(crate) fn execute(&self, base: usize, constants: &[u64], dictionary: &[u64]) -> bool {
        if self.code == 0 {
            return false;
        }
        let width = Self::width(self.flags >> 2);
        let a = Self::value(self.a, width, base, constants, dictionary);
        let value = match self.code {
            1 => a,
            2 => {
                let b = Self::value(
                    self.b,
                    Self::width(self.flags >> 4),
                    base,
                    constants,
                    dictionary,
                );
                let op = match self.aux {
                    0 => IntBinOp::Add,
                    1 => IntBinOp::Sub,
                    2 => IntBinOp::Mul,
                    3 => IntBinOp::Div,
                    4 => IntBinOp::Rem,
                    5 => IntBinOp::BitAnd,
                    6 => IntBinOp::BitOr,
                    7 => IntBinOp::BitXor,
                    8 => IntBinOp::Shl,
                    _ => IntBinOp::Shr,
                };
                crate::vm::semantics::arith::int_bin(op, self.flags & 64 != 0, a, b, width)
            }
            3 => {
                let b = Self::value(
                    self.b,
                    Self::width(self.flags >> 4),
                    base,
                    constants,
                    dictionary,
                );
                let cc = match self.aux {
                    0 => IntCc::Eq,
                    1 => IntCc::Ne,
                    2 => IntCc::Lt,
                    3 => IntCc::Le,
                    4 => IntCc::Gt,
                    _ => IntCc::Ge,
                };
                crate::vm::semantics::arith::int_cmp(cc, self.flags & 64 != 0, a, b, width)
            }
            4 => !a & width.mask(),
            5 => (a ^ 1) & 1,
            6 => a.wrapping_neg() & width.mask(),
            7 => {
                let from = Self::width(self.aux);
                let value = if self.flags & 64 != 0 {
                    crate::vm::semantics::arith::sext(a, from) as u64
                } else {
                    a & from.mask()
                };
                value & Self::width(self.aux >> 2).mask()
            }
            _ => unreachable!("verified prepared operation"),
        };
        crate::vm::semantics::memory::mem_write(
            base as u64 + u64::from(self.dst),
            Self::width(self.flags),
            value,
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::vm::ir::*;

    #[test]
    fn prepared_scalars_match_the_reference_for_widths_signedness_and_casts() {
        assert_eq!(std::mem::size_of::<Operation>(), 16);
        let engine = crate::vm::ctx::Engine::new(crate::vm::ctx::Shared::new(Module::default()));
        let activation = crate::vm::ctx::activate(engine.shared());
        for width in [Width::W8, Width::W16, Width::W32, Width::W64] {
            for signed in [false, true] {
                for value in [0, 1, 0x80, 0xff, 0x8000_0000, u64::MAX] {
                    let a = Operand::Imm { bits: value, width };
                    let b = Operand::Slot(Slot {
                        off: 0,
                        width: Width::W64,
                    });
                    let mut values = vec![
                        Rvalue::Use(a.clone()),
                        Rvalue::NotBits(a.clone()),
                        Rvalue::NotBool(a.clone()),
                        Rvalue::Neg(a.clone()),
                    ];
                    for op in [
                        IntBinOp::Add,
                        IntBinOp::Sub,
                        IntBinOp::Mul,
                        IntBinOp::Div,
                        IntBinOp::Rem,
                        IntBinOp::BitAnd,
                        IntBinOp::BitOr,
                        IntBinOp::BitXor,
                        IntBinOp::Shl,
                        IntBinOp::Shr,
                    ] {
                        values.push(Rvalue::IntBin {
                            op,
                            signed,
                            a: a.clone(),
                            b: b.clone(),
                        });
                    }
                    for cc in [
                        IntCc::Eq,
                        IntCc::Ne,
                        IntCc::Lt,
                        IntCc::Le,
                        IntCc::Gt,
                        IntCc::Ge,
                    ] {
                        values.push(Rvalue::IntCmp {
                            cc,
                            signed,
                            a: a.clone(),
                            b: b.clone(),
                        });
                    }
                    for to in [Width::W8, Width::W16, Width::W32, Width::W64] {
                        values.push(Rvalue::Cast {
                            from: (width, signed),
                            to,
                            a: a.clone(),
                        });
                    }
                    for rv in values {
                        let mut frame = [7u64, 0u64];
                        let base = frame.as_mut_ptr() as usize;
                        let expected =
                            super::super::rvalue::eval_rvalue(activation.ctx(), base, &rv);
                        let blocks = vec![Block {
                            stmts: vec![Stmt::Assign {
                                dst: ScalarPlace::Slot(Slot {
                                    off: 8,
                                    width: Width::W64,
                                }),
                                rv,
                            }],
                            term: Terminator::Return,
                        }];
                        let plan = Prepared::new(&blocks, None);
                        assert!(plan.matches(&blocks, None));
                        assert!(plan.ops[0].execute(base, &plan.constants, &[]));
                        assert_eq!(frame[1], expected, "{:?}", blocks[0].stmts[0]);
                    }
                }
            }
        }
    }

    #[test]
    fn dictionary_words_are_bound_without_following_a_frame_pointer() {
        let blocks = vec![Block {
            stmts: vec![Stmt::Assign {
                dst: ScalarPlace::Slot(Slot {
                    off: 0,
                    width: Width::W64,
                }),
                rv: Rvalue::Use(Operand::Mem {
                    expr: PlaceExpr {
                        base: PlaceBase::Local(16),
                        steps: vec![PlaceStep::Deref, PlaceStep::Offset(8)].into(),
                    },
                    width: Width::W16,
                }),
            }],
            term: Terminator::Return,
        }];
        let mut plan = Prepared::new(&blocks, Some(16));
        assert!(plan.matches(&blocks, Some(16)));
        let mut result = 0u64;
        assert!(plan.ops[0].execute(
            &mut result as *mut u64 as usize,
            &plan.constants,
            &[1, 0x12345]
        ));
        assert_eq!(result, 0x2345);
        plan.ops[0].dst = 1000;
        assert!(!plan.matches(&blocks, Some(16)));
    }
}
