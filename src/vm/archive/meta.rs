//! The non-function portion of a relocatable execution image. All addresses are logical;
//! each instantiation restores its own frozen domains and address tables.

use crate::error::Error;
use serde::Deserialize;
#[cfg(any(feature = "compiler", test))]
use serde::Serialize;

/// Execution metadata stores only non-function metadata. A borrowed write form avoids copying the frozen
/// region and index tables.
#[derive(Serialize)]
#[cfg(any(feature = "compiler", test))]
pub(crate) struct ModuleMetaRef<'a> {
    exports: &'a std::collections::HashMap<Box<str>, crate::vm::ir::FuncId>,
    frozen: Option<crate::vm::frozen::FrozenSnapshot>,
    image_frozens: Vec<(u64, &'a [u8])>,
    link_fn_addrs: std::collections::HashMap<crate::vm::ir::LinkAddr, crate::vm::ir::FuncId>,
    native_libs: &'a [Box<str>],
    required_native_libs: &'a [Box<str>],
    tls: &'a [crate::vm::ir::TlsSlot],
    asm_stub_count: usize,
    asm_sites: &'a [crate::vm::ir::AsmSite],
    foreign_syms: &'a [crate::vm::ir::GotSym],
    got_fixups: &'a [crate::vm::ir::GotFixup],
    frozen_relocs: &'a [crate::vm::ir::FrozenReloc],
    entry_stub_sites: Vec<crate::vm::ir::EntryStubSite>,
    custom_alloc_shims: Option<crate::vm::ir::AllocShims>,
    guest_panic_cleanup: Option<crate::vm::ir::GuestPanicCleanup>,
    entry: Option<crate::vm::ir::EntryPlan>,
}

#[cfg(any(feature = "compiler", test))]
impl<'a> From<(&'a crate::vm::ir::Module, &'a crate::vm::instance::Instance)>
    for ModuleMetaRef<'a>
{
    fn from(
        (module, instance): (&'a crate::vm::ir::Module, &'a crate::vm::instance::Instance),
    ) -> Self {
        Self {
            exports: &module.exports,
            frozen: module.frozen.clone(),
            image_frozens: instance
                .image_frozens
                .iter()
                .map(|arena| (arena.link_base(), arena.snapshot()))
                .collect(),
            link_fn_addrs: module
                .fn_entry_links
                .iter()
                .copied()
                .chain(
                    instance
                        .link_fn_addrs
                        .iter()
                        .map(|(&address, &func)| (address, func)),
                )
                .collect(),
            native_libs: &module.native_libs,
            required_native_libs: &module.required_native_libs,
            tls: &module.tls,
            asm_stub_count: module.asm_sites.len(),
            asm_sites: &module.asm_sites,
            foreign_syms: &module.foreign_syms,
            got_fixups: &module.got_fixups,
            frozen_relocs: &module.frozen_relocs,
            entry_stub_sites: module
                .entry_stub_sites
                .iter()
                .chain(
                    instance
                        .image_entry_stubs
                        .iter()
                        .flat_map(|(_, sites, _)| sites.iter()),
                )
                .cloned()
                .collect(),
            custom_alloc_shims: module.custom_alloc_shims,
            guest_panic_cleanup: module.guest_panic_cleanup,
            entry: module.entry,
        }
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct ModuleMeta {
    exports: std::collections::HashMap<Box<str>, crate::vm::ir::FuncId>,
    frozen: Option<crate::vm::frozen::FrozenSnapshot>,
    image_frozens: Vec<(u64, crate::vm::frozen::SnapshotBytes)>,
    link_fn_addrs: std::collections::HashMap<crate::vm::ir::LinkAddr, crate::vm::ir::FuncId>,
    native_libs: Vec<Box<str>>,
    required_native_libs: Vec<Box<str>>,
    tls: Vec<crate::vm::ir::TlsSlot>,
    asm_stub_count: usize,
    asm_sites: Vec<crate::vm::ir::AsmSite>,
    foreign_syms: Vec<crate::vm::ir::GotSym>,
    got_fixups: Vec<crate::vm::ir::GotFixup>,
    frozen_relocs: Vec<crate::vm::ir::FrozenReloc>,
    entry_stub_sites: Vec<crate::vm::ir::EntryStubSite>,
    custom_alloc_shims: Option<crate::vm::ir::AllocShims>,
    guest_panic_cleanup: Option<crate::vm::ir::GuestPanicCleanup>,
    entry: Option<crate::vm::ir::EntryPlan>,
}

impl ModuleMeta {
    pub(super) fn native_paths(&self) -> &[Box<str>] {
        &self.required_native_libs
    }

    pub(crate) fn instantiate(
        &self,
        functions: std::sync::Arc<super::functions::Functions>,
    ) -> Result<(crate::vm::ir::Module, crate::vm::instance::Instance), Error> {
        self.clone().into_instance(functions)
    }

    pub(crate) fn into_instance(
        self,
        functions: std::sync::Arc<super::functions::Functions>,
    ) -> Result<(crate::vm::ir::Module, crate::vm::instance::Instance), Error> {
        let module = crate::vm::ir::Module {
            funcs: Default::default(),
            function_names: crate::vm::ir::FunctionNames::archived(functions),
            exports: self.exports,
            frozen: self.frozen,
            fn_entry_links: self.link_fn_addrs.into_iter().collect(),
            native_libs: self.native_libs,
            required_native_libs: self.required_native_libs,
            required_native_hashes: Vec::new(),
            tls: self.tls,
            asm_sites: self.asm_sites,
            foreign_syms: self.foreign_syms,
            got_fixups: self.got_fixups,
            frozen_relocs: self.frozen_relocs,
            entry_stub_sites: self.entry_stub_sites,
            custom_alloc_shims: self.custom_alloc_shims,
            guest_panic_cleanup: self.guest_panic_cleanup,
            entry: self.entry,
        };
        // Names borrow the validated FUNCS section without constructing executable bodies.
        // Each instantiation gets its own frozen mapping so several instances of one package can run
        // at once; the embedded addresses are translated by the load map, which a packaged artifact
        // always fully covers.
        let mut instance = crate::vm::instance::Instance::materialize_dynamic(&module)?;
        instance.asm_stub_addrs = vec![0; self.asm_stub_count];
        let mut homes = std::collections::HashSet::new();
        if let Some(snapshot) = &module.frozen {
            homes.insert(snapshot.home());
        }
        for (home, bytes) in self.image_frozens {
            let home = usize::try_from(home)
                .map_err(|_| crate::fail!(Engine, "frozen domain does not fit this host"))?;
            if !homes.insert(home) {
                return Err(crate::fail!(
                    Engine,
                    "duplicate frozen domain in execution image"
                ));
            }
            let snapshot = crate::vm::frozen::FrozenSnapshot::from_home_and_bytes(home, bytes.0)?;
            instance
                .image_frozens
                .push(crate::vm::frozen::FrozenArena::restore_dynamic(&snapshot)?);
        }
        instance.rebuild_load_map();
        instance.rebuild_fn_addrs();
        instance.load_map.require_mapped();
        Ok((module, instance))
    }
}
