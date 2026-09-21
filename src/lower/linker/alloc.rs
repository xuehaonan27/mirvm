//! Frozen-region materialization: `ensure_alloc` places constants, statics, vtables and
//! function bytes into a `FrozenArena`; `frozen_alloc_bytes` allocates raw bytes;
//! `record_addr` and `record_both` keep the dedup tables of the delta and image contexts.
//! `impl Linker` sub-block.

use crate::lower::Error;

use super::*;
use crate::lower::purity::arg_mentions_local;

impl<'tcx> Linker<'tcx> {
    /// Materializes raw bytes in the frozen region: the general path for small constants
    /// such as 128-bit literals. Plain bytes carry no pointers and no locality, so the
    /// current context decides the region.
    pub(crate) fn frozen_alloc_bytes(&mut self, bytes: &[u8]) -> u64 {
        let arena: &mut FrozenArena = match &mut self.split {
            Some(s) if s.current_image => &mut s.image_frozen,
            _ => &mut self.frozen,
        };
        let p = arena.alloc(bytes.len() as u64, 16);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len()) };
        p
    }

    /// Resolves an allocation to its real frozen-region address, materializing it
    /// recursively on demand. Allocating before filling keeps pointer cycles safe.
    ///
    /// Context routing:
    /// - an image context only accepts image-region addresses, which stay stable across
    ///   runs; a delta context accepts either region (the delta map first, falling back to
    ///   and reusing the image map, since delta -> image is stable in one direction);
    /// - identity-bearing paths (statics, weak cells, fn entries) are placed by `krate` or
    ///   class and recorded in **both** tables, so a single identity never splits into two
    ///   addresses. `Memory` and vtables have unspecified identity, so the context decides
    ///   their region; in an image context, an allocation already present in the delta
    ///   region is **promoted** (materialized twice, which is safe because constants are
    ///   read-only).
    pub(crate) fn ensure_alloc(&mut self, id: AllocId) -> Result<u64, Error> {
        let ctx_image = self.split.as_ref().is_some_and(|s| s.current_image);
        if ctx_image {
            if let Some(&a) = self
                .split
                .as_ref()
                .expect("split")
                .image_alloc_addrs
                .get(&id)
            {
                return Ok(a);
            }
        } else {
            if let Some(&a) = self.alloc_addrs.get(&id) {
                return Ok(a);
            }
            if let Some(s) = &self.split
                && let Some(&a) = s.image_alloc_addrs.get(&id)
            {
                return Ok(a);
            }
        }
        match self.tcx.global_alloc(id) {
            GlobalAlloc::Memory(alloc) => self.materialize_in(id, alloc, ctx_image),
            GlobalAlloc::Static(def_id) => {
                // An extern static is a real symbol reached through the os:: passthrough:
                // - weak (the null-test pattern for fn symbols such as gettid): the cell
                //   holds 0, meaning absent. A weak **fn** symbol must not expose its real
                //   address even when it exists, because a guest call would jump into native
                //   code and the entry lookup would fail; it takes the fallback path until a
                //   thunk handles it.
                // - non-weak (data symbols such as environ): the allocation base is the real
                //   dlsym address.
                if self.tcx.is_foreign_item(def_id) {
                    // dlsym uses the link symbol name: it keeps the `#[link_name]` prefix of
                    // ring's prefixed extern statics, which `item_name` would drop, and it
                    // strips the LLVM verbatim `\x01` marker used by aws-lc-sys.
                    // `canonical_link_name` does both, as on `resolve_call`'s fn path.
                    let name = canonical_link_name(
                        self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name,
                    );
                    // An item inside an extern block carries its linkage in the
                    // `import_linkage` field.
                    let weak = self.tcx.codegen_fn_attrs(def_id).import_linkage
                        == Some(rustc_hir::attrs::Linkage::ExternalWeak);
                    if weak {
                        // Native extern-weak semantics (the slot is refilled by name at
                        // startup): a hit yields the real symbol address, an absence yields
                        // 0. Symbols whose semantics the engine owns are forced absent: a
                        // builtin that is not a pure passthrough, a denylisted name, or an
                        // engine-model symbol such as a TLS dtor. std then takes its fallback
                        // path, so a real symbol cannot bypass the engine's takeover.
                        const FORCE_ABSENT_WEAK: &[&str] = &["__cxa_thread_atexit_impl"];
                        let engine_owned =
                            self.builtins.get(&Symbol::intern(name)).is_some_and(|b| {
                                !matches!(
                                    b,
                                    ir::Builtin::HostGetenv
                                        | ir::Builtin::HostWrite
                                        | ir::Builtin::HostStrlen
                                        | ir::Builtin::HostAbort
                                )
                            }) || DENY_EXACT.contains(&name)
                                || DENY_PREFIX.iter().any(|p| name.starts_with(p))
                                || FORCE_ABSENT_WEAK.contains(&name);
                        let cell = if engine_owned {
                            // Null-test cell: the symbol is absent (placed by krate and
                            // recorded in both tables as above).
                            if let Some(s) = &mut self.split
                                && def_id.krate != rustc_hir::def_id::LOCAL_CRATE
                            {
                                s.image_frozen.alloc(8, 8)
                            } else {
                                self.frozen.alloc(8, 8)
                            }
                        } else {
                            // GOT slot, initialized to 0; the startup phase refills it by
                            // name with the resolved value or 0.
                            self.foreign_slot(name, 0, true)
                        };
                        self.record_both(id, cell);
                        return Ok(cell);
                    }
                    // Resolution order matches the fn-address path: hidden fallback table,
                    // then archive handles in link order, then global dlsym. This mirrors
                    // native link-time binding, where a definition inside an archive always
                    // beats a same-named global one.
                    let cname = std::ffi::CString::new(name)
                        .map_err(|_| Error::internal("symbol name contains a NUL byte"))?;
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
                        return Err(Error::internal(format!(
                            "extern static `{name}` not found (neither the archive fallback \
                             table nor global dlsym has it)"
                        )));
                    }
                    // The value is still initialized from this process's resolution, which
                    // keeps the cold path bit-for-bit unchanged, and the slot is additionally
                    // registered as a foreign allocation: constant emission reads the slot,
                    // frozen byte relocation records a fixup, and the startup phase refills
                    // both by name.
                    let _ = self.foreign_slot(name, p, false);
                    self.record_both(id, p);
                    self.foreign_alloc_sym.insert(id, (name.into(), false));
                    return Ok(p);
                }
                // Base-image static dedup: materializing one static twice would split the
                // identity of a `static mut` or interior-mutable static into two addresses
                // that evolve independently, so a base-image hit must reuse its address.
                if !self.base_statics.is_empty() {
                    let sym = self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name;
                    if let Some(&addr) = self.base_statics.get(sym) {
                        self.record_both(id, addr);
                        return Ok(addr);
                    }
                }
                // A static's identity must be unique (`static mut`, interior mutability,
                // `&static` equality), so it is always placed by `def_id.krate`: a non-local
                // static goes to the image region. An image context that meets a local
                // static means the purity closure is broken by a classifier bug, so fail
                // loudly.
                let to_image = if let Some(s) = &self.split {
                    if def_id.krate == rustc_hir::def_id::LOCAL_CRATE {
                        if s.current_image {
                            panic!(
                                "A2 closure violation: image instance references a local static \
                                 (classifier missed)"
                            );
                        }
                        false
                    } else {
                        true
                    }
                } else {
                    false
                };
                // A static's bytes come from evaluating its initializer and may be writable
                // (`static mut`, interior mutability).
                let alloc = self.tcx.eval_static_initializer(def_id).map_err(|e| {
                    Error::internal(format!("static initializer evaluation failed: {e:?}"))
                })?;
                let addr = self.materialize_in(id, alloc, to_image)?;
                if to_image {
                    self.record_both(id, addr);
                }
                self.static_defs.push((def_id, addr)); // material for base-image/image export
                Ok(addr)
            }
            GlobalAlloc::Function { instance } => {
                let addr = self.fn_entry_addr(instance)?;
                self.record_both(id, addr);
                // Taking an extern fn's address (its fn-pointer value is the host code
                // address from dlsym) registers a foreign allocation: constant emission
                // reads the slot through `foreign_const_operand`, and frozen bytes get a
                // fixup through `materialize_in` relocation. Baking already created the slot.
                if self.tcx.is_foreign_item(instance.def_id()) {
                    let name = canonical_link_name(self.tcx.symbol_name(instance).name);
                    let weak = self.tcx.codegen_fn_attrs(instance.def_id()).import_linkage
                        == Some(rustc_hir::attrs::Linkage::ExternalWeak);
                    self.foreign_alloc_sym.insert(id, (name.into(), weak));
                }
                Ok(addr)
            }
            GlobalAlloc::VTable(ty, dyn_ty) => {
                // Split guard: an image context that meets a vtable for a local `Self` type
                // breaks the purity closure. Vtable address identity is unspecified, since
                // rustc itself duplicates vtables per CGU, so the context decides the region
                // (promoting to two copies is allowed) and `krate` plays no part.
                if self.split.as_ref().is_some_and(|s| s.current_image)
                    && ty.walk().any(arg_mentions_local)
                {
                    panic!(
                        "A2 closure violation: image instance references a local type's vtable \
                         (classifier missed)"
                    );
                }
                // rustc already provides the vtable allocation; recurse through the Memory
                // path, which relocates fn entries too.
                let principal = dyn_ty
                    .principal()
                    .map(|b| self.tcx.instantiate_bound_regions_with_erased(b));
                let vt_id = self.tcx.vtable_allocation((ty, principal));
                let addr = self.ensure_alloc(vt_id)?;
                self.record_addr(id, addr, ctx_image);
                Ok(addr)
            }
            GlobalAlloc::TypeId { .. } => {
                // A TypeId "allocation" has base 0: after relocation, base + addend is the
                // pointer-width piece of the 128-bit type hash itself, as in tier-0
                // `resolve_addr` and Miri.
                self.record_addr(id, 0, ctx_image);
                Ok(0)
            }
        }
    }

    /// Records the address in the dedup table of the context: the delta table unless
    /// split mode selects the image table.
    pub(super) fn record_addr(&mut self, id: AllocId, addr: u64, ctx_image: bool) {
        match &mut self.split {
            Some(s) if ctx_image => {
                s.image_alloc_addrs.insert(id, addr);
            }
            _ => {
                self.alloc_addrs.insert(id, addr);
            }
        }
    }

    /// Records an identity-bearing address in both tables, so either context reproduces
    /// the same address and the identity stays single.
    pub(super) fn record_both(&mut self, id: AllocId, addr: u64) {
        self.alloc_addrs.insert(id, addr);
        if let Some(s) = &mut self.split {
            s.image_alloc_addrs.insert(id, addr);
        }
    }
}
