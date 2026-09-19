//! Call resolution: `func_id` dedups an instance and enqueues it for lowering,
//! `resolve_call` classifies a callee, `freeze_foreign_sig` freezes a foreign signature,
//! and `exported_defs` supplies the rlib symbol set for the native-archive rescue chain.
//! `impl Linker` sub-block.

use super::*;

impl<'tcx> Linker<'tcx> {
    /// Maps an instance to a `FuncId`, assigning a new id and enqueuing it for lowering on
    /// first sight. On first sight the base image is consulted by `symbol_name`: a hit
    /// reuses the base id without enqueuing, and `symbol_name` is only computed when a base
    /// image exists, so the base-less path pays nothing.
    ///
    /// Split mode routes non-base instances by purity: an image-class (pure) instance gets
    /// a tagged id and joins the image queue, while a delta-class (local or tainted)
    /// instance takes an untagged id and joins the delta queue.
    pub(crate) fn func_id(&mut self, inst: Instance<'tcx>) -> ir::FuncId {
        if let Some(&id) = self.ids.get(&inst) {
            return id;
        }
        if !self.base_fns.is_empty()
            && let Some(&bid) = self.base_fns.get(self.tcx.symbol_name(inst).name)
        {
            self.ids.insert(inst, bid);
            return bid;
        }
        if let Some(s) = &mut self.split {
            let id = if classify_purity(inst).is_image() {
                let j = s.image_fn_next;
                s.image_fn_next += 1;
                let id = IMAGE_TAG | j;
                s.image_queue.push_back((id, inst));
                if s.image_funcs.len() <= j as usize {
                    s.image_funcs.resize_with(j as usize + 1, || None);
                }
                s.image_insts.push(inst);
                id
            } else {
                let id = self.next_fn;
                self.next_fn += 1;
                self.queue.push_back((id, inst));
                id
            };
            self.ids.insert(inst, id);
            return id;
        }
        let id = self.next_fn;
        self.next_fn += 1;
        self.ids.insert(inst, id);
        self.queue.push_back((id, inst));
        id
    }

    /// Resolves the callee at a call site (for the `Call` terminator). An `Err` traps the
    /// block with a phrased diagnostic.
    pub(crate) fn resolve_call(&mut self, inst: Instance<'tcx>) -> Result<Callee, String> {
        // An intrinsic with a fallback body is collected like an ordinary function: the
        // collector skips replaced_intrinsics, so the interpreted view must collect it
        // itself, constructing the instance the same way (Instance::new_raw). One that
        // must_be_overridden belongs to the engine builtin table.
        if let InstanceKind::Intrinsic(def_id) = inst.def {
            let intrinsic = self
                .tcx
                .intrinsic(def_id)
                .expect("InstanceKind::Intrinsic always has an IntrinsicDef");
            if intrinsic.must_be_overridden {
                return Err(format!(
                    "intrinsic `{}` has no fallback body (engine builtin table)",
                    intrinsic.name
                ));
            }
            let item = Instance::new_raw(def_id, inst.args);
            return Ok(Callee::Func(self.func_id(item)));
        }
        if let InstanceKind::Virtual(..) = inst.def {
            return Err("dyn virtual call dispatch is not supported".into());
        }
        if self.tcx.is_foreign_item(inst.def_id()) {
            let link_name = Symbol::intern(canonical_link_name(self.tcx.symbol_name(inst).name));
            // (1) Engine primitives: allocation, unwinding, stubs and fast-path passthrough.
            if let Some(b) = self.builtins.get(&link_name).cloned() {
                return Ok(Callee::Builtin(b));
            }
            // (2) Link simulation: look the symbol name up among the exported definitions of
            // the linked crates, mirroring tier-0 `find_exported_symbol`. panic_impl ->
            // rust_begin_unwind and __rdl_* take this path.
            let target = self.exported_defs().get(&link_name).copied();
            if let Some((target, is_weak)) = target {
                // Native linker semantics: a weak definition yields to a strong symbol in a
                // dynamic library (compiler-builtins' weak sqrt/memcmp versus libc/libm).
                // Rust-internal ABI symbols are the exception: the host process
                // (librustc_driver) exports them too, and passing them through would break
                // the engine's heap and panic model.
                let name = link_name.as_str();
                let rust_internal = name.starts_with("__rust")
                    || name.starts_with("__rdl")
                    || name.starts_with("rust_");
                if is_weak && !rust_internal {
                    let cname = std::ffi::CString::new(name).unwrap();
                    let strong = crate::os::dll::sym(0, &cname);
                    if strong != 0 {
                        return self.freeze_foreign_sig(inst, name);
                    }
                }
                return Ok(Callee::Func(self.func_id(target)));
            }
            // (3) os:: passthrough: the denylist is rejected; everything else goes through
            // dlsym and libffi using the frozen signature.
            let name = link_name.as_str();
            if DENY_EXACT.contains(&name) || DENY_PREFIX.iter().any(|p| name.starts_with(p)) {
                return Err(format!(
                    "foreign `{name}` (denylisted: thread and process model are not passed through)"
                ));
            }
            if name.starts_with("llvm.") {
                return Err(format!(
                    "foreign `{name}` (LLVM-internal symbol, built in on demand)"
                ));
            }
            return self.freeze_foreign_sig(inst, name);
        }
        // A naked fn's body is raw machine code with no ordinary MIR body. It is
        // materialized into the global-asm `.so` during collection, and the call site
        // passes through as a foreign call to its mangled symbol under the real ABI.
        if self
            .tcx
            .codegen_fn_attrs(inst.def_id())
            .flags
            .contains(rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags::NAKED)
        {
            let name = canonical_link_name(self.tcx.symbol_name(inst).name);
            return self.freeze_foreign_sig(inst, name);
        }
        // An ordinary function: extend the worklist closure, since cross-crate non-generic
        // functions are not in the collector's seed set.
        Ok(Callee::Func(self.func_id(inst)))
    }

    /// Freezes an os:: passthrough signature: a foreign fn signature becomes a list of
    /// `FfiKind`s, mirroring tier-0 `ty_to_ffitype`. An fn-pointer parameter (such as
    /// pthread_create's thread_start) additionally freezes its **inner signature**: if that
    /// slot receives an fn entry address at execution time, the thunk factory materializes
    /// real machine code before passing it through.
    pub(super) fn freeze_foreign_sig(
        &mut self,
        inst: Instance<'tcx>,
        name: &str,
    ) -> Result<Callee, String> {
        let sig = self
            .tcx
            .fn_sig(inst.def_id())
            .instantiate(self.tcx, inst.args)
            .skip_binder();
        let unwind = crate::lower::ffi_sig::c_abi_unwind(sig.abi()).ok_or_else(|| {
            format!(
                "foreign `{name}` ABI {:?} is not supported for libffi passthrough (only C/System \
                 and their unwind forms)",
                sig.abi()
            )
        })?;
        let env = TypingEnv::fully_monomorphized();
        let mut args = Vec::with_capacity(sig.inputs().len());
        let mut thunk_args = Vec::new();
        for (i, &t) in sig.inputs().iter().enumerate() {
            args.push(ffi_kind_of(self.tcx, env, t).map_err(|e| {
                format!(
                    "foreign `{name}` argument {t}: {e} (libffi passthrough accepts only scalars \
                     and pointers)"
                )
            })?);
            // fn-pointer parameter slot: a bare fn pointer or `Option<fn>` (a nullable
            // callback such as pthread_key_create's dtor, where the niche layout makes None
            // zero and it passes through unchanged). An unfreezable inner signature traps
            // the whole call site, because passing an entry address straight to native code
            // is a silent crash.
            let fnptr_ty = if t.is_fn_ptr() {
                Some(t)
            } else if let rustc_middle::ty::TyKind::Adt(def, sub) = t.kind()
                && self
                    .tcx
                    .is_diagnostic_item(rustc_span::sym::Option, def.did())
                && sub.type_at(0).is_fn_ptr()
            {
                Some(sub.type_at(0))
            } else {
                None
            };
            if let Some(t) = fnptr_ty {
                let inner = t.fn_sig(self.tcx).skip_binder();
                if inner.c_variadic() {
                    return Err(format!(
                        "foreign `{name}` argument {t}: variadic callbacks do not support a thunk"
                    ));
                }
                // The C-unwind callback's unwind property is preserved in the inner
                // signature, so the thunk/P1 factory can pick an unwindable entry.
                let inner_unwind =
                    crate::lower::ffi_sig::c_abi_unwind(inner.abi()).ok_or_else(|| {
                        format!(
                            "foreign `{name}` callback `{t}` ABI {:?} is not supported for a thunk \
                             (only C/System and their unwind forms)",
                            inner.abi()
                        )
                    })?;
                let mut in_args = Vec::with_capacity(inner.inputs().len());
                for &it in inner.inputs() {
                    let k = ffi_kind_of(self.tcx, env, it).map_err(|e| {
                        format!(
                            "foreign `{name}` callback argument {it}: {e} (a thunk accepts only \
                             scalars and pointers)"
                        )
                    })?;
                    if k == ir::FfiKind::Void {
                        return Err(format!(
                            "foreign `{name}` callback argument {it}: ZST cannot be a cif argument"
                        ));
                    }
                    in_args.push(k);
                }
                let in_ret = ffi_kind_of(self.tcx, env, inner.output()).map_err(|e| {
                    format!("foreign `{name}` callback return {}: {e}", inner.output())
                })?;
                thunk_args.push((
                    i,
                    ir::ForeignSig {
                        args: in_args,
                        ret: in_ret,
                        fixed: None,
                        thunk_args: vec![],
                        unwind: inner_unwind,
                    },
                ));
            }
        }
        let ret = ffi_kind_of(self.tcx, env, sig.output())
            .map_err(|e| format!("foreign `{name}` return {}: {e}", sig.output()))?;
        Ok(Callee::Foreign {
            sym: name.into(),
            args,
            ret,
            variadic: sig.c_variadic(),
            thunk_args,
            unwind,
        })
    }

    /// Lazily built, once: the exported-symbol table used by link simulation. It walks
    /// every non-generic exported def that the final binary would link to, mirroring tier-0
    /// `for_each_linked_def`, and maps symbol name -> mono instance, with strong beating
    /// weak. A guest export (`#[no_mangle]`, `#[export_name]` or `#[used]`, non-generic)
    /// becomes `(defining instance, is_weak)`. This is the authoritative table for link
    /// simulation, drawn from the same symbol set as the native final link's
    /// `exported_non_generic_symbols`. The native-archive "is the symbol in an rlib" closure
    /// test reads the same table, mapping a name to a materializable P1 entry.
    pub(crate) fn exported_defs(&mut self) -> &FxHashMap<Symbol, (Instance<'tcx>, bool)> {
        let tcx = self.tcx;
        self.exports.get_or_insert_with(|| {
            use rustc_hir::def_id::LOCAL_CRATE;
            use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
            use rustc_middle::middle::exported_symbols::ExportedSymbol;
            use rustc_session::config::CrateType;

            // (instance, is_weak): a non-weak entry overrides a weak one
            let mut map: FxHashMap<Symbol, (Instance<'tcx>, bool)> = FxHashMap::default();
            let mut add = |def_id: rustc_hir::def_id::DefId| {
                if tcx.is_foreign_item(def_id)
                    || !matches!(tcx.def_kind(def_id), rustc_hir::def::DefKind::Fn)
                {
                    return;
                }
                let inst = Instance::mono(tcx, def_id);
                let name = Symbol::intern(tcx.symbol_name(inst).name);
                let is_weak = tcx.codegen_fn_attrs(def_id).linkage
                    == Some(rustc_hir::attrs::Linkage::WeakAny);
                match map.entry(name) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if e.get().1 && !is_weak {
                            e.insert((inst, false));
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert((inst, is_weak));
                    }
                }
            };

            // Local crate: walk the HIR, because exported_symbols misses #[used]
            for def_id in tcx.hir_crate_items(()).definitions() {
                if !tcx.def_kind(def_id).has_codegen_attrs() {
                    continue;
                }
                let attrs = tcx.codegen_fn_attrs(def_id);
                let exported = attrs.contains_extern_indicator()
                    || attrs.flags.contains(CodegenFnAttrFlags::USED_COMPILER)
                    || attrs.flags.contains(CodegenFnAttrFlags::USED_LINKER);
                if !exported || tcx.generics_of(def_id).requires_monomorphization(tcx) {
                    continue;
                }
                add(def_id.into());
            }
            // Non-generic exported symbols of dependency crates
            let dependency_formats = tcx.dependency_formats(());
            if let Some(format) = dependency_formats.get(&CrateType::Executable) {
                for (cnum, &linkage) in format.iter_enumerated() {
                    if cnum == LOCAL_CRATE
                        || linkage == rustc_middle::middle::dependency_format::Linkage::NotLinked
                    {
                        continue;
                    }
                    for &(symbol, _) in tcx.exported_non_generic_symbols(cnum) {
                        if let ExportedSymbol::NonGeneric(def_id) = symbol {
                            add(def_id);
                        }
                    }
                }
            }
            map
        })
    }
}
