//! Terminator lowering: the exhaustive `ir::Terminator` match, including the unwind edges and
//! the signal poll each back edge carries. `impl Translator` sub-block; the struct is in
//! `super`.

use super::*;

impl Translator<'_, '_> {
    pub(super) fn term(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        t: &Terminator,
        blocks: &[cranelift_codegen::ir::Block],
    ) {
        match t {
            Terminator::Goto(bb) => {
                self.b.ins().jump(blocks[*bb as usize], &[]);
            }
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => match discr {
                SwitchDiscr::Scalar(op) => {
                    let (v, _) = self.operand(op);
                    // An icmp+brif chain; the values are sparse, so a br_table is left as a
                    // possible optimization.
                    for (val, bb) in targets {
                        let hit = self.b.ins().icmp_imm(IntCC::Equal, v, *val as u64 as i64);
                        let next = self.b.create_block();
                        self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(blocks[*otherwise as usize], &[]);
                }
                SwitchDiscr::Wide(pe) => {
                    // 128-bit discriminant: read the whole place at once (iconcat) and compare
                    // against each target as an I128 constant, so both the targets and the
                    // discriminant keep their full 128 bits.
                    let (lo, hi) = self.read_wide(pe);
                    let v = self.i128_of(lo, hi);
                    for (val, bb) in targets {
                        let c = self.iconst128(*val);
                        let hit = self.b.ins().icmp(IntCC::Equal, v, c);
                        let next = self.b.create_block();
                        self.b.ins().brif(hit, blocks[*bb as usize], &[], next, &[]);
                        self.b.switch_to_block(next);
                    }
                    self.b.ins().jump(blocks[*otherwise as usize], &[]);
                }
            },
            Terminator::Call {
                callee,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                // Flatten the arguments in the interpreter's Call order: for RetDest::Indirect
                // the destination's real address comes first, then each argument. Lowering has
                // already split Pair arguments into two slots and appended the phantom tail
                // argument.
                let mut av: Vec<Value> = Vec::with_capacity(args.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    let a = self.place_addr(dst);
                    av.push(a);
                }
                for a in args {
                    av.push(self.operand(a).0);
                }
                // Write-back template, the same shape for PLT results and the c2i ret_ss.
                macro_rules! write_back {
                    ($lo:expr, $hi:expr) => {
                        match ret {
                            RetDest::Ignore | RetDest::Indirect(_) => {}
                            RetDest::Scalar(ScalarPlace::Slot(s)) => {
                                self.def_slot(*s, $lo);
                            }
                            RetDest::Pair(ScalarPlace::Slot(pl), ScalarPlace::Slot(ph)) => {
                                self.def_slot(*pl, $lo);
                                self.def_slot(*ph, $hi);
                            }
                            _ => unreachable!("admit screens the ret shapes"),
                        }
                    };
                }
                match unwind {
                    UnwindAction::Cleanup(bb) => {
                        // try_call: `normal` is the ok block (write back, then jump to target) and
                        // exception-table tag 0 goes to pad(TryCallExn(0)). Every case goes through
                        // c2i-try_call because that is the one authoritative implementation; a PLT
                        // try_call_indirect is left as a possible optimization.
                        let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            (av.len().max(1) * 8) as u32,
                            3,
                        ));
                        for (i, v) in av.iter().enumerate() {
                            self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                        }
                        let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            16,
                            3,
                        ));
                        let mut sig0 = self.module.make_signature();
                        for _ in 0..4 {
                            sig0.params.push(AbiParam::new(types::I64));
                        }
                        let (et, ok, pad) = self.prepare_cleanup(sig0);
                        let fref = self.module.declare_func_in_func(self.c2i, self.b.func);
                        let fv = self.b.ins().iconst(types::I64, *callee as i64);
                        let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                        let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                        let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                        self.b.ins().try_call(fref, &[fv, ap, nv, rp], et);
                        self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                        let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                        let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                        write_back!(lo, hi);
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                    UnwindAction::Terminate => {
                        // The Terminate boundary is mirvm_call_terminate, a c2i-shaped wrapper with
                        // the same semantics as the interpreter's call_guarding_terminate.
                        let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            (av.len().max(1) * 8) as u32,
                            3,
                        ));
                        for (i, v) in av.iter().enumerate() {
                            self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                        }
                        let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            16,
                            3,
                        ));
                        let fref = self
                            .module
                            .declare_func_in_func(self.call_terminate, self.b.func);
                        let fv = self.b.ins().iconst(types::I64, *callee as i64);
                        let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                        let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                        let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                        self.b.ins().call(fref, &[fv, ap, nv, rp]);
                        let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                        let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                        write_back!(lo, hi);
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                    UnwindAction::Continue => {
                        if matches!(role, ir::CallRole::MainPanicBoundary) {
                            let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                (av.len().max(1) * 8) as u32,
                                3,
                            ));
                            for (i, v) in av.iter().enumerate() {
                                self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                            }
                            let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                StackSlotKind::ExplicitSlot,
                                16,
                                3,
                            ));
                            let fref = self
                                .module
                                .declare_func_in_func(self.call_main_catch, self.b.func);
                            let fv = self.b.ins().iconst(types::I64, *callee as i64);
                            let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                            let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                            let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                            self.b.ins().call(fref, &[fv, ap, nv, rp]);
                            let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                            let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                            write_back!(lo, hi);
                        } else {
                            let cb = &self.shared.module.funcs[*callee as usize];
                            let plt = callee_abi(cb).filter(|cabi| cabi.nparams == av.len());
                            if let Some(cabi) = plt {
                                // Hot path: indirect through the PLT's memory -- load
                                // slots_fast[callee], then call_indirect. The shape is
                                // constant, and the trampoline-to-fast upgrade is invisible to
                                // the call site. The slot array is the one for this body's
                                // domain: a trace body only ever resolves trace slots, a plain
                                // body only plain slots.
                                let slot_addr = &self.shared.jit.slots_for(self.domain).slots_fast
                                    [*callee as usize]
                                    as *const std::sync::atomic::AtomicU64
                                    as i64;
                                let ap = self.b.ins().iconst(types::I64, slot_addr);
                                let fp =
                                    self.b
                                        .ins()
                                        .load(types::I64, MemFlagsData::trusted(), ap, 0);
                                let sig = {
                                    let mut s = self.module.make_signature();
                                    for _ in 0..av.len() {
                                        s.params.push(AbiParam::new(types::I64));
                                    }
                                    for _ in 0..cabi.nrets {
                                        s.returns.push(AbiParam::new(types::I64));
                                    }
                                    s
                                };
                                let sigref = self.b.import_signature(sig);
                                let call = self.b.ins().call_indirect(sigref, fp, &av);
                                let lo = if cabi.nrets >= 1 {
                                    self.b.inst_results(call)[0]
                                } else {
                                    self.b.ins().iconst(types::I64, 0)
                                };
                                let hi = if cabi.nrets >= 2 {
                                    self.b.inst_results(call)[1]
                                } else {
                                    self.b.ins().iconst(types::I64, 0)
                                };
                                write_back!(lo, hi);
                            } else {
                                // Cold path: call c2i directly from the call site, packing the
                                // flattened arguments back to the interpreter. The interpreter
                                // already consumes a flattened argument vector, so any callee
                                // ABI agrees; this is also where panic-like branches end up.
                                let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                    StackSlotKind::ExplicitSlot,
                                    (av.len().max(1) * 8) as u32,
                                    3,
                                ));
                                for (i, v) in av.iter().enumerate() {
                                    self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                                }
                                let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                                    StackSlotKind::ExplicitSlot,
                                    16,
                                    3,
                                ));
                                let fref = self.module.declare_func_in_func(self.c2i, self.b.func);
                                let fv = self.b.ins().iconst(types::I64, *callee as i64);
                                let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                                let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                                let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                                self.b.ins().call(fref, &[fv, ap, nv, rp]);
                                let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                                let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                                write_back!(lo, hi);
                            }
                        }
                        self.b.ins().jump(blocks[*target as usize], &[]);
                    }
                }
            }
            Terminator::Return => {
                self.poll_signals();
                match body.ret {
                    RetAbi::Zst => {
                        self.b.ins().return_(&[]);
                    }
                    RetAbi::Scalar(s) => {
                        let v = self.read_slot(s);
                        self.b.ins().return_(&[v]);
                    }
                    RetAbi::Pair(lo_s, hi_s) => {
                        let lo = self.read_slot(lo_s);
                        let hi = self.read_slot(hi_s);
                        self.b.ins().return_(&[lo, hi]);
                    }
                    RetAbi::Indirect {
                        ret_off,
                        size,
                        sret_off,
                    } => {
                        // The same semantics as the interpreter's Return: read the destination
                        // address from the sret slot, memcpy _0 -> dst for `size` bytes, and
                        // return no values.
                        let dst = self.read_slot(Slot {
                            off: sret_off,
                            width: Width::W64,
                        });
                        let src = self.addr_of_local(ret_off);
                        let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                        let n = self.b.ins().iconst(types::I64, i64::from(size));
                        self.b.ins().call(fref, &[dst, src, n]);
                        self.b.ins().return_(&[]);
                    }
                }
            }
            Terminator::CallIndirect {
                callee,
                args,
                ret,
                target,
                unwind,
                null_ok,
                native_sig,
            } => {
                // The mirvm_call_indirect helper dispatches as the interpreter's CallIndirect
                // arm does: look the address up in fn_addrs and call_guest, or on a miss with a
                // native_sig, ffi::call_addr.
                let (addr, _) = self.operand(callee);
                // Flatten the arguments in the interpreter's order: for RetDest::Indirect the
                // destination address first, then each argument.
                let mut av: Vec<Value> = Vec::with_capacity(args.len() + 1);
                if let RetDest::Indirect(dst) = ret {
                    let a = self.place_addr(dst);
                    av.push(a);
                }
                for a in args {
                    av.push(self.operand(a).0);
                }
                let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (av.len().max(1) * 8) as u32,
                    3,
                ));
                for (i, v) in av.iter().enumerate() {
                    self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                }
                let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    16,
                    3,
                ));
                let fref = self
                    .module
                    .declare_func_in_func(self.call_indirect, self.b.func);
                let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                let nok = self.b.ins().iconst(types::I64, i64::from(*null_ok));
                let nsig = self.b.ins().iconst(
                    types::I64,
                    native_sig
                        .as_ref()
                        .map_or(0, |s| s as *const ir::ForeignSig as i64),
                );
                let fv = self.b.ins().iconst(types::I64, func as i64);
                if let UnwindAction::Cleanup(bb) = unwind {
                    // try_call: the ok block writes back and then jumps to target; the pad jumps to
                    // the IR cleanup block.
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..8 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    let (et, ok, pad) = self.prepare_cleanup(sig0);
                    let z = self.b.ins().iconst(types::I64, 0);
                    self.b
                        .ins()
                        .try_call(fref, &[addr, ap, nv, rp, nok, nsig, fv, z], et);
                    self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    // Continue / Terminate are distinguished by a flag; the Terminate flag means
                    // the call is wrapped in catch_unwind + abort, the same semantics as
                    // call_guarding_terminate.
                    let term = self.b.ins().iconst(
                        types::I64,
                        i64::from(matches!(unwind, UnwindAction::Terminate)),
                    );
                    self.b
                        .ins()
                        .call(fref, &[addr, ap, nv, rp, nok, nsig, fv, term]);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::InlineAsm {
                stub,
                buf_size,
                ins,
                outs,
                target,
            } => {
                // Call the asm stub's real address directly, using the same slot ABI as the
                // interpreter's InlineAsm arm: a stack buffer; scalar `ins` occupy the low 8
                // bytes of a slot while VecBytes are copied at full width; call fn(*mut u8);
                // then read `outs` back.
                let buf = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (*buf_size).max(1),
                    4,
                ));
                let base = self.b.ins().stack_addr(types::I64, buf, 0);
                for (off, v) in ins {
                    match v {
                        ir::AsmIoVal::Scalar(o) => {
                            let (x, _) = self.operand(o);
                            self.b.ins().stack_store(x, buf, *off as i32);
                        }
                        ir::AsmIoVal::VecBytes(pe, size) => {
                            let src = self.place_addr(pe);
                            let dst = self.b.ins().stack_addr(types::I64, buf, *off as i32);
                            let n = self.b.ins().iconst(types::I64, i64::from(*size));
                            let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                            self.b.ins().call(fref, &[dst, src, n]);
                        }
                    }
                }
                let stub_addr = self.b.ins().iconst(
                    types::I64,
                    self.shared.module.asm_stub_addrs[*stub as usize] as i64,
                );
                let mut s = self.module.make_signature();
                s.params.push(AbiParam::new(types::I64));
                let sigref = self.b.import_signature(s);
                self.b.ins().call_indirect(sigref, stub_addr, &[base]);
                for (off, d) in outs {
                    match d {
                        ir::AsmIoDst::Scalar(sp) => {
                            let v = self.b.ins().stack_load(types::I64, buf, *off as i32);
                            self.write_scalar_place(sp, v);
                        }
                        ir::AsmIoDst::VecBytes(pe, size) => {
                            let dst = self.place_addr(pe);
                            let src = self.b.ins().stack_addr(types::I64, buf, *off as i32);
                            let n = self.b.ins().iconst(types::I64, i64::from(*size));
                            let fref = self.module.declare_func_in_func(self.memmove, self.b.func);
                            self.b.ins().call(fref, &[dst, src, n]);
                        }
                    }
                }
                self.b.ins().jump(blocks[*target as usize], &[]);
            }
            Terminator::CallForeign {
                sym,
                sig,
                args,
                ret,
                target,
                unwind,
            } => {
                // The mirvm_call_foreign helper mirrors the interpreter's CallForeign arm:
                // materialize thunk_args, resolve the C1 Indirect destination, restore the
                // enlarged pthread stack, and call ffi::call itself.
                let mut av: Vec<Value> = Vec::with_capacity(args.len());
                for a in args {
                    av.push(self.operand(a).0);
                }
                // An aggregate returned by value uses the Indirect destination: the ffi layer
                // memcpys to the destination's real address.
                let ret_dst = if let RetDest::Indirect(dst) = ret {
                    self.place_addr(dst)
                } else {
                    self.b.ins().iconst(types::I64, 0)
                };
                let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (av.len().max(1) * 8) as u32,
                    3,
                ));
                for (i, v) in av.iter().enumerate() {
                    self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                }
                let fref = self
                    .module
                    .declare_func_in_func(self.call_foreign, self.b.func);
                let sp = self.b.ins().iconst(types::I64, sym.as_ptr() as i64);
                let sl = self.b.ins().iconst(types::I64, sym.len() as i64);
                let sg = self
                    .b
                    .ins()
                    .iconst(types::I64, sig as *const ir::ForeignSig as i64);
                let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                macro_rules! foreign_write_back {
                    ($r:expr) => {
                        let r = $r;
                        match ret {
                            RetDest::Ignore => {}
                            RetDest::Scalar(p) => {
                                self.write_scalar_place(p, r);
                            }
                            // The by-value aggregate bytes have already been memcpy'd to dst by the
                            // ffi layer.
                            RetDest::Indirect(_) => {}
                            _ => unreachable!("admit screens the foreign return shapes"),
                        }
                    };
                }
                if let UnwindAction::Cleanup(bb) = unwind {
                    // try_call: the ok block writes back and then jumps to target; the pad jumps to
                    // the IR cleanup block. The eight-parameter signature matches
                    // mirvm_call_foreign's actual arguments: seven slots plus the terminate flag.
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..8 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    sig0.returns.push(AbiParam::new(types::I64));
                    let (et, ok, pad) = self.prepare_cleanup(sig0);
                    let z = self.b.ins().iconst(types::I64, 0);
                    self.b
                        .ins()
                        .try_call(fref, &[sp, sl, sg, ap, nv, ret_dst, fv, z], et);
                    self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                    foreign_write_back!(self.b.block_params(ok)[0]);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    let term = self.b.ins().iconst(
                        types::I64,
                        i64::from(matches!(unwind, UnwindAction::Terminate)),
                    );
                    let call = self
                        .b
                        .ins()
                        .call(fref, &[sp, sl, sg, ap, nv, ret_dst, fv, term]);
                    foreign_write_back!(self.b.inst_results(call)[0]);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::CallBuiltin {
                builtin,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                // The trace domain has its own syscall site. The recorder is read from the
                // register the activation boundary pinned; the site itself consults neither TLS
                // nor the session, and the syscall plus its paired Enter/Exit records all happen
                // inside the helper.
                if self.domain == CodeDomain::Trace
                    && matches!(builtin, ir::Builtin::HostSyscallTrace)
                    && matches!(unwind, UnwindAction::Continue)
                {
                    self.trace_syscall_site(args, ret, *target, blocks);
                    return;
                }
                // The four allocation builtins take the mirvm_alloc fast path -- the engine
                // heap's single entry point, dispatched by tag. Everything else goes through
                // mirvm_call_builtin, which shares its body with exec_builtin.
                let alloc_tag = match builtin {
                    ir::Builtin::RustAlloc => Some(0i64),
                    ir::Builtin::RustAllocZeroed => Some(1),
                    ir::Builtin::RustRealloc => Some(2),
                    ir::Builtin::RustDealloc => Some(3),
                    _ => None,
                };
                if let UnwindAction::Cleanup(bb) = unwind {
                    // Everything goes through the generic mirvm_call_builtin path with try_call;
                    // the allocation builtins live in that same body, so they must not be routed
                    // around the exception table via the fast path.
                    let mut av: Vec<Value> = Vec::with_capacity(args.len());
                    for a in args {
                        av.push(self.operand(a).0);
                    }
                    let ret_dst = if let RetDest::Indirect(dst) = ret {
                        self.place_addr(dst)
                    } else {
                        self.b.ins().iconst(types::I64, 0)
                    };
                    let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (av.len().max(1) * 8) as u32,
                        3,
                    ));
                    for (i, v) in av.iter().enumerate() {
                        self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                    }
                    let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    let fref = self
                        .module
                        .declare_func_in_func(self.call_builtin, self.b.func);
                    let bp = self
                        .b
                        .ins()
                        .iconst(types::I64, builtin as *const ir::Builtin as i64);
                    let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                    let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                    let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                    let fv = self.b.ins().iconst(types::I64, func as i64);
                    let mut sig0 = self.module.make_signature();
                    for _ in 0..8 {
                        sig0.params.push(AbiParam::new(types::I64));
                    }
                    let (et, ok, pad) = self.prepare_cleanup(sig0);
                    let z = self.b.ins().iconst(types::I64, 0);
                    let role = self.b.ins().iconst(types::I64, *role as i64);
                    self.b
                        .ins()
                        .try_call(fref, &[bp, ap, nv, ret_dst, fv, rp, z, role], et);
                    self.enter_cleanup_continuation(pad, *bb, ok, blocks);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else if matches!(unwind, UnwindAction::Continue)
                    && let Some(tag) = alloc_tag
                    && matches!(ret, RetDest::Ignore | RetDest::Scalar(ScalarPlace::Slot(_)))
                {
                    // A fixed four-slot argument vector: realloc uses all four, while
                    // alloc/dealloc pad with 0 and the helper consumes them by tag, since
                    // exec_builtin's body only reads the a(i) it needs.
                    let mut av: Vec<Value> = Vec::with_capacity(4);
                    for i in 0..4 {
                        av.push(match args.get(i) {
                            Some(o) => self.operand(o).0,
                            None => self.b.ins().iconst(types::I64, 0),
                        });
                    }
                    let fref = self.module.declare_func_in_func(self.alloc, self.b.func);
                    let tv = self.b.ins().iconst(types::I64, tag);
                    let fv = self.b.ins().iconst(types::I64, func as i64);
                    let call = self
                        .b
                        .ins()
                        .call(fref, &[tv, av[0], av[1], av[2], av[3], fv]);
                    let r = self.b.inst_results(call)[0];
                    match ret {
                        RetDest::Ignore => {}
                        RetDest::Scalar(ScalarPlace::Slot(s)) => self.def_slot(*s, r),
                        _ => unreachable!("this branch screens the ret shapes"),
                    }
                    self.b.ins().jump(blocks[*target as usize], &[]);
                } else {
                    // Flatten the arguments with no sret prepended -- a builtin's Indirect
                    // destination is evaluated separately, in the interpreter's thin-arm order.
                    // ret_dst is the Indirect destination's real address, otherwise 0.
                    let mut av: Vec<Value> = Vec::with_capacity(args.len());
                    for a in args {
                        av.push(self.operand(a).0);
                    }
                    let ret_dst = if let RetDest::Indirect(dst) = ret {
                        self.place_addr(dst)
                    } else {
                        self.b.ins().iconst(types::I64, 0)
                    };
                    let args_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        (av.len().max(1) * 8) as u32,
                        3,
                    ));
                    for (i, v) in av.iter().enumerate() {
                        self.b.ins().stack_store(*v, args_ss, (i * 8) as i32);
                    }
                    let ret_ss = self.b.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        16,
                        3,
                    ));
                    let fref = self
                        .module
                        .declare_func_in_func(self.call_builtin, self.b.func);
                    let bp = self
                        .b
                        .ins()
                        .iconst(types::I64, builtin as *const ir::Builtin as i64);
                    let ap = self.b.ins().stack_addr(types::I64, args_ss, 0);
                    let nv = self.b.ins().iconst(types::I64, av.len() as i64);
                    let rp = self.b.ins().stack_addr(types::I64, ret_ss, 0);
                    let fv = self.b.ins().iconst(types::I64, func as i64);
                    // The Terminate flag, with the same catch_unwind + abort semantics as
                    // elsewhere.
                    let term = self.b.ins().iconst(
                        types::I64,
                        i64::from(matches!(unwind, UnwindAction::Terminate)),
                    );
                    let role = self.b.ins().iconst(types::I64, *role as i64);
                    self.b
                        .ins()
                        .call(fref, &[bp, ap, nv, ret_dst, fv, rp, term, role]);
                    let lo = self.b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = self.b.ins().stack_load(types::I64, ret_ss, 8);
                    self.write_ret(ret, lo, hi);
                    self.b.ins().jump(blocks[*target as usize], &[]);
                }
            }
            Terminator::Resume => {
                // Read the exception pointer from exception_var -- defined by the pad's
                // TryCallExn(0), or 0 from the entry when no pad reached it, since build
                // pre-declares it for any function containing Resume -- and call _Unwind_Resume
                // to continue unwinding, as cg_clif's Resume does.
                let ev = self.exception_var();
                let exn = self.b.use_var(ev);
                let fref = self
                    .module
                    .declare_func_in_func(self.unwind_resume, self.b.func);
                self.b.ins().call(fref, &[exn]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            Terminator::TerminateAbort => {
                // mirvm_jit_terminate_abort, with the message and code of the interpreter's
                // TerminateAbort arm: an UnwindTerminate (double panic or ABI boundary) aborts.
                let fref = self
                    .module
                    .declare_func_in_func(self.terminate_abort, self.b.func);
                self.b.ins().call(fref, &[]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            Terminator::Unreachable => {
                let fref = self
                    .module
                    .declare_func_in_func(self.unreachable, self.b.func);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                self.b.ins().call(fref, &[fv]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
            // The Trap-stub terminator: the terminator form of mirvm_jit_trap, which takes
            // the function name and exits with the interpreter's runblocks-arm message and
            // error code 70.
            Terminator::Trap(reason) => {
                let fref = self.module.declare_func_in_func(self.trap, self.b.func);
                let p = self.b.ins().iconst(types::I64, reason.as_ptr() as i64);
                let n = self.b.ins().iconst(types::I64, reason.len() as i64);
                let fv = self.b.ins().iconst(types::I64, func as i64);
                self.b.ins().call(fref, &[p, n, fv]);
                self.b.ins().trap(TrapCode::user(1).unwrap());
            }
        }
    }
}
