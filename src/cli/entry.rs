//! `mirvm` user entry points: the `pack`, `cache`, `deps` and `run` subcommands.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, exit};

use crate::cargo_shim;

use super::driver::{pack_driver, parse_stack_size, run_driver, run_vm_engine};
use super::frontmatter::{materialize_script, parse_frontmatter};
use super::usage;

// ===== user entry points =====

/// `mirvm pack <target> [-o out.mirvm]`: cargo project (directory/Cargo.toml), frontmatter
/// script, or plain single file -> .mirvm package. Projects/frontmatter default to cargoless;
/// `MIRVM_DEPS=cargo` is passed into runner via MIRVM_PACK. Both paths force the full cold
/// route so the package is self-contained.
pub(super) fn pack_main(argv: impl Iterator<Item = String>) -> ExitCode {
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

    let deps_self = match crate::options::get().deps() {
        Ok(crate::options::DepsTrack::Own) => true,
        Ok(crate::options::DepsTrack::Cargo) => false,
        Err(message) => {
            eprintln!("{message}");
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
        // The pack route crosses a process boundary; see set_cargo_pack_env.
        set_cargo_pack_env(&out_abs);
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
        let dir = match materialize_script(&input_path, &manifest, &body) {
            Ok(dir) => dir,
            Err(message) => {
                eprintln!("mirvm: {}: {message}", input_path.display());
                exit(2);
            }
        };
        // The pack route crosses a process boundary; see set_cargo_pack_env.
        set_cargo_pack_env(&out_abs);
        cargo_shim::phase_cargo(&dir, &[], None, false);
    }

    // Plain single file: pack_driver directly (same args as run form 3)
    let sysroot = match crate::options::get().sysroot.clone() {
        Some(path) => path.display().to_string(),
        None => match crate::sysroot::ensure_sysroot() {
            Ok(p) => p.display().to_string(),
            Err(e) => {
                eprintln!("mirvm: fail to build sysroot: {e}");
                exit(1);
            }
        },
    };
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

/// Prepare the `cargo` phase of `mirvm pack`. The route crosses a process boundary (mirvm -> cargo
/// -> the mirvm wrapper), so the values are exported through the environment, and the two cache
/// bypasses force the full cold route that keeps the package self-contained.
fn set_cargo_pack_env(out: &Path) {
    crate::options::export_os_to_process("pack", out.as_os_str());
    crate::options::export_to_process("no_base_image", "1");
    crate::options::export_to_process("no_deps_image", "1");
}

/// `mirvm cache status|purge ...`: manage the local store ($HOME/.mirvm, relocatable via MIRVM_HOME).
pub(super) fn cache_main(args: impl Iterator<Item = String>) -> ExitCode {
    let root = crate::options::get().home.clone();
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
            "--data" => plan.data = true,
            _ => {
                eprintln!("mirvm cache: unknown argument `{a}`\n{}", usage());
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
            // No flag at all means the conservative default: drop stale generations only. Naming
            // any family means the user asked for that family, so do not also sweep.
            if !(plan.deps
                || plan.base
                || plan.ir
                || plan.scripts
                || plan.target
                || plan.all
                || plan.data)
            {
                plan.stale = true;
            }
            print!("{}", crate::cachectl::purge(&root, plan));
            ExitCode::SUCCESS
        }
        _ => {
            eprint!("{}", usage());
            ExitCode::from(2)
        }
    }
}

/// `mirvm deps audit <target...>`: target = project directory (containing Cargo.toml) or
/// frontmatter script; resolve per target and reconcile against the reference lock, exiting
/// non-zero if any target fails to resolve or the reconciliation mismatches.
pub(super) fn deps_main(args: impl Iterator<Item = String>) -> ExitCode {
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

pub(super) fn run_main(args: impl Iterator<Item = String>) -> ExitCode {
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
                // Export so the Cargo form (wrapper -> runner subprocess) sees the same value; the
                // command line outranks the environment, so record the source as well.
                crate::options::note_cli("stack_size");
                crate::options::export_to_process("stack_size", &v);
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
                // Same as --stack-size: export so the Cargo form takes effect through the runner.
                crate::options::note_cli("jit");
                crate::options::export_to_process("jit", &v);
            }
            _ if input.is_none() && !arg.starts_with('-') => input = Some(arg),
            _ => {
                crate::diagnostics::control(format_args!(
                    "mirvm: unknown argument `{arg}`\n{}",
                    crate::cli::usage()
                ));
                exit(2);
            }
        }
    }
    let Some(input) = input else {
        crate::diagnostics::control_raw(format_args!("{}", usage()));
        exit(2);
    };
    let input_path = PathBuf::from(&input);

    // Default = self zero-cargo own scheduling (cargoless::driver); =cargo uses the long-term
    // cargo three-phase compat track (user fallback + behavioral differential); any other value
    // is rejected loudly
    let deps_self = match crate::options::get().deps() {
        Ok(crate::options::DepsTrack::Own) => true,
        Ok(crate::options::DepsTrack::Cargo) => false,
        Err(message) => {
            crate::diagnostics::control(format_args!("{message}"));
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
        let dir = match materialize_script(&input_path, &manifest, &body) {
            Ok(dir) => dir,
            Err(message) => {
                crate::diagnostics::control(format_args!(
                    "mirvm: {}: {message}",
                    input_path.display()
                ));
                exit(2);
            }
        };
        cargo_shim::phase_cargo(&dir, &program_args, None, ignore_rust_version);
    }

    // Form 3: plain single file, zero-cargo fast path
    let sysroot = sysroot
        .or_else(|| {
            crate::options::get()
                .sysroot
                .as_deref()
                .map(|path| path.display().to_string())
        })
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
