//! CLI and rustc driver shim. Three runtime forms:
//! - `mirvm run <script|project>`: user entry point
//! - `mirvm <rustc> <args...>` (under MIRVM_CARGO_SESSION): cargo's RUSTC_WRAPPER
//! - `mirvm runner <fake-binary> <args...>`: cargo's target runner, the real interpreter entry
//!
//! Engine = M4 bytecode VM (loading phase lower + execution phase engine). The differential
//! oracle is native compile-and-run (`differential.programs`).

use std::path::{Path, PathBuf};
use std::process::{ExitCode, exit};

use rustc_data_structures::AtomicRef;
use rustc_driver::{Callbacks, Compilation};
use rustc_errors::{DiagInner, ErrorGuaranteed, Level};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::cargo_shim;

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
        return runner_main(argv);
    }
    // Base-image build subprocess (must precede MIRVM_CARGO_SESSION dispatch: builds triggered
    // inside runner carry the cargo session env and must not be misrouted into phase_wrapper)
    if first == "__build-base-image" {
        return crate::baseimage::build_main(argv);
    }
    // Cargoless dep compilation subprocess (landing point for cargoless::driver scheduling;
    // also must precede MIRVM_CARGO_SESSION dispatch)
    if first == "__cless-dep" {
        return run_cless_dep(argv.collect());
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
        "run" => run_main(argv),
        "capture" => capture_main(argv),
        "test" => test_main(argv),
        "pack" => pack_main(argv),
        "log" => crate::telemetry::tool::main(argv),
        "cache" => cache_main(argv),
        "deps" => deps_main(argv),
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
    run_main(command.into_iter().skip(1))
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

// ===== user entry points =====

/// `mirvm pack <target> [-o out.mirvm]`: cargo project (directory/Cargo.toml), frontmatter
/// script, or plain single file -> .mirvm package. Projects/frontmatter default to cargoless;
/// `MIRVM_DEPS=cargo` is passed into runner via MIRVM_PACK. Both paths force the full cold
/// route so the package is self-contained.
fn pack_main(argv: impl Iterator<Item = String>) -> ExitCode {
    let mut input = None;
    let mut out: Option<std::path::PathBuf> = None;
    let mut it = argv.peekable();
    while let Some(arg) = it.next() {
        if arg == "-o" || arg == "--output" {
            let Some(v) = it.next() else {
                eprintln!("mirvm: `pack -o` needs argument");
                exit(2);
            };
            out = Some(std::path::PathBuf::from(v));
        } else if input.is_none() && !arg.starts_with('-') {
            input = Some(arg);
        } else {
            eprintln!("mirvm: pack unknown argument `{arg}`");
            exit(2);
        }
    }
    let Some(input) = input else {
        eprintln!("mirvm: `pack` needs target (cargo project directory / Cargo.toml / script)");
        exit(2);
    };
    let input_path = PathBuf::from(&input);
    let default_out = || -> std::path::PathBuf {
        let stem = if input_path.is_dir() {
            input_path
                .canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "package".into())
        } else {
            input_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "package".into())
        };
        std::path::PathBuf::from(format!("{stem}.mirvm"))
    };
    let out = out.unwrap_or_else(default_out);
    let out_abs = std::path::absolute(&out).unwrap_or(out);

    let deps_self = match std::env::var("MIRVM_DEPS").as_deref() {
        Err(_) | Ok("self") => true,
        Ok("cargo") => false,
        Ok(other) => {
            eprintln!("mirvm: MIRVM_DEPS only accepts `cargo` or `self` (got `{other}`)");
            exit(2);
        }
    };

    // Project form: default to own scheduling; Cargo track enters runner only on explicit fallback.
    let is_cargo_dir =
        input_path.is_dir() || input_path.file_name().is_some_and(|f| f == "Cargo.toml");
    if is_cargo_dir {
        let dir = if input_path.is_dir() {
            input_path.as_path()
        } else {
            input_path.parent().unwrap_or(Path::new("."))
        };
        if deps_self {
            return crate::cargoless::driver::pack_project(dir, &out_abs);
        }
        // SAFETY: single-threaded startup phase.
        unsafe { set_cargo_pack_env(&out_abs) };
        cargo_shim::phase_cargo(dir, &[], None, false);
    }
    let src = std::fs::read_to_string(&input_path).unwrap_or_else(|e| {
        eprintln!("mirvm: fail to read {input}: {e}");
        exit(1);
    });
    if let Some((manifest, body)) = parse_frontmatter(&src) {
        if deps_self {
            return crate::cargoless::driver::pack_script(&input_path, &out_abs);
        }
        let dir = materialize_script(&input_path, &manifest, &body);
        // SAFETY: single-threaded startup phase.
        unsafe { set_cargo_pack_env(&out_abs) };
        cargo_shim::phase_cargo(&dir, &[], None, false);
    }

    // Plain single file: pack_driver directly (same args as run form 3)
    let sysroot =
        std::env::var("MIRVM_SYSROOT").unwrap_or_else(|_| match crate::sysroot::ensure_sysroot() {
            Ok(p) => p.display().to_string(),
            Err(e) => {
                eprintln!("mirvm: fail to build sysroot: {e}");
                exit(1);
            }
        });
    let rustc_args = vec![
        "mirvm".to_string(),
        input.clone(),
        "--edition=2024".to_string(),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot,
    ];
    let program_argv = vec![input];
    pack_driver(rustc_args, program_argv, out_abs)
}

unsafe fn set_cargo_pack_env(out: &Path) {
    // SAFETY: caller guarantees we are still in the CLI single-threaded startup phase.
    unsafe {
        std::env::set_var("MIRVM_PACK", out);
        std::env::set_var("MIRVM_NO_BASE_IMAGE", "1");
        std::env::set_var("MIRVM_NO_DEPS_IMAGE", "1");
    }
}

/// `mirvm cache status|purge ...`: manage the local store ($HOME/.mirvm, relocatable via MIRVM_HOME).
fn cache_main(args: impl Iterator<Item = String>) -> ExitCode {
    let root = crate::sysroot::cache_dir();
    let mut plan = crate::cachectl::Purge::default();
    let mut sub = None;
    for a in args {
        match a.as_str() {
            "status" | "purge" if sub.is_none() => sub = Some(a),
            "--dry-run" => plan.dry_run = true,
            "--deps" => plan.deps = true,
            "--base" => plan.base = true,
            "--ir" => plan.ir = true,
            "--scripts" => plan.scripts = true,
            "--target" => plan.target = true,
            "--all" => plan.all = true,
            "--sysroot" => plan.sysroot = true,
            _ => {
                eprintln!("mirvm cache: unknown argument `{a}`\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    match sub.as_deref() {
        Some("status") => {
            print!("{}", crate::cachectl::status(&root));
            ExitCode::SUCCESS
        }
        Some("purge") => {
            // No flags by default = remove stale generations (conservative); any target flag present follows the flag
            if !(plan.deps || plan.base || plan.ir || plan.scripts || plan.target || plan.all) {
                plan.stale = true;
            }
            print!("{}", crate::cachectl::purge(&root, plan));
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `mirvm deps audit <target...>`: target = project directory (containing Cargo.toml) or
/// frontmatter script; resolve per target and reconcile against the reference lock, exiting
/// non-zero if any target fails to resolve or the reconciliation mismatches.
fn deps_main(args: impl Iterator<Item = String>) -> ExitCode {
    let mut sub = None;
    let mut targets: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "audit" if sub.is_none() => sub = Some(a),
            _ if sub.is_some() => targets.push(a),
            _ => {
                eprintln!(
                    "mirvm deps: unknown argument `{a}`\nusage: mirvm deps audit <project dir|script.rs>..."
                );
                return ExitCode::from(2);
            }
        }
    }
    if sub.is_none() || targets.is_empty() {
        eprintln!("usage: mirvm deps audit <project dir|script.rs>...");
        return ExitCode::from(2);
    }
    let mut failures = 0usize;
    for t in &targets {
        let path = std::path::Path::new(t);
        let result = if path.is_dir() || path.file_name().is_some_and(|f| f == "Cargo.toml") {
            let dir = if path.is_dir() {
                path.to_path_buf()
            } else {
                path.parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf()
            };
            crate::cargoless::audit::audit_project(&dir)
        } else {
            crate::cargoless::audit::audit_script(path)
        };
        match result {
            Ok(report) => {
                if report.mode == "skip" {
                    println!(
                        "SKIP {} (needs absent, not counted as failure per the same gate standard)",
                        report.name
                    );
                    continue;
                }
                let head = format!(
                    "{} ({} mode, {} units, {} package versions)",
                    report.name,
                    report.mode,
                    report.units,
                    report.plan.version_map.len()
                );
                // Failure conditions: project = lock reconciliation equality; script = cargo acceptance chain
                let mut fail: Option<String> = None;
                if report.mode == "lock"
                    && let Some((lock_desc, mismatches)) = &report.lock_check
                    && !mismatches.is_empty()
                {
                    fail = Some(format!(
                        "reconciliation mismatch: {} entries vs {lock_desc}",
                        mismatches.len()
                    ));
                    for m in mismatches.iter().take(5) {
                        println!("     {m}");
                    }
                }
                if let Some(acc) = &report.acceptance
                    && let Err(diag) = acc
                {
                    fail = Some(diag.clone());
                }
                match fail {
                    Some(why) => {
                        println!("FAIL {head}: {why}");
                        failures += 1;
                    }
                    None => {
                        print!("OK   {head}");
                        if let Some((lock_desc, mismatches)) = &report.lock_check {
                            if mismatches.is_empty() {
                                print!("; reconciliation == {lock_desc}");
                            } else if report.mode == "fresh" {
                                print!(
                                    "; {} timestamp-drift entries in historical reference (informational, not a failure)",
                                    mismatches.len()
                                );
                            }
                        }
                        if report.acceptance.is_some() {
                            print!("; cargo --locked --offline accepted");
                        }
                        println!();
                    }
                }
            }
            Err(e) => {
                // P5 loud rejections are an upfront-stated boundary and not ordinary resolution failures.
                if e.contains("P5") {
                    println!("P5   {t}: {e}");
                } else {
                    println!("FAIL {t}: {e}");
                    failures += 1;
                }
            }
        }
    }
    println!("---");
    println!(
        "deps audit: {} targets, {} failures",
        targets.len(),
        failures
    );
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn run_main(args: impl Iterator<Item = String>) -> ExitCode {
    let mut args = args.peekable();
    let mut input = None;
    let mut dump_mir = false;
    let mut edition = "2024".to_string();
    let mut sysroot = None;
    let mut vm_call: Option<String> = None;
    let mut vm_stats = false;
    let mut bin_sel: Option<String> = None;
    let mut ignore_rust_version = false;
    let mut program_args: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        let mut next = |name: &str| {
            args.next().unwrap_or_else(|| {
                crate::diagnostics::control(format_args!("mirvm: {name} needs argument(s)"));
                exit(2);
            })
        };
        match arg.as_str() {
            "--" => {
                program_args.extend(args.by_ref());
                break;
            }
            "--dump-mir" => dump_mir = true,
            "--edition" => edition = next("--edition"),
            "--sysroot" => sysroot = Some(next("--sysroot")),
            // Backward compat for old gate scripts: --engine vm is the only engine, just consume it
            "--engine" => {
                let e = next("--engine");
                if e != "vm" {
                    crate::diagnostics::control(format_args!(
                        "mirvm: engine `{e}` no longer exists (tier-0 removed; the only engine is vm)"
                    ));
                    exit(2);
                }
            }
            "--vm-call" => vm_call = Some(next("--vm-call")),
            "--vm-stats" => vm_stats = true,
            // cargo run --bin semantics (project form only; no meaning for script/single-file)
            "--bin" => bin_sel = Some(next("--bin")),
            "--ignore-rust-version" => ignore_rust_version = true,
            "--stack-size" => {
                let v = next("--stack-size");
                if let Err(message) = parse_stack_size(&v) {
                    crate::diagnostics::control(format_args!("{message}"));
                    exit(2);
                }
                // Set env so the cargo form (wrapper -> runner subprocess) uses the same knob.
                // We are still in the single-threaded startup phase (rustc session has not begun).
                unsafe { std::env::set_var("MIRVM_STACK_SIZE", v) };
            }
            "--jit" => {
                let v = next("--jit");
                if v != "on" && v != "off" {
                    // TODO: tiered JIT?
                    crate::diagnostics::control(format_args!(
                        "mirvm: --jit only accepts on|off (got `{v}`)"
                    ));
                    exit(2);
                }
                // Same as --stack-size: set env so the cargo form takes effect via runner
                unsafe { std::env::set_var("MIRVM_JIT", v) };
            }
            _ if input.is_none() && !arg.starts_with('-') => input = Some(arg),
            _ => {
                crate::diagnostics::control(format_args!(
                    "mirvm: unknown argument `{arg}`\n{USAGE}"
                ));
                exit(2);
            }
        }
    }
    let Some(input) = input else {
        crate::diagnostics::control_raw(format_args!("{USAGE}"));
        exit(2);
    };
    let input_path = PathBuf::from(&input);

    // Default = self zero-cargo own scheduling (cargoless::driver); =cargo uses the long-term
    // cargo three-phase compat track (user fallback + behavioral differential); any other value
    // is rejected loudly
    let deps_self = match std::env::var("MIRVM_DEPS").as_deref() {
        Err(_) | Ok("self") => true,
        Ok("cargo") => false,
        Ok(other) => {
            crate::diagnostics::control(format_args!(
                "mirvm: MIRVM_DEPS only accepts `cargo` or `self` (got `{other}`)"
            ));
            exit(2);
        }
    };

    // Form 1: cargo project (directory or Cargo.toml)
    if input_path.is_dir() {
        if deps_self {
            return crate::cargoless::driver::run_project(
                &input_path,
                &program_args,
                bin_sel.as_deref(),
                ignore_rust_version,
            );
        }
        cargo_shim::phase_cargo(
            &input_path,
            &program_args,
            bin_sel.as_deref(),
            ignore_rust_version,
        );
    }
    if input_path.file_name().is_some_and(|f| f == "Cargo.toml") {
        let dir = input_path.parent().unwrap_or(Path::new("."));
        if deps_self {
            return crate::cargoless::driver::run_project(
                dir,
                &program_args,
                bin_sel.as_deref(),
                ignore_rust_version,
            );
        }
        cargo_shim::phase_cargo(dir, &program_args, bin_sel.as_deref(), ignore_rust_version);
    }
    if let Some(b) = &bin_sel {
        // Script/single-file/package forms have no --bin concept (same as cargo script) — reject loudly, do not silently swallow
        crate::diagnostics::control(format_args!(
            "mirvm: --bin {b} is only valid for cargo project form (directory/Cargo.toml)"
        ));
        exit(2);
    }

    // Sniff for a .mirvm package (before the text read -- a package is binary)
    if crate::pack::is_package(&input_path) {
        let module = match crate::pack::load_package(&input_path)
            .and_then(|package| package.instantiate())
        {
            Ok(module) => module,
            Err(reason) => {
                crate::diagnostics::control(format_args!(
                    "mirvm: fail to load {}: {reason}",
                    input_path.display()
                ));
                exit(70);
            }
        };
        // warm second half mirrors run_driver hot path (empty image stack: asm recipes idempotently rematerialized)
        let mut module = module;
        module.asm_stub_addrs = crate::lower::asm::materialize(&module.asm_sites);
        let mut program_argv = vec![input];
        program_argv.extend(program_args);
        let code = run_vm_engine(module, &program_argv, vm_call.as_deref(), vm_stats, true);
        exit(code);
    }

    let src = std::fs::read_to_string(&input_path).unwrap_or_else(|e| {
        crate::diagnostics::control(format_args!("mirvm: fail to read {input}: {e}"));
        exit(1);
    });

    // Form 2: single-file script with frontmatter dependency declaration -> materialize into cargo project
    if let Some((manifest, body)) = parse_frontmatter(&src) {
        if deps_self {
            return crate::cargoless::driver::run_script(
                &input_path,
                &program_args,
                ignore_rust_version,
            );
        }
        let dir = materialize_script(&input_path, &manifest, &body);
        cargo_shim::phase_cargo(&dir, &program_args, None, ignore_rust_version);
    }

    // Form 3: plain single file, zero-cargo fast path
    let sysroot = sysroot
        .or_else(|| std::env::var("MIRVM_SYSROOT").ok())
        .unwrap_or_else(|| match crate::sysroot::ensure_sysroot() {
            Ok(p) => p.display().to_string(),
            Err(e) => {
                crate::diagnostics::control(format_args!("mirvm: fail to build sysroot: {e}"));
                exit(1);
            }
        });
    let rustc_args = vec![
        "mirvm".to_string(),
        input.clone(),
        format!("--edition={edition}"),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot,
    ];
    let mut program_argv = vec![input];
    program_argv.extend(program_args);
    run_driver(
        rustc_args,
        program_argv,
        dump_mir,
        vm_call,
        vm_stats,
        false,
        None,
    )
}

// ===== cargo runner callback =====

fn runner_main(argv: impl Iterator<Item = String>) -> ExitCode {
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

    fn enter(&self) -> Result<(), String> {
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
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
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
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    });
    exit(if code == ExitCode::SUCCESS { 0 } else { 1 })
}

/// Cargoless dep compilation subprocess entry: restore argv0, then feed the rest to
/// `run_dep_compiler`. Arguments are computed by `cargoless::schedule::dep_rustc_args` and
/// scheduled by `cargoless::driver`; shares the DepCallbacks/global_asm extraction channel with
/// the cargo_shim wrapper.
fn run_cless_dep(rest: Vec<String>) -> ExitCode {
    let mut args = Vec::with_capacity(rest.len() + 1);
    args.push("mirvm-cless-rustc".to_string());
    args.extend(rest);
    run_dep_compiler(args)
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

fn is_runner_warning_summary(diagnostic: &DiagInner) -> bool {
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
    module: Option<crate::vm::engine::ir::Module>,
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
    /// Split image produced by `lower_program` when `MIRVM_DEPS_IMAGE=1` and a base image is
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
    if !force && std::env::var_os("MIRVM_TIMING").is_none() {
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
                    crate::vm::engine::verify::Prefix {
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
fn parse_stack_size(s: &str) -> Result<usize, String> {
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

fn run_vm_engine(
    mut module: crate::vm::engine::ir::Module,
    program_argv: &[String],
    vm_call: Option<&str>,
    vm_stats: bool,
    already_verified: bool,
) -> i32 {
    if !already_verified && let Err(e) = crate::vm::engine::verify::module(&module) {
        crate::diagnostics::control(format_args!("mirvm: bytecode verification failed: {e}"));
        return 70;
    }
    if vm_stats {
        print!("{}", crate::vm::engine::stats::report(&module));
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

fn run_vm_engine_loaded(module: crate::vm::engine::ir::Module, vm_call: Option<&str>) -> i32 {
    let shared = crate::vm::engine::ctx::Shared::new(module);
    // Make entry stubs executable: recipe -> closure -> stub bytes -> whole-region RX (a
    // full-phase step alongside the two above; an occupied region means load failure).
    let engine = match crate::vm::engine::ctx::Engine::try_new(shared) {
        Ok(engine) => engine,
        Err(e) => {
            crate::diagnostics::control(format_args!("mirvm: {e}"));
            return 70;
        }
    };
    let Some(spec) = vm_call else {
        // main startup chain: interpret lang_start as usual; the exit code is Termination's product.
        let execution = engine.clone();
        let result = match on_guest_stack(move || crate::vm::engine::interp::run_main(&execution)) {
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
            Ok(crate::vm::engine::interp::RunOutcome::Returned(code)) => code,
            // `lang_start` has already run the guest panic hook. Match native
            // stderr here and only translate the structured outcome to its OS
            // exit status.
            Ok(crate::vm::engine::interp::RunOutcome::GuestPanic) => 101,
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
        unsafe { crate::vm::engine::interp::run_export(&execution, &name, &args) }
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
        Ok(crate::vm::engine::interp::RunOutcome::Returned(r)) => {
            println!("{}", r.lo);
            0
        }
        Ok(crate::vm::engine::interp::RunOutcome::GuestPanic) => {
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
    let reserve = match std::env::var("MIRVM_STACK_SIZE") {
        Ok(s) => parse_stack_size(&s).map_err(|message| GuestStackStartError {
            message,
            exit_code: 2,
        })?,
        Err(_) => 1 << 30,
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
            crate::vm::engine::verify::Prefix {
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
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
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

// ===== frontmatter (cargo script RFC 3424 syntax) =====

/// Parse the manifest embedded in a `---` fence. Returns (manifest, body with the manifest lines
/// blanked so line numbers are preserved). Script entry for cargoless::audit; the core stays
/// private.
pub(crate) fn parse_frontmatter_pub(src: &str) -> Option<(String, String)> {
    parse_frontmatter(src)
}

fn parse_frontmatter(src: &str) -> Option<(String, String)> {
    let mut lines = src.lines().enumerate().peekable();
    // Skip shebang
    if lines.peek().is_some_and(|(_, l)| l.starts_with("#!")) {
        lines.next();
    }
    // Skip blank lines
    while lines.peek().is_some_and(|(_, l)| l.trim().is_empty()) {
        lines.next();
    }
    let (_open_idx, open) = lines.next()?;
    let fence = open.trim_end();
    if !fence.starts_with("---") {
        return None;
    }
    // An infostring (such as `---cargo`) is allowed; its content is ignored.
    let mut manifest = String::new();
    let mut close_idx = None;
    for (i, l) in lines {
        if l.trim_end() == "---" {
            close_idx = Some(i);
            break;
        }
        manifest.push_str(l);
        manifest.push('\n');
    }
    let close_idx = close_idx?; // no closing fence => not frontmatter
    // Body = the original file with lines [0, close_idx] replaced by blanks (keeps diagnostic
    // line numbers).
    let body: String = src
        .lines()
        .enumerate()
        .map(|(i, l)| if i <= close_idx { "" } else { l })
        .collect::<Vec<_>>()
        .join("\n");
    Some((manifest, body))
}

/// Materialize a script as a cargo project in the cache and return the project directory.
fn materialize_script(script: &Path, manifest: &str, body: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};

    let abs = std::path::absolute(script).unwrap_or_else(|_| script.to_path_buf());
    let mut hasher = std::hash::DefaultHasher::new();
    abs.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let dir = crate::sysroot::cache_dir().join("scripts").join(&hash);
    std::fs::create_dir_all(dir.join("src")).expect("failed to create the script cache directory");
    std::fs::create_dir_all(dir.join(".cargo"))
        .expect("failed to create the script .cargo directory");

    let stem = script
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || name.chars().next().unwrap().is_ascii_digit() {
        name = format!("s{name}");
    }

    // The bin name carries a short path-hash suffix: in the shared target dir the final binary
    // lands in the fingerprint-free debug/<binname>, so scripts sharing a stem but not a path
    // (scratch variants under /tmp) do not overwrite each other. The package name keeps the stem
    // so recipes can find the script dir by name.
    let bin_name = format!("{name}-{}", &hash[..8]);
    let cargo_toml = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"{bin_name}\"\npath = \"src/main.rs\"\n\n{manifest}"
    );
    // Idempotent materialization: unchanged content is not rewritten. Stable mtime is a
    // precondition for both the L2 IR cache manifest and cargo fingerprints.
    write_if_changed(&dir.join("Cargo.toml"), &cargo_toml);
    write_if_changed(&dir.join("src/main.rs"), body);
    // The native differential build also uses the unified store: the shim build overrides this key
    // with an explicit --target-dir, while native cargo run uses the file config. The two families
    // get separate directories (their sysroots and rustflags differ, so fingerprints would not
    // collide anyway; separate directories only make purge semantics clearer).
    let native_target = crate::sysroot::cache_dir().join("target/native");
    write_if_changed(
        &dir.join(".cargo/config.toml"),
        &format!("[build]\ntarget-dir = \"{}\"\n", native_target.display()),
    );
    dir
}

fn write_if_changed(path: &Path, contents: &str) {
    if std::fs::read(path).is_ok_and(|old| old == contents.as_bytes()) {
        return;
    }
    std::fs::write(path, contents)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

#[cfg(test)]
mod tests {
    use rustc_errors::{DiagInner, Level};

    use super::{
        INTERNAL_CAPTURE_DIRECTORY_ARG, is_runner_warning_summary, take_internal_capture_directory,
    };

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
