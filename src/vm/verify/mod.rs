//! Frozen bytecode verification.
//!
//! Deserialization only proves that bytes have the right Rust shape. This pass
//! checks the indexes and memory ranges that the interpreter and JIT otherwise
//! consume with direct indexing or raw pointer arithmetic.
//!
//! The `Verifier` is one walk with four parts, so the walk is split by what it is looking at:
//! [`body`] checks a function's blocks, statements, rvalues and terminators; [`ffi`] checks a
//! foreign signature and the libffi layout it implies; [`vocab`] checks the operands, places,
//! slots and ids those are written in. This file holds the entry points, the walk itself, and the
//! header counts.

use super::instance::Instance;
use super::ir::*;

const REGION_CAP: u64 = 1 << 30;

/// Numbering already occupied by lower image layers. Delta and dependency
/// images keep their final, absolute ids even while stored separately.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prefix {
    pub funcs: usize,
    pub tls: usize,
    pub asm: usize,
}

pub fn module(module: &Module, instance: &Instance) -> Result<(), String> {
    module_with_prefix(module, instance, Prefix::default())
}

pub fn module_with_prefix(
    module: &Module,
    instance: &Instance,
    prefix: Prefix,
) -> Result<(), String> {
    Verifier::new(module, instance, prefix)?.run()
}

/// Indexed packages first verify module-level references; function bodies are decoded one by one
/// by the caller from mmap slices via `function_with_count`, so the entire function set is not kept in memory.
pub(crate) fn module_header_with_count(
    module: &Module,
    instance: &Instance,
    funcs: usize,
) -> Result<(), String> {
    Verifier::new_with_count(module, instance, Prefix::default(), funcs)?.run_header()
}

pub(crate) fn function_with_count(
    module: &Module,
    instance: &Instance,
    funcs: usize,
    index: usize,
    body: &FuncBody,
) -> Result<(), String> {
    let verifier = Verifier::new_with_count(module, instance, Prefix::default(), funcs)?;
    verifier
        .body(body)
        .map_err(|error| format!("function {index} `{}`: {error}", body.name))
}

pub(crate) fn main_role_counts(
    module: &Module,
    boundaries: usize,
    catchers: usize,
) -> Result<(), String> {
    if module.entry.is_some() && boundaries != 1 {
        return Err(format!(
            "executable module has {boundaries} main panic boundaries, expected exactly one"
        ));
    }
    if module.entry.is_some() && catchers != 1 {
        return Err(format!(
            "executable module has {catchers} main panic catchers, expected exactly one"
        ));
    }
    Ok(())
}

pub(crate) fn body_main_role_counts(body: &FuncBody) -> (usize, usize) {
    body.blocks
        .iter()
        .fold((0, 0), |(boundaries, catchers), block| match block.term {
            Terminator::Call {
                role: CallRole::MainPanicBoundary,
                ..
            } => (boundaries + 1, catchers),
            Terminator::CallBuiltin {
                role: BuiltinCallRole::MainPanicCatcher,
                ..
            } => (boundaries, catchers + 1),
            _ => (boundaries, catchers),
        })
}

struct Verifier<'a> {
    module: &'a Module,
    instance: &'a Instance,
    prefix: Prefix,
    funcs: usize,
    tls: usize,
    asm: usize,
}

mod body;
mod ffi;
mod vocab;

impl<'a> Verifier<'a> {
    pub(super) fn new(
        module: &'a Module,
        instance: &'a Instance,
        prefix: Prefix,
    ) -> Result<Self, String> {
        Self::new_with_count(module, instance, prefix, module.funcs.len())
    }

    pub(super) fn new_with_count(
        module: &'a Module,
        instance: &'a Instance,
        prefix: Prefix,
        local_funcs: usize,
    ) -> Result<Self, String> {
        let funcs = total("function", prefix.funcs, local_funcs)?;
        let tls = total("TLS", prefix.tls, module.tls.len())?;
        let asm = total("inline-asm stub", prefix.asm, module.asm_sites.len())?;
        Ok(Self {
            module,
            instance,
            prefix,
            funcs,
            tls,
            asm,
        })
    }

    pub(super) fn run(&self) -> Result<(), String> {
        self.run_header()?;
        let mut main_boundaries = 0;
        let mut main_catchers = 0;
        for (i, body) in self.module.funcs.iter().enumerate() {
            self.body(body)
                .map_err(|e| format!("function {} `{}`: {e}", self.prefix.funcs + i, body.name))?;
            let (boundaries, catchers) = body_main_role_counts(body);
            main_boundaries += boundaries;
            main_catchers += catchers;
        }
        // Delta images can refer to a boundary stored in an earlier image layer. The merged
        // executable (prefix zero) must contain exactly one before it can run or be packed.
        if self.prefix.funcs == 0 {
            main_role_counts(self.module, main_boundaries, main_catchers)?;
        }
        Ok(())
    }

    pub(super) fn run_header(&self) -> Result<(), String> {
        if self.module.entry.is_some() && self.instance.frozen.is_none() {
            return Err("executable module has an entry plan but no frozen memory for argv".into());
        }
        if !self.instance.asm_stub_addrs.is_empty()
            && self.instance.asm_stub_addrs.len() != self.module.asm_sites.len()
            && self.instance.asm_stub_addrs.len() != self.asm
        {
            return Err(format!(
                "inline-asm address table has {} entries, expected {} local or {} merged entries",
                self.instance.asm_stub_addrs.len(),
                self.module.asm_sites.len(),
                self.asm
            ));
        }

        for (name, &id) in &self.module.exports {
            self.func(id).map_err(|e| format!("export `{name}`: {e}"))?;
        }
        for (&addr, &id) in &self.instance.fn_addrs {
            if addr == 0 {
                return Err("function address table contains a null address".into());
            }
            self.func(id)
                .map_err(|e| format!("function address {addr:#x}: {e}"))?;
        }
        for (&addr, &id) in &self.instance.link_fn_addrs {
            if addr.0 == 0 {
                return Err("logical function address table contains a null address".into());
            }
            self.func(id)
                .map_err(|e| format!("logical function address {:#x}: {e}", addr.0))?;
        }
        for (i, slot) in self.module.tls.iter().enumerate() {
            if slot.align == 0 || !slot.align.is_power_of_two() {
                return Err(format!(
                    "TLS slot {} has invalid alignment {}",
                    self.prefix.tls + i,
                    slot.align
                ));
            }
            let template = self.instance.try_resolve_link_addr(slot.template)?;
            self.frozen_range(template, slot.size, false)
                .map_err(|e| format!("TLS slot {} template: {e}", self.prefix.tls + i))?;
        }
        for (i, fixup) in self.module.got_fixups.iter().enumerate() {
            if fixup.sym as usize >= self.module.foreign_syms.len() {
                return Err(format!(
                    "GOT fixup {i} refers to symbol {}, but only {} symbols exist",
                    fixup.sym,
                    self.module.foreign_syms.len()
                ));
            }
            let addr = self.instance.try_resolve_link_addr(fixup.addr)?;
            self.frozen_range(addr, 8, false)
                .map_err(|e| format!("GOT fixup {i}: {e}"))?;
        }
        for (i, reloc) in self.module.frozen_relocs.iter().enumerate() {
            let at = self.instance.try_resolve_link_addr(reloc.at)?;
            self.frozen_range(at, 8, false)
                .map_err(|e| format!("frozen relocation {i} write address: {e}"))?;
            match reloc.target {
                FrozenRelocTarget::Frozen(target) => {
                    let target = self.instance.try_resolve_link_addr(target)?;
                    self.frozen_range(target, 0, true)
                        .map_err(|e| format!("frozen relocation {i} target: {e}"))?;
                }
                FrozenRelocTarget::Entry(target) => {
                    if !self.instance.link_fn_addrs.contains_key(&target) {
                        return Err(format!(
                            "frozen relocation {i} refers to unknown entry {:#x}",
                            target.0
                        ));
                    }
                }
            }
        }
        let mut entry_sites = std::collections::HashMap::new();
        {
            let mut verify_entry_site = |label: &str, site: &EntryStubSite| -> Result<(), String> {
                self.func(site.func).map_err(|e| format!("{label}: {e}"))?;
                if self.instance.link_fn_addrs.get(&site.link_addr) != Some(&site.func) {
                    return Err(format!(
                        "{label} link address {:#x} is absent or names a different function",
                        site.link_addr.0
                    ));
                }
                if self.instance.load_map.resolves_frozen(site.link_addr) {
                    return Err(format!(
                        "{label} link address {:#x} overlaps frozen memory",
                        site.link_addr.0
                    ));
                }
                if entry_sites.insert(site.link_addr, site.func).is_some() {
                    return Err(format!(
                        "{label} duplicates entry link address {:#x}",
                        site.link_addr.0
                    ));
                }
                self.foreign_sig(&site.sig)
                    .map_err(|e| format!("{label}: {e}"))?;
                Ok(())
            };
            for (i, site) in self.module.entry_stub_sites.iter().enumerate() {
                verify_entry_site(&format!("entry stub {i}"), site)?;
            }
            for (arena_i, (_, sites, _)) in self.instance.image_entry_stubs.iter().enumerate() {
                for (site_i, site) in sites.iter().enumerate() {
                    verify_entry_site(&format!("image entry stub {arena_i}:{site_i}"), site)?;
                }
            }
        }
        if self.instance.load_map.is_strict() {
            for (&addr, &func) in &self.instance.link_fn_addrs {
                if !self.instance.load_map.resolves_frozen(addr)
                    && entry_sites.get(&addr) != Some(&func)
                {
                    return Err(format!(
                        "logical function address {:#x} is outside frozen memory but has no matching entry stub",
                        addr.0
                    ));
                }
            }
        }
        if let Some(shims) = self.module.custom_alloc_shims {
            for (name, id) in [
                ("alloc", shims.alloc),
                ("dealloc", shims.dealloc),
                ("realloc", shims.realloc),
                ("alloc_zeroed", shims.alloc_zeroed),
            ] {
                self.func(id)
                    .map_err(|e| format!("global allocator `{name}` shim: {e}"))?;
            }
        }
        if let Some(plan) = self.module.guest_panic_cleanup {
            if plan.cleanup == plan.drop_payload {
                return Err(
                    "guest panic cleanup and payload drop glue refer to the same function".into(),
                );
            }
            self.func(plan.cleanup)
                .map_err(|e| format!("guest panic cleanup: {e}"))?;
            self.func(plan.drop_payload)
                .map_err(|e| format!("guest panic payload drop glue: {e}"))?;
        }
        if let Some(entry) = self.module.entry {
            self.func(entry.lang_start)
                .map_err(|e| format!("entry lang_start: {e}"))?;
            let known = if self.instance.link_fn_addrs.is_empty() {
                self.instance.fn_addrs.contains_key(&entry.main_addr.0)
            } else {
                self.instance.link_fn_addrs.contains_key(&entry.main_addr)
            };
            if !known {
                return Err(format!(
                    "entry main address {:#x} is absent from the function address table",
                    entry.main_addr
                ));
            }
        }

        Ok(())
    }
}

pub(super) fn total(kind: &str, prefix: usize, local: usize) -> Result<usize, String> {
    let n = prefix
        .checked_add(local)
        .ok_or_else(|| format!("{kind} count overflows"))?;
    if n > u32::MAX as usize + 1 {
        return Err(format!("{kind} count {n} exceeds the IR id space"));
    }
    Ok(n)
}

pub(super) fn vector(lanes: u16, lane_bytes: u8) -> Result<(), String> {
    if lanes == 0 || lane_bytes == 0 {
        return Err(format!(
            "invalid SIMD geometry: {lanes} lanes x {lane_bytes} bytes"
        ));
    }
    Ok(())
}

pub(super) fn buffer_span(buf_size: u32, off: u32, width: u32) -> Result<(), String> {
    let end = off
        .checked_add(width)
        .ok_or("inline-asm buffer range overflows")?;
    if end > buf_size {
        return Err(format!("buffer range {off}..{end} exceeds size {buf_size}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
