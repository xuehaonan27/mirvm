//! Addressing and the slot channel: frame/local addresses, place evaluation,
//! the slot read/write pair that keeps the I64 slot invariant, scalar-place
//! stores, the width masking helpers and the statement-level traps.
//! `impl Translator` sub-block; the struct and the walk are in `super`.

use super::*;

impl Translator<'_, '_> {
    /// Real address of an in-frame offset: with `frame_base_var` set (the
    /// over-16-alignment case) it is `base + off`, otherwise the stack slot's
    /// `stack_addr`.
    pub(super) fn frame_addr(&mut self, off: u32) -> Value {
        if let Some(v) = self.frame_base_var {
            let base = self.b.use_var(v);
            self.b.ins().iadd_imm(base, i64::from(off))
        } else {
            let ss = self
                .frame_ss
                .expect("an address-taken offset must be in frame memory");
            self.b.ins().stack_addr(types::I64, ss, off as i32)
        }
    }

    /// Read a slot. A frame-memory slot is loaded and zero-extended to I64; an SSA
    /// slot reads its variable.
    pub(super) fn read_slot(&mut self, s: Slot) -> Value {
        if self.frame_offs.contains(s.off) {
            let v = if let Some(fbv) = self.frame_base_var {
                let base = self.b.use_var(fbv);
                let a = self.b.ins().iadd_imm(base, i64::from(s.off));
                self.b
                    .ins()
                    .load(Self::narrow_ty(s.width), MemFlagsData::trusted(), a, 0)
            } else {
                let ss = self
                    .frame_ss
                    .expect("a frame offset must have a stack slot");
                self.b
                    .ins()
                    .stack_load(Self::narrow_ty(s.width), ss, s.off as i32)
            };
            if s.width == Width::W64 {
                v
            } else {
                self.b.ins().uextend(types::I64, v)
            }
        } else {
            let v = self.var(s.off);
            self.b.use_var(v)
        }
    }

    /// Write a slot. A frame-memory slot is masked, narrowed and stored; an SSA slot
    /// is masked and defined.
    pub(super) fn write_slot(&mut self, s: Slot, v: Value) {
        let masked = self.mask_val(v, s.width);
        if self.frame_offs.contains(s.off) {
            let n = if s.width == Width::W64 {
                masked
            } else {
                self.b.ins().ireduce(Self::narrow_ty(s.width), masked)
            };
            if let Some(fbv) = self.frame_base_var {
                let base = self.b.use_var(fbv);
                let a = self.b.ins().iadd_imm(base, i64::from(s.off));
                self.b.ins().store(MemFlagsData::trusted(), n, a, 0);
            } else {
                let ss = self
                    .frame_ss
                    .expect("a frame offset must have a stack slot");
                self.b.ins().stack_store(n, ss, s.off as i32);
            }
        } else {
            let var = self.var(s.off);
            self.b.def_var(var, masked);
        }
    }

    /// Real address of an in-frame offset; address-taken analysis guarantees it is
    /// in frame memory.
    pub(super) fn addr_of_local(&mut self, off: u32) -> Value {
        self.frame_addr(off)
    }

    /// Evaluate a `PlaceExpr`, bit-for-bit mirroring `interp::eval_place_addr`.
    /// Deref and Offset use wrapping semantics.
    pub(super) fn place_addr(&mut self, pe: &ir::PlaceExpr) -> Value {
        let mut addr = match pe.base {
            ir::PlaceBase::Local(off) => self.addr_of_local(off),
            ir::PlaceBase::Static(a) => self
                .b
                .ins()
                .iconst(types::I64, self.shared.module.resolve_link_addr(a) as i64),
        };
        for step in pe.steps.iter() {
            match step {
                ir::PlaceStep::Deref => {
                    addr = self
                        .b
                        .ins()
                        .load(types::I64, MemFlagsData::trusted(), addr, 0)
                }
                ir::PlaceStep::Offset(d) => addr = self.b.ins().iadd_imm(addr, i64::from(*d)),
                ir::PlaceStep::IndexScaled { idx, stride } => {
                    let i = self.read_slot(*idx);
                    let scaled = self.b.ins().imul_imm(i, *stride as i64);
                    addr = self.b.ins().iadd(addr, scaled);
                }
                ir::PlaceStep::VTableAlignOffset {
                    meta,
                    unaligned,
                    packed,
                } => {
                    // Interpreter identity: align = *(vtable + 16), `packed` takes the
                    // min, and a non-power-of-two or overflowing alignment aborts --
                    // on the JIT side through mirvm_jit_trap, the same diagnostic exit
                    // the interpreter arm takes through `unwind::engine_abort`.
                    let (vtable, _) = self.operand(meta);
                    let mut align =
                        self.b
                            .ins()
                            .load(types::I64, MemFlagsData::trusted(), vtable, 16);
                    if let Some(p) = packed {
                        let p = self.b.ins().iconst(types::I64, *p as i64);
                        align = self.b.ins().umin(align, p);
                    }
                    // Power-of-two check: align != 0 && (align & (align - 1)) == 0,
                    // otherwise trap.
                    let is_zero = self.b.ins().icmp_imm(IntCC::Equal, align, 0);
                    let am1 = self.b.ins().iadd_imm(align, -1);
                    let pow2 = self.b.ins().band(align, am1);
                    let not_pow2 = self.b.ins().icmp_imm(IntCC::NotEqual, pow2, 0);
                    let bad = self.b.ins().bor(is_zero, not_pow2);
                    self.trap_if(bad, "dyn vtable alignment is not a power of two");
                    // (unaligned + align - 1) & !(align - 1); an overflowing
                    // checked_add traps.
                    let uv = self.b.ins().iconst(types::I64, *unaligned as i64);
                    let sum = self
                        .b
                        .ins()
                        .uadd_overflow_trap(uv, am1, TrapCode::user(2).unwrap());
                    let off = self.b.ins().band_not(sum, am1);
                    addr = self.b.ins().iadd(addr, off);
                }
            }
        }
        addr
    }

    /// Trap when `cond` holds -- the JIT form of `unwind::engine_abort`.
    fn trap_if(&mut self, cond: Value, _msg: &'static str) {
        let t_blk = self.b.create_block();
        let f_blk = self.b.create_block();
        self.b.ins().brif(cond, t_blk, &[], f_blk, &[]);
        self.b.switch_to_block(t_blk);
        let fref = self
            .module
            .declare_func_in_func(self.unreachable, self.b.func);
        let fv = self.b.ins().iconst(types::I64, 0);
        self.b.ins().call(fref, &[fv]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(f_blk);
    }

    /// Division-by-zero branch: when `cond` holds, call `mirvm_jit_div_zero`, which
    /// exits with the interpreter's message and code. `wide` picks the 128-bit kinds
    /// (2/3) over the 64-bit ones (0/1).
    pub(super) fn div_zero_if(&mut self, cond: Value, is_rem: bool, wide: bool) {
        let t_blk = self.b.create_block();
        let f_blk = self.b.create_block();
        self.b.ins().brif(cond, t_blk, &[], f_blk, &[]);
        self.b.switch_to_block(t_blk);
        let fref = self.module.declare_func_in_func(self.div_zero, self.b.func);
        let kind = match (wide, is_rem) {
            (false, false) => 0,
            (false, true) => 1,
            (true, false) => 2,
            (true, true) => 3,
        };
        let kv = self.b.ins().iconst(types::I64, kind);
        self.b.ins().call(fref, &[kv]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(f_blk);
    }

    /// Write a scalar destination: a `Slot` goes through `write_slot`, a `Mem` is
    /// masked, narrowed and stored.
    pub(super) fn write_scalar_place(&mut self, sp: &ScalarPlace, v: Value) {
        match sp {
            ScalarPlace::Slot(s) => {
                let s = *s;
                self.write_slot(s, v);
            }
            ScalarPlace::Mem { expr, width } => {
                let a = self.place_addr(expr);
                let masked = self.mask_val(v, *width);
                let n = if *width == Width::W64 {
                    masked
                } else {
                    self.b.ins().ireduce(Self::narrow_ty(*width), masked)
                };
                self.b.ins().store(MemFlagsData::trusted(), n, a, 0);
            }
        }
    }
}
