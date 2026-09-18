//! P1 fn entries + FFI signatures (carried over intact from lower/mod.rs M5 entries): fn_entry_addr
//! (FFI-derivable entry executable = local stub code address) / entry_ffi_sig / alloc_entry_stub /
//! foreign_fn_entry_addr. impl Linker sub-block; fields are in the Linker struct in mod.rs.

use super::*;

impl<'tcx> Linker<'tcx> {
    /// fn-ptr entry address (D4): each instance has a real address identity.
    /// P1 (decision-history §7.6): FFI-derivable entries **executable** — value = local stub
    /// code address (any path flowing to native lands at the executable entry, structurally
    /// eliminating the thunk blind spot); others remain data slots (containing FuncId, for
    /// debugging; Rust ABI / aggregate / variadic have no legal native calling surface).
    /// S4: reuse if the base function already has an entry (single address identity; base vtable
    /// and delta addressing are consistent); if the base function has no entry (not address-taken
    /// at build time), add one in the delta area — total still exactly one copy.
    /// A2 split: entries are domain-assigned by instance class (image class → image area, single
    /// address identity unchanged); image context encountering delta class = purity closure violated
    /// (classifier bug), loudly rejected.
    /// extern fn (kernel function declared in an extern block inside an fn body, used as fn-ptr,
    /// ring dispatch pattern): no MIR to lower, go through foreign_fn_entry_addr — value = real
    /// symbol address resolved by the native linker.
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
        // P1: FFI-derivable ⇒ executable entry (value = local stub code address)
        if let Some(sig) = self.entry_ffi_sig(inst) {
            let addr = self.alloc_entry_stub(inst, fid, sig);
            self.fn_entries.insert(inst, addr);
            self.fn_addrs.insert(addr, fid);
            return Ok(addr);
        }
        let addr = if let Some(s) = &mut self.split {
            if fid & IMAGE_TAG != 0 || fid < self.delta_first_fn {
                // image class, or base hit but base has no entry (split variant for S4 adding entries):
                // image area — single address identity (delta referencing image domain is always
                // stable; delta area is unstable across runs for image bytecode, must never be used).
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

    /// P1: instance's frozen cif signature (FnDef and freeze_c_fnptr_sig derivable);
    /// result cached (including None — non-derivable always goes through data slot, no repeated
    /// probing cost).
    /// native_archive C2 rescue chain judges "rlib fn materializable entry" by the same criterion.
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

    /// P1 executable entry allocation (§7.6): value = local stub code address (domain determined
    /// by instance class — image class always image domain, stable across runs; layout order =
    /// stub offset, startup phase materializes in the same order to reproduce).
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

    /// extern fn entry address (fn-ptr address-taking): foreign items without MIR cannot enter
    /// worklist (instance_mir = rustc query panic); its fn-ptr value semantics = real symbol
    /// address resolved by the native linker. Resolution order is isomorphic to resolve_call:
    /// ① engine built-ins ② exported-symbol simulation ③ denylist/llvm/rust-internal
    /// ④ archive hidden fallback table → global dlsym.
    /// Value still initially filled with host real code address, but consumers read through GOT
    /// slots (P2, decision-history §7.5c): slots are serialized with the module and refilled by
    /// name at startup, so the module is position-independent wrt. ASLR.
    pub(super) fn foreign_fn_entry_addr(&mut self, inst: Instance<'tcx>) -> Result<u64, String> {
        let name = canonical_link_name(self.tcx.symbol_name(inst).name);
        // extern weak absent address-taking = NULL (same semantics as native); weak flag tells
        // GOT startup phase to write 0 on miss instead of aborting
        let weak = self.tcx.codegen_fn_attrs(inst.def_id()).import_linkage
            == Some(rustc_hir::attrs::Linkage::ExternalWeak);
        if let Some(&a) = self.foreign_fn_entries.get(&inst) {
            // P2: cache hit must also guarantee [current context] slot present (slots split by
            // (name, context))
            let _ = self.foreign_slot(name, a, weak);
            return Ok(a);
        }
        let bake = |this: &mut Self, addr: u64| {
            // P2 GOT: slots initially filled with this process's resolved value, refilled by name
            // at startup
            let _ = this.foreign_slot(name, addr, weak);
            this.foreign_fn_entries.insert(inst, addr);
            addr
        };
        let link_name = Symbol::intern(name);
        // ① Engine built-ins: pure passthrough fast path (semantics bit-identical to generic
        // dlsym+libffi path) can give real address; other built-ins (alloc/unwind/fork/atexit/
        // signal/backtrace families) are engine-taken-over semantics, no address to materialize —
        // loudly Trap (host process also exports __rust/_Unwind symbols; directly taking punches
        // through the engine's heap/panic/unwind model).
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
        // ② Link simulation: symbol provided by exported definition of already-linked crate ⇒
        // value = entry address defined by that guest; weak definition yields to dynamic library
        // strong symbol (except Rust internal symbols — see note ①).
        let exported = self.exported_defs().get(&link_name).copied();
        if let Some((target, is_weak)) = exported {
            if is_weak && !rust_internal {
                let cname = std::ffi::CString::new(name).map_err(|_| "symbol name contains NUL".to_string())?;
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
        // ④ Resolution order: hidden fallback table → archive handles (link order) → global
        // dlsym (native link-time binding — objects linked in by guest, visible in hidden or
        // dynsym, always beat host-process libraries of the same name: libLLVM's ZSTD_*
        // (c_zstd_stream) and librustc_driver's rust_psm_on_stack (c_polars_frame) are two
        // confirmed cases). Archive `.so` files are loaded with RTLD_NOW|RTLD_GLOBAL before
        // draining the worklist (head of lower_inner); handles are in self.archive_handles;
        // global fallback to real system libraries.
        let cname = std::ffi::CString::new(name).map_err(|_| "symbol name contains NUL".to_string())?;
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
            // weak symbol absent = NULL (native address-taking semantics for undefined weak symbol);
            // indirect call through it loudly terminates at execution phase (CallIndirect null
            // pointer diagnostics)
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
