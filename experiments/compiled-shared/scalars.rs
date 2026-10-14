//! Scalar locations that cannot alias a template's addressable frame storage.

use std::collections::{BTreeMap, BTreeSet};

use crate::vm::ir::*;

#[derive(Default)]
pub(super) struct Scalars {
    slots: BTreeMap<u32, Slot>,
}

impl Scalars {
    pub(super) fn collect(template: &GenericTemplate) -> Self {
        // Differing-width and overlapping accesses must keep byte-addressed memory semantics.
        let mut widths = BTreeMap::<u32, (Width, bool)>::new();
        let mut visit = |slot: Slot| {
            widths
                .entry(slot.off)
                .and_modify(|(width, exact)| {
                    *exact &= *width == slot.width;
                    if slot.width.bytes() > width.bytes() {
                        *width = slot.width;
                    }
                })
                .or_insert((slot.width, true));
        };
        for block in &template.blocks {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Assign { dst, rv } => {
                        match dst {
                            ScalarPlace::Slot(slot) => visit(*slot),
                            ScalarPlace::Mem { expr, .. } => place(expr, &mut visit),
                        }
                        match rv {
                            Rvalue::Use(a) | Rvalue::Cast { a, .. } => operand(a, &mut visit),
                            Rvalue::IntBin { a, b, .. } | Rvalue::IntCmp { a, b, .. } => {
                                operand(a, &mut visit);
                                operand(b, &mut visit);
                            }
                            Rvalue::Ref(expr) => place(expr, &mut visit),
                            _ => unreachable!("verified shared rvalue"),
                        }
                    }
                    Stmt::MemCopy {
                        src, dst, count, ..
                    } => {
                        operand(src, &mut visit);
                        operand(dst, &mut visit);
                        operand(count, &mut visit);
                    }
                    _ => unreachable!("verified shared statement"),
                }
            }
            if let Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(op),
                ..
            } = &block.term
            {
                operand(op, &mut visit);
            }
        }
        // Reuse the JIT's exhaustive address-taken analysis. Escapes cover the rest of the address
        // space, so this does not depend on one concrete instance's opaque storage layout.
        let body = FuncBody {
            frame_size: u32::MAX,
            blocks: template.blocks.clone(),
            ..super::header(0)
        };
        let memory = crate::vm::jit::frame::analyze_frame(&body);
        let mut slots = BTreeMap::new();
        let mut previous_end = 0;
        let mut entries = widths.iter().peekable();
        while let Some((&off, &(width, exact))) = entries.next() {
            let end = off.saturating_add(width.bytes());
            let overlaps =
                off < previous_end || entries.peek().is_some_and(|(next, _)| **next < end);
            if exact && !overlaps && !(off..end).any(|byte| memory.contains(byte)) {
                let mapped = 32 + u32::try_from(slots.len()).unwrap() * 8;
                slots.insert(off, Slot { off: mapped, width });
            }
            previous_end = previous_end.max(end);
        }
        Self { slots }
    }

    pub(super) fn get(&self, slot: Slot) -> Option<Slot> {
        self.slots.get(&slot.off).copied()
    }

    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(super) fn scratch(&self) -> u32 {
        32 + u32::try_from(self.slots.len()).unwrap() * 8
    }

    pub(super) fn sync(&self, to_frame: bool) -> Vec<Stmt> {
        self.sync_selected(to_frame, None)
    }

    pub(super) fn call(&self, term: &Terminator) -> (Vec<Stmt>, Vec<Stmt>) {
        let Terminator::Call {
            args, ret, unwind, ..
        } = term
        else {
            unreachable!("direct call boundary")
        };
        let writes: Vec<Slot> = match ret {
            RetDest::Ignore => Vec::new(),
            RetDest::Scalar(ScalarPlace::Slot(slot)) => vec![*slot],
            RetDest::Pair(ScalarPlace::Slot(a), ScalarPlace::Slot(b)) => vec![*a, *b],
            // Opaque destinations change shape during concrete binding. Keep the full boundary
            // until the template carries a precise, layout-independent write set for them.
            _ => return (self.sync(true), self.sync(false)),
        };
        let written = self.overlapping(&writes);
        // Preserve the untouched bytes of a partially written scalar before the adapter merges
        // its return into frame memory. No promoted local's address can escape to the callee.
        let mut reads = writes;
        for arg in args {
            operand(arg, &mut |slot| reads.push(slot));
        }
        let needed = self.overlapping(&reads);
        let before = if matches!(unwind, UnwindAction::Cleanup(_)) {
            // Cleanup resumes in the interpreter and may read any local, including one not
            // passed to this call. Continue/terminate edges never resume this activation.
            self.sync(true)
        } else {
            self.sync_selected(true, Some(&needed))
        };
        (before, self.sync_selected(false, Some(&written)))
    }

    fn overlapping(&self, accesses: &[Slot]) -> BTreeSet<u32> {
        self.slots
            .iter()
            .filter_map(|(&off, slot)| {
                accesses
                    .iter()
                    .any(|access| {
                        off < access.off.saturating_add(access.width.bytes())
                            && access.off < off.saturating_add(slot.width.bytes())
                    })
                    .then_some(off)
            })
            .collect()
    }

    fn sync_selected(&self, to_frame: bool, selected: Option<&BTreeSet<u32>>) -> Vec<Stmt> {
        self.slots
            .iter()
            .filter(|(off, _)| selected.is_none_or(|selected| selected.contains(off)))
            .map(|(&off, &slot)| {
                let expr = super::address(off);
                if to_frame {
                    Stmt::Assign {
                        dst: ScalarPlace::Mem {
                            expr,
                            width: slot.width,
                        },
                        rv: Rvalue::Use(Operand::Slot(slot)),
                    }
                } else {
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(slot),
                        rv: Rvalue::Use(Operand::Mem {
                            expr,
                            width: slot.width,
                        }),
                    }
                }
            })
            .collect()
    }
}

fn place(expr: &PlaceExpr, visit: &mut impl FnMut(Slot)) {
    if let PlaceBase::Local(off) = expr.base
        && matches!(expr.steps.first(), Some(PlaceStep::Deref))
    {
        visit(Slot {
            off,
            width: Width::W64,
        });
    }
    for step in &expr.steps {
        if let PlaceStep::IndexScaled { idx, .. } = step {
            visit(*idx);
        }
    }
}

fn operand(op: &Operand, visit: &mut impl FnMut(Slot)) {
    match op {
        Operand::Slot(slot) => visit(*slot),
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => place(expr, visit),
        Operand::Imm { .. } => {}
        _ => unreachable!("verified shared operand"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(extra: Vec<Stmt>) -> Scalars {
        let mut stmts = vec![8, 16, 24]
            .into_iter()
            .map(|off| Stmt::Assign {
                dst: ScalarPlace::Slot(Slot {
                    off,
                    width: Width::W64,
                }),
                rv: Rvalue::Use(Operand::Imm {
                    bits: 1,
                    width: Width::W64,
                }),
            })
            .collect::<Vec<_>>();
        stmts.extend(extra);
        Scalars::collect(&GenericTemplate {
            dictionary: 0,
            binding_count: 0,
            blocks: vec![Block {
                stmts,
                term: Terminator::Return,
            }],
        })
    }

    #[test]
    fn escape_keeps_the_reachable_suffix_in_memory() {
        let scalars = slots(vec![Stmt::MemCopy {
            src: Operand::AddrOf(PlaceExpr {
                base: PlaceBase::Local(16),
                steps: Box::new([]),
            }),
            dst: Operand::Imm {
                bits: 0,
                width: Width::W64,
            },
            count: Operand::Imm {
                bits: 0,
                width: Width::W64,
            },
            elem_size: 1,
            overlap: true,
        }]);
        assert_eq!(scalars.slots.keys().copied().collect::<Vec<_>>(), vec![8]);
    }

    #[test]
    fn partial_and_mixed_width_accesses_stay_in_memory() {
        let scalars = slots(vec![
            Stmt::Assign {
                dst: ScalarPlace::Slot(Slot {
                    off: 16,
                    width: Width::W32,
                }),
                rv: Rvalue::Use(Operand::Imm {
                    bits: 2,
                    width: Width::W32,
                }),
            },
            Stmt::Assign {
                dst: ScalarPlace::Mem {
                    expr: PlaceExpr {
                        base: PlaceBase::Local(28),
                        steps: Box::new([]),
                    },
                    width: Width::W8,
                },
                rv: Rvalue::Use(Operand::Imm {
                    bits: 3,
                    width: Width::W8,
                }),
            },
        ]);
        assert_eq!(scalars.slots.keys().copied().collect::<Vec<_>>(), vec![8]);
    }

    #[test]
    fn overlapping_scalar_slots_stay_in_memory() {
        let scalars = slots(vec![Stmt::Assign {
            dst: ScalarPlace::Slot(Slot {
                off: 12,
                width: Width::W32,
            }),
            rv: Rvalue::Use(Operand::Imm {
                bits: 2,
                width: Width::W32,
            }),
        }]);
        assert_eq!(
            scalars.slots.keys().copied().collect::<Vec<_>>(),
            vec![16, 24]
        );
    }

    #[test]
    fn call_synchronizes_overlapping_arguments_and_partial_returns() {
        let scalars = slots(Vec::new());
        let call = Terminator::Call {
            callee: 0,
            args: vec![Operand::Slot(Slot {
                off: 12,
                width: Width::W32,
            })],
            ret: RetDest::Scalar(ScalarPlace::Slot(Slot {
                off: 17,
                width: Width::W8,
            })),
            target: 1,
            unwind: UnwindAction::Continue,
            role: CallRole::Normal,
        };
        let (before, after) = scalars.call(&call);
        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 1);
        let Stmt::Assign {
            dst: ScalarPlace::Slot(slot),
            ..
        } = after[0]
        else {
            panic!("return reload")
        };
        assert_eq!(
            slot.off,
            scalars
                .get(Slot {
                    off: 16,
                    width: Width::W64
                })
                .unwrap()
                .off
        );
    }

    #[test]
    fn cleanup_and_opaque_returns_retain_full_frame_boundaries() {
        let scalars = slots(Vec::new());
        let mut call = Terminator::Call {
            callee: 0,
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Cleanup(2),
            role: CallRole::Normal,
        };
        let (before, after) = scalars.call(&call);
        assert_eq!(before.len(), 3);
        assert!(after.is_empty());
        if let Terminator::Call { ret, unwind, .. } = &mut call {
            *ret = RetDest::Indirect(PlaceExpr {
                base: PlaceBase::Local(0),
                steps: vec![PlaceStep::Deref].into(),
            });
            *unwind = UnwindAction::Continue;
        }
        let (before, after) = scalars.call(&call);
        assert_eq!(before.len(), 3);
        assert_eq!(after.len(), 3);
    }

    #[test]
    fn dereference_reads_pointer_value_without_escaping_its_slot() {
        let scalars = slots(vec![Stmt::Assign {
            dst: ScalarPlace::Mem {
                expr: PlaceExpr {
                    base: PlaceBase::Local(16),
                    steps: vec![PlaceStep::Deref].into(),
                },
                width: Width::W8,
            },
            rv: Rvalue::Use(Operand::Imm {
                bits: 3,
                width: Width::W8,
            }),
        }]);
        assert_eq!(scalars.len(), 3);
    }
}
