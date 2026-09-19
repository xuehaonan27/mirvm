//! Call completion and unwinding: the helper calls whose `out` parameter is 16
//! bytes or whose result is a scalar, the signal poll, the `try_call` exception
//! table and the cleanup continuation it jumps to, the trace-domain syscall
//! lowering, and the call result write-back. `impl Translator` sub-block; the
//! struct is in `super`.

use super::*;

impl Translator<'_, '_> {
    /// Call a helper whose `out` parameter is 16 bytes: capture `(lo, hi)` in a
    /// stack slot and store it back into `dst`.
    pub(super) fn call_out128(&mut self, name: &str, args: &[Value], dst: &ir::PlaceExpr) {
        let ss =
            self.b
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 4));
        let outp = self.b.ins().stack_addr(types::I64, ss, 0);
        let mut a: Vec<Value> = args.to_vec();
        a.push(outp);
        let mut sig = self.module.make_signature();
        for _ in &a {
            sig.params.push(AbiParam::new(types::I64));
        }
        let fid = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("missing helper symbol: {name}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        self.b.ins().call(fref, &a);
        let lo = self.b.ins().stack_load(types::I64, ss, 0);
        let hi = self.b.ins().stack_load(types::I64, ss, 8);
        self.write_wide(dst, lo, hi);
    }

    /// Call a helper that returns a single u64.
    pub(super) fn call_helper1(&mut self, name: &str, args: &[Value]) -> Value {
        let mut sig = self.module.make_signature();
        for _ in args {
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64));
        let fid = self
            .module
            .declare_function(name, Linkage::Import, &sig)
            .unwrap_or_else(|_| panic!("missing helper symbol: {name}"));
        let fref = self.module.declare_func_in_func(fid, self.b.func);
        let call = self.b.ins().call(fref, args);
        self.b.inst_results(call)[0]
    }

    pub(super) fn poll_signals(&mut self) {
        let fref = self
            .module
            .declare_func_in_func(self.poll_signals, self.b.func);
        self.b.ins().call(fref, &[]);
    }

    /// Emit the exception table for a `try_call`: tag 0 goes to a pad block whose
    /// `TryCallExn(0)` parameter is the exception pointer -- the pad defines
    /// `exception_var` and jumps to the IR cleanup block -- while `normal` points at
    /// a fresh ok block where the caller writes the return value back and jumps to
    /// the IR target.
    ///
    /// Returns `(exception table, ok block, pad block)`. The callee's return values
    /// reach the ok block's parameters through `TryCallRet`. The caller must emit
    /// the `try_call` from the current continuation block and then call
    /// `enter_cleanup_continuation` to fill in the pad and enter the ok block;
    /// Cranelift forbids switching to the pad before the `try_call` terminates the
    /// current block.
    ///
    /// The `try_call` must be emitted from the *current* block, not from `blocks[bi]`:
    /// an earlier statement in the same IR block may already have moved the
    /// continuation into a helper block (div_zero_if, repeat_loop, ...), and
    /// returning to `blocks[bi]` would append instructions after a brif, which the
    /// verifier rejects.
    pub(super) fn prepare_cleanup(
        &mut self,
        sig: cranelift_codegen::ir::Signature,
    ) -> (
        cranelift_codegen::ir::ExceptionTable,
        cranelift_codegen::ir::Block,
        cranelift_codegen::ir::Block,
    ) {
        use cranelift_codegen::ir::{
            BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
        };
        self.has_try_call = true;
        let pad = self.b.create_block();
        self.b.append_block_param(pad, types::I64);
        let ok = self.b.create_block();
        let mut normal_args = Vec::with_capacity(sig.returns.len());
        for (i, ret) in sig.returns.iter().enumerate() {
            self.b.append_block_param(ok, ret.value_type);
            normal_args.push(BlockArg::TryCallRet(i as u32));
        }
        let normal = BlockCall::new(ok, normal_args, &mut self.b.func.dfg.value_lists);
        let pad_call = self.b.func.dfg.block_call(pad, &[BlockArg::TryCallExn(0)]);
        let sigref = self.b.func.import_signature(sig);
        let et = self
            .b
            .func
            .dfg
            .exception_tables
            .push(ExceptionTableData::new(
                sigref,
                normal,
                [ExceptionTableItem::Tag(
                    ExceptionTag::with_number(0).unwrap(),
                    pad_call,
                )],
            ));
        (et, ok, pad)
    }

    pub(super) fn enter_cleanup_continuation(
        &mut self,
        pad: cranelift_codegen::ir::Block,
        cleanup: ir::Bb,
        ok: cranelift_codegen::ir::Block,
        blocks: &[cranelift_codegen::ir::Block],
    ) {
        self.b.switch_to_block(pad);
        let exn = self.b.block_params(pad)[0];
        let ev = self.exception_var();
        self.b.def_var(ev, exn);

        // An EngineFault means the engine itself can no longer run guest cleanup.
        // Classification uses the exception pointer the pad actually caught; another
        // fault may be parked on the thread by a native catch, but it must not affect
        // this independent unwind.
        let fault_query = self
            .module
            .declare_func_in_func(self.exception_is_engine_fault, self.b.func);
        let call = self.b.ins().call(fault_query, &[exn]);
        let fault = self.b.inst_results(call)[0];
        let is_fault = self.b.ins().icmp_imm(IntCC::NotEqual, fault, 0);
        let resume = self.b.create_block();
        self.b
            .ins()
            .brif(is_fault, resume, &[], blocks[cleanup as usize], &[]);

        self.b.switch_to_block(resume);
        let unwind_resume = self
            .module
            .declare_func_in_func(self.unwind_resume, self.b.func);
        self.b.ins().call(unwind_resume, &[exn]);
        self.b.ins().trap(TrapCode::user(1).unwrap());
        self.b.switch_to_block(ok);
    }

    /// Trace-domain syscall site. The first operand is the syscall number and the
    /// rest are its arguments. The recorder comes from the register the boundary
    /// pinned, and the helper performs the real syscall plus the paired Enter/Exit
    /// records.
    pub(super) fn trace_syscall_site(
        &mut self,
        args: &[ir::Operand],
        ret: &RetDest,
        target: ir::Bb,
        blocks: &[cranelift_codegen::ir::Block],
    ) {
        let mut av: Vec<Value> = Vec::with_capacity(args.len());
        for a in args {
            av.push(self.operand(a).0);
        }
        let nr = match av.first() {
            Some(v) => *v,
            None => self.b.ins().iconst(types::I64, 0),
        };
        let nargs = av.len().saturating_sub(1);
        let arg_ss = self.b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            (nargs.max(1) * 8) as u32,
            3,
        ));
        for (i, v) in av.iter().skip(1).enumerate() {
            self.b.ins().stack_store(*v, arg_ss, (i * 8) as i32);
        }
        let producer = self.b.ins().get_pinned_reg(types::I64);
        let ap = self.b.ins().stack_addr(types::I64, arg_ss, 0);
        let nv = self.b.ins().iconst(types::I64, nargs as i64);
        // The helper reports the recorder it actually used: a fork child's first
        // recording syscall replaces the inherited one, and this is where the
        // replacement reaches the register the rest of the body reads.
        let used_ss =
            self.b
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let up = self.b.ins().stack_addr(types::I64, used_ss, 0);
        let fref = self
            .module
            .declare_func_in_func(self.host_syscall_trace, self.b.func);
        let call = self.b.ins().call(fref, &[producer, nr, ap, nv, up]);
        let lo = self.b.inst_results(call)[0];
        let used = self.b.ins().stack_load(types::I64, used_ss, 0);
        self.b.ins().set_pinned_reg(used);
        let hi = self.b.ins().iconst(types::I64, 0);
        self.write_ret(ret, lo, hi);
        self.b.ins().jump(blocks[target as usize], &[]);
    }

    /// Write a call's result back, as the interpreter does: Ignore/Indirect write
    /// nothing, Scalar takes `lo`, Pair takes `(lo, hi)`.
    pub(super) fn write_ret(&mut self, ret: &RetDest, lo: Value, hi: Value) {
        match ret {
            RetDest::Ignore | RetDest::Indirect(_) => {}
            RetDest::Scalar(ScalarPlace::Slot(s)) => {
                let s = *s;
                self.def_slot(s, lo);
            }
            RetDest::Pair(ScalarPlace::Slot(pl), ScalarPlace::Slot(ph)) => {
                let (pl, ph) = (*pl, *ph);
                self.def_slot(pl, lo);
                self.def_slot(ph, hi);
            }
            _ => unreachable!("admit screens the ret shapes"),
        }
    }
}
