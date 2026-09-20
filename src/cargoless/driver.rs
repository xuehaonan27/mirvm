//! `cargoless/driver.rs` — the cargo-less counterpart of `cargo_shim`'s cargo +
//! RUSTC_WRAPPER + runner protocol: `mirvm run`, `mirvm test`, `mirvm pack` and the
//! `__cless-run-root` / `__cless-doctest-builder` child entry points.
//!
//! ```text
//! resolve → links mutex check → unit-level Kahn ready-queue parallel scheduling
//! (N workers, MIRVM_CLESS_JOBS override, default available_parallelism;
//! =1 reproduces the serial topological order bit-for-bit — differential-debugging anchor.
//! A unit is ready once all its deps finish; stages inside a unit remain serial):
//!   build.rs full lifecycle: host really compiles the build script
//!     (skip on fp hit) → rerun decision (buildrs::should_rerun, same semantics as cargo —
//!     stored in build/<pkg>-<fp>/{output.txt,rerun.txt}; when skipped, reparse output.txt
//!     to replay BuildOutput, same warning gate) → execute with cargo-compatible env →
//!     instruction parsing → BuildOutput returned to main thread into completion table (this unit recorded in re_ran,
//!     used for links propagation decisions)
//!   host set (proc-macro closure ∪ build-deps closure) → spawn real rustc with real codegen
//!   target set → spawn `__cless-dep` child (cli::run_dep_compiler:
//!   in-process rustc_driver + global_asm extraction)
//!   (both sides consume this unit's BuildOutput corrections: cfg/check-cfg/link flags enter argv,
//!   OUT_DIR/rustc-env enter child env — proc-macro2's build.rs cfg enters its host
//!   compilation, the key to unlocking serde_derive-like full chains)
//! → after all units converge back to main thread: root build.rs same lifecycle → bin uses existing
//!   MirvmCallbacks session (OUT_DIR/rustc-env/cfg corrections also enter bin session)
//! ```
//!
//! Propagation rules (-l only enters this package, -L enters transitive dependents, metadata only goes to direct dependents'
//! build script, no automatic DEP_*_ROOT, no automatic check-cfg patch) are documented in the
//! buildrs.rs file header.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::buildrs::{self, BuildOutput};
use super::lockfile::Lockfile;
use super::manifest::PackageManifest;
use super::registry::Registry;
use super::resolve::resolve;
use super::schedule::{self, Layout};

mod build;
mod test_driver;

use build::run_build_lifecycle_root;
pub use build::{UnitTables, compile_plan};
pub use test_driver::test_project;

/// `mirvm run <dir|Cargo.toml> [--bin <name>]` (MIRVM_DEPS=self).
/// `bin_sel` is the bin name selected by `--bin` (`cargo run --bin` semantics).
pub fn run_project(
    dir: &Path,
    program_args: &[String],
    bin_sel: Option<&str>,
    ignore_rust_version: bool,
) -> ExitCode {
    let mut manifest = match PackageManifest::read_dir(dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mirvm: failed to read project {}: {e}", dir.display());
            std::process::exit(1);
        }
    };
    manifest.ignore_rust_version = ignore_rust_version;
    drive(&manifest, program_args, bin_sel, None)
}

/// `mirvm pack <dir|Cargo.toml>` default self path. Deps, build.rs,
/// proc-macro and root package compilation are fully shared with `run_project`; only the final rustc session swaps
/// "execution" for writing a self-contained package.
pub fn pack_project(dir: &Path, out: &Path) -> ExitCode {
    let manifest = match PackageManifest::read_dir(dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("mirvm: failed to read project {}: {error}", dir.display());
            std::process::exit(1);
        }
    };
    drive(&manifest, &[], None, Some(out))
}

/// Independent execution recipe for a root test target. Cargo spawns one process per test artifact; the self
/// path also seals rustc args and compile-time env into the recipe, then executes via the `__cless-run-root` child.
#[derive(serde::Serialize, serde::Deserialize)]
struct RootRunRecipe {
    rustc_args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    argv0: String,
}

fn write_lock_atomic(path: &Path, lock: &Lockfile) -> Result<(), String> {
    let tmp = path.with_extension(format!("lock.mirvm-{}", std::process::id()));
    std::fs::write(&tmp, lock.serialize())
        .map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("publish {} failed: {e}", path.display()))
}

fn launcher_recipe_path(launcher: &Path) -> PathBuf {
    let mut path = launcher.as_os_str().to_os_string();
    path.push(".mirvm-recipe.json");
    PathBuf::from(path)
}

pub fn is_doctest_builder(argv0: &Path) -> bool {
    argv0
        .file_name()
        .is_some_and(|name| name == "mirvm-doctest-builder")
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1).cloned())
        .or_else(|| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(&format!("{flag}=")).map(str::to_owned))
        })
}

fn doctest_crate_type(args: &[String]) -> Option<String> {
    arg_value(args, "--crate-type")
}

fn remove_output_arg(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        if args[index] == "-o" {
            index += 2;
            continue;
        }
        if args[index].starts_with("-o=") {
            index += 1;
            continue;
        }
        out.push(args[index].clone());
        index += 1;
    }
    out
}

/// rustdoc `--test-builder` entry. The lib bundle uses real rustc to produce a MIR-bearing
/// metadata-only rlib; bin first does real rustc type checking, then publishes the target path as a
/// mirvm launcher. rustdoc can therefore still decide compile_fail itself.
pub fn run_doctest_builder(argv: impl Iterator<Item = String>) -> ExitCode {
    let args: Vec<String> = argv.collect();
    let crate_type = doctest_crate_type(&args).unwrap_or_default();
    let rustc = PathBuf::from(crate::options::build::DEFAULT_SYSROOT).join("bin/rustc");
    if crate_type == "lib" {
        let status = std::process::Command::new(&rustc)
            .args(&args)
            .arg("--emit=dep-info,metadata,link")
            .arg("-Zalways-encode-mir")
            .arg("-Zno-codegen")
            .status();
        return match status {
            Ok(status) if status.success() => ExitCode::SUCCESS,
            Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(error) => {
                eprintln!("mirvm doctest builder: failed to start rustc: {error}");
                ExitCode::from(1)
            }
        };
    }
    if crate_type != "bin" {
        eprintln!("mirvm doctest builder: unsupported crate type `{crate_type}`");
        return ExitCode::from(1);
    }
    let Some(output) = arg_value(&args, "-o").map(PathBuf::from) else {
        eprintln!("mirvm doctest builder: bin compilation is missing -o");
        return ExitCode::from(1);
    };
    let check_dir = output
        .parent()
        .unwrap_or(Path::new("."))
        .join("mirvm-check");
    if let Err(error) = std::fs::create_dir_all(&check_dir) {
        eprintln!(
            "mirvm doctest builder: failed to create check directory {}: {error}",
            check_dir.display()
        );
        return ExitCode::from(1);
    }
    let check_args = remove_output_arg(&args);
    let status = std::process::Command::new(&rustc)
        .args(&check_args)
        .arg("--emit=metadata")
        .arg("--out-dir")
        .arg(&check_dir)
        .arg("-Zalways-encode-mir")
        .arg("-Zno-codegen")
        .status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => return ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("mirvm doctest builder: failed to start rustc check: {error}");
            return ExitCode::from(1);
        }
    }

    let cwd = crate::options::protocol::doctest_run_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let recipe = RootRunRecipe {
        rustc_args: std::iter::once("mirvm-doctest-rustc".to_string())
            .chain(args.iter().cloned())
            .collect(),
        env: Vec::new(),
        cwd,
        argv0: output.display().to_string(),
    };
    let recipe_path = launcher_recipe_path(&output);
    let bytes = match serde_json::to_vec(&recipe) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("mirvm doctest builder: recipe serialization failed: {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = crate::store::publish_bytes(&recipe_path, &bytes) {
        eprintln!(
            "mirvm doctest builder: failed to publish recipe {}: {error}",
            recipe_path.display()
        );
        return ExitCode::from(1);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        match std::fs::remove_file(&output) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                eprintln!(
                    "mirvm doctest builder: failed to replace output {}: {error}",
                    output.display()
                );
                return ExitCode::from(1);
            }
        }
        let self_exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("mirvm doctest builder: current_exe failed: {error}");
                return ExitCode::from(1);
            }
        };
        if let Err(error) = symlink(self_exe, &output) {
            eprintln!(
                "mirvm doctest builder: failed to create launcher {}: {error}",
                output.display()
            );
            return ExitCode::from(1);
        }
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        eprintln!("mirvm doctest builder: runner currently only supports Unix hosts");
        ExitCode::from(1)
    }
}

/// CLI startup uses argv[0] in the earliest phase to recognize `CARGO_BIN_EXE_*` launchers.
pub fn root_launcher_recipe(argv0: &Path) -> Option<PathBuf> {
    let recipe = launcher_recipe_path(argv0);
    recipe.is_file().then_some(recipe)
}

fn append_capture_directory_arg(command: &mut std::process::Command, directory: Option<&Path>) {
    if let Some(directory) = directory {
        command
            .arg(crate::cli::INTERNAL_CAPTURE_DIRECTORY_ARG)
            .arg(directory);
    }
}

/// `__cless-run-root <recipe> [program args]` child entry point.
pub fn run_root_recipe(argv: impl Iterator<Item = String>) -> ExitCode {
    let mut argv = argv.peekable();
    match crate::cli::take_internal_capture_directory(&mut argv) {
        Err(()) => {
            eprintln!("mirvm capture: __cless-run-root missing capture directory");
            return ExitCode::from(2);
        }
        Ok(Some(directory)) => {
            if crate::cli::set_forwarded_capture_directory(directory).is_err() {
                eprintln!("mirvm capture: __cless-run-root received duplicate capture request");
                return ExitCode::from(2);
            }
        }
        Ok(None) => {}
    }
    let _diagnostic_router = match crate::cli::diagnostics::DiagnosticRouter::start(
        crate::cli::capture_directory(),
        true,
    ) {
        Ok(router) => router,
        Err(error) => {
            crate::cli::diagnostics::control(format_args!(
                "mirvm capture: cannot start diagnostics stream: {error}"
            ));
            return ExitCode::from(70);
        }
    };
    let Some(path) = argv.next() else {
        crate::cli::diagnostics::control(format_args!(
            "mirvm: __cless-run-root missing recipe path"
        ));
        return ExitCode::from(2);
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            crate::cli::diagnostics::control(format_args!(
                "mirvm: failed to read test recipe {path}: {e}"
            ));
            return ExitCode::from(1);
        }
    };
    let recipe: RootRunRecipe = match serde_json::from_slice(&data) {
        Ok(r) => r,
        Err(e) => {
            crate::cli::diagnostics::control(format_args!(
                "mirvm: test recipe {path} corrupted: {e}"
            ));
            return ExitCode::from(1);
        }
    };
    if let Err(e) = std::env::set_current_dir(&recipe.cwd) {
        crate::cli::diagnostics::control(format_args!(
            "mirvm: test working directory {} is not accessible: {e}",
            recipe.cwd.display()
        ));
        return ExitCode::from(1);
    }
    for (key, value) in recipe.env {
        // SAFETY: independent child startup phase; rustc/guest threads have not been created.
        unsafe { std::env::set_var(key, value) };
    }
    let mut program_argv = vec![recipe.argv0];
    program_argv.extend(argv);
    crate::cli::run_driver(
        recipe.rustc_args,
        program_argv,
        false,
        None,
        false,
        true,
        None,
    )
}

/// `mirvm run <frontmatter script>` (MIRVM_DEPS=self): the body is materialized into the script cache directory
/// (the same key cli::script_cache_dir hands the cargo track), and the pseudo-package manifest goes through the same drive.
pub fn run_script(file: &Path, program_args: &[String], ignore_rust_version: bool) -> ExitCode {
    let mut manifest = script_manifest(file);
    manifest.ignore_rust_version = ignore_rust_version;
    drive(&manifest, program_args, None, None)
}

/// `mirvm pack <frontmatter script>` default self path.
pub fn pack_script(file: &Path, out: &Path) -> ExitCode {
    let manifest = script_manifest(file);
    drive(&manifest, &[], None, Some(out))
}

fn script_manifest(file: &Path) -> PackageManifest {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mirvm: failed to read script {}: {e}", file.display());
            std::process::exit(1);
        }
    };
    let Some((manifest_text, body)) = crate::cli::parse_frontmatter_pub(&text) else {
        // The router (cli.rs run_main) only enters here when frontmatter is present; a bare single
        // file is the single-file fast path and never reaches this.
        eprintln!(
            "mirvm: {} has no frontmatter (internal routing error)",
            file.display()
        );
        std::process::exit(2);
    };
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    let cache = crate::cli::script_cache_dir(file);
    let src_dir = cache.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        eprintln!(
            "mirvm: failed to create script cache directory {}: {e}",
            src_dir.display()
        );
        std::process::exit(1);
    }
    // Layout is isomorphic to cargo-leg materialized projects (cli.rs materialize_script: body in
    // <cache>/src/main.rs) — after file!()/panic Location remap it is byte-identical to the cargo leg's
    // "src/main.rs" (redb_kv/gix_pure proven); root remains = <cache>
    // (CARGO_MANIFEST_DIR matches cargo leg).
    let main_rs = src_dir.join("main.rs");
    // write-if-changed: do not rewrite if content is identical — stable mtime is a shared prerequisite for cargo fingerprint/L2
    // (same discipline as cli.rs materialize_script)
    if std::fs::read(&main_rs)
        .ok()
        .is_none_or(|old| old != body.as_bytes())
        && let Err(e) = std::fs::write(&main_rs, &body)
    {
        eprintln!("mirvm: write {} failed: {e}", main_rs.display());
        std::process::exit(1);
    }
    match PackageManifest::from_frontmatter_at(stem, &manifest_text, &cache, &main_rs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "mirvm: failed to parse frontmatter of {}: {e}",
                file.display()
            );
            std::process::exit(1);
        }
    }
}

fn drive(
    manifest: &PackageManifest,
    program_args: &[String],
    bin_sel: Option<&str>,
    pack_out: Option<&Path>,
) -> ExitCode {
    // 1. resolver: use the lock when it is present, otherwise a fresh pubgrub solve
    let mut registry = match Registry::open_for(&manifest.lock_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mirvm: failed to open registry: {e}");
            std::process::exit(1);
        }
    };
    let plan = match resolve(manifest, &mut registry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mirvm: dependency resolution failed: {e}");
            std::process::exit(1);
        }
    };
    let lock_path = manifest.lock_root.join("Cargo.lock");
    if !lock_path.is_file()
        && let Err(error) = write_lock_atomic(&lock_path, &plan.lock)
    {
        eprintln!("mirvm: write {} failed: {error}", lock_path.display());
        std::process::exit(1);
    }

    // 2. links mutex (same as cargo: at most one package per links value; root package also checked)
    if let Err(e) =
        buildrs::check_links_unique(Some((&manifest.name, manifest.links.as_deref())), &plan)
    {
        eprintln!("mirvm: {e}");
        std::process::exit(1);
    }

    // 3. sysroot: the option takes priority, otherwise self-built (same measure as the CLI run path)
    let sysroot = match crate::options::get().sysroot.clone() {
        Some(p) => p,
        None => match crate::sysroot::ensure_sysroot() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mirvm: failed to build sysroot: {e}");
                std::process::exit(1);
            }
        },
    };

    // 4. fingerprint + compile segment (compiled by compile_plan — drive and
    // sysroot self-build share the same pipeline; sources of this segment's stamp/sysroot/rustflags
    // inputs on the drive side are annotated in the following segments)
    let layout = Layout::new();
    // sysroot stamp enters fingerprint (sysroot generation change ⇒ full rebuild); after ensure there must be a value,
    // fallback literal on absence is non-fatal (only makes fp coarser, no new error path)
    let stamp = crate::sysroot::current_stamp_value()
        .unwrap_or_else(|| "sysroot-stamp-unknown".to_string());
    // rustflags parsed once and threaded through: only enter target-side args
    // (appended at end of dep/bin), fingerprint consumed uniformly for whole unit (host side following stale is harmless;
    // parsing/priority/boundaries see rustflags.rs header)
    let rustflags = match super::rustflags::from_env_and_disk(&manifest.root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mirvm: failed to parse rustflags: {e}");
            std::process::exit(1);
        }
    };
    let compiled = match compile_plan(
        &plan,
        &layout,
        &manifest.profile,
        &rustflags,
        &sysroot,
        &stamp,
        manifest.has_build_script,
        manifest.targets.iter().any(|target| target.proc_macro),
        false,
    ) {
        Ok(t) => t,
        // first compile error (worker returns original text) — exit after loudly naming, same shape as serial
        Err(e) => {
            eprintln!("mirvm: {e}");
            std::process::exit(1);
        }
    };
    let UnitTables { outputs, re_ran } = compiled.tables;
    let fps = compiled.fps;

    // 5b onward root lib/bin session also needs own exe (__cless-dep channel); compile_plan segment's
    // self_exe is inside compile_plan, here we just take it
    let self_exe = std::env::current_exe().expect("current_exe failed");

    // 5. root build.rs same lifecycle (root is not a unit: edge table uses plan.root_deps,
    // fp computed separately; OUT_DIR/rustc-env/cfg corrections enter bin session)
    // root lib target: when [lib]+[[bin]] dual
    // targets, bin implicitly depends on the same-name lib — cargo first compiles root lib into target rlib
    // then lets bin --extern it. fp shares root_fingerprint with root build.rs (same package same recipe),
    // so fp computation condition = has_build_script || has lib target.
    let root_lib = manifest
        .targets
        .iter()
        .find(|t| t.is_lib())
        .map(|t| (t.name.clone(), t.path.clone(), t.proc_macro));
    let mut root_fp: Option<String> = None;
    let mut root_bo: Option<BuildOutput> = None;
    if manifest.has_build_script || root_lib.is_some() {
        let fp = match schedule::root_fingerprint(
            manifest,
            &plan,
            &fps,
            &manifest.profile,
            &stamp,
            &rustflags,
        ) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("mirvm: root package fingerprint computation failed: {e}");
                std::process::exit(1);
            }
        };
        root_fp = Some(fp);
    }
    if manifest.has_build_script {
        let fp = root_fp.clone().expect("computed in previous step");
        // root's ran has no consumer (root has no downstream, links cannot reach it), only goes to observation line
        let (bo, _ran) = run_build_lifecycle_root(
            manifest, &plan, &fps, &layout, &fp, &outputs, &re_ran, false,
        );
        root_bo = Some(bo);
    }

    // 5b. root lib target compilation (__cless-dep channel, skip on fp hit; root build.rs
    // bo corrections and OUT_DIR/rustc-env injected same way — must be after root build.rs)
    if let Some((lib_name, lib_path, lib_pm)) = &root_lib {
        if *lib_pm {
            // proc-macro root lib + bin combination (cargo compiles dylib then --extern) not wired in v1
            // — loudly reject and record, do not silently miscompile
            eprintln!(
                "mirvm: root package {} is a proc-macro lib and has a bin, combination not wired (P5 boundary)",
                manifest.name
            );
            std::process::exit(1);
        }
        let fp = root_fp
            .as_ref()
            .expect("root_lib present means fp must already be computed");
        let stem = format!("lib{}-{}", lib_name.replace('-', "_"), fp);
        let hit = layout.deps.join(format!("{stem}.rmeta")).is_file()
            && layout.deps.join(format!("{stem}.rlib")).is_file();
        if !hit {
            let searches = buildrs::aggregate_link_searches(&plan, &plan.root_deps, &outputs);
            let args = schedule::root_lib_rustc_args(
                manifest,
                &plan,
                &fps,
                &sysroot,
                &layout,
                lib_name,
                lib_path,
                root_bo.as_ref(),
                &searches,
                &rustflags,
                fp,
            );
            let mut cmd = std::process::Command::new(&self_exe);
            cmd.arg("__cless-dep").args(&args[1..]);
            // root package compile-time env (full CARGO_PKG_* set + two manifest entries, same as cargo)
            cmd.envs(manifest.pkg_env.iter());
            cmd.env("CARGO_CRATE_NAME", lib_name.replace('-', "_"));
            cmd.env("CARGO_MANIFEST_DIR", &manifest.root);
            cmd.env(
                "CARGO_MANIFEST_PATH",
                manifest.root.join("Cargo.toml").display().to_string(),
            );
            if let Some(bo) = &root_bo {
                cmd.env("OUT_DIR", layout.build_dir(&manifest.name, fp).join("out"));
                for (k, v) in &bo.envs {
                    cmd.env(k, v);
                }
            }
            let status = match cmd.status() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "mirvm: lib compilation child process failed to start (root package {} {}): {e}",
                        manifest.name, manifest.version
                    );
                    std::process::exit(1);
                }
            };
            if !status.success() {
                eprintln!(
                    "mirvm: lib compilation failed: root package {} {}",
                    manifest.name, manifest.version
                );
                std::process::exit(1);
            }
        }
    }

    // 6. bin: root crate goes through existing MirvmCallbacks session (stops at after_analysis, zero artifacts)
    let (bin_name, bin_path) = match manifest.runnable_bin_opt(bin_sel) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("mirvm: {e}");
            std::process::exit(1);
        }
    };
    // SAFETY: single-threaded startup phase (rustc session not started, engine not running), env writes have no concurrent readers.
    // bin session is in-process (same env-replay pattern as runner_main).
    unsafe {
        for (k, v) in &manifest.pkg_env {
            std::env::set_var(k, v);
        }
        std::env::set_var("CARGO_CRATE_NAME", bin_name.replace('-', "_"));
        std::env::set_var("CARGO_BIN_NAME", bin_name);
        std::env::set_var("CARGO_MANIFEST_DIR", &manifest.root);
        std::env::set_var("CARGO_MANIFEST_PATH", manifest.root.join("Cargo.toml"));
        if let (Some(bo), Some(fp)) = (&root_bo, &root_fp) {
            // root build.rs rustc-env + OUT_DIR enter bin session (readable by env!, same as cargo)
            std::env::set_var("OUT_DIR", layout.build_dir(&manifest.name, fp).join("out"));
            for (k, v) in &bo.envs {
                std::env::set_var(k, v);
            }
        }
    }
    let root_searches = buildrs::aggregate_link_searches(&plan, &plan.root_deps, &outputs);
    let root_lib_ref = root_lib.as_ref().map(|(n, _, _)| {
        (
            n.as_str(),
            root_fp
                .as_deref()
                .expect("root_lib present means fp must already be computed"),
        )
    });
    let args = schedule::bin_rustc_args(
        manifest,
        &plan,
        &fps,
        &sysroot,
        &layout,
        bin_name,
        bin_path,
        root_bo.as_ref(),
        &root_searches,
        &rustflags,
        root_lib_ref,
    );
    // argv0 = synthesized artifact path (cargo run argv0 semantics = final binary path; this session
    // produces zero artifacts, use deps/<bin> as placeholder — guest only sees argv string, does not read file)
    let mut program_argv = vec![layout.deps.join(bin_name).display().to_string()];
    program_argv.extend(program_args.iter().cloned());
    // no chdir throughout: guest cwd = caller cwd, consistent with cargo run semantics
    if let Some(out) = pack_out {
        crate::cli::pack_driver(args, program_argv, out.to_path_buf())
    } else {
        crate::cli::run_driver(args, program_argv, false, None, false, true, None)
    }
}

#[cfg(test)]
mod capture_argv_tests {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::process::Command;

    use super::append_capture_directory_arg;

    #[test]
    fn root_capture_argument_precedes_the_recipe_and_guest_arguments() {
        let mut command = Command::new("/tmp/mirvm");
        command.arg("__cless-run-root");
        append_capture_directory_arg(&mut command, Some(Path::new("/tmp/capture")));
        command.arg("/tmp/recipe").arg("guest-argument");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("__cless-run-root"),
                OsStr::new("--mirvm-capture-directory"),
                OsStr::new("/tmp/capture"),
                OsStr::new("/tmp/recipe"),
                OsStr::new("guest-argument"),
            ]
        );
    }
}
