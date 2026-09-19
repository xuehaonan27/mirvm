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

mod cargo;
mod driver;
mod entry;
mod frontmatter;

pub(crate) use self::cargo::{GuestProcessState, run_dep_compiler};
pub(crate) use self::driver::{pack_driver, run_driver};
pub(crate) use self::frontmatter::parse_frontmatter_pub;

static COMPILER_SESSION: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn compiler_session_guard() -> std::sync::MutexGuard<'static, ()> {
    COMPILER_SESSION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    mirvm log inspect <file | session-dir>              # validate v0 event stream and final ledger
    mirvm log export <file | session-dir> [FILTERS]     # export attested record as JSONL
    mirvm cache status                                   # local store component sizes + stale-generation size
    mirvm cache purge [--dry-run]                        # default = remove stale generations (deps/base/ir not of current build)
    mirvm cache purge --deps|--base|--ir                 # purge entire family (all generations)
    mirvm cache purge --scripts                          # purge scripts/ (materialized project list)
    mirvm cache purge --target                           # purge unified target dir (shared dep store, largest)
    mirvm cache purge --all [--sysroot]                  # purge everything except sysroot; with flag, also sysroot (full cold start)

OPTIONS:
    --dump-mir        print entry fn MIR and exit (single-file passthrough mode only)
    --edition <ED>    default 2024 (single-file passthrough mode only)
    --sysroot <PATH>  use the given sysroot (default: auto-build cached sysroot with full MIR)
    --vm-call <SPEC>  call an exported function directly (gate/debug entry), e.g. 'fib(25)'; default runs the main startup chain
    --vm-stats        print Trap-debt statistics (pre-flight survey instrument) and exit
    --stack-size <N>  guest main execution stack virtual reservation (default 1g; accepts k/m/g suffix, same seat as JVM -Xss)
    --jit <on|off>    method-level JIT (M5.3-M5.5, default on; off = pure interpreter differential benchmark)
    --ignore-rust-version  ignore package.rust-version (project/dep scripts, same semantics as Cargo)

ENV:
    MIRVM_HOME        local store root (default $HOME/.mirvm; houses sysroot/scripts/target/cache families)
    MIRVM_TARGET_DIR  relocate mirvm's unified target dir (default $MIRVM_HOME/target/mirvm)
    MIRVM_SYSROOT     equivalent to --sysroot
    MIRVM_STACK_SIZE  equivalent to --stack-size (passed via env to runner in cargo-project form)
    MIRVM_JIT         equivalent to --jit (off/0 = pure interpreter differential benchmark)
    MIRVM_JIT_THRESHOLD  compilation trigger threshold (default 1000; diagnostic)
    MIRVM_JIT_SYNC    =1 enables JIT verify mode: enqueue and wait for publish/failure, allowing
                      compile failures to terminate loudly (gate use; proves threshold=1 differential really runs machine code)
    MIRVM_JIT_STATS   =1 prints JIT helper frequency stats at process exit via atexit (diagnostic)
    MIRVM_CARGO_LOCKED when set, frontmatter/script projects build with --locked (dep lock;
                      unset = clean env may re-resolve, see open-issues G7)
    MIRVM_DEPS        =cargo routes project/script through the long-term cargo three-phase compat track
                      (user fallback + behavioral differential); **default/=self uses zero-cargo own
                      scheduling** (D15 cargoless driver, P4 default flip: dep resolution/compilation
                      scheduling/build.rs/proc-macro/rustflags/rerun-if incremental/parallel scheduling
                      full lifecycle; mirvm test already supports resolver=1/2/3 workspace,
                      alternate registry, common source replacement/patch/replace, and
                      pack shares this path; resolver 1/2/3 all follow Cargo's unified feature rules)
    MIRVM_CLESS_JOBS  =N sets cargoless compilation scheduling concurrency (default = core count; =1 falls back to
                      topological serial order, for differential debugging)
    MIRVM_TIMING      =1 writes phase ledger to stderr (frontend/lower/engine/total)
    MIRVM_NO_IR_CACHE =1 bypasses L2 engine-IR cache (read/write disabled; diagnostic/differential)
    MIRVM_NO_BASE_IMAGE =1 bypasses std pre-lowered base image (full cold lowering; diagnostic/differential)

DEV:
    mirvm spike1..5   run the frozen M4 precursor spikes (regression self-check; see docs/history/spike*.md)
    MIRVM_JIT_DEBUG   =1 logs JIT compiler thread receive/publish flow (deliberate diagnostic knob)
    MIRVM_JIT_DEBUG_DUMP =1 dumps CLIF for functions that fail compilation (stacked on MIRVM_JIT_DEBUG)
    MIRVM_SEGV_DUMP   =1 prints fault RIP on SIGSEGV (JIT code crash site location)
";

pub fn main() -> ExitCode {
    // Troubleshooting knob: print fault RIP on SIGSEGV to locate JIT code crash site.
    if std::env::var_os("MIRVM_SEGV_DUMP").is_some() {
        crate::os::signal::install_segv_dump();
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
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };

    // Two callback forms in a cargo session
    if first == "runner" {
        return cargo::runner_main(argv);
    }
    // Base-image build subprocess (must precede MIRVM_CARGO_SESSION dispatch: builds triggered
    // inside runner carry the cargo session env and must not be misrouted into phase_wrapper)
    if first == "__build-base-image" {
        return crate::baseimage::build_main(argv);
    }
    // Cargoless dep compilation subprocess (landing point for cargoless::driver scheduling;
    // also must precede MIRVM_CARGO_SESSION dispatch)
    if first == "__cless-dep" {
        return cargo::run_cless_dep(argv.collect());
    }
    if first == "__cless-run-root" {
        return crate::cargoless::driver::run_root_recipe(argv);
    }
    if std::env::var_os("MIRVM_CARGO_SESSION").is_some() {
        if std::env::var_os("MIRVM_CARGO_COMPILER").is_some() {
            // Cargo's RUSTC slot: first is already the first real rustc argument.
            cargo_shim::phase_compiler(std::iter::once(first).chain(argv));
        }
        // Backward compat for old sessions / direct wrapper calls: first = real rustc path.
        cargo_shim::phase_wrapper(std::iter::once(first).chain(argv));
    }

    match first.as_str() {
        "run" => entry::run_main(argv),
        "capture" => capture_main(argv),
        "test" => test_main(argv),
        "pack" => entry::pack_main(argv),
        "log" => crate::telemetry::tool::main(argv),
        "cache" => entry::cache_main(argv),
        "deps" => entry::deps_main(argv),
        "spike1" => crate::vm::spikes::spike1::run(),
        "spike2" => crate::vm::spikes::spike2::run(),
        "spike3" => crate::vm::spikes::spike3::run(argv),
        "spike4" => crate::vm::spikes::spike4::run(),
        #[cfg(feature = "cranelift")]
        "spike5" => crate::vm::spikes::spike5::run(argv),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
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

fn capture_main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut output = None;
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => {
                let Some(path) = args.next() else {
                    eprintln!("mirvm capture: {arg} needs a directory");
                    return ExitCode::from(2);
                };
                if output.replace(PathBuf::from(path)).is_some() {
                    eprintln!("mirvm capture: output directory was specified more than once");
                    return ExitCode::from(2);
                }
            }
            "--" => {
                command.extend(args);
                break;
            }
            _ => {
                eprintln!("mirvm capture: expected `--` before the MIRVM command\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    if command.first().map(String::as_str) != Some("run") {
        eprintln!("mirvm capture: the first implementation accepts `-- run ...`");
        return ExitCode::from(2);
    }

    let output =
        output.unwrap_or_else(|| PathBuf::from(format!("mirvm-capture-{}", std::process::id())));
    let output = if output.is_absolute() {
        output
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(output),
            Err(error) => {
                eprintln!("mirvm capture: cannot resolve the output directory: {error}");
                return ExitCode::from(1);
            }
        }
    };
    if let Err(error) = std::fs::create_dir_all(&output) {
        eprintln!(
            "mirvm capture: cannot create output directory {}: {error}",
            output.display()
        );
        return ExitCode::from(1);
    }
    if set_capture_directory(output).is_err() {
        eprintln!("mirvm capture: a capture request is already configured in this process");
        return ExitCode::from(2);
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
    entry::run_main(command.into_iter().skip(1))
}

/// `mirvm test [project] [Cargo selection args/TESTNAME] [-- libtest args]`.
/// The project argument is recognized only in the first slot; defaults to the current directory.
/// The Cargo compat track keeps original args interpreted verbatim; the self track resolves them
/// inside cargoless::driver under the same contract.
fn test_main(argv: impl Iterator<Item = String>) -> ExitCode {
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

    match std::env::var("MIRVM_DEPS").as_deref() {
        Ok("cargo") => cargo_shim::phase_cargo_test(&dir, &before, &harness_args),
        Err(_) | Ok("self") => crate::cargoless::driver::test_project(&dir, &before, &harness_args),
        Ok(other) => {
            eprintln!("mirvm: MIRVM_DEPS only accepts `cargo` or `self` (got `{other}`)");
            ExitCode::from(2)
        }
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
