//! Reusable backend output for C2I trampolines with the same flattened signature.

use super::*;
use cranelift_module::{DataId, ModuleReloc, ModuleRelocTarget};

const TARGET_PREFIX: &str = "mirvm_c2i_id_";

/// These symbols denote integer ids, not storage to dereference.
pub(super) fn target_id(name: &str) -> Option<*const u8> {
    name.strip_prefix(TARGET_PREFIX)?
        .parse::<u32>()
        .ok()
        .map(|id| id as usize as *const u8)
}

pub(super) struct Template {
    code: Vec<u8>,
    relocs: Vec<ModuleReloc>,
    target: DataId,
    alignment: u64,
    unwind: UnwindInfo,
}

impl Compiler<'_> {
    /// Define a callee-specific copy in the module's normal arena. Finalization and unwind
    /// registration still belong to the caller's publication batch.
    pub(super) fn define_c2i_trampoline(
        &mut self,
        target: u32,
        abi: CalleeAbi,
    ) -> Option<PendingJitSymbol> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("t{target}"), Linkage::Local, &sig)
            .ok()?;
        // sret, pairs, indirect parameters and caller locations are already flattened into words.
        let shape = (abi.nparams, abi.nrets);
        if !self.c2i_templates.contains_key(&shape) {
            let template = self.compile_c2i_template(id, abi)?;
            self.c2i_templates.insert(shape, template);
        }
        let target_symbol = self
            .module
            .declare_data(
                &format!("{TARGET_PREFIX}{target}"),
                Linkage::Import,
                false,
                false,
            )
            .ok()?;
        let template = &self.c2i_templates[&shape];
        let mut relocs = template.relocs.clone();
        for reloc in &mut relocs {
            if matches!(reloc.name, ModuleRelocTarget::User { namespace: reloc::DATA_NAMESPACE, index } if index == template.target.as_u32())
            {
                reloc.name = target_symbol.into();
            } else if let ModuleRelocTarget::FunctionOffset(_, offset) = reloc.name {
                // Backend-local references follow this copy, not the first instance of the shape.
                reloc.name = ModuleRelocTarget::FunctionOffset(id, offset);
            }
        }
        self.module
            .define_function_bytes(id, template.alignment, &template.code, &relocs)
            .ok()?;
        self.pending_unwind
            .push((id, template.unwind.clone(), None));
        Some(PendingJitSymbol {
            id,
            func: target,
            role: JitSymbolRole::C2i,
            size: template.code.len() as u64,
        })
    }

    fn compile_c2i_template(&mut self, id: ClifFuncId, abi: CalleeAbi) -> Option<Template> {
        let target = self
            .module
            .declare_data("mirvm_c2i_template_id", Linkage::Import, false, false)
            .ok()?;
        let mut cctx = self.take_context();
        cctx.func.signature = self.fast_sig(abi);
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
            let global = self.module.declare_data_in_func(target, b.func);
            let fv = b.ins().global_value(types::I64, global);
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

        // The normal verifier, optimization and backend passes run once for this signature.
        cctx.compile(self.module.isa(), &mut Default::default())
            .ok()?;
        let unwind = self.registrable_unwind(&cctx)?;
        let compiled = cctx.compiled_code()?;
        let template = Template {
            code: compiled.code_buffer().to_vec(),
            relocs: compiled
                .buffer
                .relocs()
                .iter()
                .map(|reloc| ModuleReloc::from_mach_reloc(reloc, &cctx.func, id))
                .collect(),
            target,
            alignment: u64::from(compiled.buffer.alignment),
            unwind,
        };
        self.recycle_context(cctx);
        Some(template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static LAST: RefCell<(u64, Vec<u64>)> = const { RefCell::new((0, Vec::new())) };
    }

    #[repr(C)]
    #[derive(Debug, PartialEq, Eq)]
    struct Pair(u64, u64);

    extern "C" fn probe(target: u64, args: *const u64, count: u64, ret: *mut u64) {
        let args = unsafe { std::slice::from_raw_parts(args, count as usize) };
        LAST.with(|last| *last.borrow_mut() = (target, args.to_vec()));
        unsafe {
            ret.write(target);
            ret.add(1).write(args.iter().fold(0, |sum, v| sum ^ v));
        }
    }

    #[test]
    fn templates_rebind_ids_and_preserve_register_and_stack_arguments() {
        let args = [u64::MAX, 7, 0x8000_0000_0000_0000, 9, 13, 17, 23, 31];
        for domain in [CodeDomain::Plain, CodeDomain::Trace] {
            for tier in [Tier::Baseline, Tier::Optimized] {
                let shared = Shared::new(ir::Module::default());
                let mut compiler = Compiler::with_tier(&shared, domain, tier);
                let mut sig = compiler.module.make_signature();
                sig.params = vec![AbiParam::new(types::I64); 4];
                compiler
                    .values
                    .lock()
                    .unwrap()
                    .insert("test_c2i_probe".into(), probe as *const u8 as usize);
                compiler.c2i = compiler
                    .module
                    .declare_function("test_c2i_probe", Linkage::Import, &sig)
                    .unwrap();
                let mut copies = Vec::new();
                let mut shapes = 0u32;
                for nparams in [0, 2, 8] {
                    for nrets in 0..=2 {
                        for target in [shapes, 0x8000_0000 + shapes, u32::MAX - shapes] {
                            let abi = CalleeAbi {
                                nparams,
                                nrets,
                                sret: nparams > 0 && nrets == 0 && target > 100,
                            };
                            let symbol = compiler.define_c2i_trampoline(target, abi).unwrap();
                            copies.push((symbol.id, target, nparams, nrets));
                        }
                        shapes += 1;
                        assert_eq!(compiler.c2i_templates.len(), shapes as usize);
                    }
                }
                compiler.module.finalize_definitions().unwrap();
                compiler.register_pending_eh_frames();
                // Published copies must own their bytes and unwind data independently of the cache.
                compiler.c2i_templates.clear();
                for (id, target, nparams, nrets) in copies {
                    let entry = compiler.module.get_finalized_function(id);
                    macro_rules! invoke {
                        ($ret:ty) => {{
                            match nparams {
                                0 => unsafe { std::mem::transmute::<*const u8, extern "C" fn() -> $ret>(entry)() },
                                2 => unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> $ret>(entry)(args[0], args[1]) },
                                8 => unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> $ret>(entry)(args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7]) },
                                _ => unreachable!(),
                            }
                        }};
                    }
                    match nrets {
                        0 => invoke!(()),
                        1 => assert_eq!(invoke!(u64), u64::from(target)),
                        2 => assert_eq!(
                            invoke!(Pair),
                            Pair(
                                u64::from(target),
                                args[..nparams].iter().fold(0, |sum, v| sum ^ v)
                            )
                        ),
                        _ => unreachable!(),
                    }
                    LAST.with(|last| {
                        assert_eq!(
                            *last.borrow(),
                            (u64::from(target), args[..nparams].to_vec())
                        )
                    });
                }
                std::mem::forget(compiler);
            }
        }
    }
}
