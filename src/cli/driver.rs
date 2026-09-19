//! Shared rustc driver: the pack/run entry points, the compile callbacks and the VM engine
//! launcher.

use std::process::{ExitCode, exit};

use rustc_data_structures::AtomicRef;
use rustc_driver::{Callbacks, Compilation};
use rustc_errors::{DiagInner, ErrorGuaranteed, Level};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use super::GuestProcessState;
use super::{
    CAPTURE_DIRECTORY, capture_directory, capture_directory_is_forwarded, compiler_session_guard,
};

/// Pack driver: the cold half of `run_driver` with an empty image stack and no L2 lookup. The
/// fork is `callbacks.pack_out`: at the end of `after_analysis` it writes the `.mirvm` package
/// instead of executing.
pub(crate) fn pack_driver(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    out: std::path::PathBuf,
) -> ExitCode {
    let t_start = std::time::Instant::now();
    let mut callbacks = MirvmCallbacks {
        dump_mir: false,
        program_argv,
        exit_code: None,
        vm_call: None,
        vm_stats: false,
        module: None,
        suppress_runner_warning_summary: true,
        runner_finalization_filter_installed: false,
        route_compiler_diagnostics: false,
        t_start,
        timing: PhaseTiming::default(),
        rustc_args: rustc_args.clone(),
        stack: crate::baseimage::ImageStack::empty(),
        split_image: None,
        session_fp: None,
        deps_image_loaded: false,
        pack_out: Some(out),
    };
    let _compiler_session = compiler_session_guard();
    // Session view only: the package header keeps the clean snapshot in `callbacks.rustc_args`.
    let mut session_args = rustc_args.clone();
    session_args.push(super::parallel_frontend_arg().to_string());
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&session_args, &mut callbacks)
    });
    if callbacks.runner_finalization_filter_installed {
        restore_runner_finalization_filter();
    }
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

pub(super) fn is_runner_warning_summary(diagnostic: &DiagInner) -> bool {
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
    stack: crate::baseimage::ImageStack,
    /// Split image produced by `lower_program` when the deps image is enabled and a base image is
    /// present; pushed onto the stack and absorbed at the end of `run_driver`.
    split_image: Option<crate::lower::SplitImage>,
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
/// `cache_store` = clean-snapshot entry on the cold path.
#[derive(Default)]
struct PhaseTiming {
    frontend: Option<std::time::Duration>,
    lower: Option<std::time::Duration>,
    cache_load: Option<std::time::Duration>,
    cache_store: Option<std::time::Duration>,
}

/// With `MIRVM_TIMING=1` (or the --vm-stats instrument) prints a one-line phase ledger to
/// stderr. Off by default: stderr takes part in byte-for-byte native differential comparison, so
/// it must not carry noise.
fn print_phase_timing(
    timing: &PhaseTiming,
    engine: Option<std::time::Duration>,
    total: std::time::Duration,
    force: bool,
) {
    if !force && !crate::options::get().timing {
        return;
    }
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let mut line = String::from("mirvm-timing:");
    if let Some(d) = timing.cache_load {
        line.push_str(&format!(" cache-load={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.frontend {
        line.push_str(&format!(" frontend={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.lower {
        line.push_str(&format!(" lower={:.1}ms", ms(d)));
    }
    if let Some(d) = timing.cache_store {
        line.push_str(&format!(" cache-store={:.1}ms", ms(d)));
    }
    if let Some(d) = engine {
        line.push_str(&format!(" engine={:.1}ms", ms(d)));
    }
    line.push_str(&format!(" total={:.1}ms", ms(total)));
    crate::diagnostics::control(format_args!("{line}"));
}

impl Callbacks for MirvmCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        // Warning-counting hook (precondition for L2 entry): psess_created fires after the
        // interface overwrites TRACK_DIAGNOSTIC and before the first parse, so no session
        // diagnostic is missed.
        let emitter = crate::diagnostics::CompilerEmitterSpec::for_capture(
            &config.opts,
            self.route_compiler_diagnostics,
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
            crate::diagnostics::control(format_args!(
                "mirvm: entry fn not found (needs `fn main`)"
            ));
            self.exit_code = Some(1);
            return Compilation::Stop;
        };
        if !matches!(entry_ty, rustc_session::config::EntryFnType::Main { .. }) {
            crate::diagnostics::control(format_args!(
                "mirvm: #![no_main]/start entry points are not supported yet"
            ));
            self.exit_code = Some(1);
            return Compilation::Stop;
        }

        if self.dump_mir {
            let body = tcx.optimized_mir(def_id);
            let mut buf = Vec::new();
            rustc_middle::mir::pretty::MirWriter::new(tcx)
                .write_mir_fn(body, &mut buf)
                .expect("write_mir_fn failed");
            print!("{}", String::from_utf8_lossy(&buf));
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
                self.stack = crate::baseimage::ImageStack::empty();
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
            let want_split = !crate::depsimage::bypassed()
                && !self.deps_image_loaded
                && !self.stack.is_empty()
                && self
                    .stack
                    .key()
                    .is_some_and(|bk| crate::depsimage::pre_key(&self.rustc_args, bk).is_some());
            let (module, split_image) = crate::lower::lower_program(tcx, &self.stack, want_split);
            self.module = Some(module);
            self.split_image = split_image;
            self.timing.lower = Some(t_lower.elapsed());
            // Pack fork: write the `.mirvm` package instead of executing, at the same clean
            // snapshot point as the L2 store; a failure aborts loudly instead of degrading silently.
            if let Some(out) = &self.pack_out {
                match crate::pack::write_package(
                    tcx,
                    &self.rustc_args,
                    self.module.as_ref().expect("just set"),
                    out,
                ) {
                    Ok(()) => {
                        crate::diagnostics::control(format_args!(
                            "mirvm: package written to {}",
                            out.display()
                        ));
                    }
                    Err(reason) => {
                        crate::diagnostics::control(format_args!(
                            "mirvm: packing failed: {reason}"
                        ));
                        self.exit_code = Some(1);
                    }
                }
            }
            // The split artifact is written to disk before it is pushed onto the stack: the key
            // chain then contains the image key, so L2 delta entries carry the complete chain (a
            // delta embeds the image absolutely; recording under a wrong chain would mean
            // mismatched loads later).
            if let Some(img) = self.split_image.take() {
                let base_key = self
                    .stack
                    .key()
                    .expect("split implies a base image is present")
                    .to_string();
                let bi = crate::depsimage::store_and_wrap(&self.rustc_args, &base_key, fp, img);
                self.stack.push(bi);
            }
            // L2 entry: the clean snapshot before the guest runs (argv is not finalized yet). Any
            // warning or error in the session blocks entry -- the warm path cannot replay
            // diagnostics (see SESSION_WARNINGS). Delta entries carry the key chain, which is
            // verified again on load to prevent a mismatched image stack.
            let t_store = std::time::Instant::now();
            if session_diagnostics_clean()
                && tcx.sess.dcx().has_errors().is_none()
                && crate::ircache::store(
                    tcx,
                    &self.rustc_args,
                    self.module.as_ref().expect("just set"),
                    self.stack.key(),
                    crate::vm::verify::Prefix {
                        funcs: self.stack.total_fns(),
                        tls: self.stack.total_tls(),
                        asm: self.stack.total_asm(),
                    },
                )
            {
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

/// Engine entry: by default runs the main startup chain; `--vm-call 'name(args…)'` calls an
/// exported function directly (gate entry); `--vm-stats` prints Trap-debt statistics.
/// `--stack-size` / `MIRVM_STACK_SIZE` parse a byte count with an optional k/m/g suffix.
pub(super) fn parse_stack_size(s: &str) -> Result<usize, String> {
    let t = s.trim();
    let (num, mult): (&str, usize) = match t.as_bytes().last() {
        Some(b'k' | b'K') => (&t[..t.len() - 1], 1 << 10),
        Some(b'm' | b'M') => (&t[..t.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    let n = num
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("mirvm: cannot parse stack size `{s}` (e.g. 8m, 1g, 67108864)"))?;
    let bytes = n.saturating_mul(mult);
    // The lower bound protects the engine's own prologue plus margin; the upper bound catches
    // typos (even a virtual reservation should not ask for 128T).
    if !(1 << 20..=1 << 40).contains(&bytes) {
        return Err(format!(
            "mirvm: stack size {s} is outside the [1m, 1t] valid range"
        ));
    }
    Ok(bytes)
}

pub(super) fn run_vm_engine(
    mut module: crate::vm::ir::Module,
    program_argv: &[String],
    vm_call: Option<&str>,
    vm_stats: bool,
    already_verified: bool,
) -> i32 {
    if !already_verified && let Err(e) = crate::vm::verify::module(&module) {
        crate::diagnostics::control(format_args!("mirvm: bytecode verification failed: {e}"));
        return 70;
    }
    if vm_stats {
        print!("{}", crate::vm::stats::report(&module));
        return 0;
    }
    // Finalize argv: runtime input is placed after snapshot semantics; one path for cold and warm.
    if let Err(e) = module.finalize_entry_argv(program_argv) {
        crate::diagnostics::control(format_args!("mirvm: {e}"));
        return 70;
    }
    let mut capture = if let Some(directory) = CAPTURE_DIRECTORY.get() {
        // Name the file after the process generation the header will record, so
        // a parent and its forked children are distinguishable before decoding.
        let process_generation = crate::telemetry::capture::claim_process_generation();
        let output = directory.join(format!(
            "events-{}-{process_generation}.mlog",
            std::process::id()
        ));
        let options = crate::telemetry::CaptureOptions::new(output)
            .with_process_generation(process_generation);
        match crate::telemetry::CaptureSession::start(options) {
            Ok(session) => Some(session),
            Err(error) => {
                crate::diagnostics::control(format_args!(
                    "mirvm capture: cannot start event writer: {error}"
                ));
                return 70;
            }
        }
    } else {
        None
    };
    let code = run_vm_engine_loaded(module, vm_call);
    if let Some(session) = &mut capture {
        match session.finish(std::time::Duration::from_secs(30)) {
            Ok(crate::telemetry::CaptureFinish::Finished(_)) => {}
            Ok(crate::telemetry::CaptureFinish::InProgress) => {
                crate::diagnostics::control(format_args!(
                    "mirvm capture: writer did not finish within 30 seconds"
                ));
                return 70;
            }
            Err(error) => {
                crate::diagnostics::control(format_args!(
                    "mirvm capture: cannot finish event file: {error}"
                ));
                return 70;
            }
        }
    }
    code
}

fn run_vm_engine_loaded(module: crate::vm::ir::Module, vm_call: Option<&str>) -> i32 {
    let shared = crate::vm::ctx::Shared::new(module);
    // Make entry stubs executable: recipe -> closure -> stub bytes -> whole-region RX (a
    // full-phase step alongside the two above; an occupied region means load failure).
    let engine = match crate::vm::ctx::Engine::try_new(shared) {
        Ok(engine) => engine,
        Err(e) => {
            crate::diagnostics::control(format_args!("mirvm: {e}"));
            return 70;
        }
    };
    let Some(spec) = vm_call else {
        // main startup chain: interpret lang_start as usual; the exit code is Termination's product.
        let execution = engine.clone();
        let result = match on_guest_stack(move || crate::vm::interp::run_main(&execution)) {
            Ok(result) => result,
            Err(error) => {
                crate::diagnostics::control(format_args!("{}", error.message));
                return error.exit_code;
            }
        };
        if let Err(error) = engine.wait_closed() {
            crate::diagnostics::control(format_args!(
                "mirvm[m4-engine]: cannot wait for Engine teardown: {error:?}"
            ));
            return 70;
        }
        return match result {
            Ok(crate::vm::interp::RunOutcome::Returned(code)) => code,
            // `lang_start` has already run the guest panic hook. Match native
            // stderr here and only translate the structured outcome to its OS
            // exit status.
            Ok(crate::vm::interp::RunOutcome::GuestPanic) => 101,
            Err(e) => {
                crate::diagnostics::control(format_args!("mirvm[m4-engine]: {e}"));
                e.exit_code
            }
        };
    };
    let (name, args) = match parse_vm_call(spec) {
        Ok(v) => v,
        Err(e) => {
            crate::diagnostics::control(format_args!("mirvm: fail to resolve `--vm-call`: {e}"));
            return 2;
        }
    };
    let execution = engine.clone();
    let result = match on_guest_stack(move || {
        // CLI arguments are scalar u64 slots parsed for the explicitly named
        // dev export; pointer-bearing embedding calls are not exposed here.
        unsafe { crate::vm::interp::run_export(&execution, &name, &args) }
    }) {
        Ok(result) => result,
        Err(error) => {
            crate::diagnostics::control(format_args!("{}", error.message));
            return error.exit_code;
        }
    };
    if let Err(error) = engine.wait_closed() {
        crate::diagnostics::control(format_args!(
            "mirvm[m4-engine]: cannot wait for Engine teardown: {error:?}"
        ));
        return 70;
    }
    match result {
        Ok(crate::vm::interp::RunOutcome::Returned(r)) => {
            println!("{}", r.lo);
            0
        }
        Ok(crate::vm::interp::RunOutcome::GuestPanic) => {
            crate::diagnostics::control(format_args!("mirvm[m4-engine]: guest panic not caught"));
            101
        }
        Err(e) => {
            crate::diagnostics::control(format_args!("mirvm[m4-engine]: {e}"));
            e.exit_code
        }
    }
}

/// Guest main execution runs on a dedicated large-stack thread (1 GiB of virtual reservation by
/// default, committed on demand on Linux). An interpreted frame costs the host tens of times more
/// than a native frame; borrowing the caller's 8-16 MiB stack holds only ~8k frames, badly
/// distorting the native stack limit. Guest panics are already absorbed inside run_main/run_export;
/// anything escaping the join is a host panic (a VM bug) and is resumed unchanged, never swallowed.
struct GuestStackStartError {
    message: String,
    exit_code: i32,
}

fn on_guest_stack<R: Send + 'static>(
    f: impl FnOnce() -> R + Send + 'static,
) -> Result<R, GuestStackStartError> {
    let reserve = match crate::options::get().stack_size() {
        Some(s) => parse_stack_size(&s).map_err(|message| GuestStackStartError {
            message,
            exit_code: 2,
        })?,
        None => 1 << 30,
    };
    let spawned = std::thread::Builder::new()
        .name("mirvm-guest".into())
        .stack_size(reserve)
        .spawn(f);
    match spawned {
        Ok(h) => match h.join() {
            Ok(r) => Ok(r),
            Err(host_panic) => std::panic::resume_unwind(host_panic),
        },
        Err(error) => {
            // Do not silently fall back to the caller's small stack (stack semantics would quietly
            // get worse); exit loudly and offer the knob. A typical trigger is a strict commit
            // environment with vm.overcommit_memory=2.
            Err(GuestStackStartError {
                message: format!(
                    "mirvm: failed to create the guest execution thread ({reserve} bytes of stack \
                     reserved): {error}; retry with a smaller --stack-size / MIRVM_STACK_SIZE"
                ),
                exit_code: 70,
            })
        }
    }
}

/// Parse `name(1,2,…)` (or a bare `name`, which means no arguments).
fn parse_vm_call(spec: &str) -> Result<(String, Vec<u64>), String> {
    let spec = spec.trim();
    let Some(open) = spec.find('(') else {
        return Ok((spec.to_string(), Vec::new()));
    };
    let name = spec[..open].trim().to_string();
    let inner = spec[open + 1..]
        .strip_suffix(')')
        .ok_or("missing closing parenthesis")?;
    let mut args = Vec::new();
    for part in inner.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        args.push(
            p.parse::<u64>()
                .map_err(|e| format!("argument `{p}`: {e}"))?,
        );
    }
    Ok((name, args))
}

pub(crate) fn run_driver(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    dump_mir: bool,
    vm_call: Option<String>,
    vm_stats: bool,
    suppress_runner_warning_summary: bool,
    guest_process: Option<GuestProcessState>,
) -> ExitCode {
    let mut diagnostic_router = match crate::diagnostics::DiagnosticRouter::start(
        capture_directory(),
        capture_directory_is_forwarded(),
    ) {
        Ok(router) => router,
        Err(error) => {
            crate::diagnostics::control(format_args!(
                "mirvm capture: cannot start diagnostics stream: {error}"
            ));
            return ExitCode::from(70);
        }
    };
    let t_start = std::time::Instant::now();
    // Image stack: load the base image plus the dependency image chain (failure or bypass = empty
    // stack, which self-heals onto the full cold path). The lowering fingerprint
    // (ub/overflow/contract checks) can only be verified inside the session; after_analysis does.
    let mut stack = if dump_mir {
        crate::baseimage::ImageStack::empty()
    } else {
        crate::baseimage::ensure()
    };
    // Deps-image load before the compiler runs: not bypassed + a base image is present. On a hit
    // it is pushed onto the stack, so the key chain then contains the image key and L2 delta
    // entries can be recorded again.
    let mut deps_image_loaded = false;
    if !dump_mir
        && !crate::depsimage::bypassed()
        && let Some(base) = stack.base_image()
        && let Some(bi) = crate::depsimage::try_load(&rustc_args, base)
    {
        deps_image_loaded = true;
        stack.push(bi);
    }
    let base_key = stack.key().map(str::to_owned);
    // L2 warm path: a hit skips the entire rustc session (frontend + metadata + mono + lower).
    // dump-mir needs the tcx and therefore forces the cold path. ircache verifies delta entries
    // against the key chain.
    if !dump_mir
        && let Some(mut module) = crate::ircache::lookup(
            &rustc_args,
            base_key.as_deref(),
            crate::vm::verify::Prefix {
                funcs: stack.total_fns(),
                tls: stack.total_tls(),
                asm: stack.total_asm(),
            },
        )
    {
        // A cache hit has no compiler session; seal that empty phase before
        // MIRVM control and guest execution begin.
        diagnostic_router.seal_compiler();
        let timing = PhaseTiming {
            cache_load: Some(t_start.elapsed()),
            ..PhaseTiming::default()
        };
        if stack.is_empty() {
            // asm-stub real addresses are process-lifetime state: re-materialize idempotently from
            // the recipe to overwrite stale addresses.
            module.asm_stub_addrs = crate::lower::asm::materialize(&module.asm_sites);
        } else {
            crate::baseimage::absorb_stack(&mut module, stack); // asm merged and re-materialized
        }
        if let Some(guest) = &guest_process
            && let Err(message) = guest.enter()
        {
            crate::diagnostics::control(format_args!("{message}"));
            if let Err(error) = diagnostic_router.finish() {
                crate::diagnostics::control(format_args!(
                    "mirvm capture: cannot finish diagnostics stream: {error}"
                ));
                exit(70);
            }
            exit(1);
        }
        let t_engine = std::time::Instant::now();
        let code = run_vm_engine(module, &program_argv, vm_call.as_deref(), vm_stats, false);
        let engine = (!vm_stats).then(|| t_engine.elapsed());
        print_phase_timing(&timing, engine, t_start.elapsed(), vm_stats);
        if let Err(error) = diagnostic_router.finish() {
            crate::diagnostics::control(format_args!(
                "mirvm capture: cannot finish diagnostics stream: {error}"
            ));
            exit(70);
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
        suppress_runner_warning_summary,
        runner_finalization_filter_installed: false,
        route_compiler_diagnostics: diagnostic_router.is_active(),
        t_start,
        timing: PhaseTiming::default(),
        rustc_args: rustc_args.clone(),
        stack,
        split_image: None,
        session_fp: None,
        deps_image_loaded,
        pack_out: None,
    };
    let _compiler_session = compiler_session_guard();
    // Session view only: `callbacks.rustc_args` is the snapshot the L2 key, the header replay and
    // the deps-image wrap read, so the frontend flag goes on a separate copy.
    let mut session_args = rustc_args.clone();
    session_args.push(super::parallel_frontend_arg().to_string());
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&session_args, &mut callbacks)
    });
    if callbacks.runner_finalization_filter_installed {
        restore_runner_finalization_filter();
    }
    // run_compiler performs finish_diagnostics, delayed-bug flushing and
    // compiler drop before returning. Only now may its stream be sealed.
    diagnostic_router.seal_compiler();
    if compiler_code != ExitCode::SUCCESS {
        if let Err(error) = diagnostic_router.finish() {
            crate::diagnostics::control(format_args!(
                "mirvm capture: cannot finish diagnostics stream: {error}"
            ));
            return ExitCode::from(70);
        }
        return compiler_code;
    }
    if let Some(code) = callbacks.exit_code {
        if let Err(error) = diagnostic_router.finish() {
            crate::diagnostics::control(format_args!(
                "mirvm capture: cannot finish diagnostics stream: {error}"
            ));
            exit(70);
        }
        exit(code);
    }
    if let Some(mut module) = callbacks.module.take() {
        // Cold-path merge (the store already wrote the delta in after_analysis; the engine consumes
        // the merged module). An empty stack (no image) skips it: the module's asm_stub_addrs were
        // already materialized inside the lower session. The split artifact was written to disk and
        // pushed onto the stack in after_analysis, so it is absorbed here like any other layer.
        let stack = std::mem::replace(&mut callbacks.stack, crate::baseimage::ImageStack::empty());
        if !stack.is_empty() {
            crate::baseimage::absorb_stack(&mut module, stack);
        }
        if let Some(guest) = &guest_process
            && let Err(message) = guest.enter()
        {
            crate::diagnostics::control(format_args!("{message}"));
            if let Err(error) = diagnostic_router.finish() {
                crate::diagnostics::control(format_args!(
                    "mirvm capture: cannot finish diagnostics stream: {error}"
                ));
                exit(70);
            }
            exit(1);
        }
        let t_engine = std::time::Instant::now();
        let code = run_vm_engine(
            module,
            &callbacks.program_argv,
            callbacks.vm_call.as_deref(),
            callbacks.vm_stats,
            false,
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
            crate::diagnostics::control(format_args!(
                "mirvm capture: cannot finish diagnostics stream: {error}"
            ));
            exit(70);
        }
        exit(code);
    }
    if let Err(error) = diagnostic_router.finish() {
        crate::diagnostics::control(format_args!(
            "mirvm capture: cannot finish diagnostics stream: {error}"
        ));
        return ExitCode::from(70);
    }
    compiler_code
}
