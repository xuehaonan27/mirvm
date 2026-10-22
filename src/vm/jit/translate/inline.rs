//! A selected leaf uses private SSA variables and retains the caller's frame and block numbering.

use super::*;

impl Translator<'_, '_> {
    pub(super) fn inline_leaf(&mut self, args: &[Value]) -> Option<(Value, Value)> {
        let callee = self.inlining.callee(self.block)?;
        let body = self.shared.module.funcs.get(callee as usize).unwrap();
        // Preserve the call's headroom check even though the leaf adds no addressable frame.
        self.direct_callee(callee, args.len())?;
        let frame = std::mem::replace(
            &mut self.frame_offs,
            super::super::frame::analyze_frame(body),
        );
        let vars = std::mem::take(&mut self.vars);
        let zero = self.b.ins().iconst(types::I64, 0);
        for off in self.frame_offs.ssa_offsets() {
            let var = self.var(off);
            self.b.def_var(var, zero);
        }
        let mut args = args.iter().copied();
        for param in &body.params {
            match param {
                ParamAbi::Zst => {}
                ParamAbi::Scalar(slot) => self.write_slot(*slot, args.next().unwrap()),
                ParamAbi::Pair(a, b) => {
                    self.write_slot(*a, args.next().unwrap());
                    self.write_slot(*b, args.next().unwrap());
                }
                ParamAbi::Indirect { .. } => unreachable!("inline admission"),
            }
        }
        if let Some(off) = body.caller_loc_off {
            self.write_slot(
                Slot {
                    off,
                    width: Width::W64,
                },
                args.next().unwrap(),
            );
        }
        self.poll_signals();
        for stmt in &body.blocks[0].stmts {
            self.stmt(stmt);
        }
        self.poll_signals();
        let result = match body.ret {
            RetAbi::Zst => (zero, zero),
            RetAbi::Scalar(slot) => (self.read_slot(slot), zero),
            RetAbi::Pair(a, b) => (self.read_slot(a), self.read_slot(b)),
            RetAbi::Indirect { .. } => unreachable!("inline admission"),
        };
        self.vars = vars;
        self.frame_offs = frame;
        Some(result)
    }
}
