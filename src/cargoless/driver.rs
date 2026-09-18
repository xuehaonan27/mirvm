//! `cargoless/driver.rs` — the cargo-less new path for `mirvm run` (D15 P2 cuts ①/②/③, design §3.6/§5 P2), replacing the three phases of cargo_shim::phase_cargo (cargo run +
//! RUSTC_WRAPPER + runner protocol):
//!
//! ```text
//! resolve (P1 resolver) → links mutex check → unit-level Kahn ready-queue parallel scheduling
//! (P3 cut ⑤c: N workers, MIRVM_CLESS_JOBS override, default available_parallelism;
//! =1 matches the old serial topo order bit-for-bit — differential debugging anchor. A unit is ready once all its deps finish,
//! stages inside a unit remain serial):
//!   build.rs full lifecycle (cut ③ + P3 cut ⑤b fine-grained incrementality): host really compiles the build script
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
//! build script, no automatic DEP_*_ROOT, no automatic check-cfg patch) are all cut ③ empirical
//! conclusions; details are in the buildrs.rs file header.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::buildrs::{self, BuildOutput};
use super::lockfile::{LockedDep, Lockfile};
use super::manifest::{DepKind, DepSource, PackageManifest, Target, TargetKind};
use super::registry::Registry;
use super::resolve::{
    FeatureOverrides, ResolvePlan, ResolvePurpose, Unit, UnitClass, resolve, resolve_for_known,
    resolve_for_known_with_features,
};
use super::schedule::{self, Layout};
use super::workspace::WorkspaceManifest;

/// `mirvm run <dir|Cargo.toml> [--bin <name>]` (MIRVM_DEPS=self).
/// bin_sel = the bin name selected by --bin (D15 P4 cut ⑥b, cargo run --bin semantics).
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

/// `mirvm test` self path. The workspace is first reduced to a complete member list, then multi-package
/// features and compile keys are unified; tests only start after all selected packages are prepared.
pub fn test_project(dir: &Path, cargo_args: &[String], harness_args: &[String]) -> ExitCode {
    let request = match TestRequest::parse(cargo_args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mirvm test: {e}");
            return ExitCode::from(2);
        }
    };
    if request.doc && request.no_run {
        eprintln!("error: can't skip running doc tests with --no-run");
        return ExitCode::from(101);
    }
    if request.doc && request.has_non_doc_target_selection() {
        eprintln!("error: can't mix --doc with other target selecting options");
        return ExitCode::from(101);
    }
    let mut workspace = match WorkspaceManifest::read(dir) {
        Ok(workspace) => workspace,
        Err(e) => {
            eprintln!("mirvm: failed to read project {}: {e}", dir.display());
            return ExitCode::from(1);
        }
    };
    if request.offline {
        // SAFETY: CLI startup phase; worker/rustc/guest threads have not started yet.
        unsafe { std::env::set_var("MIRVM_OFFLINE", "1") };
    }
    if request.ignore_rust_version {
        for member in &mut workspace.members {
            member.ignore_rust_version = true;
        }
    }
    if request.locked && !workspace.root.join("Cargo.lock").is_file() {
        eprintln!("mirvm test: --locked requires an existing Cargo.lock at the workspace root");
        return ExitCode::from(1);
    }
    let mut manifests = match request.select_packages(&workspace) {
        Ok(manifests) => manifests,
        Err(e) => {
            eprintln!("mirvm test: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = request.restrict_named_targets(&mut manifests) {
        eprintln!("mirvm test: {e}");
        return ExitCode::from(1);
    }
    if let Err(e) = request.apply_features(&mut manifests) {
        eprintln!("mirvm test: {e}");
        return ExitCode::from(1);
    }
    sort_packages_dependency_first(&mut manifests);
    let mut known = workspace.members.clone();
    for selected in &manifests {
        if let Some(member) = known.iter_mut().find(|member| member.root == selected.root) {
            *member = selected.clone();
        }
    }
    if workspace.members.len() > 1 && !workspace.root.join("Cargo.lock").is_file() {
        let mut lock_workspace = workspace.clone();
        lock_workspace.members = known.clone();
        if let Err(e) = generate_workspace_lock(&lock_workspace) {
            eprintln!("mirvm test: failed to generate workspace Cargo.lock: {e}");
            return ExitCode::from(1);
        }
    }
    let plans = match resolve_workspace_plans(&manifests, &known) {
        Ok(plans) => plans,
        Err(e) => {
            eprintln!("mirvm: dependency resolution failed: {e}");
            return ExitCode::from(1);
        }
    };
    let mut prepared = Vec::new();
    for (manifest, plan) in manifests.into_iter().zip(plans) {
        match prepare_test_package(manifest, plan, &request) {
            Ok(package) => prepared.push(package),
            Err(code) => return code,
        }
    }
    if request.no_run {
        return ExitCode::SUCCESS;
    }
    let mut test_args = Vec::new();
    if request.quiet {
        test_args.push("--quiet".into());
    }
    if let Some(filter) = &request.filter {
        test_args.push(filter.clone());
    }
    test_args.extend(harness_args.iter().cloned());
    let mut failed = false;
    let mut stopped = false;
    'packages: for package in &prepared {
        for recipe in &package.recipes {
            let code = run_recipe_child(
                &package.self_exe,
                &package.root,
                &package.sysroot,
                recipe,
                &test_args,
            );
            if code != 0 {
                failed = true;
                if !request.no_fail_fast {
                    stopped = true;
                    break 'packages;
                }
            }
        }
    }
    if !stopped {
        'doctests: for package in &prepared {
            let Some(task) = &package.doctest else {
                continue;
            };
            if run_doctest_task(task, &package.sysroot, &test_args, request.quiet) != 0 {
                failed = true;
                if !request.no_fail_fast {
                    break 'doctests;
                }
            }
        }
    }
    if failed {
        ExitCode::from(101)
    } else {
        ExitCode::SUCCESS
    }
}

struct PreparedTests {
    self_exe: PathBuf,
    root: PathBuf,
    sysroot: PathBuf,
    recipes: Vec<PathBuf>,
    doctest: Option<DoctestTask>,
}

struct DoctestTask {
    package: String,
    rustdoc: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
}

/// Cargo resolver v2 unions features reaching the same package, same version, same
/// normal/build class within one workspace command. Each root resolves its graph independently, then feeds the result back, until all
/// roots reach the same union; intermediate results are not compiled, so unconverged graphs are not published to cache.
fn resolve_workspace_plans(
    manifests: &[PackageManifest],
    known_members: &[PackageManifest],
) -> Result<Vec<ResolvePlan>, String> {
    let project = manifests
        .first()
        .map(|manifest| manifest.lock_root.as_path())
        .unwrap_or(Path::new("."));
    let mut registry = Registry::open_for(project)?;
    if manifests.len() == 1 {
        return resolve_for_known(
            &manifests[0],
            &mut registry,
            ResolvePurpose::Test,
            known_members,
        )
        .map(|plan| vec![plan]);
    }

    let mut features = FeatureOverrides::new();
    for _ in 0..64 {
        let mut plans = Vec::with_capacity(manifests.len());
        let mut next = features.clone();
        for manifest in manifests {
            let plan = resolve_for_known_with_features(
                manifest,
                &mut registry,
                ResolvePurpose::Test,
                known_members,
                &features,
            )?;
            next.entry((
                plan.root_name.clone(),
                plan.root_version.clone(),
                UnitClass::Normal,
            ))
            .or_default()
            .extend(plan.root_features.iter().cloned());
            for unit in &plan.units {
                next.entry((unit.package.clone(), unit.version.clone(), unit.class))
                    .or_default()
                    .extend(unit.features.iter().cloned());
            }
            plans.push(plan);
        }
        if next == features {
            return Ok(plans);
        }
        features = next;
    }
    Err("workspace feature unification did not converge after 64 rounds (graph anomaly)".into())
}

fn prepare_test_package(
    mut manifest: PackageManifest,
    plan: ResolvePlan,
    request: &TestRequest,
) -> Result<PreparedTests, ExitCode> {
    manifest.profile = manifest.test_profile;
    if request.locked && !manifest.lock_root.join("Cargo.lock").is_file() {
        eprintln!("mirvm test: --locked requires an existing Cargo.lock");
        return Err(ExitCode::from(1));
    }

    if let Err(e) =
        buildrs::check_links_unique(Some((&manifest.name, manifest.links.as_deref())), &plan)
    {
        eprintln!("mirvm: {e}");
        return Err(ExitCode::from(1));
    }
    let selected = match request.select(&manifest, &plan.root_features) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mirvm test: {e}");
            return Err(ExitCode::from(1));
        }
    };
    let doctest_target = request.wants_doctest().then(|| {
        manifest
            .targets
            .iter()
            .find(|target| target.is_lib() && target.doctest)
    });
    let doctest_target = match doctest_target {
        Some(Some(target)) => Some(target),
        Some(None) if request.doc => {
            eprintln!(
                "mirvm test: --doc requires a package with a lib target that has doctest enabled"
            );
            return Err(ExitCode::from(101));
        }
        _ => None,
    };

    let sysroot = match std::env::var_os("MIRVM_SYSROOT") {
        Some(p) => PathBuf::from(p),
        None => match crate::sysroot::ensure_sysroot() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mirvm: failed to build sysroot: {e}");
                return Err(ExitCode::from(1));
            }
        },
    };
    let layout = Layout::new();
    let stamp = crate::sysroot::current_stamp_value()
        .unwrap_or_else(|| "sysroot-stamp-unknown".to_string());
    let rustflags = match super::rustflags::from_env_and_disk(&manifest.root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mirvm: failed to parse rustflags: {e}");
            return Err(ExitCode::from(1));
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
        request.quiet,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mirvm: {e}");
            return Err(ExitCode::from(1));
        }
    };
    let UnitTables { outputs, re_ran } = compiled.tables;
    let fps = compiled.fps;
    let self_exe = std::env::current_exe().expect("current_exe failed");
    let root_lib = manifest
        .targets
        .iter()
        .find(|t| t.is_lib())
        .map(|t| (t.name.clone(), t.path.clone(), t.proc_macro));
    let root_fp = match schedule::root_fingerprint(
        &manifest,
        &plan,
        &fps,
        &manifest.profile,
        &stamp,
        &rustflags,
    ) {
        Ok(fp) => fp,
        Err(e) => {
            eprintln!("mirvm: root package fingerprint computation failed: {e}");
            return Err(ExitCode::from(1));
        }
    };
    let root_bo = manifest.has_build_script.then(|| {
        run_build_lifecycle_root(
            &manifest,
            &plan,
            &fps,
            &layout,
            &root_fp,
            &outputs,
            &re_ran,
            request.quiet,
        )
        .0
    });

    let root_searches = buildrs::aggregate_link_searches(&plan, &plan.root_deps, &outputs);
    let needs_root_lib = selected.iter().any(|s| {
        matches!(
            s.target.kind,
            TargetKind::Bin | TargetKind::Test | TargetKind::Example | TargetKind::Bench
        )
    }) || doctest_target.is_some();
    if needs_root_lib
        && let Some((name, path, proc_macro)) = &root_lib
        && let Err(code) = if *proc_macro {
            compile_root_proc_macro(
                &manifest,
                &plan,
                &fps,
                &layout,
                name,
                path,
                root_bo.as_ref(),
                &root_searches,
                &rustflags,
                &root_fp,
            )
        } else {
            compile_root_lib(
                &manifest,
                &plan,
                &fps,
                &sysroot,
                &layout,
                name,
                path,
                root_bo.as_ref(),
                &root_searches,
                &rustflags,
                &root_fp,
                &outputs,
                &self_exe,
            )
        }
    {
        return Err(code);
    }
    let root_lib_ref = root_lib
        .as_ref()
        .map(|(n, _, _)| (n.as_str(), root_fp.as_str()));

    let mut bin_launchers = BTreeMap::new();
    if selected
        .iter()
        .any(|item| matches!(item.target.kind, TargetKind::Test | TargetKind::Bench))
    {
        for target in manifest.targets.iter().filter(|target| target.is_bin()) {
            if !target
                .required_features
                .iter()
                .all(|feature| plan.root_features.contains(feature))
            {
                continue;
            }
            let target_fp = target_fingerprint(&root_fp, target);
            let args = schedule::bin_rustc_args(
                &manifest,
                &plan,
                &fps,
                &sysroot,
                &layout,
                &target.name,
                &target.path,
                root_bo.as_ref(),
                &root_searches,
                &rustflags,
                root_lib_ref,
            );
            let env = root_target_env(
                &manifest,
                target,
                root_bo.as_ref(),
                &layout,
                &root_fp,
                &BTreeMap::new(),
            );
            match write_bin_launcher(
                &self_exe, &layout, &manifest, &root_fp, target, args, env, &target_fp,
            ) {
                Ok(path) => {
                    bin_launchers.insert(target.name.clone(), path);
                }
                Err(e) => {
                    eprintln!("mirvm test: {e}");
                    return Err(ExitCode::from(1));
                }
            }
        }
    }

    let mut recipes = Vec::new();
    for item in &selected {
        let target_fp = target_fingerprint(&root_fp, item.target);
        let args = if item.run {
            schedule::test_target_rustc_args(
                &manifest,
                &plan,
                &fps,
                &sysroot,
                &layout,
                &item.target.name,
                &item.target.path,
                item.target.harness,
                root_bo.as_ref(),
                &root_searches,
                &rustflags,
                (!item.target.is_lib()).then_some(root_lib_ref).flatten(),
            )
        } else {
            schedule::check_root_target_rustc_args(
                &manifest,
                &plan,
                &fps,
                &sysroot,
                &layout,
                &item.target.name,
                &item.target.path,
                item.include_dev,
                root_bo.as_ref(),
                &root_searches,
                &rustflags,
                root_lib_ref,
                &target_fp,
            )
        };
        let env = root_target_env(
            &manifest,
            item.target,
            root_bo.as_ref(),
            &layout,
            &root_fp,
            &bin_launchers,
        );
        if item.run {
            let recipe_path = write_root_recipe(
                &layout,
                &manifest,
                &root_fp,
                item.target,
                args.clone(),
                env.clone(),
                &target_fp,
            );
            recipes.push((item.target.name.clone(), recipe_path, args, target_fp, env));
        } else if !run_root_check(&self_exe, &manifest.root, &args, &env, false) {
            return Err(ExitCode::from(1));
        }
    }

    // Cargo compiles all test artifacts before execution starts. Pre-check suppresses warnings; real execution
    // emits them once through the runner session; if pre-check fails, replay diagnostics with the real session.
    for (_, recipe, args, fp, env) in &recipes {
        let check = check_args(args, &layout, fp);
        if !run_root_check(&self_exe, &manifest.root, &check, env, !request.no_run) {
            if !request.no_run {
                let _ = run_recipe_child(&self_exe, &manifest.root, &sysroot, recipe, &[]);
            }
            return Err(ExitCode::from(1));
        }
    }
    let doctest = if let Some(target) = doctest_target {
        let builder = match write_doctest_builder(&self_exe, &layout, &manifest, &root_fp) {
            Ok(builder) => builder,
            Err(error) => {
                eprintln!("mirvm test: {error}");
                return Err(ExitCode::from(1));
            }
        };
        let args = schedule::doctest_rustdoc_args(
            &manifest,
            &plan,
            &fps,
            &sysroot,
            &layout,
            target,
            &root_fp,
            root_bo.as_ref(),
            &root_searches,
            &builder,
        );
        let env = root_target_env(
            &manifest,
            target,
            root_bo.as_ref(),
            &layout,
            &root_fp,
            &BTreeMap::new(),
        );
        Some(DoctestTask {
            package: manifest.name.clone(),
            rustdoc: PathBuf::from(&args[0]),
            args: args[1..].to_vec(),
            env,
            cwd: manifest.root.clone(),
        })
    } else {
        None
    };
    Ok(PreparedTests {
        self_exe,
        root: manifest.root,
        sysroot,
        recipes: recipes
            .into_iter()
            .map(|(_, recipe, _, _, _)| recipe)
            .collect(),
        doctest,
    })
}

#[derive(Clone, Default)]
struct TestRequest {
    lib: bool,
    bins: bool,
    bin_names: BTreeSet<String>,
    tests: bool,
    test_names: BTreeSet<String>,
    examples: bool,
    example_names: BTreeSet<String>,
    benches: bool,
    bench_names: BTreeSet<String>,
    doc: bool,
    all_targets: bool,
    no_run: bool,
    no_fail_fast: bool,
    locked: bool,
    offline: bool,
    quiet: bool,
    filter: Option<String>,
    workspace: bool,
    packages: BTreeSet<String>,
    excludes: BTreeSet<String>,
    features: BTreeSet<String>,
    all_features: bool,
    no_default_features: bool,
    ignore_rust_version: bool,
}

struct SelectedTarget<'a> {
    target: &'a Target,
    run: bool,
    include_dev: bool,
}

impl TestRequest {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = Self::default();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            let take_value = |i: &mut usize, name: &str| -> Result<String, String> {
                *i += 1;
                args.get(*i)
                    .cloned()
                    .ok_or_else(|| format!("{name} requires a target name"))
            };
            match arg.as_str() {
                "--lib" => out.lib = true,
                "--bins" => out.bins = true,
                "--tests" => out.tests = true,
                "--examples" => out.examples = true,
                "--benches" => out.benches = true,
                "--bin" => {
                    let value = take_value(&mut i, "--bin")?;
                    out.bin_names.insert(value);
                }
                "--test" => {
                    let value = take_value(&mut i, "--test")?;
                    out.test_names.insert(value);
                }
                "--example" => {
                    let value = take_value(&mut i, "--example")?;
                    out.example_names.insert(value);
                }
                "--bench" => {
                    let value = take_value(&mut i, "--bench")?;
                    out.bench_names.insert(value);
                }
                "--all-targets" => out.all_targets = true,
                "--no-run" => out.no_run = true,
                "--no-fail-fast" => out.no_fail_fast = true,
                "--locked" => out.locked = true,
                "--offline" => out.offline = true,
                "-q" | "--quiet" => out.quiet = true,
                "--workspace" | "--all" => out.workspace = true,
                "-p" | "--package" => {
                    let value = take_value(&mut i, arg)?;
                    out.packages.insert(value);
                }
                "--exclude" => {
                    let value = take_value(&mut i, "--exclude")?;
                    out.excludes.insert(value);
                }
                "--doc" => out.doc = true,
                "--features" | "-F" => {
                    let value = take_value(&mut i, arg)?;
                    add_feature_values(&mut out.features, &value);
                }
                "--all-features" => out.all_features = true,
                "--no-default-features" => out.no_default_features = true,
                "--ignore-rust-version" => out.ignore_rust_version = true,
                _ if arg.starts_with("--bin=") => {
                    out.bin_names.insert(arg[6..].to_string());
                }
                _ if arg.starts_with("--test=") => {
                    out.test_names.insert(arg[7..].to_string());
                }
                _ if arg.starts_with("--example=") => {
                    out.example_names.insert(arg[10..].to_string());
                }
                _ if arg.starts_with("--bench=") => {
                    out.bench_names.insert(arg[8..].to_string());
                }
                _ if arg.starts_with("--package=") => {
                    out.packages.insert(arg[10..].to_string());
                }
                _ if arg.starts_with("--exclude=") => {
                    out.excludes.insert(arg[10..].to_string());
                }
                _ if arg.starts_with("--features=") => {
                    add_feature_values(&mut out.features, &arg[11..]);
                }
                _ if arg.starts_with('-') => {
                    return Err(format!(
                        "unsupported Cargo test argument `{arg}`, will not be silently swallowed"
                    ));
                }
                _ => {
                    if out.filter.replace(arg.clone()).is_some() {
                        return Err("Cargo test accepts only one TESTNAME filter string".into());
                    }
                }
            }
            i += 1;
        }
        Ok(out)
    }

    fn select_packages(
        &self,
        workspace: &WorkspaceManifest,
    ) -> Result<Vec<PackageManifest>, String> {
        if self.workspace && !self.packages.is_empty() {
            return Err("--workspace and --package cannot be used together".into());
        }
        if !self.workspace && !self.excludes.is_empty() {
            return Err("--exclude can only be used with --workspace".into());
        }
        let mut roots = BTreeSet::new();
        if self.workspace {
            roots.extend(workspace.members.iter().map(|member| member.root.clone()));
        } else if !self.packages.is_empty() {
            for spec in &self.packages {
                let member = workspace.member_by_spec(spec)?;
                roots.insert(member.root.clone());
            }
        } else if let Some(current) = &workspace.current_member {
            roots.insert(current.clone());
        } else {
            roots.extend(workspace.default_members.iter().cloned());
        }
        for spec in &self.excludes {
            let member = workspace.member_by_spec(spec)?;
            roots.remove(&member.root);
        }
        let selected: Vec<_> = workspace
            .members
            .iter()
            .filter(|member| roots.contains(&member.root))
            .cloned()
            .collect();
        if selected.is_empty() {
            return Err("package selection is empty".into());
        }
        Ok(selected)
    }

    fn apply_features(&self, manifests: &mut [PackageManifest]) -> Result<(), String> {
        for manifest in manifests.iter_mut() {
            manifest.default_features_enabled = !self.no_default_features;
            if self.all_features {
                manifest
                    .requested_features
                    .extend(manifest.check_cfg_feature_values());
            }
        }
        for spec in &self.features {
            let (package, feature) = match spec.split_once('/') {
                Some((package, feature)) => (Some(package), feature),
                None => (None, spec.as_str()),
            };
            if let Some(package) = package {
                if let Some(manifest) = manifests
                    .iter_mut()
                    .find(|manifest| manifest.name == package)
                {
                    if !manifest.check_cfg_feature_values().contains(feature) {
                        return Err(format!(
                            "package `{}` has no feature `{feature}`",
                            manifest.name
                        ));
                    }
                    manifest.requested_features.insert(feature.to_string());
                    continue;
                }
                let mut found = false;
                for manifest in manifests.iter_mut() {
                    if let Some(dep_key) = manifest
                        .deps
                        .iter()
                        .find(|dep| dep.key == package || dep.package == package)
                        .map(|dep| dep.key.clone())
                    {
                        manifest
                            .dependency_features
                            .entry(dep_key)
                            .or_default()
                            .insert(feature.to_string());
                        found = true;
                    }
                }
                if !found {
                    return Err(format!(
                        "feature `{spec}` points to neither a selected package nor its direct dependency"
                    ));
                }
                continue;
            }
            let mut found = false;
            for manifest in manifests.iter_mut() {
                if manifest.check_cfg_feature_values().contains(feature) {
                    manifest.requested_features.insert(feature.to_string());
                    found = true;
                }
            }
            if !found {
                return Err(format!(
                    "none of the selected packages have feature `{feature}`"
                ));
            }
        }
        Ok(())
    }

    fn restrict_named_targets(&self, manifests: &mut Vec<PackageManifest>) -> Result<(), String> {
        for (kind, names) in [
            (TargetKind::Bin, &self.bin_names),
            (TargetKind::Test, &self.test_names),
            (TargetKind::Example, &self.example_names),
            (TargetKind::Bench, &self.bench_names),
        ] {
            for name in names {
                if !manifests
                    .iter()
                    .flat_map(|manifest| &manifest.targets)
                    .any(|target| target.kind == kind && &target.name == name)
                {
                    return Err(format!("no {kind:?} target named `{name}`"));
                }
            }
        }
        let broad = self.lib
            || self.bins
            || self.tests
            || self.examples
            || self.benches
            || self.all_targets;
        if !broad
            && (!self.bin_names.is_empty()
                || !self.test_names.is_empty()
                || !self.example_names.is_empty()
                || !self.bench_names.is_empty())
        {
            manifests.retain(|manifest| {
                manifest.targets.iter().any(|target| match target.kind {
                    TargetKind::Bin => self.bin_names.contains(&target.name),
                    TargetKind::Test => self.test_names.contains(&target.name),
                    TargetKind::Example => self.example_names.contains(&target.name),
                    TargetKind::Bench => self.bench_names.contains(&target.name),
                    TargetKind::Lib => false,
                })
            });
        }
        if manifests.is_empty() {
            return Err("target selection is empty".into());
        }
        Ok(())
    }

    fn has_explicit_selection(&self) -> bool {
        self.doc
            || self.lib
            || self.bins
            || self.tests
            || self.examples
            || self.benches
            || self.all_targets
            || !self.bin_names.is_empty()
            || !self.test_names.is_empty()
            || !self.example_names.is_empty()
            || !self.bench_names.is_empty()
    }

    fn has_non_doc_target_selection(&self) -> bool {
        self.lib
            || self.bins
            || self.tests
            || self.examples
            || self.benches
            || self.all_targets
            || !self.bin_names.is_empty()
            || !self.test_names.is_empty()
            || !self.example_names.is_empty()
            || !self.bench_names.is_empty()
    }

    fn wants_doctest(&self) -> bool {
        self.doc || (!self.has_explicit_selection() && !self.no_run)
    }

    fn select<'a>(
        &self,
        manifest: &'a PackageManifest,
        root_features: &BTreeSet<String>,
    ) -> Result<Vec<SelectedTarget<'a>>, String> {
        let explicit = self.has_explicit_selection();
        let mut selected = Vec::new();
        for target in &manifest.targets {
            let named = match target.kind {
                TargetKind::Bin => self.bin_names.contains(&target.name),
                TargetKind::Test => self.test_names.contains(&target.name),
                TargetKind::Example => self.example_names.contains(&target.name),
                TargetKind::Bench => self.bench_names.contains(&target.name),
                TargetKind::Lib => false,
            };
            let requested = if explicit {
                match target.kind {
                    TargetKind::Lib => self.lib || self.tests || self.all_targets,
                    TargetKind::Bin => self.bins || self.tests || self.all_targets || named,
                    TargetKind::Test => self.tests || self.all_targets || named,
                    TargetKind::Example => self.examples || self.all_targets || named,
                    TargetKind::Bench => self.benches || self.all_targets || named,
                }
            } else {
                match target.kind {
                    TargetKind::Lib | TargetKind::Bin | TargetKind::Test => target.test,
                    TargetKind::Example => true, // compiled by default; only runs when test=true
                    TargetKind::Bench => false,
                }
            };
            if !requested {
                continue;
            }
            let features_ready = target
                .required_features
                .iter()
                .all(|f| root_features.contains(f));
            if !features_ready {
                if explicit && named {
                    return Err(format!(
                        "target `{}` requires features that are not enabled: {}",
                        target.name,
                        target.required_features.join(", ")
                    ));
                }
                continue;
            }
            let run = target.kind != TargetKind::Example || explicit || target.test;
            selected.push(SelectedTarget {
                target,
                run,
                include_dev: matches!(target.kind, TargetKind::Example | TargetKind::Bench),
            });
        }
        if selected.is_empty() && !self.doc {
            return Err("no testable targets".into());
        }

        // 选中 integration test 时，Cargo 还编译所有可用普通 bin，为
        // CARGO_BIN_EXE_* 提供进程入口；bin unit-test 与普通 bin 是两单元。
        let has_integration = selected
            .iter()
            .any(|s| matches!(s.target.kind, TargetKind::Test | TargetKind::Bench));
        if has_integration {
            for target in manifest.targets.iter().filter(|t| t.is_bin()) {
                if target
                    .required_features
                    .iter()
                    .all(|f| root_features.contains(f))
                {
                    selected.push(SelectedTarget {
                        target,
                        run: false,
                        include_dev: false,
                    });
                }
            }
        }
        Ok(selected)
    }
}

fn add_feature_values(out: &mut BTreeSet<String>, value: &str) {
    out.extend(
        value
            .split(|ch: char| ch == ',' || ch.is_ascii_whitespace())
            .filter(|part| !part.is_empty())
            .map(str::to_string),
    );
}

/// One-time all-member resolution for a lockless workspace. Cargo's workspace lock covers all members'
/// feature-reachable dependencies, separate from this compilation's actual selection; the synthetic root only forms this maximal resolution graph.
/// Delete it before writing, and add members' non-optional Dev edges back to their respective lock rows.
fn generate_workspace_lock(workspace: &WorkspaceManifest) -> Result<(), String> {
    let mut synthetic = workspace
        .members
        .first()
        .cloned()
        .ok_or_else(|| "workspace has no members".to_string())?;
    synthetic.name = format!("__mirvm_workspace_root_{:x}", std::process::id());
    synthetic.version = semver::Version::new(0, 0, 0);
    synthetic.root = workspace.root.clone();
    synthetic.lock_root = workspace.root.clone();
    synthetic.targets.clear();
    synthetic.features.clear();
    synthetic.requested_features.clear();
    synthetic.dependency_features.clear();
    synthetic.has_build_script = false;
    synthetic.build_script_path = None;
    synthetic.links = None;
    synthetic.deps = workspace
        .members
        .iter()
        .map(|member| super::manifest::DepDecl {
            key: member.name.clone(),
            package: member.name.clone(),
            source: DepSource::Path(member.root.clone()),
            features: member.check_cfg_feature_values().into_iter().collect(),
            optional: false,
            default_features: true,
            kind: DepKind::Normal,
            platform_cfg: None,
        })
        .collect();
    // Non-optional Dev packages must also enter version selection; member lock rows get dependency references added below.
    for (member_ix, member) in workspace.members.iter().enumerate() {
        for dep in member
            .deps
            .iter()
            .filter(|dep| dep.kind == DepKind::Dev && !dep.optional)
        {
            let mut dep = dep.clone();
            dep.key = format!("__mirvm_dev_{member_ix}_{}", dep.key);
            dep.kind = DepKind::Normal;
            synthetic.deps.push(dep);
        }
    }

    let mut registry = Registry::open_for(&workspace.root)?;
    let mut plan = resolve_for_known(
        &synthetic,
        &mut registry,
        ResolvePurpose::Run,
        &workspace.members,
    )?;
    plan.lock.packages.retain(|package| {
        !(package.name == synthetic.name && package.version == synthetic.version)
    });
    for member in &workspace.members {
        let Some(row) = plan
            .lock
            .packages
            .iter_mut()
            .find(|package| package.name == member.name && package.version == member.version)
        else {
            return Err(format!(
                "resolution result is missing workspace member {}",
                member.name
            ));
        };
        for dep in member
            .deps
            .iter()
            .filter(|dep| dep.kind == DepKind::Dev && !dep.optional)
        {
            let version = match &dep.source {
                DepSource::Path(path) => workspace
                    .members
                    .iter()
                    .find(|candidate| candidate.root == *path)
                    .map(|candidate| candidate.version.clone())
                    .or_else(|| PackageManifest::read_dir(path).ok().map(|m| m.version)),
                DepSource::Registry(req, _) => plan
                    .version_map
                    .get(&dep.package)
                    .and_then(|versions| versions.iter().find(|version| req.matches(version)))
                    .cloned(),
                DepSource::Git(spec) => plan
                    .version_map
                    .get(&dep.package)
                    .and_then(|versions| {
                        versions
                            .iter()
                            .find(|version| spec.version.matches(version))
                    })
                    .cloned(),
            }
            .ok_or_else(|| format!("Dev dependency {} has no resolved version", dep.package))?;
            let ambiguous = plan
                .version_map
                .get(&dep.package)
                .is_some_and(|versions| versions.len() > 1);
            row.dependencies.push(LockedDep {
                name: dep.package.clone(),
                version: ambiguous.then_some(version),
                source: None,
            });
        }
        row.dependencies.sort();
        row.dependencies.dedup();
    }
    write_lock_atomic(&workspace.root.join("Cargo.lock"), &plan.lock)
}

fn write_lock_atomic(path: &Path, lock: &Lockfile) -> Result<(), String> {
    let tmp = path.with_extension(format!("lock.mirvm-{}", std::process::id()));
    std::fs::write(&tmp, lock.serialize())
        .map_err(|e| format!("write {} failed: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("publish {} failed: {e}", path.display()))
}

fn sort_packages_dependency_first(manifests: &mut Vec<PackageManifest>) {
    let mut remaining: BTreeMap<String, PackageManifest> = manifests
        .drain(..)
        .map(|manifest| (manifest.name.clone(), manifest))
        .collect();
    let selected_names: BTreeSet<String> = remaining.keys().cloned().collect();
    let mut ordered = Vec::new();
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .find(|(_, manifest)| {
                manifest.deps.iter().all(|dep| {
                    !selected_names.contains(&dep.package)
                        || ordered
                            .iter()
                            .any(|done: &PackageManifest| done.name == dep.package)
                })
            })
            .map(|(name, _)| name.clone());
        let Some(name) = ready else {
            // Cargo will loudly report cycles in later dependency resolution; here we keep a deterministic order and do not fabricate diagnostics.
            ordered.extend(remaining.into_values());
            break;
        };
        ordered.push(remaining.remove(&name).unwrap());
    }
    *manifests = ordered;
}

#[allow(clippy::too_many_arguments)]
fn compile_root_lib(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    lib_name: &str,
    lib_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    fp: &str,
    _outputs: &BTreeMap<usize, BuildOutput>,
    self_exe: &Path,
) -> Result<(), ExitCode> {
    let stem = format!("lib{}-{fp}", lib_name.replace('-', "_"));
    if layout.deps.join(format!("{stem}.rmeta")).is_file()
        && layout.deps.join(format!("{stem}.rlib")).is_file()
    {
        return Ok(());
    }
    let args = schedule::root_lib_rustc_args(
        manifest, plan, fps, sysroot, layout, lib_name, lib_path, bo, searches, rustflags, fp,
    );
    let mut cmd = std::process::Command::new(self_exe);
    cmd.arg("__cless-dep").args(&args[1..]);
    cmd.envs(manifest.pkg_env.iter());
    cmd.env("CARGO_CRATE_NAME", lib_name.replace('-', "_"));
    cmd.env("CARGO_MANIFEST_DIR", &manifest.root);
    cmd.env("CARGO_MANIFEST_PATH", manifest.root.join("Cargo.toml"));
    if let Some(bo) = bo {
        cmd.env("OUT_DIR", layout.build_dir(&manifest.name, fp).join("out"));
        cmd.envs(bo.envs.iter().map(|(key, value)| (key, value)));
    }
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => {
            eprintln!(
                "mirvm: lib compilation failed: root package {} {}",
                manifest.name, manifest.version
            );
            Err(ExitCode::from(1))
        }
        Err(e) => {
            eprintln!("mirvm: lib compilation child process failed to start: {e}");
            Err(ExitCode::from(1))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compile_root_proc_macro(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    lib_name: &str,
    lib_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    fp: &str,
) -> Result<(), ExitCode> {
    let stem = format!("lib{}-{fp}", lib_name.replace('-', "_"));
    if layout
        .host_deps
        .join(format!("{stem}{}", std::env::consts::DLL_SUFFIX))
        .is_file()
    {
        return Ok(());
    }
    let args = schedule::root_proc_macro_rustc_args(
        manifest, plan, fps, layout, lib_name, lib_path, bo, searches, rustflags, fp,
    );
    let mut cmd = std::process::Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.envs(manifest.pkg_env.iter());
    cmd.env("CARGO_CRATE_NAME", lib_name.replace('-', "_"));
    cmd.env("CARGO_MANIFEST_DIR", &manifest.root);
    cmd.env("CARGO_MANIFEST_PATH", manifest.root.join("Cargo.toml"));
    if let Some(bo) = bo {
        cmd.env("OUT_DIR", layout.build_dir(&manifest.name, fp).join("out"));
        cmd.envs(bo.envs.iter().map(|(key, value)| (key, value)));
    }
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => {
            eprintln!(
                "mirvm: proc-macro compilation failed: root package {} {}",
                manifest.name, manifest.version
            );
            Err(ExitCode::from(1))
        }
        Err(error) => {
            eprintln!("mirvm: proc-macro compilation child process failed to start: {error}");
            Err(ExitCode::from(1))
        }
    }
}

fn target_fingerprint(root_fp: &str, target: &Target) -> String {
    let key = format!(
        "{root_fp}\x1f{:?}\x1f{}\x1f{}\x1f{}",
        target.kind,
        target.name,
        target.path.display(),
        target.harness as u8
    );
    format!("{:016x}", crate::lower::asm::fnv1a(key.as_bytes()))
}

fn root_target_env(
    manifest: &PackageManifest,
    target: &Target,
    bo: Option<&BuildOutput>,
    layout: &Layout,
    root_fp: &str,
    bin_launchers: &BTreeMap<String, PathBuf>,
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = manifest.pkg_env.clone();
    env.insert("CARGO_CRATE_NAME".into(), target.name.replace('-', "_"));
    env.insert(
        "CARGO_MANIFEST_DIR".into(),
        manifest.root.display().to_string(),
    );
    env.insert(
        "CARGO_MANIFEST_PATH".into(),
        manifest.root.join("Cargo.toml").display().to_string(),
    );
    env.insert("CARGO_PRIMARY_PACKAGE".into(), "1".into());
    if matches!(target.kind, TargetKind::Bin | TargetKind::Example) {
        env.insert("CARGO_BIN_NAME".into(), target.name.clone());
    }
    if matches!(target.kind, TargetKind::Test | TargetKind::Bench) {
        let tmp = layout.deps.parent().unwrap_or(&layout.deps).join("tmp");
        let _ = std::fs::create_dir_all(&tmp);
        env.insert("CARGO_TARGET_TMPDIR".into(), tmp.display().to_string());
        for (name, path) in bin_launchers {
            env.insert(format!("CARGO_BIN_EXE_{name}"), path.display().to_string());
        }
    }
    if let Some(bo) = bo {
        env.insert(
            "OUT_DIR".into(),
            layout
                .build_dir(&manifest.name, root_fp)
                .join("out")
                .display()
                .to_string(),
        );
        env.extend(bo.envs.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    env.into_iter().collect()
}

fn write_root_recipe(
    layout: &Layout,
    manifest: &PackageManifest,
    root_fp: &str,
    target: &Target,
    rustc_args: Vec<String>,
    env: Vec<(String, String)>,
    target_fp: &str,
) -> PathBuf {
    let dir = layout
        .build_dir(&manifest.name, root_fp)
        .join("test-recipes");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
        eprintln!(
            "mirvm: failed to create test recipe directory {}: {e}",
            dir.display()
        );
        std::process::exit(1);
    });
    let kind = format!("{:?}", target.kind).to_ascii_lowercase();
    let safe_name: String = target
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{kind}-{safe_name}-{target_fp}.json"));
    let recipe = RootRunRecipe {
        rustc_args,
        env,
        cwd: manifest.root.clone(),
        argv0: layout
            .deps
            .join(format!("{safe_name}-{target_fp}"))
            .display()
            .to_string(),
    };
    let bytes = serde_json::to_vec(&recipe).expect("test recipe serialization failed");
    if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&path, bytes).unwrap_or_else(|e| {
            eprintln!("mirvm: failed to write test recipe {}: {e}", path.display());
            std::process::exit(1);
        });
    }
    path
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

fn write_doctest_builder(
    self_exe: &Path,
    layout: &Layout,
    manifest: &PackageManifest,
    root_fp: &str,
) -> Result<PathBuf, String> {
    let dir = layout.build_dir(&manifest.name, root_fp).join("doctest");
    let builder = dir.join("mirvm-doctest-builder");
    crate::cargo_shim::ensure_self_symlink(self_exe, &builder)?;
    Ok(builder)
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
    let rustc = PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/rustc");
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

    let cwd = std::env::var_os("MIRVM_DOCTEST_RUN_DIR")
        .map(PathBuf::from)
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
    let tmp = recipe_path.with_extension(format!("tmp-{}", std::process::id()));
    if let Err(error) =
        std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &recipe_path))
    {
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

fn run_doctest_task(task: &DoctestTask, sysroot: &Path, test_args: &[String], quiet: bool) -> i32 {
    if !quiet {
        eprintln!("{:>12} {}", "Doc-tests", task.package);
    }
    let mut command = std::process::Command::new(&task.rustdoc);
    command.args(&task.args);
    for arg in test_args {
        command.arg("--test-args").arg(arg);
    }
    command
        .current_dir(&task.cwd)
        .envs(task.env.iter().cloned())
        .env("MIRVM_SYSROOT", sysroot)
        .env("MIRVM_DOCTEST_RUN_DIR", &task.cwd)
        .env("MIRVM_NO_IR_CACHE", "1");
    let code = command
        .status()
        .ok()
        .and_then(|status| status.code())
        .unwrap_or(1);
    if code != 0 {
        eprintln!("error: doctest failed, to rerun pass `--doc`");
    }
    code
}

/// CLI startup uses argv[0] in the earliest phase to recognize `CARGO_BIN_EXE_*` launchers.
pub fn root_launcher_recipe(argv0: &Path) -> Option<PathBuf> {
    let recipe = launcher_recipe_path(argv0);
    recipe.is_file().then_some(recipe)
}

#[allow(clippy::too_many_arguments)]
fn write_bin_launcher(
    self_exe: &Path,
    layout: &Layout,
    manifest: &PackageManifest,
    root_fp: &str,
    target: &Target,
    rustc_args: Vec<String>,
    env: Vec<(String, String)>,
    target_fp: &str,
) -> Result<PathBuf, String> {
    let dir = layout
        .build_dir(&manifest.name, root_fp)
        .join("bin-launchers");
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "failed to create bin launcher directory {}: {e}",
            dir.display()
        )
    })?;
    let safe_name: String = target
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let launcher = dir.join(format!("{safe_name}-{target_fp}"));
    let recipe_path = launcher_recipe_path(&launcher);
    let recipe = RootRunRecipe {
        rustc_args,
        env,
        cwd: manifest.root.clone(),
        argv0: launcher.display().to_string(),
    };
    let bytes =
        serde_json::to_vec(&recipe).map_err(|e| format!("bin recipe serialization failed: {e}"))?;
    if std::fs::read(&recipe_path).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&recipe_path, bytes)
            .map_err(|e| format!("write bin recipe {} failed: {e}", recipe_path.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let correct = std::fs::read_link(&launcher)
            .ok()
            .is_some_and(|target| target == self_exe);
        if !correct {
            match std::fs::remove_file(&launcher) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(format!(
                        "replace bin launcher {} failed: {e}",
                        launcher.display()
                    ));
                }
            }
            symlink(self_exe, &launcher)
                .map_err(|e| format!("create bin launcher {} failed: {e}", launcher.display()))?;
        }
        Ok(launcher)
    }
    #[cfg(not(unix))]
    {
        let _ = self_exe;
        Err("CARGO_BIN_EXE launcher currently only supports Unix hosts".into())
    }
}

fn check_args(args: &[String], layout: &Layout, fp: &str) -> Vec<String> {
    let mut out = args.to_vec();
    out.push("--emit=dep-info,metadata".into());
    out.push("-C".into());
    out.push(format!("metadata={fp}"));
    out.push("-C".into());
    out.push(format!("extra-filename=-{fp}"));
    out.push("--out-dir".into());
    out.push(layout.deps.display().to_string());
    out.push("-Zalways-encode-mir".into());
    out.push("-Zno-codegen".into());
    out
}

fn run_root_check(
    self_exe: &Path,
    cwd: &Path,
    args: &[String],
    env: &[(String, String)],
    quiet: bool,
) -> bool {
    let mut cmd = std::process::Command::new(self_exe);
    cmd.arg("__cless-dep")
        .args(&args[1..])
        .current_dir(cwd)
        .envs(env.iter().cloned());
    if quiet {
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
    }
    cmd.status().is_ok_and(|s| s.success())
}

fn run_recipe_child(
    self_exe: &Path,
    cwd: &Path,
    sysroot: &Path,
    recipe: &Path,
    args: &[String],
) -> i32 {
    let mut cmd = std::process::Command::new(self_exe);
    cmd.arg("__cless-run-root");
    append_capture_directory_arg(&mut cmd, crate::cli::capture_directory());
    cmd.arg(recipe)
        .args(args)
        .current_dir(cwd)
        .env("MIRVM_SYSROOT", sysroot);
    cmd.status().ok().and_then(|s| s.code()).unwrap_or(1)
}

fn append_capture_directory_arg(command: &mut std::process::Command, directory: Option<&Path>) {
    if let Some(directory) = directory {
        command
            .arg(crate::cli::INTERNAL_CAPTURE_DIRECTORY_ARG)
            .arg(directory);
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
    let _diagnostic_router =
        match crate::diagnostics::DiagnosticRouter::start(crate::cli::capture_directory(), true) {
            Ok(router) => router,
            Err(error) => {
                crate::diagnostics::control(format_args!(
                    "mirvm capture: cannot start diagnostics stream: {error}"
                ));
                return ExitCode::from(70);
            }
        };
    let Some(path) = argv.next() else {
        crate::diagnostics::control(format_args!("mirvm: __cless-run-root missing recipe path"));
        return ExitCode::from(2);
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            crate::diagnostics::control(format_args!(
                "mirvm: failed to read test recipe {path}: {e}"
            ));
            return ExitCode::from(1);
        }
    };
    let recipe: RootRunRecipe = match serde_json::from_slice(&data) {
        Ok(r) => r,
        Err(e) => {
            crate::diagnostics::control(format_args!("mirvm: test recipe {path} corrupted: {e}"));
            return ExitCode::from(1);
        }
    };
    if let Err(e) = std::env::set_current_dir(&recipe.cwd) {
        crate::diagnostics::control(format_args!(
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
/// (same key as audit::script_cache_dir), and the pseudo-package manifest goes through the same drive.
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
        // 路由层（cli.rs run_main）保证只在有 frontmatter 时进来；
        // 裸单文件是形态 3 快路径，不经此
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
    let cache = super::audit::script_cache_dir(file);
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
    // 1. P1 resolver: lock present use lock (closed), lock absent pubgrub fresh solve
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

    // 3. sysroot: MIRVM_SYSROOT env takes priority, otherwise self-built (same measure as cli.rs run path)
    let sysroot = match std::env::var_os("MIRVM_SYSROOT") {
        Some(p) => PathBuf::from(p),
        None => match crate::sysroot::ensure_sysroot() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mirvm: failed to build sysroot: {e}");
                std::process::exit(1);
            }
        },
    };

    // 4. fingerprint + compile segment (compile_plan extracted component, D15 P4 cut ⑥a — drive and
    // sysroot self-build share the same pipeline; sources of this segment's stamp/sysroot/rustflags
    // inputs on the drive side are annotated in the following segments)
    let layout = Layout::new();
    // sysroot stamp enters fingerprint (sysroot generation change ⇒ full rebuild); after ensure there must be a value,
    // fallback literal on absence is non-fatal (only makes fp coarser, no new error path)
    let stamp = crate::sysroot::current_stamp_value()
        .unwrap_or_else(|| "sysroot-stamp-unknown".to_string());
    // rustflags (D15 P3 cut ⑤a) parsed once and threaded through: only enter target-side args
    // (appended at end of dep/bin), fingerprint consumed uniformly for whole unit (host side following stale is harmless,
    // v1 simplified; parsing/priority/boundaries see rustflags.rs header)
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
    // root lib target (cut ⑤a full-layer migration surface, hexyl proven): when [lib]+[[bin]] dual
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
    // no chdir throughout: guest cwd = caller cwd, consistent with cargo run semantics (E36 closed)
    if let Some(out) = pack_out {
        crate::cli::pack_driver(args, program_argv, out.to_path_buf())
    } else {
        crate::cli::run_driver(args, program_argv, false, None, false, true, None)
    }
}

/// compile_plan return value: completion table + unit fingerprint table (drive's root package phase also uses
/// fps to compute root fingerprint — dep fp component of root_fingerprint; sysroot build does not consume).
pub struct CompiledPlan {
    pub tables: UnitTables,
    pub fps: Vec<String>,
}

/// unit compile segment (drive original segment 4, D15 P4 cut ⑥a extracted shared component): fingerprints +
/// host/target/build sets + unit-level Kahn ready-queue parallel scheduling (run_scheduler)
/// runs all unit pipelines. Shared by drive and sysroot self-build:
///
/// - drive passes MIR sysroot and its stamp (the consumption base for this run's compilation);
/// - sysroot build passes **toolchain sysroot** and its stamp — outputs cannot be their own
///   compile input (the --sysroot for compiling std can only be the distro toolchain, chicken-and-egg).
///
/// Failure = first compile error original text (caller prepends `mirvm: ` prefix and exits loudly,
/// byte-identical to before extraction).
#[allow(clippy::too_many_arguments)]
pub fn compile_plan(
    plan: &ResolvePlan,
    layout: &Layout,
    profile: &super::manifest::ProfileFlags,
    rustflags: &[String],
    sysroot: &Path,
    stamp: &str,
    root_has_build_script: bool,
    root_proc_macro: bool,
    quiet_build_warnings: bool,
) -> Result<CompiledPlan, String> {
    // unit-level Kahn ready-queue parallel scheduling (D15 P3 cut ⑤c): a unit is ready when all its deps are 'done'
    // (build.rs lifecycle + host/target compilation all finished according to set membership);
    // N workers each run the full pipeline of assigned units (build.rs decision/
    // execution → compilation), completion table is only gathered on the main thread.
    for d in [&layout.deps, &layout.host_deps, &layout.build_root] {
        std::fs::create_dir_all(d).map_err(|e| format!("create {} failed: {e}", d.display()))?;
    }
    let fps = schedule::fingerprints(plan, profile, stamp, rustflags)
        .map_err(|e| format!("dependency fingerprint computation failed: {e}"))?;
    let host_set = schedule::host_closure_for_root(plan, root_proc_macro);
    let target_set = schedule::target_units(plan);
    let build_set = schedule::build_closure(plan, root_has_build_script);
    let self_exe = std::env::current_exe().expect("current_exe failed");
    let jobs = cless_jobs();
    let (dependents, mut indeg) = schedule::dep_graph(plan);
    // Worker shared read-only context (borrowed via thread::scope, immutable for the whole scheduling phase — completion
    // table is not cross-thread, no lock needed). Thread-safety check (cut ⑤c design pin):
    // - no rustc session inside the driver process — compilation is entirely in __cless-dep/real-rustc
    //   child processes, no compiler global state between workers;
    // - all env writes are on Command instances (per-child, thread-safe); std::env::set_var is forbidden
    //   inside workers (whole-crate check: set_var only in bin phase = main thread after convergence;
    //   build script env all goes through Command.envs). std::env::var
    //   reads (rerun gate env_get, build_script_env's CARGO_HOME, etc.) do not race
    //   with set_var, safe;
    // - directory creation create_dir_all is idempotent; artifact content-addressed (fp stamped), different units
    //   use different stems so no name collision; same-fp duplicate units (Normal/Build dual units with same
    //   package/version/feature — fp omits class so names collide) are serialized by FpLocks over the whole
    //   pipeline: when the late worker starts, artifacts are already complete and rerun gate reads archive to skip,
    //   byte-identical to serial 'first runner runs, late runners all skip';
    // - MIRVM_DEBUG_BLDRS observation lines and child diagnostics may interleave when jobs>1 (debug
    //   knob; differential anchor = jobs=1 matches serial order + default N corpus judge —
    //   program output is in the converged bin session, naturally serial).
    let ctx = SharedCtx {
        plan,
        profile,
        fps: &fps,
        layout,
        sysroot,
        rustflags,
        self_exe: &self_exe,
        host_set: &host_set,
        target_set: &target_set,
        build_set: &build_set,
        quiet_build_warnings,
        fp_locks: FpLocks::default(),
    };
    if plan.units.is_empty() {
        return Ok(CompiledPlan {
            tables: UnitTables::default(),
            fps,
        });
    }
    let tables = schedule::run_scheduler(
        UnitTables::default(),
        &dependents,
        &mut indeg,
        jobs.min(plan.units.len()),
        |t: &UnitTables, ix| build_work_msg(&ctx, t, ix),
        |msg| run_unit_pipeline(msg, &ctx),
        |t, ix, done| {
            if let Some(bo) = done.bo {
                t.outputs.insert(ix, bo);
            }
            if done.ran {
                t.re_ran.insert(ix);
            }
        },
    )?;
    Ok(CompiledPlan { tables, fps })
}

/// Concurrency (cut ⑤c): MIRVM_CLESS_JOBS override, default available_parallelism
/// (fallback to 1 if unavailable). **=1 dispatch order matches old serial topo order bit-for-bit — differential debugging anchor,
/// pinned**. Illegal values (non-positive integers) are loudly rejected and exit.
fn cless_jobs() -> usize {
    match std::env::var("MIRVM_CLESS_JOBS") {
        Ok(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                eprintln!("mirvm: MIRVM_CLESS_JOBS={raw} invalid (must be a positive integer)");
                std::process::exit(1);
            }
        },
        Err(_) => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    }
}

/// Worker shared read-only context (borrowed via thread::scope; immutable for the whole scheduling phase — completion table
/// is not cross-thread, no lock needed). Thread-safety check details see drive() segment 4 header note.
struct SharedCtx<'a> {
    plan: &'a ResolvePlan,
    profile: &'a super::manifest::ProfileFlags,
    fps: &'a [String],
    layout: &'a Layout,
    sysroot: &'a Path,
    rustflags: &'a [String],
    self_exe: &'a Path,
    host_set: &'a BTreeSet<usize>,
    target_set: &'a BTreeSet<usize>,
    build_set: &'a BTreeSet<usize>,
    quiet_build_warnings: bool,
    /// Pipeline mutex lock table for same-fp duplicate units (drive() header note item 3).
    fp_locks: FpLocks,
}

/// Lazy fp → mutex table: Normal/Build dual units with same package/version/feature have the same fp (fp omits class)
/// and collide on artifact names/build dirs — the whole pipeline is mutexed by fp; when the late worker starts,
/// artifacts are already complete and the rerun gate reads the archive to skip, same effect as serial
/// 'first runner runs, late runners all skip'. The lock table itself is only held during lock acquisition;
/// unique-fp locks have zero contention.
#[derive(Default)]
struct FpLocks(std::sync::Mutex<BTreeMap<String, std::sync::Arc<std::sync::Mutex<()>>>>);

impl FpLocks {
    fn lock_for(&self, fp: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
        self.0
            .lock()
            .expect("fp lock table poisoned (internal error)")
            .entry(fp.to_string())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
            .clone()
    }
}

/// Completion table (owned only by main thread: dependency-side inputs needed by workers — DEP_* env,
/// transitive -L aggregation, links rerun list — are computed from this table by the main thread at **dispatch** time
/// and carried along with WorkMsg; at that moment all deps must be done, values are bit-identical
/// to the serial version computed at unit start). compile_plan return value (pub since D15 P4 cut ⑥a —
/// consumed by drive's root package phase; sysroot build takes Ok and does not read fields).
#[derive(Default)]
pub struct UnitTables {
    /// unit index → executed BuildOutput (consumed in three places: this unit's compilation corrections, dependents' -L
    /// aggregation, direct dependents' build script DEP_*).
    pub outputs: BTreeMap<usize, BuildOutput>,
    /// Units whose build.rs was actually rerun in this session (cut ⑤b condition 4 links propagation:
    /// a package with links in direct dependencies being in re_ran ⇒ dependent also reruns, DEP_* input may change).
    pub re_ran: BTreeSet<usize>,
}

/// A unit's work order (main thread computes all dependency-side inputs at dispatch time, see UnitTables note;
/// also computed for fp-hit units — pure computation with no output, in exchange workers never access the completion table).
struct WorkMsg {
    ix: usize,
    /// Run build.rs lifecycle (has_build_script ∧ in any compile set; orphan
    /// build-deps do not run — cargo does not compile the kind whose parent has no build.rs,
    /// running its build.rs is overreach)
    run_build: bool,
    /// DEP_* env from direct dependencies (same measure as dep_metadata_env; empty when run_build=false)
    dep_env: BTreeMap<String, String>,
    /// Names of packages with links in direct dependencies that reran in this session (rerun gate condition 4)
    dep_links_reran: Vec<String>,
    /// Transitive -L aggregation (same measure as aggregate_link_searches; empty when not in any compile set)
    searches: Vec<String>,
}

/// A unit's completion receipt (worker → main thread).
struct PerUnitDone {
    /// build.rs output (None for units that did not run build.rs)
    bo: Option<BuildOutput>,
    /// whether build.rs was actually rerun this time
    ran: bool,
}

/// Work order construction (main thread): set membership decision + dependency-side input computation.
fn build_work_msg(ctx: &SharedCtx, t: &UnitTables, ix: usize) -> WorkMsg {
    let u = &ctx.plan.units[ix];
    let in_host = ctx.host_set.contains(&ix) || ctx.build_set.contains(&ix);
    let in_target = ctx.target_set.contains(&ix);
    let run_build = u.has_build_script && (in_host || in_target);
    let (dep_env, dep_links_reran) = if run_build {
        (
            buildrs::dep_metadata_env(ctx.plan, &u.deps, &t.outputs),
            // Condition 4 links propagation: packages with links in direct dependencies that reran this time
            // (DEP_* only goes to direct dependents — further propagation is decided/covered by each layer itself)
            u.deps
                .iter()
                .filter(|d| t.re_ran.contains(&d.unit) && ctx.plan.units[d.unit].links.is_some())
                .map(|d| ctx.plan.units[d.unit].package.clone())
                .collect(),
        )
    } else {
        (BTreeMap::new(), Vec::new())
    };
    let searches = if in_host || in_target {
        buildrs::aggregate_link_searches(ctx.plan, &u.deps, &t.outputs)
    } else {
        Vec::new()
    };
    WorkMsg {
        ix,
        run_build,
        dep_env,
        dep_links_reran,
        searches,
    }
}

/// A unit's full pipeline (worker thread): fp-lock mutex (same-fp duplicate units) →
/// build.rs lifecycle → host-side compile → target-side compile; hit stages are skipped as usual
/// (**disk checked inside the lock** — artifacts from same-fp predecessor must be visible to count as hit). Failure returns
/// original error text (`mirvm: ` prefix added by main thread after convergence, byte-identical to serial text).
fn run_unit_pipeline(msg: WorkMsg, ctx: &SharedCtx) -> Result<PerUnitDone, String> {
    let ix = msg.ix;
    let u = &ctx.plan.units[ix];
    let fp = &ctx.fps[ix];
    // Same-fp duplicate unit mutex (lock poisoning can only come from a predecessor worker panic — internal error already
    // finalized at that point, take inner and continue, do not stack failures)
    let fp_mutex = ctx.fp_locks.lock_for(fp);
    let _fp_guard = fp_mutex.lock().unwrap_or_else(|e| e.into_inner());
    let stem = format!("lib{}-{}", u.lib_name, fp);
    let mut done = PerUnitDone {
        bo: None,
        ran: false,
    };
    // build.rs lifecycle (ready decision guarantees its build-deps and their build.rs are all done)
    if msg.run_build {
        let (bo, ran) = run_build_lifecycle(u, ix, ctx, msg.dep_env, msg.dep_links_reran)?;
        done.ran = ran;
        done.bo = Some(bo);
    }
    let bo = done.bo.as_ref();
    // host side: proc-macro proper produces dylib; closure normal units (including build-deps closure) produce host rlib
    if ctx.host_set.contains(&ix) || ctx.build_set.contains(&ix) {
        let hit = if u.proc_macro {
            ctx.layout
                .host_deps
                .join(format!("{stem}{}", std::env::consts::DLL_SUFFIX))
                .is_file()
        } else {
            ctx.layout.host_deps.join(format!("{stem}.rmeta")).is_file()
                && ctx.layout.host_deps.join(format!("{stem}.rlib")).is_file()
        };
        if !hit {
            let (args, what) = if u.proc_macro {
                (
                    schedule::proc_macro_rustc_args(
                        ctx.plan,
                        ix,
                        ctx.profile,
                        ctx.fps,
                        ctx.layout,
                        bo,
                        &msg.searches,
                    ),
                    "proc-macro",
                )
            } else {
                (
                    schedule::host_rustc_args(
                        ctx.plan,
                        ix,
                        ctx.profile,
                        ctx.fps,
                        ctx.layout,
                        bo,
                        &msg.searches,
                    ),
                    "host dep",
                )
            };
            let mut cmd = std::process::Command::new(&args[0]);
            cmd.args(&args[1..]);
            apply_unit_env(&mut cmd, u);
            apply_build_env(&mut cmd, ctx.layout, u, fp, bo);
            run_compile(&mut cmd, u, what)?;
        }
    }
    // target side: same as before __cless-dep (-Zno-codegen rlib)
    if ctx.target_set.contains(&ix) {
        // fingerprint hit: content-addressed, same-name artifact means same content, skip
        let hit = ctx.layout.deps.join(format!("{stem}.rmeta")).is_file()
            && ctx.layout.deps.join(format!("{stem}.rlib")).is_file();
        if !hit {
            let args = schedule::dep_rustc_args(
                ctx.plan,
                ix,
                ctx.profile,
                ctx.fps,
                ctx.sysroot,
                ctx.layout,
                bo,
                &msg.searches,
                ctx.rustflags,
            );
            let mut cmd = std::process::Command::new(ctx.self_exe);
            cmd.arg("__cless-dep").args(&args[1..]);
            apply_unit_env(&mut cmd, u);
            apply_build_env(&mut cmd, ctx.layout, u, fp, bo);
            run_compile(&mut cmd, u, "dep")?;
        }
    }
    Ok(done)
}

/// A unit's build.rs full lifecycle: build script compilation (skip on fp hit) →
/// rerun decision (cut ⑤b: buildrs::should_rerun, same semantics as cargo) — if skipped, read
/// output.txt and reparse to replay BuildOutput; if run, execute with cargo-compatible env and write archive
/// (only after successful execution — failure already returned by this function, no partial archive) → (BuildOutput, whether it actually ran).
/// `dep_env`/`dep_links_reran` are computed and carried by the main thread at dispatch time (see WorkMsg note).
/// Any step failure returns original error text (main thread loudly names after convergence).
fn run_build_lifecycle(
    u: &Unit,
    ix: usize,
    ctx: &SharedCtx,
    dep_env: BTreeMap<String, String>,
    dep_links_reran: Vec<String>,
) -> Result<(BuildOutput, bool), String> {
    let fp = &ctx.fps[ix];
    let bdir = ctx.layout.build_dir(&u.package, fp);
    if let Err(e) = std::fs::create_dir_all(bdir.join("out")) {
        return Err(format!(
            "failed to create build directory {} ({} {}): {e}",
            bdir.display(),
            u.package,
            u.version
        ));
    }
    let bexe = bdir.join(format!("build_script_build-{fp}"));
    if !bexe.is_file() {
        let args =
            schedule::build_script_rustc_args(ctx.plan, ix, ctx.profile, ctx.fps, ctx.layout);
        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        apply_unit_env(&mut cmd, u);
        // The compiled crate is the build script itself (same as cargo: CARGO_CRATE_NAME
        // follows the compiled crate, not the owning package's lib name)
        cmd.env("CARGO_CRATE_NAME", "build_script_build");
        run_compile(&mut cmd, u, "build script")?;
    }
    let env = buildrs::build_script_env(&buildrs::ExecCtx {
        pkg_env: &u.pkg_env,
        source_dir: &u.source_dir,
        features: &u.features,
        profile: ctx.profile,
        out_dir: &bdir.join("out"),
        dep_env,
        links: u.links.as_deref(),
        ld_dirs: &[ctx.layout.host_deps.clone(), ctx.layout.deps.clone()],
    });
    rerun_gate(
        &u.package,
        &u.version.to_string(),
        u.from_registry,
        &u.source_dir,
        &bdir,
        &bexe,
        &env,
        &dep_links_reran,
        ctx.quiet_build_warnings,
    )
}

/// Root package build.rs lifecycle (root is not a unit: pkg_env/features/profile supplied
/// directly by manifest/plan; root is a local path package, warnings shown normally).
#[allow(clippy::too_many_arguments)]
fn run_build_lifecycle_root(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    root_fp: &str,
    outputs: &BTreeMap<usize, BuildOutput>,
    re_ran: &BTreeSet<usize>,
    quiet_build_warnings: bool,
) -> (BuildOutput, bool) {
    let bdir = layout.build_dir(&manifest.name, root_fp);
    if let Err(e) = std::fs::create_dir_all(bdir.join("out")) {
        eprintln!(
            "mirvm: failed to create build directory {} (root package {}): {e}",
            bdir.display(),
            manifest.name
        );
        std::process::exit(1);
    }
    let bexe = bdir.join(format!("build_script_build-{root_fp}"));
    if !bexe.is_file() {
        let args = schedule::root_build_script_rustc_args(manifest, plan, fps, layout, root_fp);
        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        // root package compile-time env (full CARGO_PKG_* set + two manifest entries, same as cargo)
        cmd.envs(manifest.pkg_env.iter());
        cmd.env("CARGO_CRATE_NAME", "build_script_build");
        cmd.env("CARGO_MANIFEST_DIR", &manifest.root);
        cmd.env(
            "CARGO_MANIFEST_PATH",
            manifest.root.join("Cargo.toml").display().to_string(),
        );
        let status = match cmd.status() {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "mirvm: build script compilation child process failed to start (root package {} {}): {e}",
                    manifest.name, manifest.version
                );
                std::process::exit(1);
            }
        };
        if !status.success() {
            eprintln!(
                "mirvm: build script compilation failed: root package {} {}",
                manifest.name, manifest.version
            );
            std::process::exit(1);
        }
    }
    let env = buildrs::build_script_env(&buildrs::ExecCtx {
        pkg_env: &manifest.pkg_env,
        source_dir: &manifest.root,
        features: &plan.root_features,
        profile: &manifest.profile,
        out_dir: &bdir.join("out"),
        dep_env: buildrs::dep_metadata_env(plan, &plan.root_deps, outputs),
        links: manifest.links.as_deref(),
        ld_dirs: &[layout.host_deps.clone(), layout.deps.clone()],
    });
    let dep_links_reran: Vec<String> = plan
        .root_deps
        .iter()
        .filter(|d| re_ran.contains(&d.unit) && plan.units[d.unit].links.is_some())
        .map(|d| plan.units[d.unit].package.clone())
        .collect();
    // root phase runs on main thread after convergence — errors still exit loudly (text same shape as worker returns)
    match rerun_gate(
        &manifest.name,
        &manifest.version.to_string(),
        false,
        &manifest.root,
        &bdir,
        &bexe,
        &env,
        &dep_links_reran,
        quiet_build_warnings,
    ) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("mirvm: {e}");
            std::process::exit(1);
        }
    }
}

/// rerun gate (cut ⑤b): decision (buildrs::should_rerun) → if skip, read archive
/// output.txt and reparse to replay (instruction stream zero serialization distortion, warnings replayed from cache
/// under same gate — same as cargo); if run, execute + write both output.txt and rerun.txt archives.
/// When MIRVM_DEBUG_BLDRS=1, print `bldrs run|skip <pkg> <reason>` observation line to stderr
/// (cut ⑤c: observation lines from workers may interleave when jobs>1 — debug knob, not differential surface).
/// Returns (BuildOutput, whether this run actually ran); failure returns original error text (caller prepends `mirvm: `
/// prefix and exits loudly — root path on main thread prepends in place, worker path prepends after convergence).
// flat parameter precedent same as run_build_lifecycle
#[allow(clippy::too_many_arguments)]
fn rerun_gate(
    pkg: &str,
    ver: &str,
    from_registry: bool,
    pkg_root: &Path,
    bdir: &Path,
    bexe: &Path,
    env: &BTreeMap<String, String>,
    dep_links_reran: &[String],
    quiet_build_warnings: bool,
) -> Result<(BuildOutput, bool), String> {
    let env_get = |k: &str| std::env::var(k).ok();
    let (rerun, why) = buildrs::should_rerun(
        bdir,
        from_registry,
        pkg,
        pkg_root,
        dep_links_reran,
        &env_get,
    );
    if std::env::var_os("MIRVM_DEBUG_BLDRS").is_some() {
        eprintln!("bldrs {} {pkg} {why}", if rerun { "run" } else { "skip" });
    }
    if !rerun {
        // skip execution: output.txt reparse is BuildOutput (replay failure treated as corrupted archive, self-healing by falling through to run)
        if let Ok(stdout) = std::fs::read_to_string(bdir.join("output.txt"))
            && let Ok(bo) = buildrs::parse_instructions(&stdout)
        {
            show_warnings(pkg, ver, from_registry, &bo, quiet_build_warnings);
            return Ok((bo, false));
        }
    }
    let (bo, stdout) = exec_and_parse(
        pkg,
        ver,
        from_registry,
        bexe,
        pkg_root,
        env,
        quiet_build_warnings,
    )?;
    // archive write failure is non-fatal — next no-record rerun self-heals (disk-layer failure would have already blown
    // earlier compilation writes); silent, does not disturb differential stderr
    let _ = buildrs::write_record(bdir, &stdout, &bo, from_registry, pkg, pkg_root, &env_get);
    Ok((bo, true))
}

/// build script warning 回吐门控（cargo 同格式同口径：`warning: <pkg>@<ver>:
/// <msg>`；registry 包默认吞，path 包显示；执行与存档回放两路同款）。
fn show_warnings(pkg: &str, ver: &str, from_registry: bool, bo: &BuildOutput, quiet: bool) {
    if !from_registry && !quiet {
        for w in &bo.warnings {
            eprintln!("warning: {pkg}@{ver}: {w}");
        }
    }
}

/// Execute + instruction parse + warning replay, returns (BuildOutput, raw stdout)
/// (raw stdout for caller to write output.txt archive — replay relies on reparse, zero serialization distortion).
/// Failure returns original error text (caller prepends `mirvm: ` prefix and exits loudly).
fn exec_and_parse(
    pkg: &str,
    ver: &str,
    from_registry: bool,
    bexe: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    quiet_build_warnings: bool,
) -> Result<(BuildOutput, String), String> {
    let stdout = buildrs::run_build_script(bexe, cwd, env)
        .map_err(|e| format!("build script execution failed ({pkg} {ver}): {e}"))?;
    let bo = buildrs::parse_instructions(&stdout)
        .map_err(|e| format!("build script instruction parse failed ({pkg} {ver}): {e}"))?;
    show_warnings(pkg, ver, from_registry, &bo, quiet_build_warnings);
    Ok((bo, stdout))
}

/// cargo compile-time env contract (readable by source env!): full CARGO_PKG_* set + three crate/manifest
/// entries (cargo sets these on every rustc call; same on both host real rustc and __cless-dep sides).
fn apply_unit_env(cmd: &mut std::process::Command, u: &Unit) {
    cmd.envs(u.pkg_env.iter());
    cmd.env("CARGO_CRATE_NAME", &u.lib_name);
    cmd.env("CARGO_MANIFEST_DIR", &u.source_dir);
    cmd.env(
        "CARGO_MANIFEST_PATH",
        u.source_dir.join("Cargo.toml").display().to_string(),
    );
}

/// This unit's build script compile-time env injection: OUT_DIR + rustc-env (set by cargo when compiling
/// packages with a build script; readable by env!).
fn apply_build_env(
    cmd: &mut std::process::Command,
    layout: &Layout,
    u: &Unit,
    fp: &str,
    bo: Option<&BuildOutput>,
) {
    if let Some(bo) = bo {
        cmd.env("OUT_DIR", layout.build_dir(&u.package, fp).join("out"));
        for (k, v) in &bo.envs {
            cmd.env(k, v);
        }
    }
}

/// Compile child process runs synchronously to completion; startup/compile failure returns original error text (what = artifact category,
/// names the crate — main thread prepends `mirvm: ` prefix and exits loudly after convergence, same shape as serial text).
fn run_compile(cmd: &mut std::process::Command, u: &Unit, what: &str) -> Result<(), String> {
    let status = cmd.status().map_err(|e| {
        format!(
            "{what} compilation child process failed to start ({} {}): {e}",
            u.package, u.version
        )
    })?;
    if !status.success() {
        return Err(format!(
            "{what} compilation failed: {} {}",
            u.package, u.version
        ));
    }
    Ok(())
}
