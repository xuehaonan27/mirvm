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
    /// Frozen area holding statics, the constant pool and fn entries, in artifact form: the clean
    /// bytes as lowered plus the domain they were linked against. It carries no mapping; the instance
    /// that will run the module owns that.
    #[serde(
        default,
        serialize_with = "crate::vm::frozen::serialize_module_frozen",
        deserialize_with = "crate::vm::frozen::deserialize_module_frozen"
    )]
    pub frozen: Option<crate::vm::frozen::FrozenSnapshot>,
    /// Link address of every function entry whose address is taken, in lowering order. It is the
    /// artifact form of the reverse function-address table: the running instance translates each link
    /// address to its own real address. Entries cover frozen data slots, executable entry stubs and
    /// resolved foreign symbols alike.
    pub fn_entry_links: Vec<(LinkAddr, FuncId)>,
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
    /// Guest TLS slot table, indexed by TlsId. The per-thread instances live in `Ctx.tls`.
    pub tls: Vec<TlsSlot>,
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
    /// frozen-domain LinkAddr of this module; the instance's load map turns it into the real slot.
    pub got_fixups: Vec<GotFixup>,
    /// Object pointer relocations inside/between frozen domains, excluding foreign GOT fixup points.
    pub frozen_relocs: Vec<FrozenReloc>,
    /// Entry recipes for guest functions in this domain that have their address taken and can be exported
    /// with the C ABI. Each Engine builds its own closures and LinkAddr mappings from them.
    pub entry_stub_sites: Vec<EntryStubSite>,
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
}

impl Module {
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
