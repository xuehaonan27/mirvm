//! Loading phase: lowering MIR to M4-engine bytecode (rustc_private domain; tcx is confined here).
//!
//! Orchestration (M4.0 design §3 + D1 amendment, debt-map §2-A): mono collection yields the
//! **seed** (the collector takes a codegen/linking view, so non-generic functions across crates
//! are not collected) → **worklist closure expansion** (when lowering meets a callee not in the
//! table it assigns a FuncId and enqueues it — from the interpreter’s point of view there is no
//! "link libstd.so"; all MIR is lowered by us) → produces a pure-Rust `ir::Module`.
//!
//! **Trap-stub full coverage**: lowering is total for the whole input — unrecognized constructs
//! never abort; they are lowered in-place to `Trap(diagnostic)`; only executed paths must be
//! trap-free (M4 incremental protocol).

pub mod asm;
pub mod collect;
pub mod frame;
pub mod func;
pub mod global_asm;

use std::collections::VecDeque;

use rustc_data_structures::fx::FxHashMap;
use rustc_middle::mir::interpret::{AllocId, ConstAllocation, GlobalAlloc};
use rustc_middle::ty::{self, Instance, InstanceKind, TyCtxt, TypingEnv};
use rustc_span::Symbol;

use crate::vm::engine::frozen::FrozenArena;
use crate::vm::engine::ir;

/// Resolution result for a call target (foreign three-way handling, debt-map §2-B).
pub(crate) enum Callee {
    /// Ordinary guest function (includes std implementations resolved by linking emulation②,
    /// and intrinsic fallback bodies collected retroactively).
    Func(ir::FuncId),
    /// Engine primitive① (std runtime extern boundary: alloc/unwind family + stubs).
    Builtin(ir::Builtin),
    /// os:: pass-through③ (dlsym+libffi): fixed arguments are frozen as FfiKind; the variadic
    /// tail is filled from call-site arguments.
    /// thunk_args = fn-ptr typed argument positions + their inner frozen signatures (M4.4 D1 thunk factory).
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

/// Dangerous symbols (P7 denylist): never pass through to native — they would bypass the
/// process/thread model.
/// M4.4 D2: pthread_create/join/detach removed (real thread pass-through; fn-ptr arguments go
/// through the thunk factory).
/// M4.5 D3: posix_spawn family removed (child execs immediately; VM state never runs in the
/// child — fundamentally different from a raw fork landing with a full VM image; file_actions/
/// attr are opaque pointers, so passing real addresses works).
/// Keep pthread_exit (glibc forces unwind around FrameGuard) and the raw fork/exec/setjmp families.
/// M5.2 D8f: fork removed (→ HostFork builtin, allowed when guest is single-threaded); exec removed
/// to DENY_PREFIX (process-replacement semantics = VM state disappearing is already correct, foreign pass-through).
/// vfork/clone/setjmp family remain rejected (frame-model level engineering, D8l).
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

/// A2 split tag bit (s3b-a2-design §4.2): image-class id = `IMAGE_TAG | bit-index`,
/// delta-class id = the untagged value for today’s path. Never enter the execution phase before
/// rebase (2^31 instances impossible).
/// FuncId/TlsId/AsmStubId are isomorphic (all u32).
const IMAGE_TAG: u32 = 0x8000_0000;

/// A2 split state (s3b-a2-design §4): dual queue / dual arena / dual dedup tables.
/// Delta side reuses the main Linker fields (queue/funcs/frozen/alloc_addrs/tls_slots/asm_sites).
pub(crate) struct Split<'tcx> {
    /// Frozen area for image-class instances (spline k=0 domain, 0x6A00).
    image_frozen: FrozenArena,
    /// Pending-lowering queue for image-class instances (tagged ids).
    image_queue: VecDeque<(ir::FuncId, Instance<'tcx>)>,
    /// Image-class function bodies (bit-index j → tagged id `IMAGE_TAG|j`).
    image_funcs: Vec<Option<ir::FuncBody>>,
    /// Next image-class id ordinal.
    image_fn_next: ir::FuncId,
    /// Image-class TLS slots (TlsId is isomorphic).
    image_tls_slots: Vec<ir::TlsSlot>,
    /// Image-class asm sites (symbol names mirvm_asm_xi{j}).
    image_asm_sites: Vec<ir::AsmSite>,
    /// Image-side constant dedup table (delta side = Linker.alloc_addrs; promotion = duplicate
    /// materialization, see §4.3).
    image_alloc_addrs: FxHashMap<AllocId, u64>,
    /// Image-side fn entry table (instance → entry address; includes base hits that are
    /// supplementary-built in the image area — the sole authority for the loader-side
    /// fn_entry_syms index, preserving reproducible single identity "exactly one copy total").
    image_fn_entries: FxHashMap<Instance<'tcx>, u64>,
    /// Whether the current lowering instance is image-class (used for ensure_alloc Memory routing
    /// and closure guard).
    current_image: bool,
    /// Image-class instance table (for post-rebase disk self-check: per-instance review confirms
    /// no LOCAL_CRATE contamination).
    image_insts: Vec<Instance<'tcx>>,
    /// P2 GOT (decision-history §7.5c) image-side three tables: symbol table / dedup / fixup points
    /// (slots live in image_frozen; finalization goes with the image module, merged by name into
    /// delta during absorb and renumbered idx).
    image_got_syms: Vec<ir::GotSym>,
    image_got_idx: FxHashMap<Box<str>, u32>,
    image_got_fixups: Vec<ir::GotFixup>,
    image_frozen_relocs: Vec<ir::FrozenReloc>,
    /// P1 (§7.6) image-side stub code area and recipe table (executable entries of image-class
    /// instances always live in the image domain — the cross-run stable domain, same-domain
    /// discipline as fn entries; finalization goes with the image module).
    image_code_arena: crate::vm::engine::codearena::StubArena,
    image_stub_sites: Vec<ir::EntryStubSite>,
}

/// A2 split output (s3b-a2-design): deps-image module + stack index material (isomorphic to BaseExports).
/// Module frozen area is in spline k=0 domain; fn/TLS/asm and exports/fn_addrs have been rebased
/// to absolute ids.
pub struct SplitImage {
    pub module: ir::Module,
    pub fn_entry_syms: Vec<(Box<str>, u64)>,
    pub static_syms: Vec<(Box<str>, u64)>,
    pub tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

impl SplitImage {
    /// Wrap into a stack layer (A2-1 in-memory absorb; A2-2 after disk write, replaced by file load).
    /// fp = build-session lowering fingerprint (same-session build, always consistent with the stack).
    pub fn into_base_image(self, fp: (bool, bool, bool)) -> crate::baseimage::BaseImage {
        crate::baseimage::BaseImage {
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
        }
    }
}

/// Loading-phase "linker": FuncId allocation + worklist closure expansion (D1 amendment), plus
/// emulation of native-linker responsibilities — **the special-case is not "what panic is" but
/// "what the linker would have done"** (debt-map §2-B):
/// ① engine primitive table (same symbol list as the codegen allocator-shim);
/// ② exported symbol resolution (weak lang item: core’s extern `panic_impl` → std’s `rust_begin_unwind`);
/// ③ unknown foreign temporarily trapped (os:: registry M4.3).
pub(crate) mod linker;
use linker::Linker;
mod builtins;
pub(crate) mod ffi_sig;
mod purity;
mod rebase;
use builtins::engine_builtins;
pub(crate) use ffi_sig::{canonical_link_name, ffi_kind_of, freeze_c_fnptr_sig};
use purity::{PurityStats, classify_purity};
use rebase::Rebase;

/// Base export material (S4: produced together with the module in a base build session; not used in
/// program sessions).
pub struct BaseExports {
    pub fn_entry_syms: Vec<(Box<str>, u64)>,
    pub static_syms: Vec<(Box<str>, u64)>,
    pub tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

/// Program-session lowering: when the base is present, reuse it by symbol_name (fn/static/TLS),
/// producing a delta module (fn/TLS/asm ids start from base counters; absorb-merged before running).
/// A2 (s3b-a2-design, when `MIRVM_DEPS_IMAGE=1` and base is present): split lower — bin-unrelated
/// instances are split off into deps-image (SplitImage, spline k=0 domain); delta only contains
/// bin attachments.
pub fn lower_program(
    tcx: TyCtxt<'_>,
    stack: &crate::baseimage::ImageStack,
    split: bool,
) -> (ir::Module, Option<SplitImage>) {
    // A2 v1: split decision (enable/bypass/already-loaded-this-session/base-present Q2) is given by the caller (cli).
    let (module, _, split_image) = lower_inner(tcx, stack, FrozenArena::new(), false, false, split);
    (module, split_image)
}

/// Base build session lowering (synthetic empty main): empty stack, frozen area in base domain,
/// export sym index.
/// Exclude LOCAL_CRATE — local items of the synthetic crate (empty main + shim) carry a local
/// disambiguator in their symbol names, do not belong to the "sysroot face", and will not collide
/// with a real program.
pub fn lower_for_base_build(tcx: TyCtxt<'_>) -> (ir::Module, BaseExports) {
    let empty = crate::baseimage::ImageStack::empty();
    let (module, exports, _) = lower_inner(
        tcx,
        &empty,
        FrozenArena::new_base_image(),
        true,
        true,
        false,
    );
    (
        module,
        exports.expect("image build mode must produce export material"),
    )
}

/// Dependency image build session lowering (S3′b): stack = already-loaded image chain below,
/// frozen area in spline k domain, export sym index. This crate’s mono set minus what is already
/// below the stack = this image’s content (offset merge same as base).
/// **Does NOT exclude LOCAL_CRATE** — LOCAL_CRATE is precisely the dependency crate to be imaged;
/// its symbol names are stable across programs (same-version dependency = same symbol name, which
/// is the prerequisite for reuse).
///
/// Reserved hook (kept by user decision on 2026-07-19): currently zero callers in the repo — the
/// dependency image build side is not yet wired up (S3′b only delivered the loader-side A2
/// aggregation). Enable when the dependency image build line restarts in the future; do not
/// propose deletion again just because there are no callers.
pub fn lower_for_image_build(
    tcx: TyCtxt<'_>,
    stack: &crate::baseimage::ImageStack,
    k: usize,
) -> (ir::Module, BaseExports) {
    let (module, exports, _) =
        lower_inner(tcx, stack, FrozenArena::new_image(k), true, false, false);
    (
        module,
        exports.expect("image build mode must produce export material"),
    )
}

/// A2 rebase (s3b-a2-design §4.2): split lower finalization, unifying tagged/dual-space ids into
/// absolute ids.
/// Lower a single instance (worklist loop body): trap-stub full coverage + purity probe accounting.
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
        .unwrap_or_else(|reason| func::trap_body(&sym, &reason));
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

/// From the real MIR call graph of the lang item `start`, find the outer call wrapping the user
/// `main`, and finally the intrinsic call that performs the catch. Call relationships and
/// monomorphization args are derived from MIR; the paths only confirm that these derived nodes are
/// still the std implementations agreed upon by the pinned toolchain, without relying on
/// drift-prone DefId numbers.
fn discover_main_catch_site<'tcx>(
    tcx: TyCtxt<'tcx>,
    typing_env: TypingEnv<'tcx>,
    start: Instance<'tcx>,
) -> Result<
    (
        Instance<'tcx>,
        Instance<'tcx>,
        Instance<'tcx>,
        Instance<'tcx>,
    ),
    String,
> {
    fn body<'tcx>(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        instance: Instance<'tcx>,
    ) -> rustc_middle::mir::Body<'tcx> {
        let source = tcx.instance_mir(instance.def);
        instance.instantiate_mir_and_normalize_erasing_regions(
            tcx,
            typing_env,
            rustc_middle::ty::EarlyBinder::bind(tcx, source.clone()),
        )
    }

    fn direct_calls<'tcx>(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        body: &rustc_middle::mir::Body<'tcx>,
    ) -> Vec<(Instance<'tcx>, rustc_middle::mir::UnwindAction)> {
        body.basic_blocks
            .iter()
            .filter_map(|block| {
                let rustc_middle::mir::TerminatorKind::Call { func, unwind, .. } =
                    &block.terminator().kind
                else {
                    return None;
                };
                let ty::FnDef(def_id, args) = func.ty(&body.local_decls, tcx).kind() else {
                    return None;
                };
                Some((
                    Instance::expect_resolve(
                        tcx,
                        typing_env,
                        *def_id,
                        args,
                        block.terminator().source_info.span,
                    ),
                    *unwind,
                ))
            })
            .collect()
    }

    fn operand_local<'tcx>(
        operand: &rustc_middle::mir::Operand<'tcx>,
    ) -> Option<rustc_middle::mir::Local> {
        match operand {
            rustc_middle::mir::Operand::Copy(place) | rustc_middle::mir::Operand::Move(place)
                if place.projection.is_empty() =>
            {
                Some(place.local)
            }
            _ => None,
        }
    }

    fn reified_fn<'tcx>(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        body: &rustc_middle::mir::Body<'tcx>,
        local: rustc_middle::mir::Local,
        use_loc: rustc_middle::mir::Location,
    ) -> Result<Instance<'tcx>, String> {
        use rustc_middle::mir::visit::{PlaceContext, Visitor};

        struct Writes {
            local: rustc_middle::mir::Local,
            locations: Vec<rustc_middle::mir::Location>,
        }

        impl<'tcx> Visitor<'tcx> for Writes {
            fn visit_place(
                &mut self,
                place: &rustc_middle::mir::Place<'tcx>,
                context: PlaceContext,
                location: rustc_middle::mir::Location,
            ) {
                if place.local == self.local && context.is_mutating_use() {
                    self.locations.push(location);
                }
                self.super_place(place, context, location);
            }
        }

        let mut writes = Writes {
            local,
            locations: Vec::new(),
        };
        writes.visit_body(body);
        let [definition] = writes.locations.as_slice() else {
            return Err(format!(
                "local {local:?} expected exactly one write, found {}",
                writes.locations.len()
            ));
        };
        if !definition.dominates(use_loc, body.basic_blocks.dominators()) {
            return Err(format!(
                "the only write to local {local:?} at {definition:?} does not dominate the catch call {use_loc:?}"
            ));
        }
        let block = &body.basic_blocks[definition.block];
        let Some(statement) = block.statements.get(definition.statement_index) else {
            return Err(format!(
                "the only write to local {local:?} occurs at a terminator, not a fn-ptr reify assignment"
            ));
        };
        let rustc_middle::mir::StatementKind::Assign(assign) = &statement.kind else {
            return Err(format!("the only write to local {local:?} is not Assign"));
        };
        let (destination, rvalue) = &**assign;
        if destination.local != local || !destination.projection.is_empty() {
            return Err(format!(
                "the only write to local {local:?} is not a whole-local assignment"
            ));
        }
        let rustc_middle::mir::Rvalue::Cast(
            rustc_middle::mir::CastKind::PointerCoercion(
                ty::adjustment::PointerCoercion::ReifyFnPointer(..),
                _,
            ),
            operand,
            _,
        ) = rvalue
        else {
            return Err(format!(
                "the only write to local {local:?} is not a fn-ptr reify"
            ));
        };
        let ty::FnDef(def_id, args) = operand.ty(&body.local_decls, tcx).kind() else {
            return Err(format!("the reify source for local {local:?} is not FnDef"));
        };
        Instance::resolve_for_fn_ptr(tcx, typing_env, *def_id, args)
            .ok_or_else(|| format!("cannot resolve fn-ptr instance for local {local:?}"))
    }

    let start_calls = direct_calls(tcx, typing_env, &body(tcx, typing_env, start));
    let [(lang_start_internal, _)] = start_calls.as_slice() else {
        return Err(format!(
            "pinned toolchain start MIR expected exactly one direct call, found {}",
            start_calls.len()
        ));
    };
    let lang_start_path = tcx.def_path_str(lang_start_internal.def_id());
    if lang_start_path != "std::rt::lang_start_internal" {
        return Err(format!(
            "pinned toolchain start direct call changed from std::rt::lang_start_internal to \
             {lang_start_path}"
        ));
    }

    let internal_calls = direct_calls(
        tcx,
        typing_env,
        &body(tcx, typing_env, *lang_start_internal),
    );
    let outer_catches: Vec<_> = internal_calls
        .into_iter()
        .filter_map(|(call, _)| {
            call.args.types().find_map(|ty| match ty.kind() {
                ty::Closure(def_id, args) => Some((call, *def_id, args)),
                _ => None,
            })
        })
        .collect();
    let [(outer_catch, runtime_closure_def, runtime_closure_args)] = outer_catches.as_slice()
    else {
        return Err(format!(
            "pinned toolchain lang_start_internal MIR expected exactly one closure-typed direct call, found {}",
            outer_catches.len()
        ));
    };
    let outer_catch_path = tcx.def_path_str(outer_catch.def_id());
    if outer_catch_path != "std::panic::catch_unwind" {
        return Err(format!(
            "pinned toolchain lang_start_internal closure call changed from std::panic::catch_unwind \
             to {outer_catch_path}"
        ));
    }
    let runtime_closure = Instance::resolve_closure(
        tcx,
        *runtime_closure_def,
        runtime_closure_args,
        ty::ClosureKind::FnOnce,
    );
    let main_catches: Vec<_> =
        direct_calls(tcx, typing_env, &body(tcx, typing_env, runtime_closure))
            .into_iter()
            .filter(|(call, _)| call.def_id() == outer_catch.def_id())
            .collect();
    let [(main_catch, unwind)] = main_catches.as_slice() else {
        return Err(format!(
            "pinned toolchain lang_start runtime closure expected exactly one main catch call, found {}",
            main_catches.len()
        ));
    };
    if !matches!(unwind, rustc_middle::mir::UnwindAction::Continue) {
        return Err(format!(
            "pinned toolchain main catch call unwind changed from Continue to {unwind:?}"
        ));
    }

    let outer_body = body(tcx, typing_env, *main_catch);
    let internal_calls = direct_calls(tcx, typing_env, &outer_body);
    let [(internal_catch, internal_unwind)] = internal_calls.as_slice() else {
        return Err(format!(
            "pinned toolchain std::panic::catch_unwind MIR expected exactly one direct call, found {}",
            internal_calls.len()
        ));
    };
    let internal_path = tcx.def_path_str(internal_catch.def_id());
    if internal_path != "std::panicking::catch_unwind" {
        return Err(format!(
            "pinned toolchain std::panic::catch_unwind implementation call changed from \
             std::panicking::catch_unwind to {internal_path}"
        ));
    }
    if !matches!(internal_unwind, rustc_middle::mir::UnwindAction::Continue) {
        return Err(format!(
            "pinned toolchain std::panicking::catch_unwind call unwind changed from Continue to \
             {internal_unwind:?}"
        ));
    }

    let internal_body = body(tcx, typing_env, *internal_catch);
    let mut intrinsic_sites = Vec::new();
    for (bb, block) in internal_body.basic_blocks.iter_enumerated() {
        let rustc_middle::mir::TerminatorKind::Call {
            func, args, unwind, ..
        } = &block.terminator().kind
        else {
            continue;
        };
        let ty::FnDef(def_id, generic_args) = func.ty(&internal_body.local_decls, tcx).kind()
        else {
            continue;
        };
        let intrinsic = Instance::expect_resolve(
            tcx,
            typing_env,
            *def_id,
            generic_args,
            block.terminator().source_info.span,
        );
        if !matches!(intrinsic.def, ty::InstanceKind::Intrinsic(_))
            || tcx.item_name(intrinsic.def_id()).as_str() != "catch_unwind"
        {
            continue;
        }
        intrinsic_sites.push((intrinsic, args, *unwind, internal_body.terminator_loc(bb)));
    }
    let [(catch_intrinsic, args, intrinsic_unwind, intrinsic_loc)] = intrinsic_sites.as_slice()
    else {
        return Err(format!(
            "pinned toolchain std::panicking::catch_unwind MIR expected exactly one std \
             catch_unwind intrinsic, found {}",
            intrinsic_sites.len()
        ));
    };
    let intrinsic_path = tcx.def_path_str(catch_intrinsic.def_id());
    if intrinsic_path != "std::intrinsics::catch_unwind" {
        return Err(format!(
            "pinned toolchain catch intrinsic changed from std::intrinsics::catch_unwind to \
             {intrinsic_path}"
        ));
    }
    if args.len() != 3 {
        return Err(format!(
            "pinned toolchain std catch_unwind intrinsic expected 3 arguments, found {}",
            args.len()
        ));
    }
    if !matches!(
        intrinsic_unwind,
        rustc_middle::mir::UnwindAction::Unreachable
    ) {
        return Err(format!(
            "pinned toolchain std catch_unwind intrinsic unwind changed from Unreachable to \
             {intrinsic_unwind:?}"
        ));
    }
    let try_local = operand_local(&args[0].node).ok_or(
        "pinned toolchain std catch_unwind do_call argument no longer comes from a local fn-ptr",
    )?;
    let catch_local = operand_local(&args[2].node).ok_or(
        "pinned toolchain std catch_unwind do_catch argument no longer comes from a local fn-ptr",
    )?;
    let do_call = reified_fn(tcx, typing_env, &internal_body, try_local, *intrinsic_loc).map_err(
        |reason| format!("pinned toolchain std catch_unwind do_call fn-ptr source cannot be confirmed: {reason}"),
    )?;
    let do_catch = reified_fn(tcx, typing_env, &internal_body, catch_local, *intrinsic_loc)
        .map_err(|reason| {
            format!("pinned toolchain std catch_unwind do_catch fn-ptr source cannot be confirmed: {reason}")
        })?;
    let do_call_path = tcx.def_path_str(do_call.def_id());
    let do_catch_path = tcx.def_path_str(do_catch.def_id());
    if do_call_path != "std::panicking::catch_unwind::do_call"
        || do_catch_path != "std::panicking::catch_unwind::do_catch"
    {
        return Err(format!(
            "pinned toolchain std catch_unwind callbacks changed: try={do_call_path}, \
             catch={do_catch_path}"
        ));
    }

    Ok((
        runtime_closure,
        *main_catch,
        *internal_catch,
        *catch_intrinsic,
    ))
}

fn lower_inner(
    tcx: TyCtxt<'_>,
    stack: &crate::baseimage::ImageStack,
    frozen: FrozenArena,
    emit_exports: bool,
    exclude_local: bool,
    split: bool,
) -> (ir::Module, Option<BaseExports>, Option<SplitImage>) {
    let typing_env = TypingEnv::fully_monomorphized();
    // P1 (§7.6): this-domain stub code area shares the k-domain with the frozen area (frozen.home()
    // records the intended domain; derivation remains consistent under dynamic fallback; each
    // fixed-base/fallback decision is independent, and the cache threshold uses both decisions).
    let code_home = crate::vm::engine::addrlayout::code_home_for_frozen(frozen.home())
        .expect("P1: invalid frozen domain, cannot derive stub code domain");
    let mut linker = Linker::new(
        tcx,
        stack,
        frozen,
        crate::vm::engine::codearena::StubArena::new_at(code_home),
    );
    if split {
        linker.activate_split();
    }

    // Materialize and RTLD_NOW|RTLD_GLOBAL load static archives / global_asm+naked `.so` before
    // draining the worklist: when an extern fn is taken as a value (fn-ptr), fn_entry_addr must
    // dlsym its real symbol address during lowering (literal translation of native linker semantics).
    // Order-sensitive: reject_symbol_ambiguity relies on the RTLD_DEFAULT state "we have not dlopen'd
    // yet", so the whole module materializes only here (assembly-segment reuse list, no re-audit);
    // runtime FfiState::ensure_libs repeated dlopen is an idempotent refcount. Fail loudly.
    let required_native_libs: Vec<Box<str>> = {
        let mut v = crate::native_archive::materialize_static_libraries(tcx, &mut linker)
            .unwrap_or_else(|reason| panic!("Static native library loading failed: {reason}"));
        // C4 (decision-history §7.22): dependency crate global_asm manifests (`.mirasm.s` text
        // extracted from HIR at dep compile time, side-attached next to the rlib) — materialize and
        // load through the same assemble channel in crate-graph order; pulp LD_ST-table-like symbols
        // enter the global domain this way.
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
            // Lowering only needs symbol addresses; it does not own native lifetime. The private copy
            // is mapped/relocated by the staged loader but init/fini are not run; constructors are
            // left to the Engine startup phase.
            let image = crate::vm::engine::native_instance::open_for_lower(std::path::Path::new(
                &**so,
            ))
            .unwrap_or_else(|detail| {
                panic!("required native library `{so}` loading failed during lowering: {detail}")
            });
            let h = image.handle();
            // Record required handle (dynsym-visible symbol link-order resolution, before global scope —
            // native link-time binding, psm/rustc_driver collision confirmed).
            linker.archive_handles.push(h);
            // T5: if a global_asm-materialized .so contains syscall indirect slots, refill them now
            // (system libraries lack this symbol, silently skip).
            linker
                .archive_fallbacks
                .push((image.bias(), image.hidden_symbol_values().clone()));
            std::mem::forget(image);
        }
        v
    };

    let sess = tcx.sess;
    // Metadata dylib preload (corpus batch 5 openssl confirmed): cargo only writes `-sys` build.rs
    // rustc-link-lib into rlib metadata (the bin rustc command line has no -l/-L; rustc itself
    // supplements them from metadata at link time). In native semantics these libraries always enter
    // the final link; our fn-ptr baking (dlsym global during lowering) and runtime CallForeign both
    // need them visible in the global domain first — std’s own m/dl/pthread/rt/util/gcc_s come from
    // the same source (#[link] attributes live in libstd).
    // Static goes through the archive path above; Framework/wasm are not in this slice.
    // Collection scope is shared with native_archive closure link line (c_libgit2 fix, system_dylibs).
    let dylib_names = crate::native_archive::system_dylibs(tcx);
    let dylib_candidates = soname_candidates(&dylib_names);
    // Best-effort preload (missing ones left to existing loud diagnostics at real use sites); handles
    // live for the process lifetime.
    for cand in &dylib_candidates {
        let Ok(cpath) = std::ffi::CString::new(&**cand) else {
            continue;
        };
        let _ = crate::os::dll::open(&cpath, crate::os::dll::Mode::Now);
    }

    // Seed = mono collector set (D1: same starting point as native codegen, correctness for free).
    for inst in collect::collect(tcx) {
        linker.func_id(inst);
    }

    // Engine top level may catch guest panic outside lang_start (run_export / embedded calls);
    // these two guest functions must be kept even if they are not in the user program’s static
    // reachable set.
    let mut guest_panic_cleanup = register_guest_panic_cleanup(tcx, &mut linker);

    // Custom #[global_allocator] __rust_* shims (corpus batch 7 c_mimalloc confirmed fix):
    // when kind=Global the HIR expander has already generated __rust_{alloc,dealloc,realloc,
    // alloc_zeroed} four forwarding fns in the local crate (rustc_allocator etc. flags, body = call
    // user GlobalAlloc) — register FuncId for runtime interp CallBuiltin(Rust*) arm unified routing
    // (allocation is program-level semantics: base/deps image arms baked by Default session and user
    // allocator cannot coexist; cross-heap free = mimalloc metadata SIGSEGV).
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
            // Not all four present = incomplete generation surface (shouldn’t happen; None falls back
            // to existing engine heap discipline).
            _ => None,
        }
    } else {
        None
    };

    // Main entry plan (isomorphic to cg_ssa create_entry_fn):
    // lang_start::<main_ret>(main fn-ptr, argc, argv, sigpipe) -> isize
    let entry = tcx.entry_fn(()).map(|(main_def, entry_ty)| {
        let rustc_session::config::EntryFnType::Main { sigpipe } = entry_ty;
        let main_inst = Instance::mono(tcx, main_def);
        // main is a local Rust fn, definitely not foreign — address-taking path cannot fail.
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
            discover_main_catch_site(tcx, typing_env, start_inst).unwrap_or_else(|reason| {
                panic!("cannot frame the pinned std main panic catch site: {reason}")
            });
        let main_catch = linker.func_id(main_catch);
        linker.main_catch_site = Some(linker::MainCatchSite {
            boundary_caller,
            boundary_callee: main_catch,
            catcher_caller,
            catcher_intrinsic,
        });
        let lang_start = linker.func_id(start_inst);
        // argc/argv zero placeholder: finalize_entry_argv backfills each run (runtime input does not
        // enter the snapshot).
        ir::EntryPlan {
            lang_start,
            main_addr: ir::LinkAddr(main_addr),
            argc: 0,
            argv_ptr: 0,
            sigpipe,
        }
    });

    let mut module = ir::Module::default();
    let mut funcs: Vec<Option<ir::FuncBody>> = Vec::new();
    // S4: delta module funcs vector stored by local ordinal (after absorb base++delta concatenation,
    // position = delta_first_fn + local ordinal = absolute FuncId in bytecode).
    let first = linker.delta_first_fn;
    let mut purity = std::env::var_os("MIRVM_PURITY_STATS")
        .is_some_and(|v| !v.is_empty())
        .then(PurityStats::default);
    if linker.split.is_some() {
        // A2 split: fixed-point alternating drain of dual queues (image bodies only discover image-class
        // items — purity is downward-closed; delta bodies discover both classes). current_image decides
        // arena routing (§4.3).
        loop {
            let mut progressed = false;
            while let Some((id, inst)) = linker
                .split
                .as_mut()
                .expect("split")
                .image_queue
                .pop_front()
            {
                progressed = true;
                linker.split.as_mut().expect("split").current_image = true;
                let body = lower_one(tcx, typing_env, &mut linker, &mut purity, inst);
                let j = (id & !IMAGE_TAG) as usize;
                linker.split.as_mut().expect("split").image_funcs[j] = Some(body);
                module.exports.insert(tcx.symbol_name(inst).name.into(), id);
            }
            while let Some((id, inst)) = linker.queue.pop_front() {
                progressed = true;
                linker.split.as_mut().expect("split").current_image = false;
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

    // ===== A2 split: rebase + dual module assembly =====
    let mut split_image = None;
    if let Some(mut s) = linker.split.take() {
        let image_fns = s.image_funcs.len() as u32;
        let image_tls = s.image_tls_slots.len() as u32;
        let image_asm = s.image_asm_sites.len() as u32;
        let rb = Rebase {
            first_fn: first,
            image_fns,
            first_tls: linker.delta_first_tls,
            image_tls,
            first_asm: linker.delta_first_asm,
            image_asm,
        };
        // A2 self-check (§5.3②): per-image-instance purity review — any misclassification is a
        // value-level error.
        for inst in &s.image_insts {
            assert!(
                classify_purity(*inst).is_image(),
                "A2 self-check failed: image instance review is not pure (classifier state error)"
            );
        }
        // Function bodies + two tables + entry plan remapping.
        for b in s.image_funcs.iter_mut().flatten() {
            rb.body(b);
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
        // P1 recipe table FuncId remapping under the same rule (s.image_stub_sites before assembly below).
        for site in linker.entry_stub_sites.iter_mut() {
            site.func = rb.fn_id(site.func);
        }
        for site in s.image_stub_sites.iter_mut() {
            site.func = rb.fn_id(site.func);
        }
        // Custom allocator shim FuncId remapping under the same rule (c_mimalloc ABI wrong-call root
        // cause: missing mapping makes runtime route to a stale shifted FuncId and call_guest hits
        // the wrong body).
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

        // Split exports by value domain (design §9: base ids < first always stay on delta side).
        let image_lo = first;
        let image_hi = first + image_fns;
        let in_image = |id: &ir::FuncId| *id >= image_lo && *id < image_hi;
        let image_exports: std::collections::HashMap<Box<str>, ir::FuncId> = module
            .exports
            .iter()
            .filter(|(_, id)| in_image(id))
            .map(|(s, id)| (s.clone(), *id))
            .collect();
        module.exports.retain(|_, id| !in_image(id));
        // Split fn_addrs by address domain: entries physically in the image frozen area (value set of
        // image_fn_entries = image-class + all S4 supplementary entries) go with the image. Splitting
        // by value domain would leave supplementary entries (base value domain < first but entry in
        // image area) on the builder’s delta side — after the consumer loads this image, those baked
        // supplementary addresses in its static data are not registered in the runtime reverse lookup
        // table (absorb_stack only merges image.fn_addrs, and the consumer’s own fn_entry_addr reuse
        // branch also doesn’t register), so indirect calls abort "not a known fn entry" (corpus batch
        // 1 confirmed cache-pollution root cause; negative control edit_rand v2-v6 five consecutive
        // crashes 0x6a0000001630/core::fmt::write).
        let image_entry_addrs: std::collections::HashSet<u64> =
            s.image_fn_entries.values().copied().collect();
        let image_fn_addrs: std::collections::HashMap<u64, ir::FuncId> = linker
            .fn_addrs
            .iter()
            .filter(|(a, _)| image_entry_addrs.contains(a))
            .map(|(a, id)| (*a, *id))
            .collect();
        module.fn_addrs = linker
            .fn_addrs
            .iter()
            .filter(|(a, _)| !image_entry_addrs.contains(a))
            .map(|(a, id)| (*a, *id))
            .collect();
        let image_module = ir::Module {
            exports: image_exports,
            fn_addrs: image_fn_addrs,
            funcs: s
                .image_funcs
                .into_iter()
                .map(|f| f.expect("every id must have output when the image queue is drained"))
                .collect(),
            tls: s.image_tls_slots,
            asm_sites: s.image_asm_sites,
            frozen: Some(s.image_frozen),
            // P2 GOT image side (decision-history §7.5c): goes with the image module, merged by name
            // during load/absorb into delta and renumbered idx.
            foreign_syms: s.image_got_syms,
            got_fixups: s.image_got_fixups,
            frozen_relocs: s.image_frozen_relocs,
            // P1 image side (§7.6): recipes go with the image file; code-area handles rebuilt at runtime
            // per domain.
            entry_stub_sites: s.image_stub_sites,
            entry_stubs: s.image_code_arena,
            ..Default::default()
        };
        let mut image_module = image_module;
        image_module.ensure_function_names();
        image_module.rebuild_load_map();
        image_module.rebuild_fn_addrs();
        // Image export material (zero tcx dependency on loader side, isomorphic to BaseExports): fn
        // entry/static/TLS three indexes contain only image-class items. Fn entries use the image-area
        // entry table as authority (includes base hits that are supplementary-built in the image area —
        // reproducible single identity "exactly one copy total" on the loader side).
        let fn_entry_syms = s
            .image_fn_entries
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
        let tls_first = rb.first_tls;
        let tls_syms = linker
            .tls_ids
            .iter()
            .filter(|(_, id)| **id >= tls_first && **id < tls_first + image_tls)
            .map(|(&def_id, &id)| {
                let sym = tcx.symbol_name(Instance::mono(tcx, def_id)).name;
                (Box::from(sym), id)
            })
            .collect();
        split_image = Some(SplitImage {
            module: image_module,
            fn_entry_syms,
            static_syms,
            tls_syms,
        });
        // Delta-side tls_slots/asm_sites already only contain delta slots (image slots live in Split
        // fields and have already been moved out with SplitImage), so no further action needed.
    }
    module.funcs = funcs
        .into_iter()
        .map(|f| f.expect("every FuncId must have output when the queue is drained"))
        .collect::<Vec<_>>()
        .into();
    module.ensure_function_names();

    // Entry alias (used by --vm-stats for reachability analysis from the program entry).
    if let Some((entry_def, _)) = tcx.entry_fn(())
        && let Some(&id) = linker.ids.get(&Instance::mono(tcx, entry_def))
    {
        module.exports.insert("@entry".into(), id);
    }
    // dylib dlopen candidates (optional class for runtime FfiState ensure_libs): same list as the
    // lowering-time preload (metadata + CLI merge, ldconfig-expanded versioned entries) — one-build
    // scope.
    module.native_libs = dylib_candidates.clone();
    // CLI `-l` additionally supplements search-path limited form (tier-0 old contract kept; Static
    // only goes through the verified required archive path above and cannot masquerade as optional
    // `.so` candidates).
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
    // Upstream crate build.rs Static native libraries (M5.1 D2) + global_asm/naked materialization
    // (M5.2 D8h): manifests were already materialized and RTLD_GLOBAL loaded before draining the
    // worklist (fn-ptr address-taking lowering-time dlsym depends on this; single-point
    // materialization preserves the "not yet dlopen'd" precondition of reject_symbol_ambiguity),
    // only hand over Module here.
    module.required_native_libs = required_native_libs;
    // asm-stub batch materialization (M5.0): all wrapper cc assembly + dlopen + dlsym → real address
    // table. Recipes stay in Module (M6 slice 2): L2 warm path idempotently rematerializes from
    // asm_sites.
    // A2 split: image sites go with SplitImage (merged and rematerialized during absorb, same
    // contract as L2 warm).
    module.asm_sites = std::mem::take(&mut linker.asm_sites);
    module.asm_stub_addrs = asm::materialize(&module.asm_sites);
    // S4 base export material (build mode): sym indexes computed once here, zero tcx dependency on
    // loader side.
    // Synthetic crate local items (empty main and its shim) are not entered into the index — their
    // symbol names carry a local disambiguator and will not collide with a real program, but the
    // index semantics are "sysroot face", so exclude truthfully.
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

    // Hand over frozen area and fn-entry reverse lookup table to the execution phase.
    module.frozen = Some(linker.frozen);
    if split_image.is_none() {
        module.fn_addrs = linker.fn_addrs.into_iter().collect();
    }
    module.tls = linker.tls_slots;
    // P2 GOT (decision-history §7.5c) delta side (image side already went with split_image).
    module.foreign_syms = linker.got_syms;
    module.got_fixups = linker.got_fixups;
    module.frozen_relocs = linker.frozen_relocs;
    // P1 (§7.6) this-domain recipe and code-area handle (image side already went with split_image).
    module.entry_stub_sites = linker.entry_stub_sites;
    module.entry_stubs = linker.code_arena;
    // Custom allocator shim (program-level semantics, delta authority: shim is always LOCAL_CRATE —
    // same place regardless of split; base/deps image arms baked by Default are routed through it at
    // runtime).
    module.custom_alloc_shims = custom_alloc_shims;
    module.guest_panic_cleanup = Some(guest_panic_cleanup);
    if split_image.is_none() {
        module.entry = entry;
    }
    module.rebuild_load_map();
    module.rebuild_fn_addrs();
    (module, base_exports, split_image)
}

/// dylib dlopen candidate SONAME list (deduplicated in order): dev symlink `lib{name}.so` →
/// versioned absolute path from `ldconfig -p` (`lib{name}.so.N` with dotted anchor prefix, like
/// libssl.so.3; if ldconfig is missing/has no hit, rely only on the symlink). cargo rlib metadata -l
/// and CLI -l share this.
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
    use super::{IMAGE_TAG, Rebase};
    use crate::vm::engine::ir;

    /// Rebase baseline for first=100, image 5 (fn/TLS/asm independent isomorphic spaces).
    fn rb() -> Rebase {
        Rebase {
            first_fn: 100,
            image_fns: 5,
            first_tls: 20,
            image_tls: 3,
            first_asm: 7,
            image_asm: 2,
        }
    }

    #[test]
    fn rebase_fn_id_three_ranges() {
        let rb = rb();
        // Base ids (< first) unchanged.
        assert_eq!(rb.fn_id(0), 0);
        assert_eq!(rb.fn_id(99), 99);
        // Delta untagged (≥ first) uniformly +image_fns.
        assert_eq!(rb.fn_id(100), 105);
        assert_eq!(rb.fn_id(137), 142);
        // Image tag (TAG|j) → first + j.
        assert_eq!(rb.fn_id(IMAGE_TAG), 100);
        assert_eq!(rb.fn_id(IMAGE_TAG | 4), 104);
        // Three isomorphic spaces: TLS/ASM same shape (each first/count).
        assert_eq!(rb.tls_id(19), 19);
        assert_eq!(rb.tls_id(20), 23);
        assert_eq!(rb.tls_id(IMAGE_TAG | 2), 22);
        assert_eq!(rb.asm_id(6), 6);
        assert_eq!(rb.asm_id(7), 9);
        assert_eq!(rb.asm_id(IMAGE_TAG | 1), 8);
        // Tag bits must never remain in the execution phase.
        for id in [0, 99, 100, 137, IMAGE_TAG, IMAGE_TAG | 4] {
            assert_eq!(rb.fn_id(id) & IMAGE_TAG, 0);
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
                callee: IMAGE_TAG | 3,
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
                rv: ir::Rvalue::TlsRef(IMAGE_TAG | 1),
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

        // InlineAsm.stub remapped; CallIndirect (no id field) and other statements unchanged.
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
        assert_eq!(*stub, 10); // untagged ≥ first_asm(7) → +image_asm(2)

        // CallBuiltin / Trap / Goto and other id-less terminators remain unchanged.
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
