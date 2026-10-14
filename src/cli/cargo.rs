//! Cargo-session callbacks: the target runner, the guest process state and in-process
//! compilation of target dependencies.

use std::process::ExitCode;

use crate::cargo_shim;

use super::diagnostics;
use super::driver::{pack_driver, run_driver};
use super::{
    capture_directory, capture_directory_is_forwarded, set_forwarded_capture_directory,
    take_internal_capture_directory,
};

// ===== cargo runner callback =====

pub(super) fn runner_main(argv: impl Iterator<Item = String>) -> ExitCode {
    let mut argv = argv.peekable();
    match take_internal_capture_directory(&mut argv) {
        Err(()) => {
            crate::diag_error!(Capture, "runner is missing the capture directory");
            return ExitCode::from(crate::diag::exit::USAGE);
        }
        Ok(Some(directory)) => {
            if set_forwarded_capture_directory(directory).is_err() {
                crate::diag_error!(Capture, "runner received more than one capture request");
                return ExitCode::from(crate::diag::exit::USAGE);
            }
        }
        Ok(None) => {}
    }
    let _diagnostic_router = match diagnostics::DiagnosticRouter::start(
        capture_directory(),
        capture_directory_is_forwarded(),
    ) {
        Ok(router) => router,
        Err(error) => {
            crate::diag_error!(Capture, "cannot start diagnostics stream: {error}");
            return ExitCode::from(crate::diag::exit::SOFTWARE);
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
    if let Some(out) = crate::options::pack() {
        return pack_driver(rustc_args, program_argv, out);
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

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct GuestProcessState {
    #[serde(with = "super::worker::optional_path")]
    cwd: Option<std::path::PathBuf>,
    #[serde(with = "super::worker::environment")]
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl GuestProcessState {
    fn from_cargo_runner() -> Self {
        let cwd = crate::options::guest_cwd();
        let caller_sysroot = crate::options::caller_sysroot();
        let mut env: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
        for key in [
            crate::options::CARGO_SESSION,
            crate::options::GUEST_CWD,
            crate::options::CALLER_SYSROOT,
            "RUSTC_WRAPPER",
        ] {
            env.remove(std::ffi::OsStr::new(key));
        }
        // This process's own `MIRVM_SYSROOT` is the effective one the wrapper set, so the guest gets
        // it back only when the caller had named a sysroot at all.
        match caller_sysroot {
            Some(value) => {
                env.insert(crate::options::env_var_name("sysroot").into(), value);
            }
            None => {
                env.remove(std::ffi::OsStr::new(crate::options::env_var_name(
                    "sysroot",
                )));
            }
        }
        Self {
            cwd,
            env: env.into_iter().collect(),
        }
    }

    pub(super) fn enter(&self) -> Result<(), super::Error> {
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
            return Err(super::Error::Enter {
                detail: format!(
                    "cannot enter the Cargo caller directory {}: {error}",
                    cwd.display()
                ),
            });
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

#[cfg(feature = "compiler")]
mod compiler;
#[cfg(feature = "compiler")]
pub(crate) use compiler::run_dep_compiler;
#[cfg(not(feature = "compiler"))]
pub(crate) fn run_dep_compiler(rustc_args: Vec<String>) -> ! {
    super::worker::exec_command(
        std::iter::once("__compiler-dependency".to_owned())
            .chain(rustc_args)
            .collect(),
    )
}
