//! Owned template instructions and concrete frame/call boundary binding.

use super::{offset, read, scalar_place};
use crate::lower::frame::ValKind;
use crate::vm::ir::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Binding {
    Size(u32),
    Const(u32, Width),
}

pub(super) struct Layout {
    pub size: u32,
    pub align: u32,
    pub kind: ValKind,
}

impl Layout {
    pub fn param(&self, off: u32) -> ParamAbi {
        match self.kind {
            ValKind::Zst => ParamAbi::Zst,
            ValKind::Scalar(width) => ParamAbi::Scalar(Slot { off, width }),
            ValKind::Pair((a, aw), (b, bw)) => ParamAbi::Pair(
                Slot {
                    off: off + a,
                    width: aw,
                },
                Slot {
                    off: off + b,
                    width: bw,
                },
            ),
            ValKind::Other { .. } => ParamAbi::Indirect {
                off,
                size: self.size,
            },
        }
    }

    pub fn ret(&self, off: u32, sret_off: u32) -> RetAbi {
        match self.param(off) {
            ParamAbi::Zst => RetAbi::Zst,
            ParamAbi::Scalar(a) => RetAbi::Scalar(a),
            ParamAbi::Pair(a, b) => RetAbi::Pair(a, b),
            ParamAbi::Indirect { off, size } => RetAbi::Indirect {
                ret_off: off,
                size,
                sret_off,
            },
        }
    }

    fn destination(&self, expr: PlaceExpr) -> Option<RetDest> {
        Some(match self.kind {
            ValKind::Zst => RetDest::Ignore,
            ValKind::Scalar(width) => RetDest::Scalar(scalar_place(expr, width)),
            ValKind::Pair((a, aw), (b, bw)) => RetDest::Pair(
                scalar_place(offset(expr.clone(), u64::from(a))?, aw),
                scalar_place(offset(expr, u64::from(b))?, bw),
            ),
            ValKind::Other { .. } => RetDest::Indirect(expr),
        })
    }

    fn arguments(&self, expr: PlaceExpr, out: &mut Vec<Operand>) -> Option<()> {
        match self.destination(expr)? {
            RetDest::Ignore => {}
            RetDest::Scalar(a) => out.push(read(a)),
            RetDest::Pair(a, b) => out.extend([read(a), read(b)]),
            RetDest::Indirect(expr) => out.push(Operand::AddrOf(expr)),
        }
        Some(())
    }
}

/// An opaque local has a fixed pointer slot; its storage follows the template's fixed frame.
pub(super) struct LocalBinding {
    pub local: usize,
    pub pointer: Slot,
    pub layout: u32,
}

/// Unflattened opaque arguments and return destinations use address placeholders until binding.
#[derive(Default)]
pub(super) struct CallAbi {
    pub args: Vec<(usize, u32)>,
    pub ret: Option<u32>,
}

/// Memory operands read the hidden immutable dictionary. Concrete emission removes that argument;
/// layout-dependent storage and call adapters are bound independently of the instruction export.
pub(super) struct Template {
    pub body: FuncBody,
    pub bindings: Vec<Binding>,
    pub dictionary: u32,
    pub locals: Vec<LocalBinding>,
    pub calls: Vec<CallAbi>,
}

pub(super) fn reserve(size: &mut u32, bytes: u32, align: u32) -> Option<u32> {
    let off = size.checked_add(align.checked_sub(1)?)? & !(align - 1);
    *size = off.checked_add(bytes)?;
    Some(off)
}

impl Template {
    pub fn specialize(&self, values: &[u64], layouts: &[Layout]) -> Option<FuncBody> {
        assert_eq!(values.len(), self.bindings.len());
        let mut body = self.body.clone();
        let bind = |operand: &mut Operand| {
            if let Operand::Mem { expr, width } = operand
                && matches!(expr.base, PlaceBase::Local(off) if off == self.dictionary)
            {
                let [PlaceStep::Deref, PlaceStep::Offset(off)] = &*expr.steps else {
                    unreachable!("template dictionary addressing")
                };
                *operand = Operand::Imm {
                    bits: values[*off as usize / 8],
                    width: *width,
                };
            }
        };
        for block in &mut body.blocks {
            for stmt in &mut block.stmts {
                match stmt {
                    Stmt::Assign { rv, .. } => match rv {
                        Rvalue::Use(a) | Rvalue::Cast { a, .. } => bind(a),
                        Rvalue::IntBin { a, b, .. } | Rvalue::IntCmp { a, b, .. } => {
                            bind(a);
                            bind(b);
                        }
                        Rvalue::Ref(_) => {}
                        _ => unreachable!("template rvalue"),
                    },
                    Stmt::MemCopy {
                        src, dst, count, ..
                    } => {
                        bind(src);
                        bind(dst);
                        bind(count);
                    }
                    _ => unreachable!("template statement"),
                }
            }
            match &mut block.term {
                Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(a),
                    ..
                } => bind(a),
                Terminator::Call {
                    callee, args, ret, ..
                } => {
                    let abi = &self.calls[*callee as usize];
                    if !abi.args.is_empty() {
                        let mut flattened = Vec::new();
                        let mut opaque = abi.args.iter().peekable();
                        for (index, arg) in std::mem::take(args).into_iter().enumerate() {
                            if let Some(&&(position, layout)) = opaque.peek()
                                && position == index
                            {
                                let Operand::AddrOf(expr) = arg else {
                                    unreachable!("opaque argument address")
                                };
                                layouts[layout as usize].arguments(expr, &mut flattened)?;
                                opaque.next();
                            } else {
                                flattened.push(arg);
                            }
                        }
                        *args = flattened;
                    }
                    if let Some(layout) = abi.ret {
                        let RetDest::Indirect(expr) = ret else {
                            unreachable!("opaque return address")
                        };
                        *ret = layouts[layout as usize].destination(expr.clone())?;
                    }
                    args.iter_mut().for_each(&bind);
                }
                Terminator::Goto(_)
                | Terminator::Return
                | Terminator::Unreachable
                | Terminator::Resume
                | Terminator::TerminateAbort => {}
                _ => unreachable!("template terminator"),
            }
        }
        body.params.pop();
        // Address temporaries may follow the dictionary; reclaim its slot only at the frame tail.
        if body.frame_size == self.dictionary + 8 {
            body.frame_size = self.dictionary;
        }
        let mut initialize = Vec::new();
        for local in &self.locals {
            let layout = &layouts[local.layout as usize];
            let off = reserve(&mut body.frame_size, layout.size, layout.align)?;
            body.frame_align = body.frame_align.max(layout.align);
            initialize.push(Stmt::Assign {
                dst: ScalarPlace::Slot(local.pointer),
                rv: Rvalue::Ref(PlaceExpr {
                    base: PlaceBase::Local(off),
                    steps: Box::new([]),
                }),
            });
            if local.local == 0 {
                let sret = if matches!(layout.kind, ValKind::Other { .. }) {
                    reserve(&mut body.frame_size, 8, 8)?
                } else {
                    0
                };
                body.ret = layout.ret(off, sret);
            } else if local.local <= body.params.len() {
                body.params[local.local - 1] = layout.param(off);
            }
        }
        initialize.append(&mut body.blocks[0].stmts);
        body.blocks[0].stmts = initialize;
        Some(body)
    }
}
