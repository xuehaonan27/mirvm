//! Linker (moved wholesale from lower/mod.rs M4-M5): lowering-phase linker — FuncId deduplication set and
//! lowering queue, frozen-area materialization, P1 fn entries, P2 GOT/foreign slots, FFI signatures, A2
//! split state. Struct and constructors are in mod.rs; impl sub-blocks are grouped by recon field: =
//! entries(P1 entries + FFI signatures) / alloc(frozen-area materialization) / got(GOT·foreign slots) / calls(call resolution).

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
    /// A2 split state (None = non-split path, behavior identical to S4/S3′a)
    pub(crate) split: Option<Split<'tcx>>,
    /// ① Engine primitive table: mangled symbol → Builtin
    pub(crate) builtins: FxHashMap<Symbol, ir::Builtin>,
    /// ② Link emulation: exported symbol name → (defining instance, is_weak) (strong overrides weak; built lazily)
    pub(crate) exports: Option<FxHashMap<Symbol, (Instance<'tcx>, bool)>>,
    /// Frozen area (statics / const pool / fn entries) — materialized during lowering, handed off to Module at end
    pub(crate) frozen: FrozenArena,
    /// Already-materialized alloc → real frozen-area address (dedup + allocate-before-fill to break pointer cycles)
    pub(crate) alloc_addrs: FxHashMap<AllocId, u64>,
    /// fn-ptr entry: instance → real entry address (D4: one real-address identity per instance)
    pub(crate) fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// Reverse lookup: real entry address → FuncId (used for indirect-call dispatch, handed off to Module)
    pub(crate) fn_addrs: FxHashMap<u64, ir::FuncId>,
    /// Guest TLS: `#[thread_local]` static → dense TlsId + slot table (M4.4 D3, handed off to Module)
    pub(crate) tls_ids: FxHashMap<rustc_hir::def_id::DefId, ir::TlsId>,
    pub(crate) tls_slots: Vec<ir::TlsSlot>,
    /// asm-stub wrapper text (M5.0): AsmStubId → symbol name + GAS source; materialized in batch at end of lowering
    /// via cc+dlopen. From A2 onward names are decoupled from bit order (split mode final bit order known only at wrap-up).
    pub(crate) asm_sites: Vec<ir::AsmSite>,
    /// Entries for extern fn taken as a value address (fn-ptr): instance → dlsym real address (extension of D4 entry
    /// semantics). Not entered in fn_addrs — a reverse lookup miss at runtime is exactly the trigger for CallIndirect's
    /// native_sig libffi direct-call path (the second of M4.4 FFI's two directions).
    pub(crate) foreign_fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// P2 GOT (decision-history §7.5c) delta-side three tables (image side is in Split)
    pub(crate) got_syms: Vec<ir::GotSym>,
    pub(crate) got_idx: FxHashMap<Box<str>, u32>,
    pub(crate) got_fixups: Vec<ir::GotFixup>,
    pub(crate) frozen_relocs: Vec<ir::FrozenReloc>,
    /// Foreign allocation → (symbol name, weak): covers non-weak extern statics and extern fn address-taking
    /// (weak extern statics go through foreign_slot directly and are not recorded here — E27, 2026-07-18);
    /// relocation / const emission uses it to turn a "baked value" into a "slot".
    pub(crate) foreign_alloc_sym: FxHashMap<AllocId, (Box<str>, bool)>,
    /// (symbol name, image context) → GOT slot real address (slot = ordinary 8-byte cell in this side's frozen area)
    pub(crate) foreign_slots: std::collections::HashMap<(Box<str>, bool), u64>,
    /// P1 entry executability (decision-history §7.6) local stub code area and recipe table (image
    /// side is in Split): instance → stub idx (domain determined by instance class, same discipline as fn entries)
    pub(crate) code_arena: crate::vm::engine::codearena::StubArena,
    pub(crate) entry_stub_sites: Vec<ir::EntryStubSite>,
    pub(crate) entry_stub_ids: FxHashMap<Instance<'tcx>, u32>,
    /// FFI derivability cache (freeze_c_fnptr_sig; None = keep as data-slot entry — Rust ABI /
    /// aggregate by value / variadic have no valid native call surface, no loss in the blind zone)
    pub(crate) entry_sig_cache: FxHashMap<Instance<'tcx>, Option<ir::ForeignSig>>,
    /// Fallback table of hidden symbols from required archives (load base, symbol→st_value): only records symbols not in
    /// .dynsym (archives with -fvisibility=hidden, e.g. ring/zstd-sys family); extern
    /// static/fn address-taking is resolved during lowering **before dlsym global scope** — native link-time binding semantics
    /// (archive-internal definitions always win over global namespace; proven by host libLLVM's embedded ZSTD_* silently shadowing
    /// see elfsym module header). Built at the top of lower_inner together with dlopen.
    pub(crate) archive_fallbacks: Vec<(u64, std::collections::HashMap<Box<str>, u64>)>,
    /// dlopen handles for required archives / system libraries (required_native_libs + dylib_candidates order
    /// = isomorphic to link order): their .dynsym-visible symbols are resolved during lowering **before dlsym global scope** —
    /// native link-time binding (guest's own objects always win over host process libraries with the same name; proven by psm's
    /// rust_psm_on_stack vs host librustc_driver embedded copy, corpus batch 6
    /// c_polars_frame). Handles are not closed on process shutdown (same as runtime FfiState).
    pub(crate) archive_handles: Vec<usize>,
    // ===== S4 base (s4-base-image-design; offset merge) =====
    /// sym → base FuncId (reuse on hit, not enqueued). Empty table = no base / base-build mode.
    pub(crate) base_fns: FxHashMap<Box<str>, ir::FuncId>,
    /// sym → base fn-entry real address (only those whose address was taken)
    pub(crate) base_fn_entries: FxHashMap<Box<str>, u64>,
    /// sym → base static real address (duplicate materialization = static mut schizophrenia, must dedup)
    pub(crate) base_statics: FxHashMap<Box<str>, u64>,
    /// sym → base TlsId (thread-local identity likewise must be deduped)
    pub(crate) base_tls: FxHashMap<Box<str>, ir::TlsId>,
    /// delta start ids for fn/TLS/asm = base table lengths (at absorb time base++delta concatenate into single tables)
    pub(crate) delta_first_fn: ir::FuncId,
    pub(crate) delta_first_tls: ir::TlsId,
    pub(crate) delta_first_asm: ir::AsmStubId,
    /// Next FuncId to allocate (cannot use ids.len() anymore: base hits also occupy ids entries)
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
    /// S3′a: base-maps = union-find over image stack; delta start ids = stack cumulative. `frozen` is constructed by the caller
    /// for the target domain (program delta = new(), base = new_base_image(), dependency image = new_image(k)).
    /// code_arena = P1 local stub code area (§7.6, same k-domain as frozen, derived uniformly by lower_inner).
    pub(super) fn new(
        tcx: TyCtxt<'tcx>,
        stack: &crate::baseimage::ImageStack,
        frozen: FrozenArena,
        code_arena: crate::vm::engine::codearena::StubArena,
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

    /// Activate A2 split (s3b-a2-design §4): image frozen area lands in spline k=0 domain.
    /// If domain is occupied = fall back to dynamic base (semantics unchanged; A2-2 write phase refuses serialization and self-heals).
    pub(super) fn activate_split(&mut self) {
        self.split = Some(Split {
            image_frozen: FrozenArena::new_image(0),
            image_queue: VecDeque::new(),
            image_funcs: Vec::new(),
            image_fn_next: 0,
            image_tls_slots: Vec::new(),
            image_asm_sites: Vec::new(),
            image_alloc_addrs: FxHashMap::default(),
            image_fn_entries: FxHashMap::default(),
            current_image: false,
            image_insts: Vec::new(),
            image_got_syms: Vec::new(),
            image_got_idx: FxHashMap::default(),
            image_got_fixups: Vec::new(),
            image_frozen_relocs: Vec::new(),
            image_code_arena: crate::vm::engine::codearena::StubArena::new_image(0),
            image_stub_sites: Vec::new(),
        });
    }

    /// Reserve an asm-stub slot (M5.0), returning (AsmStubId, symbol name); text is filled in later via set_asm_stub.
    /// Two-step because the wrapper name must be fixed before the text is generated (self-referential .size directive).
    /// S4: ids start counting from the base (wrapper names are uniquely free across domains). A2 split splits by current class:
    /// image class = tagged id + `mirvm_asm_xi{j}` name, delta class = original id space + `mirvm_asm_xd{k}`
    /// name (final bit order known only at wrap-up, names decoupled from bit order); non-split path keeps bit-order names unchanged.
    pub(super) fn reserve_asm_stub(&mut self) -> (ir::AsmStubId, Box<str>) {
        if let Some(s) = &mut self.split {
            if s.current_image {
                let j = s.image_asm_sites.len() as ir::AsmStubId;
                let name: Box<str> = format!("mirvm_asm_xi{j}").into();
                s.image_asm_sites.push(ir::AsmSite {
                    name: name.clone(),
                    text: String::new(),
                });
                return (IMAGE_TAG | j, name);
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
        if id & IMAGE_TAG != 0 {
            let s = self
                .split
                .as_mut()
                .expect("tagged stub id exists only in split mode");
            s.image_asm_sites[(id & !IMAGE_TAG) as usize].text = text;
        } else {
            self.asm_sites[(id - self.delta_first_asm) as usize].text = text;
        }
    }

    /// `#[thread_local]` static → dense TlsId (M4.4 D3). Template = initializer evaluation result
    /// materialized into frozen area (reuses ensure_alloc, relocations come for free — runtime uses it only as a byte source, no writes).
    pub(crate) fn tls_id(&mut self, def_id: rustc_hir::def_id::DefId) -> Result<ir::TlsId, String> {
        if let Some(&id) = self.tls_ids.get(&def_id) {
            return Ok(id);
        }
        // S4 base TLS dedup: TlsId is thread-local identity; two copies = one #[thread_local] appears as
        // two different variables to base functions and delta functions (wrong-value level), so it must be reused.
        if !self.base_tls.is_empty() {
            let sym = self.tcx.symbol_name(Instance::mono(self.tcx, def_id)).name;
            if let Some(&id) = self.base_tls.get(sym) {
                self.tls_ids.insert(def_id, id);
                return Ok(id);
            }
        }
        let alloc = self
            .tcx
            .eval_static_initializer(def_id)
            .map_err(|e| format!("TLS static initializer evaluation failed: {e:?}"))?;
        let (size, align) = (alloc.inner().size().bytes(), alloc.inner().align.bytes());
        let alloc_id = self.tcx.reserve_and_set_static_alloc(def_id);
        let template = self.ensure_alloc(alloc_id)?;
        // A2 split: TLS identity domain is determined by def_id.krate (non-local → image slot area, single identity);
        // image context encountering local TLS = purity downward closure violated (classifier bug), reject loudly.
        if let Some(s) = &mut self.split {
            if def_id.krate != rustc_hir::def_id::LOCAL_CRATE {
                let j = s.image_tls_slots.len() as ir::TlsId;
                s.image_tls_slots.push(ir::TlsSlot {
                    template: ir::LinkAddr(template),
                    size,
                    align: align as u32,
                });
                let id = IMAGE_TAG | j;
                self.tls_ids.insert(def_id, id);
                return Ok(id);
            }
            if s.current_image {
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
