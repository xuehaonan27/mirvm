//! Owned template instructions and concrete frame/call boundary binding.

use super::{offset, read, scalar_place};
use crate::lower::frame::ValKind;
use crate::vm::ir::*;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum Binding {
    Size(u32),
    Field(u32, u32),
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
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct LocalBinding {
    pub local: usize,
    pub pointer: Slot,
    pub layout: u32,
}

/// Unflattened opaque arguments and return destinations use address placeholders until binding.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct CallAbi {
    pub args: Vec<(usize, u32)>,
    pub ret: Option<u32>,
}

/// The dictionary occupies a private frame slot, initialized by shared execution.
/// Layout-dependent storage and call adapters bind without copying the template instructions.
pub(super) struct Template {
    pub body: FuncBody,
    pub instructions: std::sync::Arc<GenericTemplate>,
    pub bindings: Vec<Binding>,
    pub locals: Vec<LocalBinding>,
    pub calls: Vec<CallAbi>,
}

pub(super) fn reserve(size: &mut u32, bytes: u32, align: u32) -> Option<u32> {
    let off = size.checked_add(align.checked_sub(1)?)? & !(align - 1);
    *size = off.checked_add(bytes)?;
    Some(off)
}

impl Template {
    pub fn archive_body(&self) -> Option<FuncBody> {
        let mut body = self.body.clone();
        let mut calls = vec![Terminator::Unreachable; self.calls.len()];
        for block in &self.instructions.blocks {
            if let Terminator::Call { callee, .. } = &block.term {
                *calls.get_mut(*callee as usize)? = block.term.clone();
            }
        }
        body.shared = Some(Box::new(SharedBody {
            template: self.instructions.clone(),
            values: vec![0; self.bindings.len()],
            locals: Vec::new(),
            calls,
        }));
        Some(body)
    }

    /// Check all indexes used by binding before accepting an unbound persisted product.
    pub fn validate(&self, layouts: usize, generic_args: usize, locals: usize) -> Option<()> {
        if self.body.shared.is_some()
            || !self.body.blocks.is_empty()
            || self.body.caller_loc_off.is_some()
            || self.bindings.len() != self.instructions.binding_count as usize
            || !matches!(self.body.params.last(), Some(ParamAbi::Scalar(slot))
                if slot.off == self.instructions.dictionary && slot.width == Width::W64)
        {
            return None;
        }
        for binding in &self.bindings {
            match binding {
                Binding::Size(index) | Binding::Field(index, _) if *index as usize >= layouts => {
                    return None;
                }
                Binding::Const(index, _) if *index as usize >= generic_args => return None,
                _ => {}
            }
        }
        let mut seen = std::collections::HashSet::new();
        for local in &self.locals {
            if local.local >= locals
                || !seen.insert(local.local)
                || local.layout as usize >= layouts
                || local.pointer.width != Width::W64
                || local.pointer.off == self.instructions.dictionary
                || local.pointer.off.checked_add(8)? > self.body.frame_size
            {
                return None;
            }
        }
        for block in &self.instructions.blocks {
            if let Terminator::Call {
                callee, args, ret, ..
            } = &block.term
            {
                let abi = self.calls.get(*callee as usize)?;
                let mut previous = None;
                for &(position, layout) in &abi.args {
                    if layout as usize >= layouts
                        || previous.is_some_and(|last| position <= last)
                        || !matches!(args.get(position), Some(Operand::AddrOf(_)))
                    {
                        return None;
                    }
                    previous = Some(position);
                }
                if let Some(layout) = abi.ret
                    && (layout as usize >= layouts || !matches!(ret, RetDest::Indirect(_)))
                {
                    return None;
                }
            }
        }
        crate::vm::verify::generic_template(&self.archive_body()?, self.calls.len()).ok()
    }

    pub fn bind(&self, values: Vec<u64>, layouts: &[Layout]) -> Option<FuncBody> {
        assert_eq!(values.len(), self.bindings.len());
        let mut body = self.body.clone();
        body.params.pop();
        let mut locals = Vec::with_capacity(self.locals.len());
        for local in &self.locals {
            let layout = &layouts[local.layout as usize];
            let off = reserve(&mut body.frame_size, layout.size, layout.align)?;
            body.frame_align = body.frame_align.max(layout.align);
            locals.push(FrameLocal {
                pointer: local.pointer,
                off,
                size: layout.size,
                align: layout.align,
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
        let mut calls = vec![Terminator::Unreachable; self.calls.len()];
        for block in &self.instructions.blocks {
            let Terminator::Call { callee, .. } = &block.term else {
                continue;
            };
            let index = *callee as usize;
            let abi = &self.calls[index];
            let mut call = block.term.clone();
            let Terminator::Call { args, ret, .. } = &mut call else {
                unreachable!()
            };
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
            calls[index] = call;
        }
        body.shared = Some(Box::new(SharedBody {
            template: self.instructions.clone(),
            values,
            locals,
            calls,
        }));
        Some(body)
    }
}
