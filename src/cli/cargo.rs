//! Cargo-session callbacks: the target runner, the guest process state and in-process
//! compilation of target dependencies.

use std::process::{ExitCode, exit};

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::cargo_shim;

use super::driver::{pack_driver, run_driver};
use super::{
    capture_directory, capture_directory_is_forwarded, compiler_session_guard,
    set_forwarded_capture_directory, take_internal_capture_directory,
};

// ===== cargo runner callback =====

pub(super) fn runner_main(argv: impl Iterator<Item = String>) -> ExitCode {
    let mut argv = argv.peekable();
    match take_internal_capture_directory(&mut argv) {
        Err(()) => {
            eprintln!("mirvm capture: runner is missing the capture directory");
            return ExitCode::from(2);
        }
        Ok(Some(directory)) => {
            if set_forwarded_capture_directory(directory).is_err() {
                eprintln!("mirvm capture: runner received more than one capture request");
                return ExitCode::from(2);
            }
        }
        Ok(None) => {}
    }
    let _diagnostic_router = match crate::diagnostics::DiagnosticRouter::start(
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
    let guest_process = GuestProcessState::from_cargo_runner();
    let (rustc_args, program_argv, env) = cargo_shim::parse_runner_invocation(argv);
    // The rustc frontend must replay the build environment recorded by the wrapper; before guest
    // execution the runner's initial runtime environment is fully restored, so this layer must not
    // leak into the guest.
    install_recorded_build_environment(env);
    // The build-time environment takes precedence (env!() expansion and CARGO_* must be visible
    // in the compiler session). CARGO_MAKEFLAGS points at a dead jobserver; passing it through
    // would only earn warnings (same handling as cargo-miri).
    // Pack session (mirvm pack passes the output path via MIRVM_PACK through phase_cargo): force
    // the full cold route so the package stays self-contained -- empty image stack plus L2/
    // deps-image bypass. mirvm pack sets both via MIRVM_NO_BASE_IMAGE and MIRVM_NO_DEPS_IMAGE.
    if let Ok(out) = std::env::var("MIRVM_PACK") {
        return pack_driver(rustc_args, program_argv, std::path::PathBuf::from(out));
    }
    run_driver(
        rustc_args,
        program_argv,
        false,
        None,
        false,
        true,
        Some(guest_process),
    )
}

pub(crate) struct GuestProcessState {
    cwd: Option<std::path::PathBuf>,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl GuestProcessState {
    fn from_cargo_runner() -> Self {
        let cwd = std::env::var_os("MIRVM_GUEST_CWD").map(std::path::PathBuf::from);
        let caller_sysroot = std::env::var_os("MIRVM_CALLER_SYSROOT");
        let caller_had_sysroot =
            std::env::var_os("MIRVM_CALLER_SYSROOT_PRESENT").is_some_and(|value| value == "1");
        let mut env: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
        for key in [
            "MIRVM_CARGO_SESSION",
            "MIRVM_GUEST_CWD",
            "MIRVM_CALLER_SYSROOT",
            "MIRVM_CALLER_SYSROOT_PRESENT",
            "RUSTC_WRAPPER",
        ] {
            env.remove(std::ffi::OsStr::new(key));
        }
        if caller_had_sysroot {
            if let Some(value) = caller_sysroot {
                env.insert("MIRVM_SYSROOT".into(), value);
            }
        } else {
            env.remove(std::ffi::OsStr::new("MIRVM_SYSROOT"));
        }
        Self {
            cwd,
            env: env.into_iter().collect(),
        }
    }

    pub(super) fn enter(&self) -> Result<(), String> {
        let current_keys: Vec<_> = std::env::vars_os().map(|(key, _)| key).collect();
        // SAFETY: rustc has returned and the guest/JIT threads have not started.
        unsafe {
            for key in current_keys {
                std::env::remove_var(key);
            }
            for (key, value) in &self.env {
                std::env::set_var(key, value);
            }
        }
        if let Some(cwd) = &self.cwd
            && let Err(error) = std::env::set_current_dir(cwd)
        {
            return Err(format!(
                "mirvm: cannot enter the Cargo caller directory {}: {error}",
                cwd.display()
            ));
        }
        Ok(())
    }
}

fn install_recorded_build_environment(env: Vec<(String, String)>) {
    let current_keys: Vec<_> = std::env::vars_os()
        .map(|(key, _)| key)
        .filter(|key| !key.as_encoded_bytes().starts_with(b"MIRVM_"))
        .collect();
    // SAFETY: runner is still in its single-threaded startup phase.
    unsafe {
        for key in current_keys {
            std::env::remove_var(key);
        }
        for (key, value) in env {
            if key != "CARGO_MAKEFLAGS" && !key.starts_with("MIRVM_") {
                std::env::set_var(key, value);
            }
        }
    }
}

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
}

impl Callbacks for DepCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let _ = tcx.collect_and_partition_mono_items(());
        match crate::lower::global_asm::materialize_dep_text(tcx) {
            Ok(crate::lower::global_asm::DepAsmText::Text(text)) => {
                let path = format!("{}/{}.mirasm.s", self.out_dir, self.rlib_stem);
                // Atomic publish: write the temp name fully, then rename.
                let tmp = format!("{path}.tmp{}", std::process::id());
                std::fs::write(&tmp, text)
                    .unwrap_or_else(|e| panic!("fail to write dep global_asm list: {e}"));
                std::fs::rename(&tmp, &path)
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
    };
    let _compiler_session = compiler_session_guard();
    // Arguments the scheduler already consumed for its unit fingerprint; the flag below only shapes
    // this session, and stays out of that fingerprint by being appended here rather than upstream.
    let mut session_args = rustc_args;
    session_args.push(crate::cli::SEQUENTIAL_FRONTEND_ARG.to_string());
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&session_args, &mut callbacks)
    });
    exit(if code == ExitCode::SUCCESS { 0 } else { 1 })
}

/// Cargoless dep compilation subprocess entry: restore argv0, then feed the rest to
/// `run_dep_compiler`. Arguments are computed by `cargoless::schedule::dep_rustc_args` and
/// scheduled by `cargoless::driver`; shares the DepCallbacks/global_asm extraction channel with
/// the cargo_shim wrapper.
pub(super) fn run_cless_dep(rest: Vec<String>) -> ExitCode {
    let mut args = Vec::with_capacity(rest.len() + 1);
    args.push("mirvm-cless-rustc".to_string());
    args.extend(rest);
    run_dep_compiler(args)
}
