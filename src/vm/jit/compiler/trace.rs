//! The trace domain's entry point: the pinned register the recorder is reached through, and the
//! stub that installs it around a trace body.

use super::*;

impl<'a> Compiler<'a> {
    /// Define and publish the trace domain's boundary entry.
    ///
    /// Trace bodies address the recorder through the pinned register, which is
    /// not callee-saved under that convention, so `r15` must be installed on the
    /// way in and restored on the way out -- including when the call unwinds, or
    /// ABI-conforming Rust would get a clobbered callee-saved register back.
    /// This is the one place that happens; internal trace calls inherit the pin.
    ///
    /// If the definition fails nothing is published, and [`Compiler::compile`]
    /// refuses to produce a trace body, so the interpreter (which records through
    /// TLS) stays the fallback exactly as it does for any other compile failure.
    pub(super) fn install_trace_enter(&mut self) {
        let Some(id) = self.define_trace_enter() else {
            return;
        };
        if self.module.finalize_definitions().is_err() {
            return;
        }
        self.register_pending_eh_frames();
        let addr = self.module.get_finalized_function(id) as u64;
        debug_assert_ne!(addr, 0, "a finalized trace entry has an address");
        self.shared.jit.trace_enter.store(addr, Ordering::Release);
    }

    pub(super) fn define_trace_enter(&mut self) -> Option<ClifFuncId> {
        use cranelift_codegen::ir::{
            BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
        };
        let mut sig = self.module.make_signature();
        for _ in 0..4 {
            sig.params.push(AbiParam::new(types::I64));
        }
        let id = self
            .module
            .declare_function("mirvm_trace_enter", Linkage::Local, &sig)
            .ok()?;
        let mut cctx = self.module.make_context();
        cctx.func.signature = sig.clone();
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let ps = b.block_params(entry).to_vec();
            let (producer, body, args, ret) = (ps[0], ps[1], ps[2], ps[3]);

            let saved = b.ins().get_pinned_reg(types::I64);
            b.ins().set_pinned_reg(producer);

            let pad = b.create_block();
            b.append_block_param(pad, types::I64);
            let ok = b.create_block();
            let normal = BlockCall::new(
                ok,
                std::iter::empty::<BlockArg>(),
                &mut b.func.dfg.value_lists,
            );
            let pad_call = b.func.dfg.block_call(pad, &[BlockArg::TryCallExn(0)]);
            let mut body_sig = self.module.make_signature();
            body_sig.params.push(AbiParam::new(types::I64));
            body_sig.params.push(AbiParam::new(types::I64));
            let sigref = b.func.import_signature(body_sig);
            let et = b.func.dfg.exception_tables.push(ExceptionTableData::new(
                sigref,
                normal,
                [ExceptionTableItem::Tag(
                    ExceptionTag::with_number(0).unwrap(),
                    pad_call,
                )],
            ));
            b.ins().try_call_indirect(body, &[args, ret], et);

            b.switch_to_block(ok);
            b.ins().set_pinned_reg(saved);
            b.ins().return_(&[]);

            b.switch_to_block(pad);
            let exn = b.block_params(pad)[0];
            b.ins().set_pinned_reg(saved);
            let resume = self.module.declare_func_in_func(self.unwind_resume, b.func);
            b.ins().call(resume, &[exn]);
            b.ins().trap(TrapCode::user(1).unwrap());

            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if crate::options::get().jit_debug {
                eprintln!("mirvm-jit-debug: trace boundary define failed: {e:#?}");
            }
            return None;
        }
        let ui = cctx
            .compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())?;
        let lsda = build_lsda(&collect_call_sites(&cctx));
        self.pending_unwind.push((id, ui, Some(lsda)));
        self.module.clear_context(&mut cctx);
        Some(id)
    }
}
