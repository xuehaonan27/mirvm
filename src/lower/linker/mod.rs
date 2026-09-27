//! The lowering-phase linker: the FuncId deduplication set and lowering queue, frozen-area
//! materialization, fn entries, GOT/foreign slots, FFI signatures and split (image/delta) state.
//! The struct and constructors live here; the `impl` sub-blocks are grouped by concern:
//! entries (fn entries + FFI signatures) / alloc (frozen-area materialization) /
//! got (GOT/foreign slots) / calls (call resolution).

use crate::lower::Error;

mod alloc;
mod calls;
mod entries;
mod got;

use super::*;

pub(crate) struct Linker<'tcx> {
    pub(crate) tcx: TyCtxt<'tcx>,
    /// instance → FuncId (dedup set; includes already-lowered and queued)
    pub(crate) ids: FxHashMap<Instance<'tcx>, ir::FuncId>,
    /// Lowering queue (FuncId allocated, body not yet emitted) — in split mode this is the **delta-class** queue
    pub(crate) queue: VecDeque<(ir::FuncId, Instance<'tcx>)>,
    /// Split (image/delta) state; `None` = single-domain lowering.
    pub(crate) split: Option<Split<'tcx>>,
    /// ① Engine primitive table: mangled symbol → Builtin
    pub(crate) builtins: FxHashMap<Symbol, ir::Builtin>,
    /// ② Link emulation: exported symbol name → (defining instance, is_weak) (strong overrides weak; built lazily)
    pub(crate) exports: Option<FxHashMap<Symbol, (Instance<'tcx>, bool)>>,
    /// Frozen area (statics / const pool / fn entries) — materialized during lowering, handed off to Module at end
    pub(crate) frozen: FrozenArena,
    /// Already-materialized alloc → real frozen-area address (dedup + allocate-before-fill to break pointer cycles)
    pub(crate) alloc_addrs: FxHashMap<AllocId, u64>,
    /// fn-ptr entry: instance → real entry address (exactly one address identity per instance)
    pub(crate) fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// Reverse lookup: real entry address → FuncId (used for indirect-call dispatch, handed off to Module)
    pub(crate) fn_addrs: FxHashMap<u64, ir::FuncId>,
    /// Guest TLS: `#[thread_local]` static → dense TlsId + slot table (handed off to Module)
    pub(crate) tls_ids: FxHashMap<rustc_hir::def_id::DefId, ir::TlsId>,
    pub(crate) tls_slots: Vec<ir::TlsSlot>,
    /// asm-stub wrapper text: AsmStubId → symbol name + GAS source; materialized in one batch at the end of
    /// lowering via cc+dlopen. In split mode names are decoupled from bit order, which is only known at wrap-up.
    pub(crate) asm_sites: Vec<ir::AsmSite>,
    /// Entries for extern fn taken as a value address (fn-ptr): instance → dlsym real address. Not entered in
    /// `fn_addrs`: a reverse lookup miss at runtime is exactly what triggers `CallIndirect`'s `native_sig`
    /// libffi direct-call path.
    pub(crate) foreign_fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// Delta-side GOT tables and frozen-area relocations (the image side lives in `Split`).
    pub(crate) got_syms: Vec<ir::GotSym>,
    pub(crate) got_idx: FxHashMap<Box<str>, u32>,
    pub(crate) got_fixups: Vec<ir::GotFixup>,
    pub(crate) frozen_relocs: Vec<ir::FrozenReloc>,
    /// Foreign allocation → (symbol name, weak): covers non-weak extern statics and extern fn address-taking.
    /// Weak extern statics go through `foreign_slot` directly and are not recorded here. Relocation and const
    /// emission use this to turn a baked value into a slot.
    pub(crate) foreign_alloc_sym: FxHashMap<AllocId, (Box<str>, bool)>,
    /// (symbol name, image context) → GOT slot real address (slot = ordinary 8-byte cell in this side's frozen area)
    /// Foreign GOT slots, keyed by symbol and the home whose frozen area holds the slot (`None` = the
    /// delta). One symbol can need a slot in more than one domain.
    pub(crate) foreign_slots: std::collections::HashMap<(Box<str>, Option<usize>), u64>,
    /// Entry-stub code arena and recipe table (the image side lives in `Split`): instance → stub idx. The
    /// stub's domain is determined by the instance class, the same discipline as fn entries.
    pub(crate) code_arena: crate::vm::codearena::StubArena,
    pub(crate) entry_stub_sites: Vec<ir::EntryStubSite>,
    pub(crate) entry_stub_ids: FxHashMap<Instance<'tcx>, u32>,
    /// FFI derivability cache (`freeze_c_fnptr_sig`). `None` = keep as a data-slot entry: Rust ABI,
    /// by-value aggregates and variadic functions have no valid native call surface.
    pub(crate) entry_sig_cache: FxHashMap<Instance<'tcx>, Option<ir::ForeignSig>>,
    /// Fallback table of hidden symbols from required archives (load base, symbol→st_value): only symbols
    /// absent from .dynsym (archives built with -fvisibility=hidden, e.g. the ring/zstd-sys family). Extern
    /// static/fn address-taking resolves through this during lowering, before global dlsym, matching native
    /// link-time binding: archive-internal definitions always win over the global namespace. Built at the top
    /// of `lower_inner` together with dlopen.
    pub(crate) archive_fallbacks: Vec<(u64, std::collections::HashMap<Box<str>, u64>)>,
    /// dlopen handles for required archives / system libraries, in `required_native_libs` +
    /// `dylib_candidates` order (isomorphic to link order). Their .dynsym-visible symbols are resolved during
    /// lowering before global dlsym, matching native link-time binding: the guest's own objects always win
    /// over host process libraries of the same name. Handles are not closed on process shutdown, same as
    /// runtime `FfiState`.
    pub(crate) archive_handles: Vec<usize>,
    // ===== Base-image tables =====
    /// sym → base FuncId (reuse on hit, not enqueued). Empty table = no base / base-build mode.
    pub(crate) base_fns: FxHashMap<Box<str>, ir::FuncId>,
    /// sym → base fn-entry real address (only those whose address was taken)
    pub(crate) base_fn_entries: FxHashMap<Box<str>, u64>,
    /// sym → base static real address (materializing twice would give one static two identities; must dedup)
    pub(crate) base_statics: FxHashMap<Box<str>, u64>,
    /// sym → base TlsId (thread-local identity likewise must be deduped)
    pub(crate) base_tls: FxHashMap<Box<str>, ir::TlsId>,
    /// delta start ids for fn/TLS/asm = base table lengths (at absorb time base++delta concatenate into single tables)
    pub(crate) delta_first_fn: ir::FuncId,
    pub(crate) delta_first_tls: ir::TlsId,
    pub(crate) delta_first_asm: ir::AsmStubId,
    /// Next FuncId to allocate (not `ids.len()`: base hits also occupy `ids` entries)
    pub(crate) next_fn: ir::FuncId,
    /// Base exported material: non-foreign statics materialized in this session (DefId, frozen-area address)
    pub(crate) static_defs: Vec<(rustc_hir::def_id::DefId, u64)>,
    /// Intrinsic call site in the fixed std startup chain, at the outer call boundary and final execution catch.
    pub(crate) main_catch_site: Option<MainCatchSite<'tcx>>,
}

#[derive(Clone, Copy)]
pub(crate) struct MainCatchSite<'tcx> {
    pub(crate) boundary_caller: Instance<'tcx>,
    pub(crate) boundary_callee: ir::FuncId,
    pub(crate) catcher_caller: Instance<'tcx>,
    pub(crate) catcher_intrinsic: Instance<'tcx>,
}

impl<'tcx> Linker<'tcx> {
    /// Base maps are a union-find over the image stack; delta start ids are the stack's cumulative counts.
    /// `frozen` is constructed by the caller for the target domain (program delta = `new()`, base =
    /// `new_base_image()`, dependency image = `new_image(k)`); `code_arena` is the local stub code area in
    /// the same k-domain as `frozen`, derived uniformly by `lower_inner`.
    pub(super) fn new(
        tcx: TyCtxt<'tcx>,
        stack: &crate::image::ImageStack,
        frozen: FrozenArena,
        code_arena: crate::vm::codearena::StubArena,
    ) -> Self {
        fn clone_map<V: Copy>(
            m: &std::collections::HashMap<Box<str>, V>,
        ) -> FxHashMap<Box<str>, V> {
            m.iter().map(|(k, v)| (k.clone(), *v)).collect()
        }
        let base_fns = clone_map(stack.fn_by_sym());
        let base_fn_entries = clone_map(stack.entry_by_sym());
        let base_statics = clone_map(stack.static_by_sym());
        let base_tls = clone_map(stack.tls_by_sym());
        let delta_first_fn = stack.total_fns() as ir::FuncId;
        Linker {
            tcx,
            ids: FxHashMap::default(),
            queue: VecDeque::new(),
            builtins: engine_builtins(tcx),
            exports: None,
            frozen,
            alloc_addrs: FxHashMap::default(),
            fn_entries: FxHashMap::default(),
            fn_addrs: FxHashMap::default(),
            tls_ids: FxHashMap::default(),
            tls_slots: Vec::new(),
            asm_sites: Vec::new(),
            foreign_fn_entries: FxHashMap::default(),
            got_syms: Vec::new(),
            got_idx: FxHashMap::default(),
            got_fixups: Vec::new(),
            frozen_relocs: Vec::new(),
            foreign_alloc_sym: FxHashMap::default(),
            foreign_slots: std::collections::HashMap::new(),
            code_arena,
            entry_stub_sites: Vec::new(),
            entry_stub_ids: FxHashMap::default(),
            entry_sig_cache: FxHashMap::default(),
            archive_fallbacks: Vec::new(),
            archive_handles: Vec::new(),
            split: None,
            base_fns,
            base_fn_entries,
            base_statics,
            base_tls,
            delta_first_fn,
            delta_first_tls: stack.total_tls() as ir::TlsId,
            delta_first_asm: stack.total_asm() as ir::AsmStubId,
            next_fn: delta_first_fn,
            static_defs: Vec::new(),
            main_catch_site: None,
        }
    }

    /// The home a *definition's crate* belongs to: the delta (`None`) for the local crate, a home index
    /// otherwise. Statics, weak cells, fn entries and TLS carry one identity each, so their placement
    /// follows the defining crate rather than the referencing context; a reference from a higher home
    /// finds them through the lower homes' tables.
    pub(super) fn home_of_krate(&self, krate: rustc_span::def_id::CrateNum) -> Option<usize> {
        if self.split.is_none() || krate == rustc_hir::def_id::LOCAL_CRATE {
            return None;
        }
        match crate::image::units::current() {
            Some(table) => {
                // A crate no unit covers — a sysroot crate, or a path dependency the table does not
                // know — is below every unit, so the first one hosts it. That is the rule `place`
                // applies to a function naming no unit; putting it in the delta instead would leave a
                // home body referencing a layer above itself.
                let unit = crate::lower::purity::unit_of_crate(self.tcx, krate, table)
                    .or_else(|| (table.len() > 0).then_some(0))?;
                self.placeable(unit as usize).then_some(unit as usize)
            }
            // Without a build graph the closure is one home.
            None => Some(0),
        }
    }

    /// Whether this session can still add to a home: it must be addressable, and the stack must not
    /// already provide its layer. A loaded layer is immutable, so an instance it does not provide by
    /// symbol — one rustc instantiated in this program, which is why its mangled name names this
    /// crate — is residue: the delta duplicates it instead of rewriting a shared unit's manifest from
    /// one program's view of it.
    fn placeable(&self, home: usize) -> bool {
        home < MAX_HOMES
            && !self
                .split
                .as_ref()
                .is_some_and(|split| split.is_loaded(home))
    }

    /// Which home an instance is lowered in: the delta (`None`) or a home index.
    ///
    /// With a unit table (the cargoless track) the home rule decides: the first unit whose closure
    /// covers the instance's defining crate and every unit its arguments mention. Without one (the
    /// Cargo track, which has no build graph) the closure is one home and the purity classifier
    /// decides. An instance the rule cannot place is residue and stays in the delta.
    pub(super) fn place(&self, inst: Instance<'tcx>) -> Option<usize> {
        match crate::image::units::current() {
            Some(table) => match crate::lower::purity::home_of(inst, self.tcx, table) {
                Some(home) => {
                    let home = home.unit as usize;
                    if !self.placeable(home) {
                        if crate::options::a2_debug() && home < MAX_HOMES {
                            eprintln!(
                                "[a2-debug] residue in this program: {} belongs to loaded home {home}",
                                self.tcx.symbol_name(inst).name
                            );
                        }
                        return None;
                    }
                    Some(home)
                }
                None => None,
            },
            None => classify_purity(inst).is_image().then_some(0),
        }
    }

    /// Activate split (home/delta) lowering: each home's frozen area lands in its spline domain. If that
    /// domain is occupied, fall back to a dynamic base; semantics are unchanged, but the write phase refuses
    /// serialization and self-heals.
    pub(super) fn activate_split(&mut self, taken_slots: Vec<usize>, loaded_homes: Vec<usize>) {
        self.split = Some(Split::activate(taken_slots, loaded_homes));
    }

    /// Reserve an asm-stub slot, returning (AsmStubId, symbol name); the text follows via `set_asm_stub`.
    /// Two steps because the wrapper name must be fixed before the text is generated (the `.size` directive is
    /// self-referential).
    ///
    /// Ids start at the base so wrapper names are unique across domains. In split mode the slot goes to the
    /// current class's domain: image = tagged id + `mirvm_asm_xi{j}`, delta = original id space +
    /// `mirvm_asm_xd{k}`. Delta names are decoupled from bit order because that order is only known at wrap-up.
    pub(super) fn reserve_asm_stub(&mut self) -> (ir::AsmStubId, Box<str>) {
        if let Some(s) = &mut self.split {
            if let Some(home) = s.current {
                let j = s.cur().asm_sites.len() as ir::AsmStubId;
                let name: Box<str> = format!("mirvm_asm_xh{home}_{j}").into();
                s.cur().asm_sites.push(ir::AsmSite {
                    name: name.clone(),
                    text: String::new(),
                });
                return (home_tag(home, j), name);
            }
            let k = self.delta_first_asm + self.asm_sites.len() as ir::AsmStubId;
            let name: Box<str> = format!("mirvm_asm_xd{k}").into();
            self.asm_sites.push(ir::AsmSite {
                name: name.clone(),
                text: String::new(),
            });
            return (k, name);
        }
        let id = self.delta_first_asm + self.asm_sites.len() as ir::AsmStubId;
        let name: Box<str> = format!("mirvm_asm_{id}").into();
        self.asm_sites.push(ir::AsmSite {
            name: name.clone(),
            text: String::new(),
        });
        (id, name)
    }
    pub(super) fn set_asm_stub(&mut self, id: ir::AsmStubId, text: String) {
        if let Some(home) = id_home(id) {
            let s = self
                .split
                .as_mut()
                .expect("tagged stub id exists only in split mode");
            s.home_mut(home).asm_sites[id_ordinal(id) as usize].text = text;
        } else {
            self.asm_sites[(id - self.delta_first_asm) as usize].text = text;
        }
    }

    /// `#[thread_local]` static → dense TlsId. The template is the initializer evaluation result
    /// materialized into the frozen area via `ensure_alloc`, so relocations come for free; the runtime reads
    /// it only as a byte source and never writes it.
    pub(crate) fn tls_id(&mut self, def_id: rustc_hir::def_id::DefId) -> Result<ir::TlsId, Error> {
        if let Some(&id) = self.tls_ids.get(&def_id) {
            return Ok(id);
        }
        // Base TLS dedup: TlsId is thread-local identity. Two copies would make one #[thread_local] appear as
        // two different variables to base and delta functions (wrong values), so the base id must be reused.
        if !self.base_tls.is_empty() {
            let sym = self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name;
            if let Some(&id) = self.base_tls.get(sym) {
                self.tls_ids.insert(def_id, id);
                return Ok(id);
            }
        }
        let alloc = self.tcx.eval_static_initializer(def_id).map_err(|e| {
            Error::internal(format!("TLS static initializer evaluation failed: {e:?}"))
        })?;
        let (size, align) = (alloc.inner().size().bytes(), alloc.inner().align.bytes());
        let alloc_id = self.tcx.reserve_and_set_static_alloc(def_id);
        let template = self.ensure_alloc(alloc_id)?;
        // In split mode the TLS identity domain follows def_id.krate: non-local goes to the image slot area
        // (a single identity). An image context encountering local TLS violates the purity downward closure
        // (a classifier bug), so reject loudly.
        let home = self
            .split
            .is_some()
            .then(|| self.home_of_krate(def_id.krate))
            .flatten();
        if let Some(s) = &mut self.split {
            if let Some(home) = home {
                let layer = s.home_mut(home);
                let j = layer.tls_slots.len() as ir::TlsId;
                layer.tls_slots.push(ir::TlsSlot {
                    template: ir::LinkAddr(template),
                    size,
                    align: align as u32,
                });
                let id = home_tag(home, j);
                self.tls_ids.insert(def_id, id);
                return Ok(id);
            }
            if s.current.is_some() {
                panic!(
                    "A2 closure violation: image instance references local TLS static (classifier missed)"
                );
            }
        }
        let id = self.delta_first_tls + self.tls_slots.len() as ir::TlsId;
        self.tls_slots.push(ir::TlsSlot {
            template: ir::LinkAddr(template),
            size,
            align: align as u32,
        });
        self.tls_ids.insert(def_id, id);
        Ok(id)
    }
}
