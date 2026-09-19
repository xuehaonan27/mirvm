//! Fn entries and FFI signatures: `fn_entry_addr` (an FFI-derivable entry is executable and its value is
//! the local stub code address), `entry_ffi_sig`, `alloc_entry_stub` and `foreign_fn_entry_addr`.
//! An `impl Linker` sub-block; the fields are in the Linker struct in mod.rs.

use super::*;

impl<'tcx> Linker<'tcx> {
    /// fn-ptr entry address: every instance has exactly one real address identity.
    ///
    /// An FFI-derivable entry is **executable**: its value is the local stub code address, so any path
    /// flowing to native code lands at the executable entry and the thunk blind spot cannot arise. Other
    /// entries stay data slots holding the FuncId (for debugging): Rust ABI, by-value aggregates and
    /// variadic functions have no legal native calling surface.
    ///
    /// If the base function already has an entry, reuse it — one address identity keeps base vtables and
    /// delta addressing consistent. If it has none (its address was not taken at build time), add one in
    /// the delta area; there is still exactly one copy.
    ///
    /// In split mode the entry is domain-assigned by instance class (image class → image area, still a
    /// single address identity). An image context reaching a delta-class entry violates the purity closure
    /// (a classifier bug) and is loudly rejected.
    ///
    /// An extern fn — a kernel function declared in an extern block inside an fn body and used as an
    /// fn-ptr, i.e. the ring dispatch pattern — has no MIR to lower and goes through
    /// `foreign_fn_entry_addr`: its value is the real symbol address resolved by the native linker.
    pub(crate) fn fn_entry_addr(&mut self, inst: Instance<'tcx>) -> Result<u64, String> {
        if let Some(&a) = self.fn_entries.get(&inst) {
            return Ok(a);
        }
        if self.tcx.is_foreign_item(inst.def_id()) {
            return self.foreign_fn_entry_addr(inst);
        }
        let fid = self.func_id(inst);
        if fid < self.delta_first_fn
            && let Some(&a) = self.base_fn_entries.get(self.tcx.symbol_name(inst).name)
        {
            self.fn_entries.insert(inst, a);
            return Ok(a);
        }
        // FFI-derivable ⇒ executable entry (value = local stub code address)
        if let Some(sig) = self.entry_ffi_sig(inst) {
            let addr = self.alloc_entry_stub(inst, fid, sig);
            self.fn_entries.insert(inst, addr);
            self.fn_addrs.insert(addr, fid);
            return Ok(addr);
        }
        let addr = if let Some(s) = &mut self.split {
            if fid & IMAGE_TAG != 0 || fid < self.delta_first_fn {
                // image class, or a base hit whose base has no entry: allocate in the image area for a single
                // address identity. The delta area is unstable across runs for image bytecode, so it must
                // never be used there.
                let a = s.image_frozen.alloc(8, 16);
                s.image_fn_entries.insert(inst, a);
                a
            } else {
                if s.current_image {
                    panic!(
                        "A2 closure violation: image instance references delta-class fn entry (classifier missed): {}",
                        self.tcx.symbol_name(inst).name
                    );
                }
                self.frozen.alloc(8, 16)
            }
        } else {
            self.frozen.alloc(8, 16)
        };
        unsafe { (addr as *mut u64).write(fid as u64) };
        self.fn_entries.insert(inst, addr);
        self.fn_addrs.insert(addr, fid);
        Ok(addr)
    }

    /// The instance's frozen cif signature, or `None` when it cannot be derived from a `FnDef`. The result
    /// is cached including `None`, so a non-derivable instance goes through a data slot without repeated
    /// probing. The native-archive rescue chain judges "rlib fn materializable entry" by this same criterion.
    pub(crate) fn entry_ffi_sig(&mut self, inst: Instance<'tcx>) -> Option<ir::ForeignSig> {
        if let Some(sig) = self.entry_sig_cache.get(&inst) {
            return sig.clone();
        }
        let env = TypingEnv::fully_monomorphized();
        let ty = inst.ty(self.tcx, env);
        let sig = if matches!(ty.kind(), rustc_middle::ty::FnDef(..)) {
            freeze_c_fnptr_sig(self.tcx, env, ty)
        } else {
            None
        };
        self.entry_sig_cache.insert(inst, sig.clone());
        sig
    }

    /// Allocate an executable entry: its value is a local stub code address. The domain follows the instance
    /// class — image class always lands in the image domain, which is stable across runs. Layout order is
    /// stub offset, and the startup phase materializes in that same order to reproduce it.
    pub(super) fn alloc_entry_stub(
        &mut self,
        inst: Instance<'tcx>,
        fid: ir::FuncId,
        sig: ir::ForeignSig,
    ) -> u64 {
        let image_side =
            self.split.is_some() && (fid & IMAGE_TAG != 0 || fid < self.delta_first_fn);
        if let Some(&i) = self.entry_stub_ids.get(&inst) {
            return if image_side {
                self.split
                    .as_ref()
                    .expect("split")
                    .image_code_arena
                    .addr_of(i as u64)
            } else {
                self.code_arena.addr_of(i as u64)
            };
        }
        if image_side {
            let s = self.split.as_mut().expect("split");
            let i = s.image_stub_sites.len() as u32;
            let addr = s.image_code_arena.addr_of(i as u64);
            s.image_stub_sites.push(ir::EntryStubSite {
                link_addr: ir::LinkAddr(addr),
                func: fid,
                sig,
            });
            s.image_fn_entries.insert(inst, addr);
            self.entry_stub_ids.insert(inst, i);
            addr
        } else {
            if self.split.as_ref().is_some_and(|s| s.current_image) {
                panic!(
                    "A2 closure violation: image instance references delta-class fn entry (classifier missed): {}",
                    self.tcx.symbol_name(inst).name
                );
            }
            let i = self.entry_stub_sites.len() as u32;
            let addr = self.code_arena.addr_of(i as u64);
            self.entry_stub_sites.push(ir::EntryStubSite {
                link_addr: ir::LinkAddr(addr),
                func: fid,
                sig,
            });
            self.entry_stub_ids.insert(inst, i);
            addr
        }
    }

    /// extern fn entry address for fn-ptr address-taking. A foreign item has no MIR and cannot enter the
    /// worklist (the `instance_mir` query would panic); its fn-ptr value is the real symbol address resolved
    /// by the native linker. Resolution order, isomorphic to `resolve_call`: ① engine built-ins
    /// ② exported-symbol simulation ③ denylist/llvm/rust-internal ④ archive hidden fallback table, then
    /// global dlsym.
    ///
    /// The value starts as this process's real code address, but consumers read it through GOT slots. Slots
    /// are serialized with the module and refilled by name at startup, so the module is position-independent
    /// with respect to ASLR.
    pub(super) fn foreign_fn_entry_addr(&mut self, inst: Instance<'tcx>) -> Result<u64, String> {
        let name = canonical_link_name(self.tcx.symbol_name(inst).name);
        // An absent weak extern taken as an address is NULL, as in native code; the weak flag tells the GOT
        // startup phase to write 0 on a miss instead of aborting.
        let weak = self.tcx.codegen_fn_attrs(inst.def_id()).import_linkage
            == Some(rustc_hir::attrs::Linkage::ExternalWeak);
        if let Some(&a) = self.foreign_fn_entries.get(&inst) {
            // A cache hit must still ensure the current context's slot exists (slots are split by
            // (name, context)).
            let _ = self.foreign_slot(name, a, weak);
            return Ok(a);
        }
        let bake = |this: &mut Self, addr: u64| {
            // GOT: slots start with this process's resolved value and are refilled by name at startup.
            let _ = this.foreign_slot(name, addr, weak);
            this.foreign_fn_entries.insert(inst, addr);
            addr
        };
        let link_name = Symbol::intern(name);
        // ① Engine built-ins. A pure passthrough built-in (bit-identical semantics to the generic
        // dlsym+libffi path) yields a real address; the others (alloc/unwind/fork/atexit/signal/backtrace
        // families) are engine-taken-over semantics with no address to materialize, so abort loudly — the
        // host process also exports __rust/_Unwind symbols, and taking them directly would punch through the
        // engine's heap/panic/unwind model.
        if let Some(b) = self.builtins.get(&link_name).cloned() {
            use ir::Builtin as B;
            if !matches!(
                b,
                B::HostGetenv | B::HostWrite | B::HostStrlen | B::HostAbort
            ) {
                return Err(format!(
                    "extern fn `{name}` taken as value address (fn-ptr), but it is an engine built-in semantic symbol with no address to materialize"
                ));
            }
        }
        let rust_internal =
            name.starts_with("__rust") || name.starts_with("__rdl") || name.starts_with("rust_");
        // ② Link simulation: the symbol is provided by an exported definition of an already-linked crate, so
        // the value is the entry address that guest defines. A weak definition yields to a dynamic library's
        // strong symbol, except for Rust internal symbols (see ①).
        let exported = self.exported_defs().get(&link_name).copied();
        if let Some((target, is_weak)) = exported {
            if is_weak && !rust_internal {
                let cname = std::ffi::CString::new(name)
                    .map_err(|_| "symbol name contains NUL".to_string())?;
                let strong = crate::os::dll::sym(0, &cname);
                if strong != 0 {
                    return Ok(bake(self, strong as u64));
                }
            }
            return self.fn_entry_addr(target);
        }
        // ③ Same non-passthrough list discipline as resolve_call
        if DENY_EXACT.contains(&name) || DENY_PREFIX.iter().any(|p| name.starts_with(p)) {
            return Err(format!(
                "foreign `{name}` taken as value address (denylist: thread M4.4 / process model non-passthrough)"
            ));
        }
        if name.starts_with("llvm.") {
            return Err(format!(
                "foreign `{name}` taken as value address (LLVM internal symbol, built-in on demand)"
            ));
        }
        if rust_internal {
            return Err(format!(
                "foreign `{name}` taken as value address (Rust internal ABI symbol, host process also exports it, cannot take directly)"
            ));
        }
        // ④ Resolution order: hidden fallback table → archive handles in link order → global dlsym.
        // Objects the guest links in always beat host-process libraries of the same name, whether they are
        // visible in .dynsym or only hidden. Archive `.so` files are loaded with RTLD_NOW|RTLD_GLOBAL at the
        // head of `lower_inner`, before the worklist is drained; their handles live in `self.archive_handles`.
        // Global dlsym is the last fallback to real system libraries.
        let cname =
            std::ffi::CString::new(name).map_err(|_| "symbol name contains NUL".to_string())?;
        let mut p = 0u64;
        for (bias, syms) in &self.archive_fallbacks {
            if let Some(&v) = syms.get(name) {
                p = bias + v;
                break;
            }
        }
        if p == 0 {
            for &h in &self.archive_handles {
                p = crate::os::dll::sym(h, &cname) as u64;
                if p != 0 {
                    break;
                }
            }
        }
        if p == 0 {
            p = crate::os::dll::sym(0, &cname) as u64;
        }
        if p == 0 {
            // An absent weak symbol is NULL, as in native address-taking of an undefined weak symbol. An
            // indirect call through it terminates loudly at execution, via the CallIndirect null-pointer
            // diagnostics.
            if weak {
                return Ok(bake(self, 0));
            }
            return Err(format!(
                "extern fn `{name}` taken as value address, but symbol not found (neither archive fallback table nor global dlsym)"
            ));
        }
        Ok(bake(self, p))
    }
}
