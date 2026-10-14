//! Target dependency compilation through rustc.

use crate::cli::{compiler_session_guard, diagnostics};
use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;
use std::process::{ExitCode, exit};

// ===== cargo wrapper: in-process compilation of target dependencies =====

/// Dependency compilation callback: run mono collection explicitly after analysis, then let the
/// build continue. A native build performs post-mono const-eval during codegen (required consts
/// are evaluated when mono items are collected); `-Zno-codegen` skips codegen and would swallow
/// those build-time errors too (const panics in dead dependency code and the like, which a native
/// cargo build reports). cargo-miri's dummy backend fills in the same step explicitly, keeping
/// the dependency build error surface identical to native.
///
/// The same mono collection also extracts the dep crate's global_asm/naked text into a side file
/// next to the rlib (`<rlib stem>.mirasm.s`): mirvm is the dep crate's compiler (it holds the HIR
/// in `after_analysis`), so no template has to be recovered from rmeta/rlib. Assembly actions
/// then take the same assemble channel as bin loading, and cache self-heal comes for free.
/// Crates without asm (the vast majority) just pay a mono scan.
struct DepCallbacks {
    /// `<out-dir>` (directory holding the rlib)
    out_dir: String,
    /// rlib stem = `lib<crate_name><extra-filename>` (extra-filename includes its leading `-`)
    rlib_stem: String,
    publication_failed: bool,
}

impl Callbacks for DepCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        diagnostics::route_front_end_output(&mut config.opts);
        // A dependency compile belongs to the build, not to the guest: when this command holds
        // build diagnostics back, so does every unit it schedules, and the unit's own outcome
        // decides whether they are shown. A capture never reaches this process (`__cless-dep` and
        // the Cargo wrapper slot are not handed a capture directory), so there is no stream to tee
        // into here.
        let emitter = diagnostics::CompilerEmitterSpec::for_session(
            &config.opts,
            false,
            crate::options::protocol::build_log_hold(),
        );
        config.psess_created = Some(Box::new(move |psess| {
            if let Some(emitter) = emitter {
                emitter.install(psess);
            }
        }));
    }

    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let _ = tcx.collect_and_partition_mono_items(());
        if let Err(reason) = crate::lower::prepare_generic_templates(tcx) {
            crate::diag_error!(
                Build,
                "cannot prepare dependency generic templates: {reason}"
            );
            self.publication_failed = true;
            return Compilation::Stop;
        }
        match crate::lower::global_asm::materialize_dep_text(tcx) {
            Ok(crate::lower::global_asm::DepAsmText::Text(text)) => {
                let path = format!("{}/{}.mirasm.s", self.out_dir, self.rlib_stem);
                crate::store::publish_bytes(std::path::Path::new(&path), text.as_bytes())
                    .unwrap_or_else(|e| panic!("fail to release dep global_asm list: {e}"));
            }
            // UnsupportedSym: skip the side file (see the DepAsmText docs in global_asm.rs) --
            // do not drag down an entire dependency build over a symbol that may go unused.
            Ok(crate::lower::global_asm::DepAsmText::UnsupportedSym)
            | Ok(crate::lower::global_asm::DepAsmText::None) => {}
            Err(reason) => panic!("fail to extract dep global_asm: {reason}"),
        }
        Compilation::Continue
    }
}

/// Target dependencies: real rustc semantics + `-Zno-codegen`. rustc_interface::start_codegen
/// already no-ops for no-codegen (empty CompiledModules; rmeta encoding happens outside the
/// backend, so it is unaffected; `Linker::link` still runs the default link_binary and produces a
/// metadata-only rlib that cargo and downstream `--extern` accept). Driving it in-process (the
/// process is already linked against librustc_driver) also saves one rustc exec.
pub(crate) fn run_dep_compiler(rustc_args: Vec<String>) -> ! {
    let find = |flag: &str| -> Option<String> {
        rustc_args
            .iter()
            .position(|a| a == flag)
            .and_then(|i| rustc_args.get(i + 1).cloned())
    };
    let out_dir = find("--out-dir").expect("dep compile arguments should have `--out-dir`");
    let crate_name =
        find("--crate-name").expect("dep compile arguments should have `--crate-name`");
    let extra = rustc_args
        .windows(2)
        .find_map(|w| {
            (w[0] == "-C" && w[1].starts_with("extra-filename="))
                .then(|| w[1].trim_start_matches("extra-filename=").to_owned())
        })
        .unwrap_or_default();
    let mut callbacks = DepCallbacks {
        out_dir,
        rlib_stem: format!("lib{crate_name}{extra}"),
        publication_failed: false,
    };
    let _compiler_session = compiler_session_guard();
    // Arguments the scheduler already consumed for its unit fingerprint; the flag below only shapes
    // this session, and stays out of that fingerprint by being appended here rather than upstream.
    let mut session_args = rustc_args;
    session_args.push(crate::cli::SEQUENTIAL_FRONTEND_ARG.to_string());
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&session_args, &mut callbacks)
    });
    // This unit's verdict is known: release the diagnostics held back during its session when it
    // failed, drop them when it succeeded (unless the run asked for detail).
    let failed = code != ExitCode::SUCCESS || callbacks.publication_failed;
    crate::cli::build_log::finish(failed);
    exit(if failed { 1 } else { 0 })
}
