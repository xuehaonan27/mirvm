//! The serde documents a package carries: META, STAMPS, NATIVELIBS, MC, RELOC and MODULE.
//!
//! The write view borrows the frozen region and the index tables so packing copies nothing; the read
//! view owns what a load deserializes. Their field names and order are part of the format.

use serde::{Deserialize, Serialize};

use super::Error;

/// META section: the package form of the L2 header metadata (build_id is promoted into the
/// container header).
#[derive(Serialize, Deserialize)]
pub(super) struct Meta {
    pub(super) args: Vec<String>,
    /// `env!`/`option_env!` dependencies (as in L2: (name, compile-time value; None = unset at
    /// compile time))
    pub(super) envs: Vec<(String, Option<String>)>,
    /// Always None today (full modules only; the delta+BASE form is reserved).
    pub(super) base_key: Option<String>,
    pub(super) target: String,
}

/// NATIVELIBS entry: a produced library. `path` serves only cross-checking and diagnostics; the
/// bytes needed for execution always travel with the package and are never read from that path.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct NativeLibEntry {
    pub(super) path: String,
    /// 0=static_archive 1=global_asm (same family for bin/dep; cache/global-asm directory)
    pub(super) role: u8,
    pub(super) fnv: u128,
    pub(super) bytes: Vec<u8>,
}

/// MC entry: raw bytes of a produced global_asm/dep_asm `.so`. At package load time it is loaded
/// in-process (mcload) without dlopen, and cross-checked against NATIVELIBS by fnv.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct McEntry {
    pub(super) fnv: u128,
    pub(super) bytes: Vec<u8>,
}

/// RELOC section: fixed-base requirement plus entry symbol (entry semantics live in
/// `Module.entry`; this field is informational).
#[derive(Serialize, Deserialize)]
pub(super) struct Reloc {
    pub(super) requires_fixed_base: bool,
    pub(super) entry: Box<str>,
}

/// MODULE v4 stores only non-function metadata. A borrowed write form avoids copying the frozen
/// region and index tables.
#[derive(Serialize)]
pub(super) struct ModuleMetaRef<'a> {
    function_names: &'a [Box<str>],
    exports: &'a std::collections::HashMap<Box<str>, crate::vm::ir::FuncId>,
    frozen: Option<crate::vm::frozen::FrozenSnapshot>,
    link_fn_addrs: std::collections::HashMap<crate::vm::ir::LinkAddr, crate::vm::ir::FuncId>,
    native_libs: &'a [Box<str>],
    required_native_libs: &'a [Box<str>],
    tls: &'a [crate::vm::ir::TlsSlot],
    asm_stub_addrs: &'a [u64],
    asm_sites: &'a [crate::vm::ir::AsmSite],
    foreign_syms: &'a [crate::vm::ir::GotSym],
    got_fixups: &'a [crate::vm::ir::GotFixup],
    frozen_relocs: &'a [crate::vm::ir::FrozenReloc],
    entry_stub_sites: Vec<crate::vm::ir::EntryStubSite>,
    custom_alloc_shims: Option<crate::vm::ir::AllocShims>,
    guest_panic_cleanup: Option<crate::vm::ir::GuestPanicCleanup>,
    entry: Option<crate::vm::ir::EntryPlan>,
}

impl<'a> From<(&'a crate::vm::ir::Module, &'a crate::vm::instance::Instance)>
    for ModuleMetaRef<'a>
{
    fn from(
        (module, instance): (&'a crate::vm::ir::Module, &'a crate::vm::instance::Instance),
    ) -> Self {
        Self {
            function_names: &module.function_names,
            exports: &module.exports,
            frozen: module.frozen.clone(),
            link_fn_addrs: module.fn_entry_links.iter().copied().collect(),
            native_libs: &module.native_libs,
            required_native_libs: &module.required_native_libs,
            tls: &module.tls,
            asm_stub_addrs: &instance.asm_stub_addrs,
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
pub(super) struct ModuleMeta {
    function_names: Vec<Box<str>>,
    exports: std::collections::HashMap<Box<str>, crate::vm::ir::FuncId>,
    frozen: Option<crate::vm::frozen::FrozenSnapshot>,
    link_fn_addrs: std::collections::HashMap<crate::vm::ir::LinkAddr, crate::vm::ir::FuncId>,
    native_libs: Vec<Box<str>>,
    required_native_libs: Vec<Box<str>>,
    tls: Vec<crate::vm::ir::TlsSlot>,
    asm_stub_addrs: Vec<u64>,
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
    pub(super) fn instantiate(
        &self,
    ) -> Result<(crate::vm::ir::Module, crate::vm::instance::Instance), Error> {
        let module = crate::vm::ir::Module {
            funcs: Default::default(),
            function_names: self.function_names.clone(),
            exports: self.exports.clone(),
            frozen: self.frozen.clone(),
            fn_entry_links: self.link_fn_addrs.iter().map(|(&a, &f)| (a, f)).collect(),
            native_libs: self.native_libs.clone(),
            required_native_libs: self.required_native_libs.clone(),
            required_native_hashes: Vec::new(),
            tls: self.tls.clone(),
            asm_sites: self.asm_sites.clone(),
            foreign_syms: self.foreign_syms.clone(),
            got_fixups: self.got_fixups.clone(),
            frozen_relocs: self.frozen_relocs.clone(),
            entry_stub_sites: self.entry_stub_sites.clone(),
            custom_alloc_shims: self.custom_alloc_shims,
            guest_panic_cleanup: self.guest_panic_cleanup,
            entry: self.entry,
        };
        // The function table arrives separately in the FUNCS section; the name table is copied from
        // the package verbatim and must not be re-derived from the (still empty) function table.
        // Each instantiation gets its own frozen mapping so several instances of one package can run
        // at once; the embedded addresses are translated by the load map, which a packaged artifact
        // always fully covers.
        let mut instance =
            crate::vm::instance::Instance::materialize_dynamic(&module).map_err(Error::reject)?;
        instance.asm_stub_addrs = self.asm_stub_addrs.clone();
        instance.load_map.require_mapped();
        Ok((module, instance))
    }
}

/// The target this build runs on, spelled as [`Meta::target`] records it: the reader compares
/// against it and the writer stores it, so the two spellings are one fact.
pub(super) fn host_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The NATIVELIBS entry an MC image came from: the global_asm role with the same digest.
/// Load-time cross-checking and instantiation both ask this question.
pub(super) fn native_entry_for_mc(libs: &[NativeLibEntry], fnv: u128) -> Option<&NativeLibEntry> {
    libs.iter().find(|lib| lib.role == 1 && lib.fnv == fnv)
}

/// A library's bytes still hashing to the digest the container recorded for them. Both the loader's
/// cross-check and materialization ask it, and both report the same corruption.
pub(super) fn check_lib_hash(lib: &NativeLibEntry) -> Result<(), Error> {
    super::format::check_hash(
        &lib.bytes,
        lib.fnv,
        format!("package native library `{}` has wrong hash", lib.path),
    )
}

pub(super) fn postcard_bytes<T: Serialize>(v: &T) -> Result<Vec<u8>, Error> {
    postcard::to_stdvec(v).map_err(|e| Error::build(format!("cannot format the package: {e}")))
}
