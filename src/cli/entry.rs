//! `mirvm` user entry points: the `pack`, `cache`, `deps` and `run` subcommands.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cargo_shim;

use super::driver::{pack_driver, parse_stack_size, run_driver, run_vm_engine};
use super::frontmatter::{materialize_script, parse_frontmatter};
use super::usage;

// ===== user entry points =====

/// `mirvm pack <target> [-o out.mirvm]`: cargo project (directory/Cargo.toml), frontmatter
/// script, or plain single file -> .mirvm package. Projects/frontmatter default to cargoless;
/// `MIRVM_DEPS=cargo` is passed into runner via MIRVM_PACK. Both paths force the full cold
/// route so the package is self-contained.
pub(super) fn pack_main(
    argv: impl Iterator<Item = String>,
) -> Result<ExitCode, crate::error::Error> {
    use crate::diag::Component;
    let usage = || super::usage();
    let mut input = None;
    let mut out: Option<std::path::PathBuf> = None;
    let mut it = argv.peekable();
    while let Some(arg) = it.next() {
        if arg == "-o" || arg == "--output" {
            let Some(v) = it.next() else {
                return Err(crate::error::Error::usage_with(
                    Component::Pack,
                    "`-o` needs an argument",
                    usage(),
                ));
            };
            out = Some(std::path::PathBuf::from(v));
        } else if arg == "--json" {
            super::note_json_output();
        } else if input.is_none() && !arg.starts_with('-') {
            input = Some(arg);
        } else {
            return Err(crate::error::Error::usage_with(
                Component::Pack,
                format!("unknown argument `{arg}`"),
                usage(),
            ));
        }
    }
    let Some(input) = input else {
        return Err(crate::error::Error::usage_with(
            Component::Pack,
            "`pack` needs a target (cargo project directory / Cargo.toml / script)",
            usage(),
        ));
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

    let deps_self = match crate::options::get().deps()? {
        crate::options::DepsTrack::Own => true,
        crate::options::DepsTrack::Cargo => false,
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
            return Ok(crate::cargoless::driver::pack_project(dir, &out_abs));
        }
        // The pack route crosses a process boundary; see set_cargo_pack_env.
        set_cargo_pack_env(&out_abs);
        cargo_shim::phase_cargo(dir, &[], None, false);
    }
    let src = std::fs::read_to_string(&input_path).map_err(|e| {
        crate::error::Error::failure(Component::Pack, format!("cannot read {input}: {e}"))
    })?;
    if let Some((manifest, body)) = parse_frontmatter(&src) {
        if deps_self {
            return Ok(crate::cargoless::driver::pack_script(&input_path, &out_abs));
        }
        let dir = materialize_script(&input_path, &manifest, &body)?;
        // The pack route crosses a process boundary; see set_cargo_pack_env.
        set_cargo_pack_env(&out_abs);
        cargo_shim::phase_cargo(&dir, &[], None, false);
    }

    // Plain single file: pack_driver directly (same args as run form 3)
    let sysroot = match crate::options::get().sysroot.clone() {
        Some(path) => path.display().to_string(),
        None => crate::sysroot::ensure_sysroot()?.display().to_string(),
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
    Ok(pack_driver(rustc_args, program_argv, out_abs))
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
pub(super) fn cache_main(
    args: impl Iterator<Item = String>,
) -> Result<ExitCode, crate::error::Error> {
    use crate::diag::Component;
    let root = crate::options::get().home.clone();
    let mut plan = crate::store::report::Purge::default();
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
            "--json" => super::note_json_output(),
            _ => {
                return Err(crate::error::Error::usage_with(
                    Component::Cache,
                    format!("unknown argument `{a}`"),
                    usage(),
                ));
            }
        }
    }
    let json = crate::options::get().output_format()? == crate::options::OutputFormat::Json;
    match sub.as_deref() {
        Some("status") => {
            let report = crate::store::report::status(&root);
            print!("{}", if json { report.json() } else { report.text() });
            Ok(ExitCode::SUCCESS)
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
            let report = crate::store::report::purge(&root, plan);
            print!("{}", if json { report.json() } else { report.text() });
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(crate::error::Error::usage_with(
            Component::Cache,
            "`cache` needs `status` or `purge`",
            usage(),
        )),
    }
}

/// `mirvm deps audit <target...>`: target = project directory (containing Cargo.toml) or
/// frontmatter script; resolve per target and reconcile against the reference lock, exiting
/// non-zero if any target fails to resolve or the reconciliation mismatches.
pub(super) fn deps_main(
    args: impl Iterator<Item = String>,
) -> Result<ExitCode, crate::error::Error> {
    use crate::diag::Component;
    const HINT: &str = "usage: mirvm deps audit <project dir|script.rs>...\n";
    let mut sub = None;
    let mut targets: Vec<String> = Vec::new();
    let mut json_output = false;
    for a in args {
        match a.as_str() {
            "audit" if sub.is_none() => sub = Some(a),
            "--json" => {
                super::note_json_output();
                json_output = true;
            }
            // Anything after `audit` is a target, but a flag is not a path: an unrecognized one is a
            // rejected command line rather than a target that mysteriously fails to resolve.
            _ if sub.is_some() && !a.starts_with('-') => targets.push(a),
            _ => {
                return Err(crate::error::Error::usage_with(
                    Component::Deps,
                    format!("unknown argument `{a}`"),
                    HINT.to_string(),
                ));
            }
        }
    }
    if sub.is_none() || targets.is_empty() {
        return Err(crate::error::Error::usage_with(
            Component::Deps,
            "`deps` needs `audit` and at least one target",
            HINT.to_string(),
        ));
    }
    let mut summary = crate::cargoless::audit::AuditSummary {
        rows: Vec::new(),
        targets: targets.len(),
        failures: 0,
    };
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
        use crate::cargoless::audit::{AuditRow, Verdict};
        let row = match result {
            Ok(report) => {
                if report.mode == "skip" {
                    summary.rows.push(AuditRow {
                        verdict: Verdict::Skip,
                        target: report.name.clone(),
                        head: format!(
                            "{} (needs absent, not counted as failure per the same gate standard)",
                            report.name
                        ),
                        notes: Vec::new(),
                        detail: None,
                        mismatches: Vec::new(),
                    });
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
                let mut mismatches: Vec<String> = Vec::new();
                let mut notes: Vec<String> = Vec::new();
                if report.mode == "lock"
                    && let Some((lock_desc, found)) = &report.lock_check
                    && !found.is_empty()
                {
                    fail = Some(format!(
                        "reconciliation mismatch: {} entries vs {lock_desc}",
                        found.len()
                    ));
                    mismatches.extend(found.iter().take(5).cloned());
                }
                if let Some(acc) = &report.acceptance
                    && let Err(diag) = acc
                {
                    fail = Some(diag.clone());
                }
                if fail.is_none() {
                    if let Some((lock_desc, found)) = &report.lock_check {
                        if found.is_empty() {
                            notes.push(format!("reconciliation == {lock_desc}"));
                        } else if report.mode == "fresh" {
                            notes.push(format!(
                                "{} timestamp-drift entries in historical reference (informational, not a failure)",
                                found.len()
                            ));
                        }
                    }
                    if report.acceptance.is_some() {
                        notes.push("cargo --locked --offline accepted".to_string());
                    }
                }
                if fail.is_some() {
                    summary.failures += 1;
                }
                AuditRow {
                    verdict: if fail.is_some() {
                        Verdict::Fail
                    } else {
                        Verdict::Ok
                    },
                    target: report.name,
                    head,
                    notes,
                    detail: fail,
                    mismatches,
                }
            }
            Err(e) => {
                // P5 loud rejections are an upfront-stated boundary and not ordinary resolution failures.
                if e.contains("P5") {
                    AuditRow {
                        verdict: Verdict::Boundary,
                        target: t.clone(),
                        head: String::new(),
                        notes: Vec::new(),
                        detail: Some(e),
                        mismatches: Vec::new(),
                    }
                } else {
                    summary.failures += 1;
                    AuditRow {
                        verdict: Verdict::Fail,
                        target: t.clone(),
                        head: String::new(),
                        notes: Vec::new(),
                        detail: Some(e),
                        mismatches: Vec::new(),
                    }
                }
            }
        };
        summary.rows.push(row);
    }
    print!(
        "{}",
        if json_output {
            summary.json()
        } else {
            summary.text()
        }
    );
    if summary.failures == 0 {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(crate::diag::exit::FAILURE))
    }
}

pub(super) fn run_main(
    args: impl Iterator<Item = String>,
) -> Result<ExitCode, crate::error::Error> {
    use crate::diag::Component;
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
        let mut next = |name: &str| -> Result<String, crate::error::Error> {
            args.next().ok_or_else(|| {
                crate::error::Error::usage(Component::Run, format!("{name} needs argument(s)"))
            })
        };
        match arg.as_str() {
            "--" => {
                program_args.extend(args.by_ref());
                break;
            }
            "--dump-mir" => dump_mir = true,
            "--json" => super::note_json_output(),
            "--edition" => edition = next("--edition")?,
            "--sysroot" => sysroot = Some(next("--sysroot")?),
            // Backward compat for old gate scripts: --engine vm is the only engine, just consume it
            "--engine" => {
                let e = next("--engine")?;
                if e != "vm" {
                    return Err(crate::error::Error::usage(
                        Component::Run,
                        format!(
                            "engine `{e}` no longer exists (tier-0 removed; the only engine is vm)"
                        ),
                    ));
                }
            }
            "--vm-call" => vm_call = Some(next("--vm-call")?),
            "--vm-stats" => vm_stats = true,
            // cargo run --bin semantics (project form only; no meaning for script/single-file)
            "--bin" => bin_sel = Some(next("--bin")?),
            "--ignore-rust-version" => ignore_rust_version = true,
            "--stack-size" => {
                let v = next("--stack-size")?;
                parse_stack_size(&v)?;
                // Export so the Cargo form (wrapper -> runner subprocess) sees the same value; the
                // command line outranks the environment, so record the source as well.
                crate::options::note_cli("stack_size");
                crate::options::export_to_process("stack_size", &v);
            }
            "--jit" => {
                let v = next("--jit")?;
                if v != "on" && v != "off" {
                    // TODO: tiered JIT?
                    return Err(crate::error::Error::usage(
                        Component::Run,
                        format!("`--jit` only accepts on|off (got `{v}`)"),
                    ));
                }
                // Same as --stack-size: export so the Cargo form takes effect through the runner.
                crate::options::note_cli("jit");
                crate::options::export_to_process("jit", &v);
            }
            _ if input.is_none() && !arg.starts_with('-') => input = Some(arg),
            _ => {
                return Err(crate::error::Error::usage_with(
                    Component::Run,
                    format!("unknown argument `{arg}`"),
                    crate::cli::usage(),
                ));
            }
        }
    }
    let Some(input) = input else {
        return Err(crate::error::Error::usage_with(
            Component::Run,
            "`run` needs an input",
            crate::cli::usage(),
        ));
    };
    let input_path = PathBuf::from(&input);

    // Default = self zero-cargo own scheduling (cargoless::driver); =cargo uses the long-term
    // cargo three-phase compat track (user fallback + behavioral differential); any other value
    // is rejected loudly
    let deps_self = match crate::options::get().deps()? {
        crate::options::DepsTrack::Own => true,
        crate::options::DepsTrack::Cargo => false,
    };

    // Form 1: cargo project (directory or Cargo.toml)
    if input_path.is_dir() {
        if deps_self {
            return Ok(crate::cargoless::driver::run_project(
                &input_path,
                &program_args,
                bin_sel.as_deref(),
                ignore_rust_version,
            ));
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
            return Ok(crate::cargoless::driver::run_project(
                dir,
                &program_args,
                bin_sel.as_deref(),
                ignore_rust_version,
            ));
        }
        cargo_shim::phase_cargo(dir, &program_args, bin_sel.as_deref(), ignore_rust_version);
    }
    if let Some(b) = &bin_sel {
        // Script/single-file/package forms have no --bin concept (same as cargo script) — reject loudly, do not silently swallow
        return Err(crate::error::Error::usage(
            Component::Run,
            format!("`--bin {b}` is only valid for the cargo project form (directory/Cargo.toml)"),
        ));
    }

    // Sniff for a .mirvm package (before the text read -- a package is binary)
    if crate::pack::is_package(&input_path) {
        let (module, mut instance) = crate::pack::load_package(&input_path)
            .and_then(|package| package.instantiate())
            .map_err(|reason| {
                crate::error::Error::software(
                    Component::Run,
                    format!("cannot load {}: {reason}", input_path.display()),
                )
            })?;
        // warm second half mirrors run_driver hot path (empty image stack: asm recipes idempotently rematerialized)
        instance.asm_stub_addrs = crate::lower::asm::materialize(&module.asm_sites);
        let mut program_argv = vec![input];
        program_argv.extend(program_args);
        let code = run_vm_engine(
            module,
            instance,
            &program_argv,
            vm_call.as_deref(),
            vm_stats,
            true,
        );
        // The guest's own status: not mirvm's to classify.
        return Ok(ExitCode::from(code as u8));
    }

    let src = std::fs::read_to_string(&input_path).map_err(|e| {
        crate::error::Error::failure(Component::Run, format!("cannot read {input}: {e}"))
    })?;

    // Form 2: single-file script with frontmatter dependency declaration -> materialize into cargo project
    if let Some((manifest, body)) = parse_frontmatter(&src) {
        if deps_self {
            return Ok(crate::cargoless::driver::run_script(
                &input_path,
                &program_args,
                ignore_rust_version,
            ));
        }
        let dir = materialize_script(&input_path, &manifest, &body)?;
        cargo_shim::phase_cargo(&dir, &program_args, None, ignore_rust_version);
    }

    // Form 3: plain single file, zero-cargo fast path
    let sysroot = match sysroot.or_else(|| {
        crate::options::get()
            .sysroot
            .as_deref()
            .map(|path| path.display().to_string())
    }) {
        Some(sysroot) => sysroot,
        None => crate::sysroot::ensure_sysroot()?.display().to_string(),
    };
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
    Ok(run_driver(
        rustc_args,
        program_argv,
        dump_mir,
        vm_call,
        vm_stats,
        false,
        None,
    ))
}
