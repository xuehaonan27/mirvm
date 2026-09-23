//! The loaded instance of an artifact: everything a `Module` describes plus the process state that
//! only exists once this process has loaded it.
//!
//! A `Module` is a self-contained description that can be cached, packaged and moved between
//! processes. Actually running it needs addresses, mappings and handles that are valid for this
//! process alone: the frozen area's real mapping and the link-address table derived from it, the
//! reverse function-address table, the entry-closure bookkeeping, the asm-stub table, and the
//! native object images. None of that can be serialized, and none of it belongs to the artifact.
//!
//! Two ways in. [`Instance::materialize`] builds the instance a serialized artifact supports, and
//! the load phase hands over a fully built one for state an artifact cannot describe -- an asm-stub
//! table it assembled, a frozen area that fell back to a dynamic base, loaded native images. The
//! command-line loader takes the second route; `Engine::from_module_unchecked` takes the first.

use std::collections::{HashMap, HashSet};

use super::codearena::StubArena;
use super::frozen::FrozenArena;
use super::ir::{EntryStubSite, FuncId, LinkAddr, LoadMap, Module};
use super::mcload::McImage;
use super::native_instance::NativeImage;

/// The instance state of one loaded `Module`, held by `ctx::Shared` beside it.
#[derive(Debug, Default)]
pub struct Instance {
    /// The module's own frozen area, mapped. Its link-domain layout is what its bytecode embeds.
    pub frozen: Option<FrozenArena>,
    /// Frozen areas of the base and absorbed dependency images. The delta bytecode embeds absolute
    /// addresses in those domains, so they stay mapped until guest exit.
    pub image_frozens: Vec<FrozenArena>,
    /// Link address to runtime address mapping for this instance, rebuilt whenever a frozen area or
    /// an entry closure changes.
    pub load_map: LoadMap,
    /// Runtime fn-ptr entry address to FuncId, the reverse lookup indirect call dispatch uses.
    pub fn_addrs: HashMap<u64, FuncId>,
    /// Artifact-address form of `fn_addrs`, rebuilt whenever instance or entry-closure addresses
    /// change.
    pub link_fn_addrs: HashMap<LinkAddr, FuncId>,
    /// Entry-closure addresses this instance has materialized and native code can call directly.
    pub executable_entry_addrs: HashSet<u64>,
    /// Per-Engine shared library images the Engine produced itself. They have completed dependency
    /// resolution and ELF relocation, but the Engine keeps managing them so it can run init and fini
    /// at the right time. Never cached or packaged.
    pub native_images: Vec<NativeImage>,
    /// This package's self-loaded machine-code image. It is visible only to this Module's foreign
    /// symbol resolution, which keeps global_asm names from colliding across Engines. Not serialized;
    /// rebuilt from the MC section when the package loads.
    pub mc_images: Vec<McImage>,
    /// Real addresses of the asm-stub wrappers, indexed by AsmStubId. The load phase produced each one
    /// by assembling it with cc, then dlopening and dlsym'ing it. Execution only reads a u64 and calls
    /// it directly, which keeps it pure.
    /// NOTE: not part of snapshot semantics; a warm load rematerializes these idempotently from
    /// `asm_sites` and overwrites whatever the snapshot held.
    pub asm_stub_addrs: Vec<u64>,
    /// Code-area handle lower used to allocate stable logical entry addresses. The mapping is released
    /// when the Engine starts, since the runtime executes per-Engine libffi closures instead. Not part
    /// of the artifact.
    pub entry_stubs: StubArena,
    /// Entry logical-address domains and recipes of the absorbed image and base, as
    /// (link-address domain base, recipes, lower-time address allocation handle).
    pub image_entry_stubs: Vec<(usize, Vec<EntryStubSite>, StubArena)>,
}

impl Instance {
    /// Build the instance a serialized artifact supports: restore its frozen image into the home
    /// domain the bytes were linked against and derive the runtime address tables from the artifact's
    /// link table.
    ///
    /// The restore is fixed-base on purpose. Re-mapping a snapshot elsewhere would leave every
    /// absolute address its bytes embed pointing at the old domain, so an occupied home is an error
    /// (a cache miss for the caller), not a silent fallback.
    ///
    /// State the artifact cannot describe the loader owns: asm-stub addresses, native and
    /// machine-code images, and entry-closure mappings start empty here.
    pub fn materialize(module: &Module) -> Result<Self, String> {
        let frozen = match &module.frozen {
            Some(snapshot) => Some(FrozenArena::restore(snapshot.bytes(), snapshot.home())?),
            None => None,
        };
        Ok(Self::from_artifact(module, frozen))
    }

    /// The same as [`Instance::materialize`], except the frozen bytes get a fresh anonymous mapping so
    /// several instances of one artifact can run in one process. Absolute addresses the bytes embed are
    /// translated through the load map, so only relocatable artifacts (packages) may take this route.
    pub fn materialize_dynamic(module: &Module) -> Result<Self, String> {
        let frozen = match &module.frozen {
            Some(snapshot) => Some(FrozenArena::restore_dynamic(snapshot)?),
            None => None,
        };
        Ok(Self::from_artifact(module, frozen))
    }

    fn from_artifact(module: &Module, frozen: Option<FrozenArena>) -> Self {
        let mut instance = Self {
            frozen,
            ..Default::default()
        };
        instance.link_fn_addrs = module.fn_entry_links.iter().copied().collect();
        instance.rebuild_load_map();
        instance.rebuild_fn_addrs();
        instance
    }

    /// Translate a link-time address to this instance's runtime address. An unmapped address aborts:
    /// the only correct reading of a missing mapping is that the artifact and the instance disagree.
    pub fn resolve_link_addr(&self, addr: LinkAddr) -> u64 {
        self.load_map
            .resolve_or_identity(addr)
            .unwrap_or_else(|| panic!("unmapped artifact address {:#x}", addr.0))
    }

    pub fn try_resolve_link_addr(&self, addr: LinkAddr) -> Result<u64, String> {
        self.load_map
            .resolve_or_identity(addr)
            .ok_or_else(|| format!("unmapped artifact address {:#x}", addr.0))
    }

    pub fn is_executable_entry(&self, addr: u64) -> bool {
        self.executable_entry_addrs.contains(&addr)
    }

    pub fn rebuild_load_map(&mut self) {
        let mut map = LoadMap::default();
        if let Some(frozen) = &self.frozen {
            map.add_frozen(frozen);
        }
        for frozen in &self.image_frozens {
            map.add_frozen(frozen);
        }
        self.load_map = map;
    }

    /// Write every frozen-domain pointer relocation the artifact carries. Both the cell to write and
    /// the value to write it are link addresses; the load map turns them into this instance's real
    /// addresses.
    pub fn apply_frozen_relocs(&self, module: &Module) -> Result<(), String> {
        use super::ir::FrozenRelocTarget;
        for (index, reloc) in module.frozen_relocs.iter().enumerate() {
            let at = self
                .load_map
                .resolve(reloc.at)
                .ok_or_else(|| format!("frozen relocation {index} write address is unmapped"))?;
            let target_link = match reloc.target {
                FrozenRelocTarget::Frozen(addr) | FrozenRelocTarget::Entry(addr) => addr,
            };
            let target = self.load_map.resolve(target_link).ok_or_else(|| {
                format!(
                    "frozen relocation {index} target address {:#x} ({:?}) is unmapped",
                    target_link.0, reloc.target
                )
            })?;
            unsafe { (at as *mut u64).write_unaligned(target) };
        }
        Ok(())
    }

    /// Recompute the runtime fn-ptr table from the link-address form. A hand-built instance that only
    /// knows real addresses (focused tests) seeds the link form from them on the first rebuild, which
    /// is a no-op translation because no load mapping is attached.
    pub fn rebuild_fn_addrs(&mut self) {
        if self.link_fn_addrs.is_empty() {
            self.link_fn_addrs = self
                .fn_addrs
                .iter()
                .map(|(&addr, &func)| (LinkAddr(addr), func))
                .collect();
        }
        self.fn_addrs = self
            .link_fn_addrs
            .iter()
            .map(|(&addr, &func)| (self.resolve_link_addr(addr), func))
            .collect();
    }

    /// Append argv to the frozen area as a NUL-terminated C string table and fill the entry plan's
    /// `argc` and `argv_ptr`.
    /// argv is a runtime input, so it must not enter the cache snapshot. Cold and warm paths both
    /// append and backfill after the snapshot on every run, which keeps the two paths from drifting
    /// apart.
    pub fn finalize_entry_argv(
        &mut self,
        module: &mut Module,
        argv: &[String],
    ) -> Result<(), String> {
        let Some(entry) = module.entry.as_mut() else {
            return Ok(());
        };
        let frozen = self
            .frozen
            .as_mut()
            .ok_or("executable module has no frozen memory for argv")?;
        let mut ptrs: Vec<u64> = Vec::with_capacity(argv.len());
        for a in argv {
            let bytes = a.as_bytes();
            let p = frozen.alloc(bytes.len() as u64 + 1, 1);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                *((p + bytes.len() as u64) as *mut u8) = 0;
            }
            ptrs.push(p);
        }
        let table = frozen.alloc((ptrs.len() as u64 + 1) * 8, 8);
        for (i, &p) in ptrs.iter().enumerate() {
            unsafe { *((table + i as u64 * 8) as *mut u64) = p };
        }
        // The trailing NULL is guaranteed because the frozen allocator zeroes new memory.
        entry.argc = argv.len() as u64;
        entry.argv_ptr = table;
        Ok(())
    }
}
