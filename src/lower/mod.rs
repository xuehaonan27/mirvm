//! Loading phase: lowers MIR into engine bytecode. All `rustc_private` access is confined here;
//! the product is a pure-Rust `ir::Module` with no `tcx` references.
//!
//! Lowering starts from the mono collector's output as a seed and then closes over the call graph
//! by worklist: meeting a callee that has no FuncId yet assigns one and enqueues its body. The
//! collector takes a codegen/linking view, so it does not seed every function we may need -- the
//! worklist is what makes the result total. There is no separate "link libstd.so" step; every
//! function the guest runs is lowered here.
//!
//! Lowering never aborts on an unrecognized construct: such a site becomes `Trap(diagnostic)`
//! in place, and only paths that are actually executed have to be trap-free.

/// Why lowering could not produce the bytecode it was asked for.
///
/// The layer's own boundary is the important split: `Unsupported` means the program uses something
/// this lowerer does not implement (the `M4.x`, `C1` and `F-06` boundaries named in the detail), so
/// the answer is at the product boundary; `Internal` means an assumption this layer makes about the
/// MIR or the ABI shape did not hold, so the answer is in this tree. `Assemble` is the asm-recipe
/// path (`cc`, `dlopen`, `dlsym`) and `Io` is a filesystem step, with the `io::Error` as its source.
///
/// A `Trap(diagnostic)` is *not* one of these: an unexecuted path may stay a trap, so an
/// unrecognized construct only becomes an error when something asks for it.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    #[error("{detail}")]
    Unsupported { detail: String },

    #[error("{detail}")]
    Internal { detail: String },

    #[error("{detail}")]
    Assemble { detail: String },

    /// A `sym` in a dependency's asm points at that dependency's own guest function, whose
    /// cross-crate entry budget belongs to the bin link context and is not implemented: the
    /// dependency's manifest is skipped rather than failing its build. This is a separate variant
    /// because the caller acts on it, which used to be a string prefix test.
    #[error("{detail}")]
    DepGuestSym { detail: String },

    #[error("{detail}: {source}")]
    Io {
        detail: String,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },
}

crate::diag_codes! {
    Error: Lower => {
        Unsupported => "lower.unsupported",
        Internal => "lower.internal",
        Assemble => "lower.assemble",
        DepGuestSym => "lower.dep_guest_sym",
        Io => "lower.io",
    }
}

impl Error {
    fn unsupported(detail: impl Into<String>) -> Self {
        Error::Unsupported {
            detail: detail.into(),
        }
    }

    fn internal(detail: impl Into<String>) -> Self {
        Error::Internal {
            detail: detail.into(),
        }
    }

    fn assemble(detail: impl Into<String>) -> Self {
        Error::Assemble {
            detail: detail.into(),
        }
    }

    fn io(detail: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            detail: detail.into(),
            source,
        }
    }
}

pub mod asm;
pub mod collect;
pub mod frame;
pub mod func;
pub mod global_asm;

use std::collections::VecDeque;

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::{AllocId, ConstAllocation, GlobalAlloc};
use rustc_middle::ty::{Instance, InstanceKind, TyCtxt, TypingEnv};
use rustc_span::Symbol;

use crate::vm::frozen::FrozenArena;
use crate::vm::ir;

/// Resolution result for a call target (foreign three-way handling).
pub(crate) enum Callee {
    /// Ordinary guest function (includes std implementations resolved by linking emulation, and
    /// intrinsic fallback bodies collected retroactively).
    Func(ir::FuncId),
    /// Engine primitive (std runtime extern boundary: alloc/unwind family + stubs).
    Builtin(ir::Builtin),
    /// os:: pass-through (dlsym+libffi): fixed arguments are frozen as FfiKind; the variadic
    /// tail is filled from call-site arguments.
    /// `thunk_args` = fn-ptr typed argument positions plus their inner frozen signatures.
    Foreign {
        sym: Box<str>,
        args: Vec<ir::FfiKind>,
        ret: ir::FfiKind,
        variadic: bool,
        thunk_args: Vec<(usize, ir::ForeignSig)>,
        /// Whether the outer foreign declaration lets exceptions cross the call boundary (C-unwind/System-unwind).
        unwind: bool,
    },
}

/// Exact symbol names that must never pass through to native, because a real call would bypass the
/// process/thread model the VM maintains.
/// `pthread_create`/`join`/`detach` and the `posix_spawn` family are deliberately absent: real
/// threads are supported, and `posix_spawn` execs in the child before any VM state runs there.
/// `fork` and the exec family are routed to builtins instead (see `Builtin::HostFork`).
/// `pthread_exit` is denied because glibc forces an unwind around `FrameGuard`; vfork/clone/setjmp
/// stay denied because modelling them needs frame-level work the engine does not do.
const DENY_EXACT: &[&str] = &[
    "vfork",
    "clone",
    "clone3",
    "setjmp",
    "longjmp",
    "sigsetjmp",
    "siglongjmp",
    "pthread_exit",
    "pthread_atfork",
];
const DENY_PREFIX: &[&str] = &[];

/// A split-lowering id carries its home in the top byte: the delta is the untagged value (`0`), home
/// `i` is `tag(i + 1)`. The tag is stripped during rebase, which happens before the execution phase
/// ever sees an id — tagged ids must never escape lowering. FuncId/TlsId/AsmStubId are isomorphic (all
/// u32), so one tag serves all three spaces.
const HOME_TAG_SHIFT: u32 = 24;
const HOME_TAG_MASK: u32 = 0xff00_0000;
/// Highest home index a tag can carry.
pub(crate) const MAX_HOMES: usize = 255;

/// Tag one home-local ordinal.
pub(crate) fn home_tag(home: usize, ordinal: u32) -> u32 {
    debug_assert!(
        home < MAX_HOMES,
        "home index {home} does not fit the id tag"
    );
    ((home as u32 + 1) << HOME_TAG_SHIFT) | ordinal
}

/// The home a tagged id names, `None` for the delta.
pub(crate) fn id_home(id: u32) -> Option<usize> {
    match (id & HOME_TAG_MASK) >> HOME_TAG_SHIFT {
        0 => None,
        tag => Some((tag - 1) as usize),
    }
}

/// The home-local ordinal inside a tagged id.
pub(crate) fn id_ordinal(id: u32) -> u32 {
    id & !HOME_TAG_MASK
}

/// One home's lowering state: everything the two-way split kept for its single image, per home. The
/// delta is not a home — it reuses the main `Linker` fields (queue/funcs/frozen/alloc_addrs/...).
pub(crate) struct HomeLayer<'tcx> {
    /// Frozen area.
    frozen: FrozenArena,
    /// Pending-lowering queue (tagged ids).
    queue: VecDeque<(ir::FuncId, Instance<'tcx>)>,
    /// Function bodies (ordinal j maps to tagged id `home_tag(home, j)`).
    funcs: Vec<Option<ir::FuncBody>>,
    /// Next ordinal within this home.
    fn_next: ir::FuncId,
    /// TLS slots (TlsId is isomorphic).
    tls_slots: Vec<ir::TlsSlot>,
    /// asm sites (symbol names mirvm_asm_xh{home}_{j}).
    asm_sites: Vec<ir::AsmSite>,
    /// Constant dedup table (the delta side is `Linker.alloc_addrs`; promotion duplicates the
    /// materialization rather than sharing it).
    alloc_addrs: FxHashMap<AllocId, u64>,
    /// fn entry table (instance to entry address). Includes entries that are supplementary-built in
    /// this home's area even though the instance itself belongs elsewhere; this is the authority for
    /// the loader-side fn_entry_syms index, which must keep one symbol identity per function.
    fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// Instance table, used by the post-rebase self-check that confirms no LOCAL_CRATE contamination.
    insts: Vec<Instance<'tcx>>,
    /// GOT tables: symbol table / dedup / fixup points. Slots live in `frozen`; finalization travels
    /// with the home's module and the tables are merged by name into the delta module during absorb,
    /// where sym indices are renumbered.
    got_syms: Vec<ir::GotSym>,
    got_idx: FxHashMap<Box<str>, u32>,
    got_fixups: Vec<ir::GotFixup>,
    frozen_relocs: Vec<ir::FrozenReloc>,
    /// Stub code area and recipe table. Executable entries of this home's instances always live in
    /// this home's code domain so the addresses stay stable across runs, matching the fn entry
    /// discipline; finalization travels with the module.
    code_arena: crate::vm::codearena::StubArena,
    stub_sites: Vec<ir::EntryStubSite>,
}

impl<'tcx> HomeLayer<'tcx> {
    /// A home whose frozen and code arenas occupy spline slot `slot`.
    fn new(slot: usize) -> Self {
        HomeLayer {
            frozen: FrozenArena::new_image(slot),
            queue: VecDeque::new(),
            funcs: Vec::new(),
            fn_next: 0,
            tls_slots: Vec::new(),
            asm_sites: Vec::new(),
            alloc_addrs: FxHashMap::default(),
            fn_entries: FxHashMap::default(),
            insts: Vec::new(),
            got_syms: Vec::new(),
            got_idx: FxHashMap::default(),
            got_fixups: Vec::new(),
            frozen_relocs: Vec::new(),
            code_arena: crate::vm::codearena::StubArena::new_image(slot),
            stub_sites: Vec::new(),
        }
    }
}

/// Split-lowering state: one layer per home that has instances, plus which home the body being
/// lowered belongs to (`None` = the delta, whose state is the `Linker`'s own).
///
/// A home's *index* is its unit's (the id tag and the manifest key), but the *spline slot* its arenas
/// occupy is chosen from the slots the loaded layers left free: a manifest records the slot its baked
/// link addresses point at, so a loaded layer keeps its recorded slot and a fresh home takes the
/// first free one.
pub(crate) struct Split<'tcx> {
    /// Indexed by home index; `None` for a home this session has not populated.
    homes: Vec<Option<HomeLayer<'tcx>>>,
    /// The home of the body being lowered right now.
    current: Option<usize>,
    /// How many layers this session's homes produced (the delta is separate).
    populated: usize,
    /// Homes whose layer the stack already provides. A loaded layer is immutable: whatever it does
    /// not provide by symbol is residue, never an addition to that home.
    loaded: Vec<usize>,
    /// Whether this session may lower a home at all.
    ///
    /// A session that loaded any layer may not: the set of homes a cold session creates depends on
    /// which statics and instances it has to materialize, and a loaded layer answers some of those by
    /// symbol, so lowering the rest would lay the units out differently from every manifest written
    /// above them. Such a session keeps the loaded layers as its whole image stack and puts what they
    /// do not provide in the delta — the same place residue already goes — and publishes nothing, so
    /// the store stays consistent and converges from a cold start.
    lower_homes: bool,
}

impl<'tcx> Split<'tcx> {
    fn activate(loaded: Vec<usize>) -> Self {
        let lower_homes = loaded.is_empty();
        Split {
            homes: Vec::new(),
            current: None,
            populated: 0,
            loaded,
            lower_homes,
        }
    }

    /// Whether the stack already provides this home's layer.
    fn is_loaded(&self, home: usize) -> bool {
        self.loaded.contains(&home)
    }

    /// Whether this session may lower a home (see [`Split::lower_homes`]).
    pub(crate) fn lower_homes(&self) -> bool {
        self.lower_homes
    }

    /// The home currently being lowered. Every caller is reached only from a home body.
    pub(crate) fn cur(&mut self) -> &mut HomeLayer<'tcx> {
        let home = self.current.expect("a home body is being lowered");
        self.home_mut(home)
    }

    pub(crate) fn home(&self, index: usize) -> Option<&HomeLayer<'tcx>> {
        self.homes.get(index).and_then(|home| home.as_ref())
    }

    /// An allocation's existing address as seen from `home` (`None` = the delta): that home's table,
    /// then every lower home's. The delta sits above the homes, so a home body never looks there, and
    /// a body may reuse what a layer below it materialized.
    pub(crate) fn lookup_alloc(&self, home: Option<usize>, id: AllocId) -> Option<u64> {
        let last = home.map_or(self.homes.len(), |home| home + 1);
        (0..last).rev().find_map(|index| {
            self.home(index)
                .and_then(|layer| layer.alloc_addrs.get(&id).copied())
        })
    }

    /// The home's layer, created on first use with the first spline slot no loaded layer and no
    /// existing home occupies.
    pub(crate) fn home_mut(&mut self, index: usize) -> &mut HomeLayer<'tcx> {
        if self.homes.len() <= index {
            self.homes.resize_with(index + 1, || None);
        }
        if self.homes[index].is_none() {
            // The slot is the home's own index: a layer's stack position is the unit's place in
            // topological order, so a publisher and a loader of the same layer derive the same slot
            // without either recording the other's — which is what keeps a manifest's baked
            // cross-layer addresses valid in another process.
            self.homes[index] = Some(HomeLayer::new(index));
            self.populated += 1;
        }
        self.homes[index].as_mut().expect("just created")
    }

    /// The indices of the homes this session has populated, lowest first.
    pub(crate) fn populated_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.homes
            .iter()
            .enumerate()
            .filter_map(|(index, home)| home.as_ref().map(|_| index))
    }

    /// Move every home's layer out, lowest index first, for assembly.
    pub(crate) fn take_populated(&mut self) -> Vec<(usize, HomeLayer<'tcx>)> {
        self.homes
            .iter_mut()
            .enumerate()
            .filter_map(|(index, home)| home.take().map(|home| (index, home)))
            .collect()
    }
}

/// Split-lowering output: the dependency-image module plus its export index material, shaped like
/// `BaseExports`. The module's frozen area lives in the spline k=0 domain, and fn/TLS/asm ids and
/// the export tables have already been rebased to absolute ids.
pub struct SplitImage {
    pub module: ir::Module,
    pub instance: crate::vm::instance::Instance,
    pub fn_entry_syms: Vec<(Box<str>, u64)>,
    pub static_syms: Vec<(Box<str>, u64)>,
    pub tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

impl SplitImage {
    /// Wrap into a stack layer usable before the image is written to disk.
    /// `fp` is the build-session lowering fingerprint; a same-session build is always consistent
    /// with the stack it is about to join.
    pub fn into_base_image(self, fp: (bool, bool, bool)) -> crate::image::BaseImage {
        crate::image::BaseImage {
            fn_by_sym: self
                .module
                .exports
                .iter()
                .map(|(s, id)| (s.clone(), *id))
                .collect(),
            entry_by_sym: self.fn_entry_syms.into_iter().collect(),
            static_by_sym: self.static_syms.into_iter().collect(),
            tls_by_sym: self.tls_syms.into_iter().collect(),
            lowering_fp: fp,
            key: "a2-inmem".into(),
            module: self.module,
            instance: self.instance,
            // The layer this session just lowered; the driver records which unit it was for.
            unit: None,
        }
    }
}

/// Loading-phase "linker": allocates FuncId/TlsId/AsmStubId and expands the worklist closure.
///
/// It also emulates what the native linker would have done for the guest, because a special case
/// here is never "what this panic means" but "what the linker would have done":
/// 1. resolve engine primitives against the same symbol list as the codegen allocator shim;
/// 2. resolve exported symbols (weak lang item: core's extern `panic_impl` finds std's
///    `rust_begin_unwind`);
/// 3. keep a foreign call temporarily trapped until it is resolved.
pub(crate) mod linker;
use linker::Linker;
mod builtins;
pub(crate) mod ffi_sig;
mod frag_stats;
mod main_catch;
mod purity;
mod rebase;
use builtins::engine_builtins;
pub(crate) use ffi_sig::{canonical_link_name, ffi_kind_of, freeze_c_fnptr_sig};
use frag_stats::FragStats;
use purity::{PurityStats, classify_purity, home_of};
use rebase::Rebase;

/// Export material of a base-image build session: produced together with the module and not used by
/// program sessions.
pub struct BaseExports {
    pub fn_entry_syms: Vec<(Box<str>, u64)>,
    pub static_syms: Vec<(Box<str>, u64)>,
    pub tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

/// Program-session lowering. When a base image is present it is reused by symbol name (fn/static/
/// TLS), and the result is a delta module whose fn/TLS/asm ids start from the base counters; the
/// two are absorb-merged before the program runs.
/// With `split` set, instances unrelated to the binary are split off into a dependency image
/// (`SplitImage`, spline k=0 domain) and the delta module keeps only the binary's own attachments.
pub fn lower_program(
    tcx: TyCtxt<'_>,
    stack: &crate::image::ImageStack,
    split: bool,
) -> (
    ir::Module,
    crate::vm::instance::Instance,
    Vec<(usize, SplitImage)>,
) {
    // The split decision (enabled, bypassed, base present) is made by the caller in cli.
    let (module, instance, _, split_images) =
        lower_inner(tcx, stack, FrozenArena::new(), false, false, split);
    (module, instance, split_images)
}

/// Base-image build lowering (synthetic empty main): empty stack, frozen area in the base domain,
/// and an export symbol index.
/// LOCAL_CRATE is excluded: the synthetic crate's items (empty main and its shim) carry a local
/// disambiguator in their symbol names, do not belong to the "sysroot face", and cannot collide
/// with a real program.
pub fn lower_for_base_build(
    tcx: TyCtxt<'_>,
) -> (ir::Module, crate::vm::instance::Instance, BaseExports) {
    let empty = crate::image::ImageStack::empty();
    let (module, instance, exports, _) = lower_inner(
        tcx,
        &empty,
        FrozenArena::new_base_image(),
        true,
        true,
        false,
    );
    (
        module,
        instance,
        exports.expect("image build mode must produce export material"),
    )
}

/// Dependency-image build lowering: `stack` is the already-loaded image chain below this one and
/// the frozen area lives in spline domain `k`. This crate's mono set minus everything already below
/// the stack is this image's content, merged by offset exactly like the base image.
/// LOCAL_CRATE is deliberately NOT excluded -- it is precisely the dependency crate being imaged.
/// Its symbol names must be stable across programs (same dependency version means the same symbol
/// name), which is what makes reuse possible.
///
/// NOTE: the dependency-image build side is not wired up yet, so this has no callers in the repo.
/// It is the reserved entry point for that line; do not delete it for lack of callers.
pub fn lower_for_image_build(
    tcx: TyCtxt<'_>,
    stack: &crate::image::ImageStack,
    k: usize,
) -> (ir::Module, crate::vm::instance::Instance, BaseExports) {
    let (module, instance, exports, _) =
        lower_inner(tcx, stack, FrozenArena::new_image(k), true, false, false);
    (
        module,
        instance,
        exports.expect("image build mode must produce export material"),
    )
}

/// Lower one instance (the worklist loop body). A failed lower becomes a trap body carrying the
/// reason, so the body is always present; purity statistics are recorded when the probe is enabled.
fn lower_one<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    linker: &mut Linker<'tcx>,
    purity: &mut Option<PurityStats>,
    inst: Instance<'tcx>,
) -> ir::FuncBody {
    let sym = tcx.symbol_name(inst).name.to_owned();
    let started = purity.as_ref().map(|_| std::time::Instant::now());
    let body = func::lower_instance(tcx, typing_env, inst, linker)
        .unwrap_or_else(|reason| func::trap_body(&sym, &reason.to_string()));
    if let (Some(p), Some(t0)) = (purity.as_mut(), started) {
        p.record(tcx, inst, &sym, t0.elapsed().as_nanos());
    }
    body
}

/// In the pinned toolchain, standard `catch_unwind` already has a complete guest-side resource
/// cleanup chain: the private `cleanup` function unpacks the panic_unwind exception and decrements
/// the panic count, and the returned Box is then dropped by rustc-generated drop glue for its exact
/// type. Forcing both into the lowering worklist lets the Engine top level recycle everything by
/// moving opaque machine words, without reading any std private object layout.
fn register_guest_panic_cleanup<'tcx>(
    tcx: TyCtxt<'tcx>,
    linker: &mut Linker<'tcx>,
) -> ir::GuestPanicCleanup {
    const CLEANUP_PATH: &str = "std::panicking::catch_unwind::cleanup";

    let cleanup_inst = linker
        .exported_defs()
        .values()
        .map(|&(inst, _)| inst)
        .find(|inst| tcx.def_path_str(inst.def_id()) == CLEANUP_PATH)
        .unwrap_or_else(|| {
            panic!("pinned toolchain is missing `{CLEANUP_PATH}`: cannot release uncaught panic payload on the guest side")
        });
    let sig = tcx
        .fn_sig(cleanup_inst.def_id())
        .instantiate(tcx, cleanup_inst.args)
        .skip_binder();
    if sig.inputs().len() != 1 || !sig.inputs()[0].is_raw_ptr() {
        panic!(
            "pinned toolchain `{CLEANUP_PATH}` parameter signature has changed (now `{sig}`): \
             cannot reliably take over the uncaught panic payload"
        );
    }
    let payload_ty = sig.output();
    if !payload_ty.is_box_global(tcx)
        || !matches!(
            payload_ty.boxed_ty().map(|ty| ty.kind()),
            Some(rustc_middle::ty::TyKind::Dynamic(..))
        )
    {
        panic!(
            "pinned toolchain `{CLEANUP_PATH}` return type has changed (now `{payload_ty}`): \
             expected a trait-object Box on the guest global allocator"
        );
    }

    let drop_inst = Instance::resolve_drop_glue(tcx, payload_ty);
    ir::GuestPanicCleanup {
        cleanup: linker.func_id(cleanup_inst),
        drop_payload: linker.func_id(drop_inst),
    }
}

fn lower_inner(
    tcx: TyCtxt<'_>,
    stack: &crate::image::ImageStack,
    frozen: FrozenArena,
    emit_exports: bool,
    exclude_local: bool,
    split: bool,
) -> (
    ir::Module,
    crate::vm::instance::Instance,
    Option<BaseExports>,
    Vec<(usize, SplitImage)>,
) {
    let typing_env = TypingEnv::fully_monomorphized();
    // The stub code area shares its domain with the frozen area. `frozen.home()` records which
    // domain the frozen area was allocated in; the derived code domain stays consistent even when
    // the frozen area falls back to a dynamic base, because the two decisions are independent.
    let code_home = crate::os_arch::addrspace::code_home_for_frozen(frozen.home())
        .expect("invalid frozen domain, cannot derive stub code domain");
    let mut linker = Linker::new(
        tcx,
        stack,
        frozen,
        crate::vm::codearena::StubArena::new_at(code_home),
    );
    if split {
        // The homes the stack already provides: lowering must not create a second layer for one and
        // rewrite a unit's manifest from this program's view of it.
        let loaded = stack
            .layers()
            .iter()
            .filter_map(|layer| layer.unit)
            .map(|unit| unit as usize)
            .collect();
        linker.activate_split(loaded);
    }

    // Static archives and global_asm/naked `.so` files are materialized and loaded RTLD_NOW|
    // RTLD_GLOBAL before the worklist is drained: when an extern fn is taken as a value (fn-ptr),
    // `fn_entry_addr` must be able to dlsym its real symbol address during lowering, which is the
    // literal translation of native linker semantics.
    // Order-sensitive: `reject_symbol_ambiguity` relies on the RTLD_DEFAULT state "nothing has been
    // dlopen'd yet", so the whole module's libraries are materialized here and only here (later
    // assembly reuses the segment list without re-auditing). Failures are loud.
    let required_native_libs: Vec<Box<str>> = {
        let mut v =
            crate::native::artifact::archive::materialize_static_libraries(tcx, &mut linker)
                .unwrap_or_else(|reason| panic!("Static native library loading failed: {reason}"));
        // global_asm manifests of dependency crates: `.mirasm.s` text extracted from HIR when the
        // dependency was compiled and stored beside its rlib. They are assembled and loaded through
        // the same channel, in crate-graph order, which is how symbols defined by dependency
        // assembly enter the global domain.
        for cnum in tcx.used_crates(()) {
            if tcx.crate_dep_kind(*cnum).macros_only() {
                continue;
            }
            for p in tcx.used_crate_source(*cnum).paths() {
                let Some(stem) = p.to_str().and_then(|s| s.strip_suffix(".rlib")) else {
                    continue;
                };
                let manifest = std::path::PathBuf::from(format!("{stem}.mirasm.s"));
                if !manifest.is_file() {
                    continue;
                }
                let text = std::fs::read_to_string(&manifest).unwrap_or_else(|e| {
                    panic!(
                        "dep global_asm manifest `{}` read failed: {e}",
                        manifest.display()
                    )
                });
                let so = global_asm::assemble(&text).unwrap_or_else(|reason| {
                    panic!(
                        "dep global_asm manifest `{}` materialization failed: {reason}",
                        manifest.display()
                    )
                });
                v.push(so);
            }
        }
        if let Some(so) = global_asm::materialize(tcx, &mut linker)
            .unwrap_or_else(|reason| panic!("global_asm/naked materialization failed: {reason}"))
        {
            v.push(so);
        }
        for so in &v {
            // Lowering only needs symbol addresses and does not own the native lifetime. The private
            // copy is mapped and relocated by the staged loader, but its init/fini are not run:
            // constructors belong to the Engine startup phase.
            let image = crate::vm::native_instance::open_for_lower(std::path::Path::new(&**so))
                .unwrap_or_else(|detail| {
                    panic!(
                        "required native library `{so}` loading failed during lowering: {detail}"
                    )
                });
            let h = image.handle();
            // Record the required handle so dynsym-visible symbols resolve in link order, before the
            // global scope -- that is native link-time binding.
            linker.archive_handles.push(h);
            // If a global_asm-materialized .so contains syscall indirect slots, refill them now.
            // System libraries lack the symbol, so a missing one is silently skipped.
            linker
                .archive_fallbacks
                .push((image.bias(), image.hidden_symbol_values().clone()));
            std::mem::forget(image);
        }
        v
    };

    let sess = tcx.sess;
    // Metadata dylib preload. cargo only writes a `-sys` crate's build.rs `rustc-link-lib` into rlib
    // metadata -- the binary's rustc command line carries no -l/-L, and rustc itself supplements
    // them from metadata at link time. In native semantics these libraries always take part in the
    // final link, and both our fn-ptr baking (dlsym during lowering) and runtime `CallForeign` need
    // them visible in the global domain first. std's own m/dl/pthread/rt/util/gcc_s come from the
    // same source, since its `#[link]` attributes live in libstd.
    // Static libraries go through the archive path above; Framework/wasm kinds are not handled.
    // The collection scope is shared with the native archive link line.
    let dylib_names = crate::native::artifact::archive::system_dylibs(tcx);
    let dylib_candidates = soname_candidates(&dylib_names);
    // Best-effort preload: a missing library is left to the loud diagnostics at its real use site.
    // The handles live for the process lifetime.
    for cand in &dylib_candidates {
        let Ok(cpath) = std::ffi::CString::new(&**cand) else {
            continue;
        };
        let _ = crate::os::dll::open(&cpath, crate::os::dll::Mode::Now);
    }

    // Seed: the mono collector's set, the same starting point native codegen uses.
    for inst in collect::collect(tcx) {
        linker.func_id(inst);
    }

    // The Engine top level may catch a guest panic outside lang_start (run_export / embedded calls),
    // so these two guest functions must stay even when they are not statically reachable from the
    // user program.
    let mut guest_panic_cleanup = register_guest_panic_cleanup(tcx, &mut linker);

    // Custom `#[global_allocator]` `__rust_*` shims. With kind=Global the HIR expander has already
    // generated four forwarding fns in the local crate (`__rust_alloc`, `__rust_dealloc`,
    // `__rust_realloc`, `__rust_alloc_zeroed`; each body calls the user's GlobalAlloc), carrying the
    // rustc allocator flags. Register their FuncIds so the runtime `CallBuiltin(Rust*)` arms route
    // through them. Allocation is program-level semantics: bytecode baked by a Default-session image
    // and the user allocator cannot coexist, because freeing across two heaps corrupts allocator
    // metadata.
    let mut custom_alloc_shims: Option<ir::AllocShims> = if let Some(kind) = tcx.allocator_kind(())
        && matches!(kind, rustc_ast::expand::allocator::AllocatorKind::Global)
    {
        use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags as F;
        let mut found: [Option<ir::FuncId>; 4] = [None; 4];
        for def_id in tcx.hir_crate_items(()).definitions() {
            if tcx.def_kind(def_id) != rustc_hir::def::DefKind::Fn
                || !tcx.def_kind(def_id).has_codegen_attrs()
            {
                continue;
            }
            let flags = tcx.codegen_fn_attrs(def_id).flags;
            for (f, i) in [
                (F::ALLOCATOR, 0),
                (F::DEALLOCATOR, 1),
                (F::REALLOCATOR, 2),
                (F::ALLOCATOR_ZEROED, 3),
            ] {
                if flags.contains(f) {
                    found[i] = Some(linker.func_id(Instance::mono(tcx, def_id.to_def_id())));
                }
            }
        }
        match found {
            [
                Some(alloc),
                Some(dealloc),
                Some(realloc),
                Some(alloc_zeroed),
            ] => Some(ir::AllocShims {
                alloc,
                dealloc,
                realloc,
                alloc_zeroed,
            }),
            // Fewer than four shims means an incomplete generation surface, which should not happen.
            // Falling back to None keeps the engine's ordinary heap discipline.
            _ => None,
        }
    } else {
        None
    };

    // Main entry plan, shaped like cg_ssa's create_entry_fn:
    // `lang_start::<main_ret>(main fn-ptr, argc, argv, sigpipe) -> isize`
    let entry = tcx.entry_fn(()).map(|(main_def, entry_ty)| {
        let rustc_session::config::EntryFnType::Main { sigpipe } = entry_ty;
        let main_inst = Instance::mono(tcx, main_def);
        // main is a local Rust fn, never foreign, so taking its address cannot fail.
        let main_addr = linker
            .fn_entry_addr(main_inst)
            .expect("main fn entry address (local fn, not foreign)");
        let main_ret = tcx
            .fn_sig(main_def)
            .no_bound_vars()
            .expect("main has no late-bound regions")
            .output()
            .no_bound_vars()
            .expect("main return has no late-bound regions");
        let start_def = tcx.require_lang_item(rustc_hir::LangItem::Start, rustc_span::DUMMY_SP);
        let start_inst = Instance::expect_resolve(
            tcx,
            typing_env,
            start_def,
            tcx.mk_args(&[main_ret.into()]),
            rustc_span::DUMMY_SP,
        );
        let (boundary_caller, main_catch, catcher_caller, catcher_intrinsic) =
            main_catch::discover_main_catch_site(tcx, typing_env, start_inst).unwrap_or_else(
                |reason| panic!("cannot frame the pinned std main panic catch site: {reason}"),
            );
        let main_catch = linker.func_id(main_catch);
        linker.main_catch_site = Some(linker::MainCatchSite {
            boundary_caller,
            boundary_callee: main_catch,
            catcher_caller,
            catcher_intrinsic,
        });
        let lang_start = linker.func_id(start_inst);
        // argc/argv start as a zero placeholder; `finalize_entry_argv` backfills them on every run,
        // because the runtime argv must not enter the snapshot.
        ir::EntryPlan {
            lang_start,
            main_addr: ir::LinkAddr(main_addr),
            argc: 0,
            argv_ptr: 0,
            sigpipe,
        }
    });

    let mut module = ir::Module::default();
    let mut instance = crate::vm::instance::Instance::default();
    let mut funcs: Vec<Option<ir::FuncBody>> = Vec::new();
    // Delta-module bodies are stored by local ordinal; after the base and delta vectors are
    // concatenated during absorb, position `delta_first_fn + ordinal` is the absolute FuncId.
    let first = linker.delta_first_fn;
    let mut purity = crate::options::purity_stats().then(PurityStats::default);
    let mut frag_stats = crate::options::frag_stats().then(FragStats::new);
    if linker.split.is_some() {
        // Split mode: drain the home queues and the delta queue alternately until none of them
        // produces work. A home's bodies only ever discover items for that home or a lower one (the
        // closure rule), while delta bodies can discover either; whichever home is current decides
        // which arena a body's allocations land in.
        loop {
            let mut progressed = false;
            let homes: Vec<usize> = linker
                .split
                .as_ref()
                .expect("split")
                .populated_indices()
                .collect();
            for home in homes {
                loop {
                    let next = linker
                        .split
                        .as_mut()
                        .expect("split")
                        .home_mut(home)
                        .queue
                        .pop_front();
                    let Some((id, inst)) = next else { break };
                    progressed = true;
                    linker.split.as_mut().expect("split").current = Some(home);
                    let body = lower_one(tcx, typing_env, &mut linker, &mut purity, inst);
                    linker.split.as_mut().expect("split").home_mut(home).funcs
                        [id_ordinal(id) as usize] = Some(body);
                    module.exports.insert(tcx.symbol_name(inst).name.into(), id);
                }
            }
            while let Some((id, inst)) = linker.queue.pop_front() {
                progressed = true;
                linker.split.as_mut().expect("split").current = None;
                let body = lower_one(tcx, typing_env, &mut linker, &mut purity, inst);
                let slot = (id - first) as usize;
                if funcs.len() <= slot {
                    funcs.resize_with(slot + 1, || None);
                }
                funcs[slot] = Some(body);
                module.exports.insert(tcx.symbol_name(inst).name.into(), id);
            }
            if !progressed {
                break;
            }
        }
    } else {
        while let Some((id, inst)) = linker.queue.pop_front() {
            let body = lower_one(tcx, typing_env, &mut linker, &mut purity, inst);
            let slot = (id - first) as usize;
            if funcs.len() <= slot {
                funcs.resize_with(slot + 1, || None);
            }
            funcs[slot] = Some(body);
            module.exports.insert(tcx.symbol_name(inst).name.into(), id);
        }
    }
    if let Some(p) = &purity {
        p.dump();
    }

    // ===== Split mode: rebase ids and assemble one module per home =====
    let mut split_images: Vec<(usize, SplitImage)> = Vec::new();
    if let Some(mut s) = linker.split.take() {
        let mut populated = s.take_populated();
        // Absolute id bases in home (load) order: a home starts after everything below it, and the
        // delta starts after all of them.
        let max_home = populated.last().map_or(0, |(home, _)| *home);
        let mut first_fn = vec![first; max_home + 1];
        let mut first_tls = vec![linker.delta_first_tls; max_home + 1];
        let mut first_asm = vec![linker.delta_first_asm; max_home + 1];
        let (mut next_fn, mut next_tls, mut next_asm) =
            (first, linker.delta_first_tls, linker.delta_first_asm);
        for (home, layer) in &populated {
            first_fn[*home] = next_fn;
            first_tls[*home] = next_tls;
            first_asm[*home] = next_asm;
            next_fn += layer.funcs.len() as u32;
            next_tls += layer.tls_slots.len() as u32;
            next_asm += layer.asm_sites.len() as u32;
        }
        let rb = Rebase {
            total_fns: next_fn - first,
            total_tls: next_tls - linker.delta_first_tls,
            total_asm: next_asm - linker.delta_first_asm,
            first_fn,
            first_tls,
            first_asm,
        };
        // Self-check: every instance the placement put in a home must still pass the purity review. A
        // misclassification here is a value-level error, not a shape error.
        for (_, layer) in &populated {
            for inst in &layer.insts {
                assert!(
                    classify_purity(*inst).is_image(),
                    "split self-check failed: home instance review is not pure (classifier state error)"
                );
            }
        }
        // The home rule's ledger: which unit each instance belongs to (the placement rule itself when
        // a table exists, the unit rule's answer otherwise), and how much of the layer each home
        // accounts for.
        if let Some(stats) = frag_stats.as_mut()
            && let Some(table) = crate::image::units::current()
        {
            for (_, layer) in &populated {
                for inst in &layer.insts {
                    let Some(&id) = linker.ids.get(inst) else {
                        continue;
                    };
                    let ordinal = id_ordinal(id) as usize;
                    if let Some(body) = layer.funcs.get(ordinal).and_then(|body| body.as_ref()) {
                        stats.home(home_of(*inst, tcx, table), body);
                    }
                }
            }
        }
        // Function bodies, the export/fn_addrs tables and the entry plan all carry ids and are
        // remapped by the same rule, which knows every home's base.
        for (_, layer) in populated.iter_mut() {
            for b in layer.funcs.iter_mut().flatten() {
                rb.body(b);
            }
            for site in layer.stub_sites.iter_mut() {
                site.func = rb.fn_id(site.func);
            }
        }
        for b in funcs.iter_mut().flatten() {
            rb.body(b);
        }
        for v in module.exports.values_mut() {
            *v = rb.fn_id(*v);
        }
        for v in linker.fn_addrs.values_mut() {
            *v = rb.fn_id(*v);
        }
        for site in linker.entry_stub_sites.iter_mut() {
            site.func = rb.fn_id(site.func);
        }
        // The custom-allocator shims are FuncIds too. Missing this remap routes the runtime to a
        // stale, shifted FuncId, so `call_guest` lands in the wrong body.
        if let Some(shims) = custom_alloc_shims.as_mut() {
            shims.alloc = rb.fn_id(shims.alloc);
            shims.dealloc = rb.fn_id(shims.dealloc);
            shims.realloc = rb.fn_id(shims.realloc);
            shims.alloc_zeroed = rb.fn_id(shims.alloc_zeroed);
        }
        rb.guest_panic_cleanup(&mut guest_panic_cleanup);
        for v in linker.ids.values_mut() {
            *v = rb.fn_id(*v);
        }
        for v in linker.tls_ids.values_mut() {
            *v = rb.tls_id(*v);
        }
        let mut entry = entry;
        if let Some(e) = entry.as_mut() {
            e.lang_start = rb.fn_id(e.lang_start);
        }
        let entry = entry;
        module.entry = entry;
        // fn_addrs is split by address domain, not value domain: whatever physically lives in a home's
        // frozen area (`fn_entries` covers its instances plus every supplementary entry) travels with
        // that home. A supplementary entry has a base-domain FuncId but lives in the home's area;
        // leaving it on the builder's delta side means the consumer never registers that baked address
        // in its runtime reverse-lookup table (absorb only merges the layers' fn_addrs, and the
        // consumer's own fn_entry_addr reuse branch does not register either), so indirect calls
        // through it fail with "not a known fn entry".
        let home_entry_addrs: std::collections::HashSet<u64> = populated
            .iter()
            .flat_map(|(_, layer)| layer.fn_entries.values().copied())
            .collect();
        module.fn_entry_links = linker
            .fn_addrs
            .iter()
            .filter(|(a, _)| !home_entry_addrs.contains(a))
            .map(|(&a, &id)| (ir::LinkAddr(a), id))
            .collect();

        for (home, layer) in populated {
            let home_fns = layer.funcs.len() as u32;
            let home_tls = layer.tls_slots.len() as u32;
            let home_lo = rb.first_fn[home];
            let home_hi = home_lo + home_fns;
            let in_home = |id: &ir::FuncId| *id >= home_lo && *id < home_hi;
            let home_exports: std::collections::HashMap<Box<str>, ir::FuncId> = module
                .exports
                .iter()
                .filter(|(_, id)| in_home(id))
                .map(|(s, id)| (s.clone(), *id))
                .collect();
            let layer_entry_addrs: std::collections::HashSet<u64> =
                layer.fn_entries.values().copied().collect();
            let home_fn_addrs: std::collections::HashMap<u64, ir::FuncId> = linker
                .fn_addrs
                .iter()
                .filter(|(a, _)| layer_entry_addrs.contains(a))
                .map(|(a, id)| (*a, *id))
                .collect();
            let home_module = ir::Module {
                exports: home_exports,
                fn_entry_links: home_fn_addrs
                    .iter()
                    .map(|(&a, &id)| (ir::LinkAddr(a), id))
                    .collect(),
                funcs: layer
                    .funcs
                    .into_iter()
                    .map(|f| f.expect("every id must have output when the home queue is drained"))
                    .collect(),
                tls: layer.tls_slots,
                asm_sites: layer.asm_sites,
                frozen: layer.frozen.to_snapshot().ok(),
                foreign_syms: layer.got_syms,
                got_fixups: layer.got_fixups,
                frozen_relocs: layer.frozen_relocs,
                entry_stub_sites: layer.stub_sites,
                ..Default::default()
            };
            let mut home_module = home_module;
            home_module.ensure_function_names();
            // The home keeps its frozen mapping and code-area handle in its own instance; the module
            // carries the bytes for the manifest.
            let mut home_instance = crate::vm::instance::Instance {
                entry_stubs: layer.code_arena,
                ..Default::default()
            };
            home_instance.frozen = Some(layer.frozen);
            home_instance.link_fn_addrs = home_module.fn_entry_links.iter().copied().collect();
            home_instance.rebuild_load_map();
            home_instance.rebuild_fn_addrs();
            // Home export material, shaped like `BaseExports` and free of any `tcx` dependency on the
            // loader side. The fn-entry, static and TLS indexes contain home items only.
            let fn_entry_syms = layer
                .fn_entries
                .iter()
                .map(|(inst, &addr)| (Box::from(tcx.symbol_name(*inst).name), addr))
                .collect();
            let static_syms = linker
                .static_defs
                .iter()
                .filter(|(def_id, _)| def_id.krate != rustc_hir::def_id::LOCAL_CRATE)
                .map(|&(def_id, addr)| {
                    let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                    (Box::from(sym), addr)
                })
                .collect();
            let tls_first = rb.first_tls[home];
            let tls_syms = linker
                .tls_ids
                .iter()
                .filter(|(_, id)| **id >= tls_first && **id < tls_first + home_tls)
                .map(|(&def_id, &id)| {
                    let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                    (Box::from(sym), id)
                })
                .collect();
            split_images.push((
                home,
                SplitImage {
                    module: home_module,
                    instance: home_instance,
                    fn_entry_syms,
                    static_syms,
                    tls_syms,
                },
            ));
            // The delta-side tls_slots/asm_sites already hold delta slots only: home slots live in the
            // home's layer and have been moved out with SplitImage.
        }
    }
    // The delta's bodies, in ordinal order: position `delta_first_fn + ordinal` is the absolute id
    // after the homes have been folded in below it.
    module.funcs = funcs
        .into_iter()
        .map(|f| f.expect("every FuncId must have output when the queue is drained"))
        .collect::<Vec<_>>()
        .into();
    module.ensure_function_names();

    // Candidate dylibs for the runtime's lazy dlopen: the same list the lowering-time preload used
    // (rlib metadata plus CLI names, expanded by ldconfig into versioned paths).
    module.native_libs = dylib_candidates.clone();
    // CLI `-l` additionally contributes the search-path-limited `lib<name>.so` form. Static libraries
    // are deliberately skipped here: they went through the required-archive path above and must not
    // masquerade as optional `.so` candidates.
    for lib in &sess.opts.libs {
        if matches!(lib.kind, rustc_hir::attrs::NativeLibKind::Static { .. }) {
            continue;
        }
        let name = lib.name.as_str();
        for d in sess.opts.search_paths.iter().map(|sp| &sp.dir) {
            let p: Box<str> = d.join(format!("lib{name}.so")).display().to_string().into();
            if !module.native_libs.contains(&p) {
                module.native_libs.push(p);
            }
        }
    }
    // Static libraries from an upstream crate's build.rs, and global_asm/naked objects, were already
    // materialized and RTLD_GLOBAL-loaded before the worklist drained -- lowering-time dlsym for
    // fn-ptr address-taking depends on that, and doing it in one place is what preserves
    // reject_symbol_ambiguity's "nothing dlopen'd yet" precondition. Only the names are handed over
    // to the Module here.
    module.required_native_libs = required_native_libs;
    // asm-stub batch materialization: cc-assemble every wrapper, then dlopen and dlsym it into a real
    // address table. The recipes stay in the Module so a warm load can rematerialize idempotently
    // from `asm_sites`.
    // In split mode the image sites went with SplitImage; absorb merges and rematerializes them
    // under the same contract as a warm load.
    module.asm_sites = std::mem::take(&mut linker.asm_sites);
    instance.asm_stub_addrs = asm::materialize(&module.asm_sites);
    // Base export material, computed once in build mode and free of any `tcx` dependency on the
    // loader side.
    // The synthetic crate's own items (empty main and its shim) stay out of the index: their symbol
    // names carry a local disambiguator and cannot collide with a real program, and the index
    // describes the "sysroot face", so they must be excluded.
    let base_exports = emit_exports.then(|| {
        use rustc_hir::def_id::LOCAL_CRATE;
        let keep = |krate| !exclude_local || krate != LOCAL_CRATE;
        BaseExports {
            fn_entry_syms: linker
                .fn_entries
                .iter()
                .filter(|(inst, _)| keep(inst.def_id().krate))
                .map(|(inst, &addr)| (Box::from(tcx.symbol_name(*inst).name), addr))
                .collect(),
            static_syms: linker
                .static_defs
                .iter()
                .filter(|(def_id, _)| keep(def_id.krate))
                .map(|&(def_id, addr)| {
                    let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                    (Box::from(sym), addr)
                })
                .collect(),
            tls_syms: linker
                .tls_ids
                .iter()
                .filter(|(def_id, _)| keep(def_id.krate))
                .map(|(&def_id, &id)| {
                    let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                    (Box::from(sym), id)
                })
                .collect(),
        }
    });

    // Hand the frozen area and the fn-entry reverse lookup table to the execution phase. The module
    // keeps the bytes in their artifact form; the instance owns the mapping. A frozen area that fell
    // back to a dynamic base has no artifact form -- its bytes are linked against the lowering-time
    // domain -- so the module carries none and only the instance can run it.
    instance.frozen = Some(linker.frozen);
    module.frozen = instance
        .frozen
        .as_ref()
        .and_then(|arena| arena.to_snapshot().ok());
    if split_images.is_empty() {
        module.fn_entry_links = linker
            .fn_addrs
            .into_iter()
            .map(|(addr, func)| (ir::LinkAddr(addr), func))
            .collect();
    }
    module.tls = linker.tls_slots;
    // Delta side of the GOT; each home's side already left with its layer.
    module.foreign_syms = linker.got_syms;
    module.got_fixups = linker.got_fixups;
    module.frozen_relocs = linker.frozen_relocs;
    // This domain's entry-stub recipes and code-area handle; each home's side already left with its
    // layer.
    module.entry_stub_sites = linker.entry_stub_sites;
    instance.entry_stubs = linker.code_arena;
    // The custom-allocator shim is program-level and always lives on the delta side (the shim is
    // always LOCAL_CRATE, wherever the split falls). Bytecode baked by a Default-session base or
    // dependency image routes through it at runtime.
    module.custom_alloc_shims = custom_alloc_shims;
    module.guest_panic_cleanup = Some(guest_panic_cleanup);
    if split_images.is_empty() {
        module.entry = entry;
    }
    instance.link_fn_addrs = module.fn_entry_links.iter().copied().collect();
    instance.rebuild_load_map();
    instance.rebuild_fn_addrs();
    if let Some(mut stats) = frag_stats {
        let frozen_bytes = |arena: &Option<crate::vm::frozen::FrozenArena>| {
            arena.as_ref().map_or(0, |arena| arena.used())
        };
        for (home, image) in &split_images {
            // A home's layer is named by its unit when the track has a table, and is the closure's one
            // image otherwise.
            let label =
                match crate::image::units::current().and_then(|table| table.get(*home as u32)) {
                    Some(unit) => unit.name.to_string(),
                    None => "image".to_string(),
                };
            stats.layer(
                &label,
                &image.module.funcs,
                frozen_bytes(&image.instance.frozen),
            );
        }
        stats.layer("delta", &module.funcs, frozen_bytes(&instance.frozen));
        stats.dump(stack.key());
    }
    (module, instance, base_exports, split_images)
}

/// dylib dlopen candidate list, deduplicated in order: the development symlink `lib{name}.so`
/// followed by the versioned absolute paths `ldconfig -p` reports for it (`lib{name}.so.N`, matched
/// on the dotted prefix so `libssl.so.3` is found but `libssl.so.30` of another name is not). When
/// ldconfig is missing or has no entry, only the symlink remains. Both rlib metadata `-l` entries
/// and CLI `-l` entries go through this.
fn soname_candidates(names: &[Box<str>]) -> Vec<Box<str>> {
    let mut out: Vec<Box<str>> = Vec::new();
    let mut push = |c: String| {
        let c: Box<str> = c.into();
        if !out.contains(&c) {
            out.push(c);
        }
    };
    let ldconfig = std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    for name in names {
        push(format!("lib{name}.so"));
        let prefix = format!("lib{name}.so.");
        if let Some(db) = &ldconfig {
            for line in db.lines() {
                let Some((soname, path)) = line.rsplit_once(" => ") else {
                    continue;
                };
                if soname.trim_start().starts_with(&prefix) {
                    push(path.trim().to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Rebase, home_tag, id_home};
    use crate::vm::ir;

    /// Rebase baseline for one home at first=100 with 5 fns (the fn/TLS/asm spaces are isomorphic).
    fn rb() -> Rebase {
        Rebase {
            first_fn: vec![100],
            first_tls: vec![20],
            first_asm: vec![7],
            total_fns: 5,
            total_tls: 3,
            total_asm: 2,
        }
    }

    #[test]
    fn rebase_fn_id_three_ranges() {
        let rb = rb();
        // Base ids (< first) unchanged.
        assert_eq!(rb.fn_id(0), 0);
        assert_eq!(rb.fn_id(99), 99);
        // Delta untagged (>= first) uniformly +the homes' totals.
        assert_eq!(rb.fn_id(100), 105);
        assert_eq!(rb.fn_id(137), 142);
        // A home tag maps to that home's first + the ordinal.
        assert_eq!(rb.fn_id(home_tag(0, 0)), 100);
        assert_eq!(rb.fn_id(home_tag(0, 4)), 104);
        // Three isomorphic spaces: TLS/ASM same shape (each first/count).
        assert_eq!(rb.tls_id(19), 19);
        assert_eq!(rb.tls_id(20), 23);
        assert_eq!(rb.tls_id(home_tag(0, 2)), 22);
        assert_eq!(rb.asm_id(6), 6);
        assert_eq!(rb.asm_id(7), 9);
        assert_eq!(rb.asm_id(home_tag(0, 1)), 8);
        // Tag bits must never remain in the execution phase.
        for id in [0, 99, 100, 137, home_tag(0, 0), home_tag(0, 4)] {
            let rebased = rb.fn_id(id);
            assert_eq!(
                id_home(rebased),
                None,
                "a home tag escaped lowering: {rebased:#x}"
            );
        }
    }

    /// Construct minimal body: one block, term arbitrary, stmts can be added after return.
    fn body_with(term: ir::Terminator, stmts: Vec<ir::Stmt>) -> ir::FuncBody {
        ir::FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: ir::RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![ir::Block { stmts, term }],
            name: "t".into(),
        }
    }

    #[test]
    fn rebase_body_remaps_only_id_carrying_ops() {
        let rb = rb();
        // Call.callee: three ranges each remapped.
        let mut b = body_with(
            ir::Terminator::Call {
                callee: home_tag(0, 3),
                args: vec![],
                ret: ir::RetDest::Ignore,
                target: 0,
                unwind: ir::UnwindAction::Continue,
                role: ir::CallRole::Normal,
            },
            vec![ir::Stmt::Assign {
                dst: ir::ScalarPlace::Slot(ir::Slot {
                    off: 0,
                    width: ir::Width::W64,
                }),
                rv: ir::Rvalue::TlsRef(home_tag(0, 1)),
            }],
        );
        rb.body(&mut b);
        let ir::Terminator::Call { callee, .. } = &b.blocks[0].term else {
            panic!("Call is not the expected variant");
        };
        assert_eq!(*callee, 103);
        let ir::Stmt::Assign {
            rv: ir::Rvalue::TlsRef(id),
            ..
        } = &b.blocks[0].stmts[0]
        else {
            panic!("TlsRef is not the expected variant");
        };
        assert_eq!(*id, 21);

        // InlineAsm.stub is remapped; CallIndirect has no id field, so it must stay as it is.
        let mut b2 = body_with(
            ir::Terminator::InlineAsm {
                stub: 8,
                buf_size: 0,
                ins: vec![],
                outs: vec![],
                target: 0,
            },
            vec![ir::Stmt::Nop],
        );
        rb.body(&mut b2);
        let ir::Terminator::InlineAsm { stub, .. } = &b2.blocks[0].term else {
            panic!("InlineAsm is not the expected variant");
        };
        assert_eq!(*stub, 10); // untagged >= first_asm(7), so +image_asm(2)

        // Id-less terminators (CallBuiltin, Trap, Goto) stay unchanged.
        let mut b3 = body_with(
            ir::Terminator::CallBuiltin {
                builtin: ir::Builtin::HostAbort,
                args: vec![],
                ret: ir::RetDest::Ignore,
                target: 0,
                unwind: ir::UnwindAction::Continue,
                role: ir::BuiltinCallRole::Normal,
            },
            vec![],
        );
        rb.body(&mut b3);
        assert!(matches!(
            b3.blocks[0].term,
            ir::Terminator::CallBuiltin {
                builtin: ir::Builtin::HostAbort,
                ..
            }
        ));
    }
}
