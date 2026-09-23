//! The linked artifact: a `Module`'s tables — TLS slots, entry plans, GOT fixups, frozen
//! relocations, entry stubs, allocation shims and panic cleanup — and the entry-slot naming they
//! share with the JIT.

use super::*;

/// Guest TLS slot description, frozen at lower time. `template` is the real address of the initial
/// bytes in the frozen area, relocations included. A thread's first access heap-allocates `size` bytes
/// and copies the template.
/// NOTE: the destructor does not run yet; the instance is freed with the Ctx.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct TlsSlot {
    pub template: LinkAddr,
    pub size: u64,
    pub align: u32,
}

/// Startup plan for the guest's main, matching what cg_ssa's `create_entry_fn` produces:
/// `lang_start(main fn-ptr, argc, argv, sigpipe) -> isize`, where the result is the process exit
/// code.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct EntryPlan {
    pub lang_start: FuncId,
    /// Real entry address of user `main`, passed as lang_start's first argument and dispatched through
    /// a CallIndirect.
    pub main_addr: LinkAddr,
    pub argc: u64,
    /// Real address of the argv C-string pointer table in the frozen area.
    pub argv_ptr: u64,
    pub sigpipe: u8,
}

/// One entry of the GOT symbol table: a name plus whether the symbol is weak. A weak symbol that
/// fails to resolve writes 0 rather than aborting, which is the NULL semantics of an absent weak extern.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GotSym {
    pub name: Box<str>,
    pub weak: bool,
}

/// A fixup point applied at startup: the load phase writes
/// `*addr = resolve(foreign_syms[sym]) + addend`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct GotFixup {
    pub addr: LinkAddr,
    pub sym: u32,
    pub addend: u64,
}

/// One object pointer inside the frozen bytes. `at` is the 8-byte cell to write, `target` is the link
/// address it should point to (addend already folded); at instantiation both ends are translated via
/// LoadMap before writing.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct FrozenReloc {
    pub at: LinkAddr,
    pub target: FrozenRelocTarget,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum FrozenRelocTarget {
    Frozen(LinkAddr),
    Entry(LinkAddr),
}

/// Recipe for an executable entry. The artifact stores only the guest function's logical address, its
/// FuncId and its frozen C ABI signature; each Engine materializes its own libffi closure at startup.
/// Real fn-ptrs are never cached or packaged, because they differ per process.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntryStubSite {
    /// Logical identity shared by all fn-ptr references to this fn in the artifact; each Engine maps it
    /// to a unique closure.
    pub link_addr: LinkAddr,
    pub func: FuncId,
    pub sig: ForeignSig,
}

/// Hidden ELF symbol used by native bridges that call back into a guest entry.
/// The symbol identifies the artifact address only; each Engine writes its own
/// runtime closure address into the corresponding slot while instantiating.
pub(crate) fn native_entry_slot_name(link_addr: LinkAddr) -> String {
    format!("__mirvm_p1_target_{:016x}", link_addr.0)
}

/// FuncIds of the four `__rust_*` shims that a custom `#[global_allocator]` produces. For a crate with
/// that attribute, the HIR expander generates four local forwarding functions, each calling one method
/// of the user's GlobalAlloc.
/// Allocation is program-level semantics: `CallBuiltin(Rust*)` arms baked into a base or dependency
/// image and the shim in the delta module must reach the same allocator, since freeing a pointer on a
/// different heap corrupts allocator metadata. The interpreter therefore routes all of them through
/// this field, whichever session baked the bytecode.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AllocShims {
    pub alloc: FuncId,
    pub dealloc: FuncId,
    pub realloc: FuncId,
    pub alloc_zeroed: FuncId,
}

/// How to reclaim guest resources for an uncaught guest panic.
///
/// `cleanup` is std's object function `std::panicking::catch_unwind::cleanup`: it receives the
/// panic_unwind raw exception pointer, extracts the `Box<dyn Any + Send>`, and decrements the guest
/// panic count. `drop_payload` is the drop glue for that Box, which runs the user payload's Drop and
/// frees through the guest's own global allocator. The engine moves two opaque machine words and never
/// reads std's private exception, Box or vtable layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuestPanicCleanup {
    pub cleanup: FuncId,
    pub drop_payload: FuncId,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Module {
    pub funcs: FuncTable,
    /// Symbol names in FuncId order. Even when function bodies are decoded lazily, backtrace can build
    /// a standard ELF symbol table from this without decoding every body.
    pub function_names: Vec<Box<str>>,
    /// Exported `no_mangle` symbol name to FuncId, used by `--vm-call` lookup.
    pub exports: std::collections::HashMap<Box<str>, FuncId>,
    /// Frozen area holding statics, the constant pool and fn entries. Lower materializes it, and it is
    /// read-only after publication except for `static mut` cells.
    pub frozen: Option<crate::vm::frozen::FrozenArena>,
    /// Link address to runtime address mapping for this Module instance. Built when a package or image
    /// is instantiated; not part of the artifact.
    #[serde(skip)]
    pub load_map: LoadMap,
    /// fn-ptr entry real address to FuncId, the reverse lookup indirect call dispatch uses.
    pub fn_addrs: std::collections::HashMap<u64, FuncId>,
    /// Artifact-address form of `fn_addrs`, rebuilt whenever instance or entry-closure addresses
    /// change.
    pub link_fn_addrs: std::collections::HashMap<LinkAddr, FuncId>,
    /// Entry-closure addresses this Engine has materialized and native code can call directly.
    #[serde(skip)]
    pub executable_entry_addrs: std::collections::HashSet<u64>,
    /// Candidate paths of shared libraries named by `-l` directives. They are optional: a candidate that
    /// does not exist is skipped in favour of the next.
    pub native_libs: Vec<Box<str>>,
    /// Shared libraries already materialized by the loading phase that must dlopen successfully before
    /// executing foreign code, for instance the `.so` products of static archives. Loading one of these
    /// must not degrade into an ordinary dlsym miss.
    pub required_native_libs: Vec<Box<str>>,
    /// Expected content identity of each required native library, positionally matched with
    /// `required_native_libs`. A zero entry marks a raw in-process Module whose caller supplied no
    /// artifact hash; a Package always carries and verifies this list.
    #[serde(skip)]
    pub required_native_hashes: Vec<u128>,
    /// Per-Engine shared library images the Engine produced itself. They have completed dependency
    /// resolution and ELF relocation, but the Engine keeps managing them so it can run init and fini at
    /// the right time. Never cached or packaged.
    #[serde(skip)]
    pub native_images: Vec<crate::vm::native_instance::NativeImage>,
    /// This package's self-loaded machine-code image. It is visible only to this Module's foreign symbol
    /// resolution, which keeps global_asm names from colliding across Engines. Not serialized; rebuilt
    /// from the MC section when the package loads.
    #[serde(skip)]
    pub mc_images: Vec<crate::vm::mcload::McImage>,
    /// Guest TLS slot table, indexed by TlsId. The per-thread instances live in `Ctx.tls`.
    pub tls: Vec<TlsSlot>,
    /// Real addresses of the asm-stub wrappers, indexed by AsmStubId. The load phase produced each one by
    /// assembling it with cc, then dlopening and dlsym'ing it. Execution only reads a u64 and calls it
    /// directly, which keeps it pure.
    /// NOTE: not part of snapshot semantics; a warm load rematerializes these idempotently from
    /// `asm_sites` and overwrites whatever the snapshot held.
    pub asm_stub_addrs: Vec<u64>,
    /// asm-stub materialization recipe: the symbol name plus the full wrapper GAS text, ordered by
    /// AsmStubId, which is also the bit order. A warm load reruns `asm::materialize` over it, so a content
    /// hash hit in the `.so` cache costs only dlopen and dlsym, while a cleared cache re-runs cc.
    /// Symbol names are decoupled from bit order because split lowering only knows the final bit order at
    /// the end; it therefore uses class-prefixed names (`mirvm_asm_xi{j}` / `mirvm_asm_xd{k}`) and
    /// non-split lowering keeps the positional `mirvm_asm_{id}`.
    pub asm_sites: Vec<AsmSite>,
    /// GOT symbol table. The slots themselves are ordinary 8-byte cells in the frozen area, and the
    /// bytecode bakes their addresses rather than their values, so startup can re-resolve each name and
    /// rewrite the cells. That is what keeps a module position-independent across ASLR. Each image has its
    /// own table, merged by name during absorb.
    pub foreign_syms: Vec<GotSym>,
    /// Fixup points applied at startup: `*(addr) = resolve(foreign_syms[sym]) + addend`. `addr` is a
    /// frozen-domain LinkAddr of this module; LoadMap turns it into the real slot after instantiation.
    pub got_fixups: Vec<GotFixup>,
    /// Object pointer relocations inside/between frozen domains, excluding foreign GOT fixup points.
    pub frozen_relocs: Vec<FrozenReloc>,
    /// Entry recipes for guest functions in this domain that have their address taken and can be exported
    /// with the C ABI. Each Engine builds its own closures and LinkAddr mappings from them.
    pub entry_stub_sites: Vec<EntryStubSite>,
    /// Code-area handle lower used to allocate stable logical addresses. The mapping is released when the
    /// Engine starts, since the runtime executes per-Engine libffi closures instead. Not part of the
    /// artifact.
    #[serde(skip)]
    pub entry_stubs: crate::vm::codearena::StubArena,
    /// Entry logical-address domains and recipes of the absorbed image and base, as
    /// (link-address domain base, recipes, lower-time address allocation handle).
    #[serde(skip)]
    pub image_entry_stubs: Vec<(usize, Vec<EntryStubSite>, crate::vm::codearena::StubArena)>,
    /// Custom `__rust_*` shims of a `#[global_allocator]`; see the `AllocShims` note. A Global allocator is
    /// always registered on the delta side, and the interpreter's `CallBuiltin(Rust*)` arms route through
    /// it.
    pub custom_alloc_shims: Option<AllocShims>,
    /// Cleanup plan the Engine top level executes after catching an uncaught guest panic. Hand-built test
    /// Modules and non-executable image stack layers may leave it None, but every executable lower product
    /// must have one, and the run entry refuses None rather than leaking the payload.
    pub guest_panic_cleanup: Option<GuestPanicCleanup>,
    /// Startup chain of `main`; None when running in `--vm-call` mode.
    pub entry: Option<EntryPlan>,
    /// Frozen areas of the base image and every absorbed dependency image. Absorbing mounts them with the
    /// same lifetime as this module, because the delta bytecode embeds absolute addresses in those domains
    /// and they must stay mapped until guest exit.
    /// NOTE: not part of snapshot semantics; image files have their own lifecycles, and delta entries refer
    /// to them only through the key chain.
    #[serde(skip)]
    pub image_frozens: Vec<crate::vm::frozen::FrozenArena>,
}

impl Module {
    /// Translate a link-time address to this Module instance's runtime address. A freshly lowered
    /// module resolves to identity until a dynamic load mapping is attached.
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

    pub fn apply_frozen_relocs(&self) -> Result<(), String> {
        for (index, reloc) in self.frozen_relocs.iter().enumerate() {
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

    pub fn ensure_function_names(&mut self) {
        if self.function_names.len() != self.funcs.len() {
            self.function_names = self.funcs.iter().map(|body| body.name.clone()).collect();
        }
    }

    /// Switch the plain syscall builtin for the trace-capable one.
    /// The serialized Module always stores the plain form; the rewrite happens only once a session has
    /// armed capture and before `Shared` publishes the Module for execution, because the builtin must
    /// match the code domain that publication freezes.
    pub(crate) fn rewrite_host_syscalls_for_capture(&mut self) {
        for body in self.funcs.iter_mut() {
            for block in &mut body.blocks {
                let Terminator::CallBuiltin { builtin, .. } = &mut block.term else {
                    continue;
                };
                if matches!(builtin, Builtin::HostSyscall) {
                    *builtin = Builtin::HostSyscallTrace;
                }
            }
        }
    }
    /// Append argv to the frozen area as a NUL-terminated C string table and fill the entry plan's
    /// `argc` and `argv_ptr`.
    /// argv is a runtime input, so it must not enter the cache snapshot. Cold and warm paths both append
    /// and backfill after the snapshot on every run, which keeps the two paths from drifting apart.
    pub fn finalize_entry_argv(&mut self, argv: &[String]) -> Result<(), String> {
        let Some(entry) = self.entry.as_mut() else {
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

    /// Merge an image's GOT into this module: symbols are deduplicated by name and the fixups' symbol
    /// indices are remapped to the merged table. A fixup address is a frozen-domain address from the
    /// image, which uses a fixed base, so it still names the same cell after the merge and is taken
    /// over unchanged.
    pub fn absorb_got(&mut self, syms: Vec<GotSym>, mut fixups: Vec<GotFixup>) {
        if fixups.is_empty() {
            return;
        }
        let mut remap: Vec<u32> = Vec::with_capacity(syms.len());
        for s in syms {
            let idx = match self.foreign_syms.iter().position(|e| e.name == s.name) {
                Some(i) => {
                    // Merging also merges weak/strong: one strong definition makes the symbol strong.
                    if !s.weak {
                        self.foreign_syms[i].weak = false;
                    }
                    i as u32
                }
                None => {
                    self.foreign_syms.push(s);
                    (self.foreign_syms.len() - 1) as u32
                }
            };
            remap.push(idx);
        }
        for f in &mut fixups {
            f.sym = remap[f.sym as usize];
        }
        self.got_fixups.append(&mut fixups);
    }
}
