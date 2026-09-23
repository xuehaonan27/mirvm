//! Lowering from `ir::FuncBody` to Cranelift IR: slot SSA (the invariant that a
//! slot value is always I64, zero-extended up to the slot's declared width) +
//! place evaluation + the three large stmt/rvalue/terminator matches + the
//! call-helper family + `clif_rmw_op`/`collect_ssa_offs`. Semantics must be
//! bit-for-bit identical to the interpreter (an identity mirror).
//!
//! The `Translator` struct, `build` and the two free functions live here; the
//! rest of the `impl` is split by concern: `place` (addressing + slots),
//! `value` (float/wide channels), `call` (helper calls + unwinding),
//! `numeric` (integer arithmetic) and `stmt`/`rvalue`/`term` (the three
//! exhaustive matches over one body's statements, rvalues and terminators).

use super::admit::callee_abi;
use super::frame::FrameMap;
use super::*;

mod call;
mod numeric;
mod place;
mod rvalue;
mod stmt;
mod term;
mod value;

pub(super) struct Translator<'a, 'b> {
    pub(super) shared: &'a Shared,
    /// The domain this body is being compiled for. The PLT hot path bakes the
    /// address of that domain's `slots_fast` into the machine code, so a trace
    /// body can only ever resolve trace callees (and vice versa).
    pub(super) domain: CodeDomain,
    pub(super) module: &'a mut JITModule,
    pub(super) b: &'a mut FunctionBuilder<'b>,
    pub(super) vars: std::collections::HashMap<u32, Variable>,
    /// Offsets that live in frame memory, from `analyze_frame` (interval model).
    pub(super) frame_offs: FrameMap,
    /// Stack slot for the guest frame; created when `frame_offs` is non-empty, with
    /// `frame_size` bytes at `frame_align` alignment.
    pub(super) frame_ss: Option<StackSlot>,
    /// Code-level alignment base, set when `frame_align` > 16: the stack slot base
    /// is only guaranteed 16-aligned, so `(addr + align - 1) & -align` rounds it up
    /// to `frame_align`. This is the third branch of `frame_addr`.
    pub(super) frame_base_var: Option<Variable>,
    pub(super) unreachable: ClifFuncId,
    pub(super) c2i: ClifFuncId,
    pub(super) call_main_catch: ClifFuncId,
    pub(super) memmove: ClifFuncId,
    pub(super) memset: ClifFuncId,
    pub(super) memcmp: ClifFuncId,
    pub(super) div_zero: ClifFuncId,
    pub(super) volatile_load: ClifFuncId,
    pub(super) volatile_store: ClifFuncId,
    pub(super) call_indirect: ClifFuncId,
    pub(super) tls_ref: ClifFuncId,
    pub(super) call_foreign: ClifFuncId,
    pub(super) call_builtin: ClifFuncId,
    pub(super) alloc: ClifFuncId,
    /// Unwinding support: the pure `TerminateAbort` helper, `Terminate` called
    /// directly at the boundary, the `_Unwind_Resume` import, and the engine-fault
    /// classifier. `has_try_call` below decides whether the function needs an LSDA.
    pub(super) terminate_abort: ClifFuncId,
    pub(super) call_terminate: ClifFuncId,
    pub(super) unwind_resume: ClifFuncId,
    pub(super) exception_is_engine_fault: ClifFuncId,
    /// Trap helper shared by statement-level traps and the terminator form.
    pub(super) trap: ClifFuncId,
    /// Helpers for SIMD/wide statements and the three SIMD rvalues; they share
    /// their body with `semantics::simd`.
    pub(super) simd_stmt: ClifFuncId,
    pub(super) simd_rv: ClifFuncId,
    pub(super) poll_signals: ClifFuncId,
    /// Trace-domain syscall site helper; its first argument is the recorder pinned
    /// in a register.
    pub(super) host_syscall_trace: ClifFuncId,
    pub(super) exception_var: Option<Variable>,
    pub(super) has_try_call: bool,
}

impl Translator<'_, '_> {
    /// Exception-pointer slot for a `try_call` pad: where `TryCallExn(0)` lands and
    /// what `Resume` reads.
    fn exception_var(&mut self) -> Variable {
        if let Some(v) = self.exception_var {
            return v;
        }
        let v = self.b.declare_var(types::I64);
        self.exception_var = Some(v);
        v
    }

    fn var(&mut self, off: u32) -> Variable {
        if let Some(&v) = self.vars.get(&off) {
            return v;
        }
        let v = self.b.declare_var(types::I64);
        self.vars.insert(off, v);
        v
    }

    fn mask_val(&mut self, v: Value, w: Width) -> Value {
        if w == Width::W64 {
            return v;
        }
        self.b.ins().band_imm(v, w.mask() as i64)
    }

    /// Sign-extended I64 view of a slot value: W64 passes through, any narrower
    /// width is ireduced to its own type and then sextended.
    fn sext_val(&mut self, v: Value, w: Width) -> Value {
        let t = match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => return v,
        };
        let narrow = self.b.ins().ireduce(t, v);
        self.b.ins().sextend(types::I64, narrow)
    }

    fn narrow_ty(w: Width) -> cranelift_codegen::ir::Type {
        match w {
            Width::W8 => types::I8,
            Width::W16 => types::I16,
            Width::W32 => types::I32,
            Width::W64 => types::I64,
        }
    }

    fn operand(&mut self, op: &Operand) -> (Value, Width) {
        match op {
            Operand::Slot(s) => (self.read_slot(*s), s.width),
            Operand::Imm { bits, width } => {
                let v = self
                    .b
                    .ins()
                    .iconst(types::I64, (*bits & width.mask()) as i64);
                (v, *width)
            }
            Operand::AddrImm(addr) => {
                let runtime = self.shared.module.resolve_link_addr(*addr);
                (self.b.ins().iconst(types::I64, runtime as i64), Width::W64)
            }
            Operand::Mem { expr, width } => {
                let a = self.place_addr(expr);
                let v = self
                    .b
                    .ins()
                    .load(Self::narrow_ty(*width), MemFlagsData::trusted(), a, 0);
                let v = if *width == Width::W64 {
                    v
                } else {
                    self.b.ins().uextend(types::I64, v)
                };
                (v, *width)
            }
            Operand::AddrOf(expr) => {
                let a = self.place_addr(expr);
                (a, Width::W64)
            }
            Operand::SubImm { base, sub } => {
                let (v, w) = self.operand(base);
                (self.b.ins().iadd_imm(v, (*sub as i64).wrapping_neg()), w)
            }
        }
    }

    fn def_slot(&mut self, s: Slot, v: Value) {
        self.write_slot(s, v);
    }

    pub(super) fn build(&mut self, func: u32, body: &ir::FuncBody) {
        let entry = self.b.create_block();
        self.b.append_block_params_for_function_params(entry);
        let blocks: Vec<_> = (0..body.blocks.len())
            .map(|_| self.b.create_block())
            .collect();

        self.b.switch_to_block(entry);
        // Define every SSA slot variable to 0 so the generated code is
        // deterministic. The interpreter does not zero its frame, but valid MIR has
        // no read-before-write path.
        // Frame memory is zeroed for the same reason: a JIT-on/off differential must
        // be deterministic for any MIR shape.
        let mut offs: Vec<u32> = Vec::new();
        collect_ssa_offs(body, &self.frame_offs, &mut offs);
        let zero = self.b.ins().iconst(types::I64, 0);
        for off in offs {
            let var = self.var(off);
            self.b.def_var(var, zero);
        }
        if let Some(ss) = self.frame_ss {
            // frame_align > 16: a Cranelift x86_64 stack base is only 16-aligned and
            // cannot be re-aligned dynamically, so compiler.rs adds (align - 16)
            // bytes of margin to the slot and the base is rounded up here:
            // frame_base = (addr + align - 1) & -align. Every later in-frame address
            // then goes through frame_addr's third branch.
            if body.frame_align > 16 {
                let addr = self.b.ins().stack_addr(types::I64, ss, 0);
                let padded = self.b.ins().iadd_imm(addr, i64::from(body.frame_align) - 1);
                let base = self.b.ins().band_imm(padded, -i64::from(body.frame_align));
                let v = self.b.declare_var(types::I64);
                self.b.def_var(v, base);
                self.frame_base_var = Some(v);
            }
            let fref = self.module.declare_func_in_func(self.memset, self.b.func);
            let dst = self.frame_addr(0);
            let c0 = self.b.ins().iconst(types::I64, 0);
            let n = self.b.ins().iconst(types::I64, i64::from(body.frame_size));
            self.b.ins().call(fref, &[dst, c0, n]);
        }
        // Resume is the tail of a cleanup chain and is reached through the chain's
        // normal edges, so its block may precede its pad. Declaring exception_var at
        // the entry and defining it to 0 gives every use a definition; the pad's
        // definition dominates the real uses, and Cranelift requires a variable's
        // def to be resolvable at each use.
        if body
            .blocks
            .iter()
            .any(|bl| matches!(bl.term, ir::Terminator::Resume))
        {
            let ev = self.exception_var();
            self.b.def_var(ev, zero);
        }
        // Spill the parameters into slots in the interpreter ABI's flattened order:
        // sret first, then Scalar / Pair / Indirect (memmove) parameters, then the
        // phantom `track_caller` tail argument. Packed and interpreted bodies agree
        // on this order.
        let params = self.b.block_params(entry).to_vec();
        let mut pi = 0usize;
        if let RetAbi::Indirect { sret_off, .. } = body.ret {
            self.def_slot(
                Slot {
                    off: sret_off,
                    width: Width::W64,
                },
                params[pi],
            );
            pi += 1;
        }
        for p in &body.params {
            match p {
                ParamAbi::Zst => {}
                ParamAbi::Scalar(s) => {
                    self.def_slot(*s, params[pi]);
                    pi += 1;
                }
                ParamAbi::Pair(lo, hi) => {
                    self.def_slot(*lo, params[pi]);
                    self.def_slot(*hi, params[pi + 1]);
                    pi += 2;
                }
                ParamAbi::Indirect { off, size } => {
                    // An in-frame offset must be in frame memory; analyze_frame's ABI
                    // flattening forces it.
                    let dst = self.addr_of_local(*off);
                    let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                    let n = self.b.ins().iconst(types::I64, i64::from(*size));
                    self.b.ins().call(fref, &[dst, params[pi], n]);
                    pi += 1;
                }
            }
        }
        if let Some(off) = body.caller_loc_off {
            self.def_slot(
                Slot {
                    off,
                    width: Width::W64,
                },
                params[pi],
            );
        }
        self.b.ins().jump(blocks[0], &[]);

        for (bi, blk) in body.blocks.iter().enumerate() {
            self.b.switch_to_block(blocks[bi]);
            self.poll_signals();
            for st in &blk.stmts {
                self.stmt(st);
            }
            self.term(func, body, &blk.term, &blocks);
        }
    }
}

pub(super) fn clif_rmw_op(op: ir::RmwOp) -> cranelift_codegen::ir::AtomicRmwOp {
    use cranelift_codegen::ir::AtomicRmwOp as C;
    match op {
        ir::RmwOp::Xchg => C::Xchg,
        ir::RmwOp::Add => C::Add,
        ir::RmwOp::Sub => C::Sub,
        ir::RmwOp::And => C::And,
        ir::RmwOp::Or => C::Or,
        ir::RmwOp::Xor => C::Xor,
        ir::RmwOp::Nand => C::Nand,
        ir::RmwOp::Max => C::Smax,
        ir::RmwOp::Min => C::Smin,
        ir::RmwOp::UMax => C::Umax,
        ir::RmwOp::UMin => C::Umin,
    }
}

/// Collect the SSA candidate slot offsets used for 0-definition: every referenced
/// Slot minus the frame set.
pub(super) fn collect_ssa_offs(body: &ir::FuncBody, frame_offs: &FrameMap, out: &mut Vec<u32>) {
    let mut push = |s: &Slot| {
        if !frame_offs.contains(s.off) && !out.contains(&s.off) {
            out.push(s.off);
        }
    };
    let op = |o: &Operand, push: &mut dyn FnMut(&Slot)| {
        if let Operand::Slot(s) = o {
            push(s);
        }
    };
    if let RetAbi::Scalar(s) = &body.ret {
        push(s);
    }
    if let RetAbi::Pair(lo, hi) = &body.ret {
        push(lo);
        push(hi);
    }
    for p in &body.params {
        match p {
            ParamAbi::Scalar(s) => push(s),
            ParamAbi::Pair(lo, hi) => {
                push(lo);
                push(hi);
            }
            _ => {}
        }
    }
    for blk in &body.blocks {
        for st in &blk.stmts {
            match st {
                Stmt::Assign { dst, rv } => {
                    if let ScalarPlace::Slot(s) = dst {
                        push(s);
                    }
                    use ir::Rvalue as R;
                    match rv {
                        R::Use(a)
                        | R::NotBits(a)
                        | R::NotBool(a)
                        | R::Neg(a)
                        | R::Cast { a, .. }
                        | R::BitUn { a, .. } => op(a, &mut push),
                        R::IntBin { a, b, .. }
                        | R::IntCmp { a, b, .. }
                        | R::IntCmp3 { a, b, .. }
                        | R::PtrDiff { a, b, .. }
                        | R::UMax { a, b } => {
                            op(a, &mut push);
                            op(b, &mut push);
                        }
                        R::NicheDiscr { tag, .. } => op(tag, &mut push),
                        R::PtrOffset { ptr, count, .. } => {
                            op(ptr, &mut push);
                            op(count, &mut push);
                        }
                        _ => {}
                    }
                }
                Stmt::AssignOverflow {
                    a,
                    b,
                    dst_val,
                    dst_flag,
                    ..
                } => {
                    op(a, &mut push);
                    op(b, &mut push);
                    if let ScalarPlace::Slot(s) = dst_val {
                        push(s);
                    }
                    if let ScalarPlace::Slot(s) = dst_flag {
                        push(s);
                    }
                }
                // Scalar Operand slots of the SIMD family; vector places live in frame memory and
                // are not listed here.
                Stmt::SimdSplat { val, .. } => op(val, &mut push),
                Stmt::SimdExtractDyn { idx, dst, .. } => {
                    op(idx, &mut push);
                    if let ScalarPlace::Slot(s) = dst {
                        push(s);
                    }
                }
                Stmt::SimdInsertDyn { idx, val, .. } => {
                    op(idx, &mut push);
                    op(val, &mut push);
                }
                Stmt::SimdSelectBitmask { mask, .. } => op(mask, &mut push),
                Stmt::SimdMaskedLoad { base, .. } | Stmt::SimdMaskedStore { base, .. } => {
                    op(base, &mut push)
                }
                _ => {}
            }
        }
        match &blk.term {
            Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(o),
                ..
            } => op(o, &mut push),
            Terminator::Call { args, ret, .. } => {
                for a in args {
                    op(a, &mut push);
                }
                if let RetDest::Scalar(ScalarPlace::Slot(s)) = ret {
                    push(s);
                }
            }
            _ => {}
        }
    }
}
