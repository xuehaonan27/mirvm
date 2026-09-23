//! One body at a time: the entry signatures, the fast/guarded/packed definitions, the c2i
//! trampoline a not-yet-compiled callee lands in, and the shape checks admission already made
//! again at the boundary.

use super::*;

impl<'a> Compiler<'a> {
    pub(super) fn fast_sig(&mut self, abi: CalleeAbi) -> Signature {
        let mut sig = self.module.make_signature();
        for _ in 0..abi.nparams {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..abi.nrets {
            sig.returns.push(AbiParam::new(types::I64));
        }
        sig
    }

    /// Compile one function that crossed the threshold. A rejected or failed request
    /// stays interpreted, silently.
    pub(super) fn compile(&mut self, func: u32) {
        let jit = &self.shared.jit;
        if self.domain == CodeDomain::Trace && jit.trace_enter.load(Ordering::Acquire) == 0 {
            // Without the boundary pin a trace body has no recorder to read, so
            // it must not be built at all; interpretation keeps recording through
            // TLS and stays correct.
            return;
        }
        if jit.slots_for(self.domain).slots[func as usize].load(Ordering::Acquire) != 0 {
            return; // already compiled in this domain
        }
        let Some(body) = self.shared.module.funcs.get(func as usize) else {
            return;
        };
        if !admit(self.shared, body) {
            // Staying interpreted is the intended outcome for a non-admitted function,
            // not a failure; strict mode only records the set. Gated by MIRVM_JIT_DEBUG.
            if jit.sync && crate::options::get().jit_debug {
                eprintln!(
                    "mirvm-jit-strict: f{func} not admitted ({})",
                    self.shared.module.funcs[func as usize].name
                );
            }
            return;
        }
        let abi = callee_abi(body).expect("admit already checked the shape");

        // Pre-warm the fast slots of PLT-visible callees: an uncompiled one gets a c2i
        // trampoline (fast shape, so its call sites keep a constant shape). Callees that
        // do not fit the fast shape are excluded; their call sites go straight to c2i
        // (cold path).
        let mut callees: Vec<(u32, CalleeAbi)> = Vec::new();
        for blk in &body.blocks {
            if let Terminator::Call {
                callee, args, ret, ..
            } = &blk.term
                && *callee != func
                && !callees.iter().any(|(c, _)| c == callee)
                && let Some(cabi) = callee_abi(&self.shared.module.funcs[*callee as usize])
                && cabi.nparams == args.len() + usize::from(matches!(ret, RetDest::Indirect(_)))
            {
                callees.push((*callee, cabi));
            }
        }
        for (c, cabi) in callees {
            if jit.slots_for(self.domain).slots_fast[c as usize].load(Ordering::Acquire) == 0
                && let Some((tramp, ranges)) = self.define_c2i_trampoline(c, cabi)
            {
                jit.publish_c2i_entry_for(self.domain, c, tramp as u64, ranges);
            }
        }

        // Silent-failure discipline: a compile failure keeps the function interpreted
        // and never writes a panic to stderr, because the differential oracle compares
        // stderr byte-for-byte and thread ids would pollute it. MIRVM_JIT_SYNC is the
        // exception: an admitted function that fails records the FAIL sentinel loudly.
        // TODO: report these errors through the log system once one exists.
        let Some((fast_id, fast_symbol)) = self.define_fast(func, body, abi) else {
            self.strict_fail(func);
            return;
        };
        let mut symbols = vec![fast_symbol];
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::FastBody) {
            return;
        }
        let Some((guarded_id, guarded_symbol)) = self.define_guarded_fast(func, body, abi, fast_id)
        else {
            self.strict_fail(func);
            return;
        };
        symbols.push(guarded_symbol);
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::Guarded) {
            return;
        }
        let Some((packed_id, packed_symbol)) = self.define_packed(func, body, abi, guarded_id)
        else {
            self.strict_fail(func);
            return;
        };
        symbols.push(packed_symbol);
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::Packed) {
            return;
        }
        if self.module.finalize_definitions().is_err() {
            self.strict_fail(func);
            return;
        }
        self.register_pending_eh_frames();
        let ranges = self.finalized_symbol_ranges(symbols);

        let fast = self.module.get_finalized_function(guarded_id) as u64;
        let packed = self.module.get_finalized_function(packed_id) as u64;
        // Publish order: fast first (self-recursion and other compiled callers reach
        // it), then packed (only the interpreter can enter compiled code through it).
        // Every memory range is complete before these two Release stores; the perf map
        // is written only by an explicit stop.
        jit.publish_compiled_entries_for(self.domain, func, fast, packed, ranges);
    }

    /// Published fast entry. Keeping the check in a separate slot-free
    /// function is important: putting it in `define_fast` would run only after
    /// Cranelift's prologue had already moved the native stack pointer.
    pub(super) fn define_guarded_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("g{func}"), Linkage::Local, &sig)
            .ok()?;
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let guard = self.module.declare_func_in_func(self.stack_guard, b.func);
            let fv = b.ins().iconst(types::I64, func as i64);
            let explicit = u64::from(body.frame_size)
                .saturating_add(u64::from(body.frame_align.saturating_sub(16)));
            let frame = b.ins().iconst(types::I64, explicit as i64);
            b.ins().call(guard, &[fv, frame]);
            let target = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(target, &params);
            let results = b.inst_results(call).to_vec();
            b.ins().return_(&results);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if crate::options::get().jit_debug {
                eprintln!("mirvm-jit-debug: guarded entry define failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::Guarded,
                size,
            },
        ))
    }

    /// Strict verification mode (MIRVM_JIT_SYNC): an admitted function that fails to
    /// compile records the FAIL sentinel loudly, and the SYNC waiter aborts on it. The
    /// non-strict path never reaches here, so the silent-failure discipline holds.
    pub(super) fn strict_fail(&self, func: u32) {
        let jit = &self.shared.jit;
        if jit.sync {
            eprintln!(
                "mirvm-jit-strict: f{func} ({}) meets compilation threshold but failed to be compiled",
                self.shared.module.funcs[func as usize].name
            );
            jit.slots_for(self.domain).slots[func as usize].store(FAIL_SENTINEL, Ordering::Release);
        }
    }

    /// c2i trampoline: fast signature, packs the arguments into a stack array, calls
    /// `mirvm_c2i` back into the interpreter. Any compile failure returns `None`; the
    /// caller then skips pre-warming this slot and stays interpreted, silently.
    pub(super) fn define_c2i_trampoline(
        &mut self,
        target: u32,
        abi: CalleeAbi,
    ) -> Option<(*const u8, Vec<JitSymbolRange>)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("t{target}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let params = b.block_params(entry).to_vec();
            let args_ss = b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                (abi.nparams.max(1) * 8) as u32,
                3,
            ));
            for (i, p) in params.iter().enumerate() {
                b.ins().stack_store(*p, args_ss, (i * 8) as i32);
            }
            let ret_ss =
                b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16, 3));
            let fref = self.module.declare_func_in_func(self.c2i, b.func);
            let fv = b.ins().iconst(types::I64, target as i64);
            let ap = b.ins().stack_addr(types::I64, args_ss, 0);
            let nv = b.ins().iconst(types::I64, abi.nparams as i64);
            let rp = b.ins().stack_addr(types::I64, ret_ss, 0);
            b.ins().call(fref, &[fv, ap, nv, rp]);
            // mirvm_c2i always writes both the (lo, hi) slots (helpers.rs); read back
            // according to this callee's return shape.
            match abi.nrets {
                0 => {
                    b.ins().return_(&[]);
                }
                1 => {
                    let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                    b.ins().return_(&[lo]);
                }
                _ => {
                    let lo = b.ins().stack_load(types::I64, ret_ss, 0);
                    let hi = b.ins().stack_load(types::I64, ret_ss, 8);
                    b.ins().return_(&[lo, hi]);
                }
            }
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if crate::options::get().jit_debug {
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        let symbol = PendingJitSymbol {
            id,
            func: target,
            role: JitSymbolRole::C2i,
            size,
        };
        self.module.clear_context(&mut cctx);
        if self.module.finalize_definitions().is_err() {
            return None;
        }
        self.register_pending_eh_frames();
        let entry = self.module.get_finalized_function(id);
        let ranges = self.finalized_symbol_ranges(vec![symbol]);
        Some((entry, ranges))
    }

    /// Fast body: bytecode blocks -> CLIF, slots -> SSA variables under the "I64
    /// zero-extended to width" invariant. Any compile failure returns `None` and keeps
    /// the function interpreted: a panic must never pollute the stderr differential.
    pub(super) fn define_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("f{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        // The Translator sets `has_try_call` while building; it is read outside the
        // builder to decide whether an LSDA is needed.
        let has_try_call;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let frame_offs = analyze_frame(body);
            let frame_ss = if !frame_offs.needs_frame() {
                None
            } else {
                Some(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    // A forced 0-byte frame materializes as 1 byte; the offset is still
                    // counted as 0, so nothing changes semantically.
                    // frame_align > 16: cranelift's x86_64 stack base only guarantees
                    // 16-byte alignment and there is no dynamic realignment, so the slot
                    // carries (align - 16) extra bytes and the translator aligns the
                    // entry in code as (addr + align - 1) & -align. A `__m256d` local
                    // reaching the 32-byte precondition of `mem::zeroed` is the case
                    // that motivates this.
                    if body.frame_align > 16 {
                        body.frame_size + (body.frame_align - 16)
                    } else {
                        body.frame_size.max(1)
                    },
                    body.frame_align.trailing_zeros() as u8,
                )))
            };
            let mut tr = Translator {
                shared: self.shared,
                domain: self.domain,
                module: &mut self.module,
                b: &mut b,
                vars: std::collections::HashMap::new(),
                frame_offs,
                frame_ss,
                unreachable: self.unreachable,
                c2i: self.c2i,
                call_main_catch: self.call_main_catch,
                memmove: self.memmove,
                memset: self.memset,
                memcmp: self.memcmp,
                div_zero: self.div_zero,
                volatile_load: self.volatile_load,
                volatile_store: self.volatile_store,
                call_indirect: self.call_indirect,
                tls_ref: self.tls_ref,
                call_foreign: self.call_foreign,
                call_builtin: self.call_builtin,
                alloc: self.alloc,
                terminate_abort: self.terminate_abort,
                call_terminate: self.call_terminate,
                unwind_resume: self.unwind_resume,
                exception_is_engine_fault: self.exception_is_engine_fault,
                trap: self.trap,
                simd_stmt: self.simd_stmt,
                simd_rv: self.simd_rv,
                poll_signals: self.poll_signals,
                host_syscall_trace: self.host_syscall_trace,
                exception_var: None,
                has_try_call: false,
                frame_base_var: None,
            };
            tr.build(func, body);
            has_try_call = tr.has_try_call;
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if crate::options::get().jit_debug {
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            if crate::options::get().jit_debug_dump {
                eprintln!(
                    "mirvm-jit-debug: CLIF dump of failed function f{func}:\n{}",
                    cctx.func.display()
                );
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            // A function with a try_call gets an LSDA, and it must list every call site,
            // handler-less ones included; build_lsda explains why.
            let lsda = if has_try_call {
                Some(build_lsda(&collect_call_sites(&cctx)))
            } else {
                None
            };
            self.pending_unwind.push((id, ui, lsda));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::FastBody,
                size,
            },
        ))
    }

    /// Packed entry `(args: *const u64, ret: *mut u64)`: one interp i2c hop.
    pub(super) fn define_packed(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let _ = body;
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        let id = self
            .module
            .declare_function(&format!("p{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let ps = b.block_params(entry).to_vec();
            let (argp, retp) = (ps[0], ps[1]);
            let mut args: Vec<Value> = Vec::with_capacity(abi.nparams);
            for i in 0..abi.nparams {
                args.push(
                    b.ins()
                        .load(types::I64, MemFlagsData::trusted(), argp, (i * 8) as i32),
                );
            }
            let fref = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(fref, &args);
            // Both (lo, hi) slots are always written, so shapes with sret/nrets = 0
            // store zero.
            let r0 = if abi.nrets >= 1 {
                b.inst_results(call)[0]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            let r1 = if abi.nrets >= 2 {
                b.inst_results(call)[1]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            b.ins().store(MemFlagsData::trusted(), r0, retp, 0);
            b.ins().store(MemFlagsData::trusted(), r1, retp, 8);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if crate::options::get().jit_debug {
                eprintln!("mirvm-jit-debug: define_function failed: {e:#?}");
            }
            return None;
        }
        if let Some(ui) = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
        {
            self.pending_unwind.push((id, ui, None));
        }
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.module.clear_context(&mut cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::Packed,
                size,
            },
        ))
    }
}
