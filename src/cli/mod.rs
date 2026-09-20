//! CLI and rustc driver shim. Three runtime forms:
//! - `mirvm run <script|project>`: user entry point
//! - `mirvm <rustc> <args...>` (under MIRVM_CARGO_SESSION): cargo's RUSTC_WRAPPER
//! - `mirvm runner <fake-binary> <args...>`: cargo's target runner, the real interpreter entry
//!
//! Engine = M4 bytecode VM (loading phase lower + execution phase engine). The differential
//! oracle is native compile-and-run (`differential.programs`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cargo_shim;

mod base_image;
mod cargo;
pub(crate) mod diagnostics;
mod driver;
mod entry;
mod frontmatter;

pub(crate) use self::cargo::{GuestProcessState, run_dep_compiler};
pub(crate) use self::driver::{pack_driver, run_driver};
pub(crate) use self::frontmatter::{
    ScriptPackage, effective_manifest, parse_frontmatter_pub, script_cache_dir,
};

static COMPILER_SESSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn compiler_session_guard() -> std::sync::MutexGuard<'static, ()> {
    COMPILER_SESSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `-Zthreads=N` for one compiler session, from `MIRVM_THREADS` (validated once at entry, see
/// [`validate_parallel_frontend_arg`]).
///
/// Injected even for the default `1`: on the pinned toolchain `-Zthreads=1` parses back to "no
/// thread pool" (`rustc_session`'s `parse_threads` maps `n <= 1` to `None`), so it is a no-op
/// today, but it fixes this session's thread count regardless of rustc's own default — upstream is
/// moving that default to 2 frontend threads on nightly.
///
/// This is the session's *view* of the arguments, never key material: callers append it to a copy
/// taken after the L2 key, package header and cargoless unit fingerprint were snapshotted. Letting
/// the flag into a key would make enabling it re-key (or worse, silently mismatch) cached images.
pub(crate) fn parallel_frontend_arg() -> &'static str {
    PARALLEL_FRONTEND_ARG
        .get()
        .map(String::as_str)
        .unwrap_or(SEQUENTIAL_FRONTEND_ARG)
}

/// Dependency compilation units deliberately do not consult `MIRVM_THREADS`: the cargoless
/// scheduler already runs crates as parallel subprocesses, and the pinned toolchain has neither a
/// jobserver nor `--jobs` to bound a per-unit pool. Spelled out rather than omitted so a future
/// rustc default cannot quietly raise it.
pub(crate) const SEQUENTIAL_FRONTEND_ARG: &str = "-Zthreads=1";

static PARALLEL_FRONTEND_ARG: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Parse `MIRVM_THREADS` before any command runs. A bad value is a usage error, and it has to be
/// caught here rather than at the injection sites: one of those is the `__build-base-image`
/// subprocess, whose stderr is captured into `build.log` and whose failure the parent deliberately
/// treats as "no base image, lower cold" — a rejection there would be silent.
pub(crate) fn validate_parallel_frontend_arg() -> Result<(), crate::options::Error> {
    let arg = crate::options::get().threads_arg()?;
    let _ = PARALLEL_FRONTEND_ARG.set(arg);
    Ok(())
}

const USAGE: &str = "\
mirvm — a Rust runtime with its own execution engine

USAGE:
    mirvm run <file.rs>  [OPTIONS] [-- <program args>]   # single file (may declare frontmatter deps)
    mirvm run <x.mirvm>  [OPTIONS] [-- <program args>]   # run a .mirvm package (mode B slice 2)
    mirvm pack <target>  [-o out.mirvm]                  # cargo project / script / single file -> .mirvm package
    mirvm run <dir | Cargo.toml> [-- <program args>]     # cargo project (deps auto-built as MIR rlibs)
    mirvm test [dir | Cargo.toml] [OPTIONS] [TESTNAME] [-- <libtest args>]
    mirvm capture [-o DIR] -- run <input> [OPTIONS]      # record one real guest execution
    mirvm options [--json]                               # every external input mirvm defines, with value and source
    mirvm log inspect <file | session-dir>              # validate v0 event stream and final ledger
    mirvm log export <file | session-dir> [FILTERS]     # export attested record as JSONL
    mirvm cache status                                   # local store component sizes + stale-generation size
    mirvm cache purge [--dry-run]                        # default = remove stale generations (cache/base|deps|ir not of this build)
    mirvm cache purge --deps|--base|--ir                 # purge one generational cache family (all generations)
    mirvm cache purge --scripts                          # purge build/scripts (materialized project list)
    mirvm cache purge --target                           # purge build/target (shared dep store + native builds, largest)
    mirvm cache purge --all                              # purge cache/ + build/ + run/: everything that needs no network
    mirvm cache purge --all --data                       # also data/ (crate store + sysroot): full cold start
";

/// The full help text: the command summary plus the option, environment and diagnostic blocks,
/// which are generated from [`crate::options::entries`] so help cannot drift from the code.
pub(crate) fn usage() -> String {
    format!(
        "{USAGE}\n{}{}{}",
        crate::options::usage_options_section(crate::options::Scope::Run),
        crate::options::usage_env_section(),
        crate::options::usage_dev_section(),
    )
}

pub fn main() -> ExitCode {
    // Troubleshooting knob: print fault RIP on SIGSEGV to locate JIT code crash site.
    if crate::options::get().segv_dump {
        crate::os::signal::install_segv_dump();
    }
    // Rejected before any dispatch: the flag reaches every compiler session, and one injection
    // site (the base-image subprocess) has no way to report a rejection loudly.
    if let Err(error) = validate_parallel_frontend_arg() {
        return crate::error::Error::from(error).report();
    }
    // Same place, same reason: the output mode is read by the emitter itself, so it is validated
    // once here rather than silently falling back to text at every call site.
    if let Err(error) = crate::options::get().output_format() {
        return crate::error::Error::from(error).report();
    }
    let mut argv = std::env::args();
    let argv0 = argv.next().unwrap_or_default();

    if cargo_shim::is_cargo_rustdoc(Path::new(&argv0)) {
        cargo_shim::phase_cargo_rustdoc(argv);
    }
    if crate::cargoless::driver::is_doctest_builder(Path::new(&argv0)) {
        return crate::cargoless::driver::run_doctest_builder(argv);
    }

    // `CARGO_BIN_EXE_*` self launchers are symlinks to mirvm with a root bin recipe next to
    // them. Must be recognized before ordinary command dispatch, or guest args are mistaken for
    // mirvm commands.
    if let Some(recipe) = crate::cargoless::driver::root_launcher_recipe(Path::new(&argv0)) {
        return crate::cargoless::driver::run_root_recipe(
            std::iter::once(recipe.display().to_string()).chain(argv),
        );
    }
    let Some(first) = argv.next() else {
        crate::diag::write(usage().as_bytes());
        return ExitCode::from(crate::diag::exit::USAGE);
    };
    // One component per process, fixed at the dispatch boundary: every diagnostic emitted below —
    // including the engine's — is attributed to the command the user actually ran.
    crate::diag::enter(component_of(&first));

    // Two callback forms in a cargo session
    if first == "runner" {
        return cargo::runner_main(argv);
    }
    // Base-image build subprocess (must precede MIRVM_CARGO_SESSION dispatch: builds triggered
    // inside runner carry the cargo session env and must not be misrouted into phase_wrapper)
    if first == "__build-base-image" {
        return base_image::build_main(argv);
    }
    // Cargoless dep compilation subprocess (landing point for cargoless::driver scheduling;
    // also must precede MIRVM_CARGO_SESSION dispatch)
    if first == "__cless-dep" {
        return cargo::run_cless_dep(argv.collect());
    }
    if first == "__cless-run-root" {
        return crate::cargoless::driver::run_root_recipe(argv);
    }
    if crate::options::protocol::cargo_session() {
        if crate::options::protocol::cargo_compiler() {
            // Cargo's RUSTC slot: first is already the first real rustc argument.
            cargo_shim::phase_compiler(std::iter::once(first).chain(argv));
        }
        // Backward compat for old sessions / direct wrapper calls: first = real rustc path.
        cargo_shim::phase_wrapper(std::iter::once(first).chain(argv));
    }

    let command = match first.as_str() {
        "run" => entry::run_main(argv),
        "capture" => capture_main(argv),
        "test" => test_main(argv),
        "pack" => entry::pack_main(argv),
        "cache" => entry::cache_main(argv),
        "deps" => entry::deps_main(argv),
        "options" => options_main(argv),
        // `log` and the internal subcommands own their own statuses (libtest's, rustc's, the
        // guest's) and never produce a mirvm-authored failure here.
        "log" => return crate::telemetry::tool::main(argv),
        _ => {
            crate::diag::write(usage().as_bytes());
            return ExitCode::from(crate::diag::exit::USAGE);
        }
    };
    match command {
        Ok(code) => code,
        // The router this failure must reach is still armed: every frame that finishes one has
        // already returned by the time the status is built here.
        Err(error) => error.report(),
    }
}

/// Record `--json` on the command line: it is the command line's spelling of `MIRVM_OUTPUT`, and the
/// same discipline as `--stack-size` and `--jit` applies — the resolved value is exported so every
/// child process reads back one decision.
pub(crate) fn note_json_output() {
    crate::options::note_cli("output_format");
    crate::options::export_to_process("output_format", "json");
}

/// The component a dispatch spelling speaks for, in the `diag` vocabulary. An unrecognized command
/// is about to be rejected with the usage text, so its scope never reaches a diagnostic.
fn component_of(command: &str) -> crate::diag::Component {
    use crate::diag::Component;
    match command {
        "run" => Component::Run,
        "capture" => Component::Capture,
        "test" => Component::Test,
        "pack" => Component::Pack,
        "log" => Component::Log,
        "cache" => Component::Cache,
        "deps" => Component::Deps,
        "options" => Component::Options,
        "runner" => Component::Runner,
        "__build-base-image" => Component::BaseImage,
        "__cless-dep" | "__cless-run-root" => Component::Build,
        _ => Component::Run,
    }
}

/// `mirvm options [--json]`: print every external input mirvm defines, its current value and where
/// that value came from. This is the executable form of `src/options.rs`.
fn options_main(args: impl Iterator<Item = String>) -> Result<ExitCode, crate::error::Error> {
    use crate::diag::Component;
    for arg in args {
        match arg.as_str() {
            "--json" => note_json_output(),
            other => {
                return Err(crate::error::Error::usage_with(
                    Component::Options,
                    format!("unknown argument `{other}`"),
                    usage(),
                ));
            }
        }
    }
    if crate::options::get().output_format()? == crate::options::OutputFormat::Json {
        println!("{}", crate::options::render_json());
    } else {
        println!("{}", crate::options::version());
        print!("{}", crate::options::render());
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) const INTERNAL_CAPTURE_DIRECTORY_ARG: &str = "--mirvm-capture-directory";
static CAPTURE_DIRECTORY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
static FORWARDED_CAPTURE_DIRECTORY: std::sync::OnceLock<()> = std::sync::OnceLock::new();

pub(crate) fn capture_directory() -> Option<&'static Path> {
    CAPTURE_DIRECTORY.get().map(PathBuf::as_path)
}

pub(crate) fn set_capture_directory(directory: PathBuf) -> Result<(), PathBuf> {
    CAPTURE_DIRECTORY.set(directory)
}

pub(crate) fn set_forwarded_capture_directory(directory: PathBuf) -> Result<(), PathBuf> {
    CAPTURE_DIRECTORY.set(directory)?;
    let _ = FORWARDED_CAPTURE_DIRECTORY.set(());
    Ok(())
}

fn capture_directory_is_forwarded() -> bool {
    FORWARDED_CAPTURE_DIRECTORY.get().is_some()
}

pub(crate) fn take_internal_capture_directory<I>(
    argv: &mut std::iter::Peekable<I>,
) -> Result<Option<PathBuf>, ()>
where
    I: Iterator<Item = String>,
{
    if !argv
        .peek()
        .is_some_and(|arg| arg == INTERNAL_CAPTURE_DIRECTORY_ARG)
    {
        return Ok(None);
    }
    argv.next();
    argv.next().map(PathBuf::from).map(Some).ok_or(())
}

fn capture_main(mut args: impl Iterator<Item = String>) -> Result<ExitCode, crate::error::Error> {
    use crate::diag::Component;
    let mut output = None;
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => {
                let Some(path) = args.next() else {
                    return Err(crate::error::Error::usage_with(
                        Component::Capture,
                        format!("{arg} needs a directory"),
                        usage(),
                    ));
                };
                if output.replace(PathBuf::from(path)).is_some() {
                    return Err(crate::error::Error::usage_with(
                        Component::Capture,
                        "output directory was specified more than once",
                        usage(),
                    ));
                }
            }
            "--" => {
                command.extend(args);
                break;
            }
            "--json" => note_json_output(),
            _ => {
                return Err(crate::error::Error::usage_with(
                    Component::Capture,
                    "expected `--` before the MIRVM command",
                    usage(),
                ));
            }
        }
    }
    if command.first().map(String::as_str) != Some("run") {
        return Err(crate::error::Error::usage(
            Component::Capture,
            "the first implementation accepts `-- run ...`",
        ));
    }

    let output =
        output.unwrap_or_else(|| PathBuf::from(format!("mirvm-capture-{}", std::process::id())));
    let output = if output.is_absolute() {
        output
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(output),
            Err(error) => {
                return Err(crate::error::Error::failure(
                    Component::Capture,
                    format!("cannot resolve the output directory: {error}"),
                ));
            }
        }
    };
    if let Err(error) = std::fs::create_dir_all(&output) {
        return Err(crate::error::Error::failure(
            Component::Capture,
            format!(
                "cannot create output directory {}: {error}",
                output.display()
            ),
        ));
    }
    if set_capture_directory(output).is_err() {
        return Err(crate::error::Error::usage(
            Component::Capture,
            "a capture request is already configured in this process",
        ));
    }
    let _diagnostic_router = match diagnostics::DiagnosticRouter::start(
        capture_directory(),
        capture_directory_is_forwarded(),
    ) {
        Ok(router) => router,
        Err(error) => {
            return Err(crate::error::Error::software(
                Component::Capture,
                format!("cannot start diagnostics stream: {error}"),
            ));
        }
    };
    // The router stays armed until the process-exit finalizer publishes it, so a failure returned
    // from here is still inside its lifetime and reaches `diagnostics.log` byte-for-byte.
    entry::run_main(command.into_iter().skip(1))
}

/// `mirvm test [project] [Cargo selection args/TESTNAME] [-- libtest args]`.
/// The project argument is recognized only in the first slot; defaults to the current directory.
/// The Cargo compat track keeps original args interpreted verbatim; the self track resolves them
/// inside cargoless::driver under the same contract.
fn test_main(argv: impl Iterator<Item = String>) -> Result<ExitCode, crate::error::Error> {
    let mut before = Vec::new();
    let mut harness_args = Vec::new();
    let mut after_dash = false;
    for arg in argv {
        if after_dash {
            harness_args.push(arg);
        } else if arg == "--" {
            after_dash = true;
        } else {
            before.push(arg);
        }
    }

    let project = before
        .first()
        .map(PathBuf::from)
        .filter(|p| p.is_dir() || p.file_name().is_some_and(|f| f == "Cargo.toml"));
    if project.is_some() {
        before.remove(0);
    }
    let project = project.unwrap_or_else(|| PathBuf::from("."));
    let dir = if project.is_dir() {
        project
    } else {
        project.parent().unwrap_or(Path::new(".")).to_path_buf()
    };

    match crate::options::get().deps()? {
        // Diverges: the Cargo track replaces this process with cargo's.
        crate::options::DepsTrack::Cargo => {
            cargo_shim::phase_cargo_test(&dir, &before, &harness_args)
        }
        crate::options::DepsTrack::Own => Ok(crate::cargoless::driver::test_project(
            &dir,
            &before,
            &harness_args,
        )),
    }
}
#[cfg(test)]
mod tests {
    use rustc_errors::{DiagInner, Level};

    use super::driver::is_runner_warning_summary;
    use super::{INTERNAL_CAPTURE_DIRECTORY_ARG, take_internal_capture_directory};

    #[test]
    fn internal_capture_argument_is_removed_before_guest_arguments_are_built() {
        let mut argv = [
            INTERNAL_CAPTURE_DIRECTORY_ARG,
            "/tmp/capture",
            "/tmp/fake-bin",
            "guest-argument",
        ]
        .into_iter()
        .map(str::to_owned)
        .peekable();
        assert_eq!(
            take_internal_capture_directory(&mut argv).unwrap(),
            Some(std::path::PathBuf::from("/tmp/capture"))
        );
        assert_eq!(
            argv.collect::<Vec<_>>(),
            ["/tmp/fake-bin", "guest-argument"]
        );
    }

    #[test]
    fn runner_filter_accepts_only_rustc_warning_count_summaries() {
        assert!(is_runner_warning_summary(&DiagInner::new(
            Level::ForceWarning,
            "1 warning emitted"
        )));
        assert!(is_runner_warning_summary(&DiagInner::new(
            Level::ForceWarning,
            "9 warnings emitted"
        )));
        assert!(!is_runner_warning_summary(&DiagInner::new(
            Level::Warning,
            "1 warning emitted"
        )));
        assert!(!is_runner_warning_summary(&DiagInner::new(
            Level::ForceWarning,
            "warning: 1 warning emitted"
        )));
    }
}
