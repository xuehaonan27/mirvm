//! Compiler sessions and layered preparation after the bound-image lookup misses.

use super::*;
use crate::cli::compiler_session_guard;
use rustc_data_structures::AtomicRef;
use rustc_driver::{Callbacks, Compilation};
use rustc_errors::{DiagInner, ErrorGuaranteed, Level};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

/// Pack driver: the cold half of `run_driver` with an empty image stack and no L2 lookup. The
/// fork is `callbacks.pack_out`: at the end of `after_analysis` it writes the `.mirvm` package
/// instead of executing.
pub(crate) fn pack_driver(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    out: std::path::PathBuf,
) -> ExitCode {
    if let Err(error) = crate::sysroot::pin_args(&rustc_args) {
        crate::diag::emit(&crate::error::Error::from(error));
        return ExitCode::from(crate::diag::exit::FAILURE);
    }
    let t_start = std::time::Instant::now();
    let mut callbacks = MirvmCallbacks {
        dump_mir: false,
        program_argv,
        exit_code: None,
        vm_call: None,
        vm_stats: false,
        module: None,
        instance: None,
        run_inputs: None,
        suppress_runner_warning_summary: true,
        runner_finalization_filter_installed: false,
        route_compiler_diagnostics: false,
        t_start,
        timing: PhaseTiming::default(),
        rustc_args: rustc_args.clone(),
        stack: crate::image::ImageStack::empty(),
        split_images: Vec::new(),
        session_fp: None,
        deps_image_loaded: false,
        pack_out: Some(out),
    };
    let _compiler_session = compiler_session_guard();
    // Session view only: the package header keeps the clean snapshot in `callbacks.rustc_args`.
    let mut session_args = rustc_args.clone();
    session_args.push(crate::cli::parallel_frontend_arg().to_string());
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&session_args, &mut callbacks)
    });
    if callbacks.runner_finalization_filter_installed {
        restore_runner_finalization_filter();
    }
    // The session is over and its verdict is known, so the guest build diagnostics held back
    // during it can be released (a build the user must fix, or a verbose run) or dropped.
    crate::cli::build_log::finish(
        compiler_code != ExitCode::SUCCESS || callbacks.exit_code.is_some(),
    );
    if compiler_code != ExitCode::SUCCESS {
        return compiler_code;
    }
    if let Some(code) = callbacks.exit_code {
        return ExitCode::from(code as u8);
    }
    ExitCode::SUCCESS
}

// ===== shared driver =====

type TrackDiagnostic =
    fn(DiagInner, &mut dyn FnMut(DiagInner) -> Option<ErrorGuaranteed>) -> Option<ErrorGuaranteed>;

static PREVIOUS_TRACK_DIAGNOSTIC: AtomicRef<TrackDiagnostic> =
    AtomicRef::new(&(passthrough_diagnostic as TrackDiagnostic));

fn passthrough_diagnostic(
    diagnostic: DiagInner,
    emit: &mut dyn FnMut(DiagInner) -> Option<ErrorGuaranteed>,
) -> Option<ErrorGuaranteed> {
    emit(diagnostic)
}

pub(in crate::cli) fn is_runner_warning_summary(diagnostic: &DiagInner) -> bool {
    if diagnostic.level() != Level::ForceWarning
        || diagnostic.code.is_some()
        || diagnostic.lint_id.is_some()
        || diagnostic.is_lint.is_some()
        || diagnostic.span != rustc_errors::MultiSpan::new()
        || !diagnostic.children.is_empty()
        || diagnostic.suggestions.len() != 0
        || diagnostic.messages.len() != 1
    {
        return false;
    }

    let Some(message) = diagnostic.messages[0].0.as_str() else {
        return false;
    };
    if message == "1 warning emitted" {
        return true;
    }
    message
        .strip_suffix(" warnings emitted")
        .and_then(|count| count.parse::<u64>().ok())
        .is_some_and(|count| count > 1)
}

/// Preserve rustc's dependency-tracking hook while suppressing only its human warning-count
/// summary. Real warnings were emitted before this hook is installed; late errors and delayed
/// bugs still flow through `previous` and the original emitter during normal finalization.
fn track_runner_finalization_diagnostic(
    diagnostic: DiagInner,
    emit: &mut dyn FnMut(DiagInner) -> Option<ErrorGuaranteed>,
) -> Option<ErrorGuaranteed> {
    let previous = &*PREVIOUS_TRACK_DIAGNOSTIC;
    if is_runner_warning_summary(&diagnostic) {
        previous(diagnostic, &mut |_| None)
    } else {
        previous(diagnostic, emit)
    }
}

fn install_runner_finalization_filter() {
    let current: &'static TrackDiagnostic = &rustc_errors::TRACK_DIAGNOSTIC;
    PREVIOUS_TRACK_DIAGNOSTIC.swap(current);
    rustc_errors::TRACK_DIAGNOSTIC.swap(&(track_runner_finalization_diagnostic as TrackDiagnostic));
}

/// Count of diagnostics visible to the guest in this session: **a compilation with warnings is
/// never written to the L2 cache**. The warm path skips the rustc session and cannot replay
/// diagnostics; silently swallowing warnings would violate run-from-source semantics (a native
/// differential benchmark emits warnings on every fresh compile). Programs with warnings replay
/// the cold path every time; only warning-free programs get the cache.
///
/// Installed at `psess_created` (after the Session exists, before any parsing):
/// rustc_interface's setup_callbacks overwrites TRACK_DIAGNOSTIC to feed the incremental query
/// system, and psess_created fires after that, so the previous hook is chained and delegated here
/// (the same mechanism as the runner finalization filter; the hooks stack).
static SESSION_WARNINGS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

static PREVIOUS_FOR_COUNTER: AtomicRef<TrackDiagnostic> =
    AtomicRef::new(&(passthrough_diagnostic as TrackDiagnostic));

fn track_counting_diagnostic(
    diagnostic: DiagInner,
    emit: &mut dyn FnMut(DiagInner) -> Option<ErrorGuaranteed>,
) -> Option<ErrorGuaranteed> {
    if matches!(
        diagnostic.level(),
        Level::Warning | Level::ForceWarning | Level::Error
    ) {
        SESSION_WARNINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let previous = &*PREVIOUS_FOR_COUNTER;
    previous(diagnostic, emit)
}

fn install_warning_counter() {
    SESSION_WARNINGS.store(0, std::sync::atomic::Ordering::Relaxed);
    let current: &'static TrackDiagnostic = &rustc_errors::TRACK_DIAGNOSTIC;
    PREVIOUS_FOR_COUNTER.swap(current);
    rustc_errors::TRACK_DIAGNOSTIC.swap(&(track_counting_diagnostic as TrackDiagnostic));
}

fn session_diagnostics_clean() -> bool {
    SESSION_WARNINGS.load(std::sync::atomic::Ordering::Relaxed) == 0
}

fn restore_runner_finalization_filter() {
    rustc_errors::TRACK_DIAGNOSTIC.swap(&PREVIOUS_TRACK_DIAGNOSTIC);
}

struct MirvmCallbacks {
    dump_mir: bool,
    program_argv: Vec<String>,
    exit_code: Option<i32>,
    vm_call: Option<String>,
    vm_stats: bool,
    module: Option<crate::vm::ir::Module>,
    instance: Option<crate::vm::instance::Instance>,
    run_inputs: Option<crate::depinfo::InputManifest>,
    suppress_runner_warning_summary: bool,
    runner_finalization_filter_installed: bool,
    route_compiler_diagnostics: bool,
    /// Phase timing: `t_start` = the instant `run_driver` was entered.
    t_start: std::time::Instant,
    timing: PhaseTiming,
    /// L2 cache key material: exactly the arguments `run_compiler` sees.
    rustc_args: Vec<String>,
    /// Image stack: checked against the lowering fingerprint in `after_analysis`, then consulted
    /// by lower for the union; absorbed at the end of `run_driver`.
    stack: crate::image::ImageStack,
    /// Split image produced by `lower_program` when the deps image is enabled and a base image is
    /// present; pushed onto the stack and absorbed at the end of `run_driver`.
    /// One layer per home the session populated, with the home's index (the unit's, or 0 for the
    /// closure track).
    split_images: Vec<(usize, crate::lower::SplitImage)>,
    /// Lowering fingerprint of this session (recorded in `after_analysis`; used when `split_image`
    /// wraps a stack layer).
    session_fp: Option<(bool, bool, bool)>,
    /// Whether a deps-image was already loaded at session start (loaded => do not split again).
    deps_image_loaded: bool,
    /// Pack output path (Some => write the `.mirvm` package at the end of `after_analysis` instead
    /// of executing; None = ordinary run semantics).
    pack_out: Option<std::path::PathBuf>,
}

/// Loading-phase timing ledger. `frontend` = driver entry through analysis completion (including
/// dependency metadata loading); `lower` = mono collection + lowering + frozen materialization.
/// `run_driver` records the engine phase after interpretation ends. `cache_load` = L2 hit
/// deserialization + verification (the warm path replaces frontend+lower entirely);
/// `cache_store` = clean-snapshot publication. Base loading is inside `frontend` or `cache_load`;
/// base construction follows analysis and precedes program lowering. `layer_load` sums manifest
/// loading before analysis and, on a base miss, after base construction.
impl Callbacks for MirvmCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        // The session's own choice of diagnostic rendering, which the tee below then copies.
        diagnostics::route_front_end_output(&mut config.opts);
        // Warning-counting hook (precondition for L2 entry): psess_created fires after the
        // interface overwrites TRACK_DIAGNOSTIC and before the first parse, so no session
        // diagnostic is missed.
        let emitter = diagnostics::CompilerEmitterSpec::for_session(
            &config.opts,
            self.route_compiler_diagnostics,
            crate::options::protocol::build_log_hold(),
        );
        config.psess_created = Some(Box::new(move |psess| {
            install_warning_counter();
            if let Some(emitter) = emitter {
                emitter.install(psess);
            }
        }));
    }

    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        self.timing.frontend = Some(self.t_start.elapsed());
        let Some((def_id, entry_ty)) = tcx.entry_fn(()) else {
            crate::diag_error!(Run, "entry fn not found (needs `fn main`)");
            self.exit_code = Some(1);
            return Compilation::Stop;
        };
        if !matches!(entry_ty, rustc_session::config::EntryFnType::Main { .. }) {
            crate::diag_error!(Run, "#![no_main]/start entry points are not supported yet");
            self.exit_code = Some(1);
            return Compilation::Stop;
        }

        if self.dump_mir {
            let body = tcx.optimized_mir(def_id);
            let mut buf = Vec::new();
            rustc_middle::mir::pretty::MirWriter::new(tcx)
                .write_mir_fn(body, &mut buf)
                .expect("write_mir_fn failed");
            crate::out::text(String::from_utf8_lossy(&buf));
        } else {
            // Verify the lowering fingerprint inside the session: the image stack bakes in the
            // (ub/overflow/contract) checks of its build session. On a mismatch (cargo runner
            // custom profile flags, say) drop the whole stack and lower from scratch -- reusing a
            // wrong fingerprint would give image functions another set of check semantics.
            let sess = tcx.sess;
            let fp = (
                sess.ub_checks(),
                sess.overflow_checks(),
                sess.contract_checks(),
            );
            self.session_fp = Some(fp);
            if !self.stack.fp_matches(fp) {
                self.stack = crate::image::ImageStack::empty();
            }
            if self.stack.is_empty()
                && self.pack_out.is_none()
                && let Some(base) = crate::image::base::build_in_session(
                    tcx,
                    &self.rustc_args,
                    fp,
                    &mut self.timing.base,
                )
            {
                self.stack.push(base);
                let started = std::time::Instant::now();
                self.deps_image_loaded = load_dependency_layers(&mut self.stack, &self.rustc_args);
                *self.timing.layer_load.get_or_insert_default() += started.elapsed();
            }
            // The callback performs only the loading phase; execution must wait until tcx.finish,
            // diagnostic finalization and compiler drop have all completed.
            let t_lower = std::time::Instant::now();
            // Split decision: not bypassed + no image loaded in this session + a base image is
            // present (the stack can be empty after the fingerprint check -- no base, no split).
            // Without --extern (a pure-std program) the deps pre_key is always None: std residue
            // belongs to the base image, so no shared image is built. Splitting would then only
            // produce an in-memory image that cannot be written to disk (its key degrades to a
            // process placeholder), permanently breaking the L2 key chain. Lowering everything
            // into the delta keeps semantics unchanged (one id space, the classic non-split path)
            // and keeps L2 usable for pure-std programs.
            let want_split = !crate::image::deps::bypassed()
                && !self.stack.is_empty()
                && match crate::image::units::current() {
                    // With a build graph the split is what homes instances to units, whether or not
                    // this session had to lower any of them.
                    Some(_) => true,
                    // Without one the closure is the single home, and it needs key material a
                    // dependency-free program does not have.
                    None => {
                        !self.deps_image_loaded
                            && self.stack.key().is_some_and(|bk| {
                                crate::image::deps::pre_key(&self.rustc_args, bk).is_some()
                            })
                    }
                };
            let (module, instance, split_images) =
                crate::lower::lower_program(tcx, &self.stack, want_split);
            self.module = Some(module);
            self.instance = Some(instance);
            self.split_images = split_images;
            self.timing.lower = Some(t_lower.elapsed());
            // Pack fork: write the `.mirvm` package instead of executing, at the same clean
            // snapshot point as the L2 store; a failure aborts loudly instead of degrading silently.
            if let Some(out) = &self.pack_out {
                match crate::pack::write_package(
                    tcx,
                    &self.rustc_args,
                    self.module.as_ref().expect("just set"),
                    self.instance.as_ref().expect("just set"),
                    out,
                ) {
                    Ok(()) => {
                        crate::diag_info!(Pack, "package written to {}", out.display());
                    }
                    Err(reason) => {
                        crate::diag_error!(Pack, "packing failed: {reason}");
                        self.exit_code = Some(1);
                    }
                }
            }
            // The split artifacts are written to disk before they are pushed onto the stack: the key
            // chain then contains each layer's key, so L2 delta entries carry the complete chain (a
            // delta embeds the layers absolutely; recording under a wrong chain would mean mismatched
            // loads later). A track with a unit table publishes one manifest per home, in home
            // (topological) order so the chain is the same whichever subset a session had to lower; a
            // track without one publishes the closure manifest.
            for (home, img) in std::mem::take(&mut self.split_images) {
                let base_key = self
                    .stack
                    .key()
                    .expect("split implies a base image is present")
                    .to_string();
                // A layer is published only if it verifies against the stack below it, which is the
                // same predicate the loader applies.
                let bi = if crate::image::units::current().is_some() {
                    crate::image::units::store(home as u32, &self.stack, fp, img)
                } else {
                    crate::image::deps::store_and_wrap(
                        &self.rustc_args,
                        &base_key,
                        fp,
                        &self.stack,
                        img,
                    )
                };
                self.stack.push(bi);
            }
            // L2 entry: the clean snapshot before the guest runs (argv is not finalized yet). Any
            // warning or error in the session blocks entry -- the warm path cannot replay
            // diagnostics (see SESSION_WARNINGS). Delta entries carry the key chain, which is
            // verified again on load to prevent a mismatched image stack.
            let t_store = std::time::Instant::now();
            if session_diagnostics_clean()
                && tcx.sess.dcx().has_errors().is_none()
                && (!crate::options::no_ir_cache() || crate::cli::prepare_only())
            {
                self.run_inputs = crate::depinfo::InputManifest::collect(tcx);
                if let Some(inputs) = &self.run_inputs {
                    if let Err(reason) = crate::image::program::store(
                        &self.rustc_args,
                        inputs,
                        self.module.as_mut().expect("just set"),
                        self.instance.as_ref().expect("just set"),
                        &self.stack,
                    ) {
                        crate::diag_debug!(Store, "program delta was not published: {reason}");
                    }
                } else if crate::cli::prepare_only() {
                    crate::diag_error!(
                        Prepare,
                        "cannot record the program's complete input identity"
                    );
                    self.exit_code = Some(crate::diag::exit::FAILURE.into());
                }
                self.timing.cache_store = Some(t_store.elapsed());
            }
            if self.suppress_runner_warning_summary {
                install_runner_finalization_filter();
                self.runner_finalization_filter_installed = true;
            }
        }

        Compilation::Stop
    }
}

fn run_vm_engine(
    module: crate::vm::ir::Module,
    instance: crate::vm::instance::Instance,
    program_argv: &[String],
    vm_call: Option<&str>,
    vm_stats: bool,
    heat: crate::vm::jit::Heat,
) -> i32 {
    let artifact = match crate::vm::archive::VerifiedArtifact::verify(module, instance) {
        Ok(artifact) => artifact,
        Err(error) => {
            crate::diag_error!(Engine, "bytecode verification failed: {error}");
            return 70;
        }
    };
    run_verified_engine(artifact, program_argv, vm_call, vm_stats, heat)
}

/// Load dependency manifests only after a compatible base exists. The same operation serves a
/// prepared base before rustc and a newly built base inside the session.
fn load_dependency_layers(stack: &mut crate::image::ImageStack, rustc_args: &[String]) -> bool {
    let mut deps_image_loaded = false;
    if !crate::image::deps::bypassed() {
        match crate::image::units::current() {
            Some(table) => {
                let base_key = stack.base_image().map(|base| base.key.clone());
                if let Some(base_key) = base_key {
                    // Whole or nothing: a manifest the store holds must fit this stack, and if one does
                    // not, no layer is used at all. A layer's ids are laid out against the layers below
                    // it, so a stack assembled from a *subset* of what the store can provide is a stack
                    // no manifest above it was written against — and the session's own lowering would
                    // lay the remaining units out differently from every manifest the last cold session
                    // wrote. Lowering everything instead is the state that converges: the manifests it
                    // writes are the ones a cold session would have written.
                    let before = stack.layers().len();
                    let mut unusable = false;
                    for index in 0..table.len() as u32 {
                        if !crate::image::units::has_manifest(index, &base_key) {
                            continue;
                        }
                        let layer = {
                            let base = stack.base_image().expect("checked above");
                            crate::image::units::try_load(index, base, stack, &base_key)
                        };
                        match layer {
                            Some(layer) => {
                                if crate::options::a2_debug() {
                                    crate::diag::instrument(format_args!(
                                        "[a2-debug] layer loaded: {}",
                                        layer.key
                                    ));
                                }
                                stack.push(layer);
                            }
                            None => {
                                unusable = true;
                                break;
                            }
                        }
                    }
                    if unusable {
                        if crate::options::a2_debug() {
                            crate::diag::instrument(format_args!(
                                "[a2-debug] unit store not usable whole; lowering every unit instead"
                            ));
                        }
                        stack.truncate(before);
                    }
                }
            }
            None => {
                if let Some(base) = stack.base_image()
                    && let Some(bi) = crate::image::deps::try_load(rustc_args, base)
                {
                    deps_image_loaded = true;
                    stack.push(bi);
                }
            }
        }
    }
    deps_image_loaded
}

/// A complete image is required by preparation; an ordinary run can continue after a cache error.
fn publish_execution(
    args: &[String],
    inputs: crate::depinfo::InputManifest,
    module: &crate::vm::ir::Module,
    instance: &crate::vm::instance::Instance,
) -> bool {
    match crate::image::execution::store(args, inputs, module, instance) {
        Ok(()) => true,
        Err(reason) if crate::cli::prepare_only() => {
            crate::diag_error!(
                Prepare,
                "cannot publish the prepared execution image: {reason}"
            );
            false
        }
        Err(reason) => {
            crate::diag_debug!(Store, "execution image was not published: {reason}");
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_miss(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    dump_mir: bool,
    vm_call: Option<String>,
    vm_stats: bool,
    suppress_runner_warning_summary: bool,
    guest_process: Option<GuestProcessState>,
    mut diagnostic_router: diagnostics::DiagnosticRouter,
    t_start: std::time::Instant,
) -> ExitCode {
    let mut base_timing = crate::image::base::Timing::default();
    let mut stack = if dump_mir {
        crate::image::ImageStack::empty()
    } else {
        crate::image::base::ensure(&rustc_args, &mut base_timing)
    };
    let t_layers = std::time::Instant::now();
    let deps_image_loaded = !dump_mir && load_dependency_layers(&mut stack, &rustc_args);
    let layer_load = Some(t_layers.elapsed());
    // L2 warm path: a hit skips the entire rustc session (frontend + metadata + mono + lower).
    // dump-mir needs the tcx and therefore forces the cold path. The L2 entry verifies its delta
    // against the key chain.
    if !dump_mir
        && let Some((mut module, mut instance, inputs)) =
            crate::image::program::lookup(&rustc_args, &stack)
    {
        // A cache hit has no compiler session; seal that empty phase before
        // MIRVM control and guest execution begin.
        diagnostic_router.seal_compiler();
        let mut timing = PhaseTiming {
            cache_load: Some(t_start.elapsed()),
            base: base_timing,
            layer_load,
            ..PhaseTiming::default()
        };
        if stack.is_empty() {
            // asm-stub real addresses are process-lifetime state: re-materialize idempotently from
            // the recipe to overwrite stale addresses.
            instance.asm_stub_addrs =
                crate::native::artifact::assembly::materialize(&module.asm_sites);
        } else {
            stack.absorb_into(&mut module, &mut instance); // asm merged and re-materialized
        }
        let started = std::time::Instant::now();
        let published = publish_execution(&rustc_args, inputs, &module, &instance);
        timing.cache_store = Some(started.elapsed());
        if !published {
            if let Err(error) = diagnostic_router.finish() {
                crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                return ExitCode::from(crate::diag::exit::SOFTWARE);
            }
            return ExitCode::from(crate::diag::exit::FAILURE);
        }
        if crate::cli::prepare_only() {
            // The cache hit is the preparation: this run found everything already built. Report
            // what was loaded and stop short of the guest.
            print_phase_timing(&timing, None, t_start.elapsed(), crate::diag::verbose());
            if let Err(error) = diagnostic_router.finish() {
                crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                exit(crate::diag::exit::SOFTWARE.into());
            }
            exit(crate::diag::exit::SUCCESS.into());
        }
        if let Some(guest) = &guest_process
            && let Err(error) = guest.enter()
        {
            crate::diag::emit(&crate::error::Error::from(error));
            if let Err(error) = diagnostic_router.finish() {
                crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                exit(crate::diag::exit::SOFTWARE.into());
            }
            exit(crate::diag::exit::FAILURE.into());
        }
        let t_engine = std::time::Instant::now();
        // What a previous run of this program found hot: the JIT worker links those entries before it
        // serves a request, so warmup is one link wave rather than a compile wave.
        let heat = crate::image::program::heat(&rustc_args);
        let code = run_vm_engine(
            module,
            instance,
            &program_argv,
            vm_call.as_deref(),
            vm_stats,
            heat,
        );
        let engine = (!vm_stats).then(|| t_engine.elapsed());
        print_phase_timing(&timing, engine, t_start.elapsed(), vm_stats);
        if let Err(error) = diagnostic_router.finish() {
            crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
            exit(crate::diag::exit::SOFTWARE.into());
        }
        exit(code);
    }
    let mut callbacks = MirvmCallbacks {
        dump_mir,
        program_argv,
        exit_code: None,
        vm_call,
        vm_stats,
        module: None,
        instance: None,
        run_inputs: None,
        suppress_runner_warning_summary,
        runner_finalization_filter_installed: false,
        route_compiler_diagnostics: diagnostic_router.is_active(),
        t_start,
        timing: PhaseTiming {
            base: base_timing,
            layer_load,
            ..PhaseTiming::default()
        },
        rustc_args: rustc_args.clone(),
        stack,
        split_images: Vec::new(),
        session_fp: None,
        deps_image_loaded,
        pack_out: None,
    };
    let _compiler_session = compiler_session_guard();
    // Session view only: `callbacks.rustc_args` is the snapshot the L2 key, the header replay and
    // the deps-image wrap read, so the frontend flag goes on a separate copy.
    let mut session_args = rustc_args.clone();
    session_args.push(crate::cli::parallel_frontend_arg().to_string());
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&session_args, &mut callbacks)
    });
    if callbacks.runner_finalization_filter_installed {
        restore_runner_finalization_filter();
    }
    // The session is over and its verdict is known, so the guest build diagnostics held back
    // during it can be released (a build the user must fix, or a verbose run) or dropped.
    crate::cli::build_log::finish(
        compiler_code != ExitCode::SUCCESS || callbacks.exit_code.is_some(),
    );
    // run_compiler performs finish_diagnostics, delayed-bug flushing and
    // compiler drop before returning. Only now may its stream be sealed.
    diagnostic_router.seal_compiler();
    if compiler_code != ExitCode::SUCCESS {
        if let Err(error) = diagnostic_router.finish() {
            crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
            return ExitCode::from(crate::diag::exit::SOFTWARE);
        }
        return compiler_code;
    }
    if let Some(code) = callbacks.exit_code {
        if let Err(error) = diagnostic_router.finish() {
            crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
            exit(crate::diag::exit::SOFTWARE.into());
        }
        exit(code);
    }
    if let Some(module) = callbacks.module.take() {
        let mut module = module;
        let mut instance = callbacks
            .instance
            .take()
            .expect("lower sets module and instance together");
        // Cold-path merge (the store already wrote the delta in after_analysis; the engine consumes
        // the merged module). An empty stack (no image) skips it: the module's asm_stub_addrs were
        // already materialized inside the lower session. The split artifact was written to disk and
        // pushed onto the stack in after_analysis, so it is absorbed here like any other layer.
        let stack = std::mem::replace(&mut callbacks.stack, crate::image::ImageStack::empty());
        if !stack.is_empty() {
            stack.absorb_into(&mut module, &mut instance);
        }
        if let Some(inputs) = callbacks.run_inputs.take() {
            let started = std::time::Instant::now();
            let published = publish_execution(&callbacks.rustc_args, inputs, &module, &instance);
            *callbacks.timing.cache_store.get_or_insert_default() += started.elapsed();
            if !published {
                if let Err(error) = diagnostic_router.finish() {
                    crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                    return ExitCode::from(crate::diag::exit::SOFTWARE);
                }
                return ExitCode::from(crate::diag::exit::FAILURE);
            }
        } else if crate::cli::prepare_only()
            && let Err(error) = crate::vm::verify::module(&module, &instance)
        {
            // Warning-bearing programs are deliberately not cached, but preparation still verifies
            // the same complete module that execution would consume.
            crate::diag_error!(Prepare, "bytecode verification failed: {error}");
            if let Err(error) = diagnostic_router.finish() {
                crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                return ExitCode::from(crate::diag::exit::SOFTWARE);
            }
            return ExitCode::from(crate::diag::exit::SOFTWARE);
        }
        if crate::cli::prepare_only() {
            print_phase_timing(
                &callbacks.timing,
                None,
                t_start.elapsed(),
                crate::diag::verbose(),
            );
            if let Err(error) = diagnostic_router.finish() {
                crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                return ExitCode::from(crate::diag::exit::SOFTWARE);
            }
            return ExitCode::SUCCESS;
        }
        if let Some(guest) = &guest_process
            && let Err(error) = guest.enter()
        {
            crate::diag::emit(&crate::error::Error::from(error));
            if let Err(error) = diagnostic_router.finish() {
                crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
                exit(crate::diag::exit::SOFTWARE.into());
            }
            exit(crate::diag::exit::FAILURE.into());
        }
        let t_engine = std::time::Instant::now();
        let heat = crate::image::program::heat(&callbacks.rustc_args);
        let code = run_vm_engine(
            module,
            instance,
            &callbacks.program_argv,
            callbacks.vm_call.as_deref(),
            callbacks.vm_stats,
            heat,
        );
        // The vm-stats branch does not run the guest, so an engine phase would be meaningless and
        // is not reported.
        let engine = (!callbacks.vm_stats).then(|| t_engine.elapsed());
        print_phase_timing(
            &callbacks.timing,
            engine,
            t_start.elapsed(),
            callbacks.vm_stats,
        );
        if let Err(error) = diagnostic_router.finish() {
            crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
            exit(crate::diag::exit::SOFTWARE.into());
        }
        exit(code);
    }
    if let Err(error) = diagnostic_router.finish() {
        crate::diag_error!(Capture, "cannot finish diagnostics stream: {error}");
        return ExitCode::from(crate::diag::exit::SOFTWARE);
    }
    compiler_code
}
