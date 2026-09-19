//! GOT / foreign slots: `got_intern`, `got_fixup_push`, `foreign_slot`,
//! `foreign_const_operand`, `foreign_fn_slot` and `materialize_in` produce the material
//! that the startup phase refills by name. `impl Linker` sub-block.

use super::*;

impl<'tcx> Linker<'tcx> {
    fn frozen_reloc_push(&mut self, image: bool, reloc: ir::FrozenReloc) {
        if image {
            self.split
                .as_mut()
                .expect("image frozen relocation requires split")
                .image_frozen_relocs
                .push(reloc);
        } else {
            self.frozen_relocs.push(reloc);
        }
    }

    /// Index of `name` in this side's symbol table; registers it on first sight. The
    /// image side keeps its own tables in `Split`.
    pub(super) fn got_intern(&mut self, name: &str, weak: bool, image: bool) -> u32 {
        let (syms, idx_map) = if image {
            let s = self.split.as_mut().expect("image got table requires split");
            (&mut s.image_got_syms, &mut s.image_got_idx)
        } else {
            (&mut self.got_syms, &mut self.got_idx)
        };
        if let Some(&i) = idx_map.get(name) {
            // Weak/strong merge: one strong reference makes the merged entry strong.
            // Registering only the first reference's strength would write NULL for a
            // missing symbol referenced weak-then-strong, instead of failing the
            // link/load the way native linking does.
            if !weak {
                syms[i as usize].weak = false;
            }
            return i;
        }
        let i = syms.len() as u32;
        syms.push(ir::GotSym {
            name: name.into(),
            weak,
        });
        idx_map.insert(name.into(), i);
        i
    }

    /// Records a fixup site for this side; the image side stores it in `Split`.
    pub(super) fn got_fixup_push(&mut self, image: bool, f: ir::GotFixup) {
        if image {
            self.split
                .as_mut()
                .expect("image got table requires split")
                .image_got_fixups
                .push(f);
        } else {
            self.got_fixups.push(f);
        }
    }

    /// GOT slot of a foreign symbol in the current context: an ordinary 8-byte cell in
    /// this side's frozen region. The fixed base region makes the slot address
    /// serializable, while the startup phase refills its contents. Opens the cell on
    /// first use, initializes it to `init`, and records a fixup at addend 0.
    pub(super) fn foreign_slot(&mut self, name: &str, init: u64, weak: bool) -> u64 {
        let ctx_image = self.split.as_ref().is_some_and(|s| s.current_image);
        // Merge before the cache hit: a later strong reference must upgrade the merged
        // entry's weak flag even when it reuses an existing slot.
        let idx = self.got_intern(name, weak, ctx_image);
        if let Some(&a) = self.foreign_slots.get(&(name.into(), ctx_image)) {
            return a;
        }
        let addr = if ctx_image {
            self.split
                .as_mut()
                .expect("image context requires split")
                .image_frozen
                .alloc(8, 8)
        } else {
            self.frozen.alloc(8, 8)
        };
        unsafe { *(addr as *mut u64) = init };
        self.got_fixup_push(
            ctx_image,
            ir::GotFixup {
                addr: ir::LinkAddr(addr),
                sym: idx,
                addend: 0,
            },
        );
        self.foreign_slots.insert((name.into(), ctx_image), addr);
        addr
    }

    /// Constant operand for a foreign allocation: its value is this side's GOT slot
    /// contents. A non-zero addend becomes an exactly equivalent `SubImm` rewrite
    /// (`(slot) - (0 - addend)` == `(slot) + addend` in mod-2^64 arithmetic). Returns
    /// `None` for non-foreign allocations, and the caller emits a plain `Imm`.
    pub(crate) fn foreign_const_operand(
        &mut self,
        id: AllocId,
        init: u64,
        addend: u64,
    ) -> Option<ir::Operand> {
        let (name, weak) = self.foreign_alloc_sym.get(&id)?.clone();
        let slot = self.foreign_slot(&name, init, weak);
        let mem = ir::Operand::Mem {
            expr: ir::PlaceExpr {
                base: ir::PlaceBase::Static(ir::LinkAddr(slot)),
                steps: Box::new([]),
            },
            width: ir::Width::W64,
        };
        Some(if addend == 0 {
            mem
        } else {
            ir::Operand::SubImm {
                base: Box::new(mem),
                sub: 0u64.wrapping_sub(addend),
            }
        })
    }

    /// GOT slot address of an extern fn entry (fn pointer) in the current context. The
    /// slot for this context has already been created by baking. Returns `None` for
    /// non-foreign items; the Reify/Closure emission points in `func.rs` use this.
    pub(crate) fn foreign_fn_slot(&mut self, inst: Instance<'tcx>) -> Option<u64> {
        if !self.tcx.is_foreign_item(inst.def_id()) {
            return None;
        }
        let name = canonical_link_name(self.tcx.symbol_name(inst).name);
        let ctx_image = self.split.as_ref().is_some_and(|s| s.current_image);
        self.foreign_slots.get(&(name.into(), ctx_image)).copied()
    }

    /// Materializes a memory allocation: allocate an address, copy the bytes, then
    /// relocate. Each provenance entry is rewritten to the target's real address plus its
    /// addend. With `image`, the allocation lands in the image region and is recorded in
    /// the image tables; otherwise it lands in the delta region.
    pub(super) fn materialize_in(
        &mut self,
        id: AllocId,
        alloc: ConstAllocation<'tcx>,
        image: bool,
    ) -> Result<u64, String> {
        let a = alloc.inner();
        let size = a.size().bytes();
        let align = a.align.bytes();
        let base = if image {
            let s = self
                .split
                .as_mut()
                .expect("image materialization requires split");
            let base = s.image_frozen.alloc(size, align);
            s.image_alloc_addrs.insert(id, base); // allocate before filling (cycle-safe)
            base
        } else {
            let base = self.frozen.alloc(size, align);
            self.alloc_addrs.insert(id, base); // allocate before filling (cycle-safe)
            base
        };
        let bytes = a.inspect_with_uninit_and_ptr_outside_interpreter(0..size as usize);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base as *mut u8, size as usize) };
        // Relocation: the 8 bytes stored at a pointer position hold the offset of the
        // target within itself (the addend), so replace them with the target's real
        // address plus that addend.
        for (off, prov) in a.provenance().ptrs().iter() {
            // TypeId uses an AllocId-shaped provenance carrier for a plain integer hash:
            // ensure_alloc deliberately returns base zero, so its addend is already the final
            // value and must not be treated as a relocatable guest pointer.
            let entry_target = match self.tcx.global_alloc(prov.alloc_id()) {
                GlobalAlloc::Function { .. } => Some(true),
                GlobalAlloc::TypeId { .. } => None,
                _ => Some(false),
            };
            let target = self.ensure_alloc(prov.alloc_id())?;
            let at = (base + off.bytes()) as *mut u64;
            let addend = unsafe {
                let addend = at.read_unaligned();
                at.write_unaligned(target.wrapping_add(addend));
                addend
            };
            // A foreign allocation target (a weak extern static or the address of an
            // extern fn) gets a fixup at this word: the startup phase refills it by name,
            // while this process's resolution stays the cold-path initial value.
            if let Some((name, weak)) = self.foreign_alloc_sym.get(&prov.alloc_id()).cloned() {
                let idx = self.got_intern(&name, weak, image);
                self.got_fixup_push(
                    image,
                    ir::GotFixup {
                        addr: ir::LinkAddr(base + off.bytes()),
                        sym: idx,
                        addend,
                    },
                );
            } else if let Some(entry_target) = entry_target {
                self.frozen_reloc_push(
                    image,
                    ir::FrozenReloc {
                        at: ir::LinkAddr(base + off.bytes()),
                        target: if entry_target {
                            ir::FrozenRelocTarget::Entry(ir::LinkAddr(target.wrapping_add(addend)))
                        } else {
                            ir::FrozenRelocTarget::Frozen(ir::LinkAddr(target.wrapping_add(addend)))
                        },
                    },
                );
            }
        }
        Ok(base)
    }
}
