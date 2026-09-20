//! `mirvm test` self path: Cargo-compatible target selection, feature unification across
//! workspace members, and preparation of the per-artifact test recipes and doctest task.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::super::lockfile::LockedDep;
use super::super::manifest::{DepKind, DepSource, PackageManifest, Target, TargetKind};
use super::super::registry::Registry;
use super::super::resolve::{
    FeatureOverrides, ResolvePlan, ResolvePurpose, UnitClass, resolve_for_known,
    resolve_for_known_with_features,
};
use super::super::workspace::WorkspaceManifest;
use super::build::{UnitTables, compile_plan, run_build_lifecycle_root};
use super::buildrs::{self, BuildOutput};
use super::schedule::{self, Layout};
use super::{RootRunRecipe, append_capture_directory_arg, launcher_recipe_path, write_lock_atomic};

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
        // `--offline` outranks the environment, and child processes must see the same decision.
        crate::options::note_cli("offline");
        crate::options::export_to_process("offline", "1");
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

    let sysroot = match crate::options::get().sysroot.clone() {
        Some(p) => p,
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
    let rustflags = match crate::cargoless::rustflags::from_env_and_disk(&manifest.root) {
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

        // When an integration test is selected, Cargo also compiles every available normal bin to
        // provide a process entry point for CARGO_BIN_EXE_*; the bin unit-test and the normal bin
        // are two units.
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
        .map(|member| crate::cargoless::manifest::DepDecl {
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
    format!("{:016x}", crate::utils::content::fnv1a(key.as_bytes()))
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
        .envs(task.env.iter().cloned());
    command.env(crate::options::env_var_name("sysroot"), sysroot);
    crate::options::protocol::set_doctest_run_dir(&mut command, &task.cwd);
    command.env(crate::options::env_var_name("no_ir_cache"), "1");
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
    cmd.arg(recipe).args(args).current_dir(cwd);
    cmd.env(crate::options::env_var_name("sysroot"), sysroot);
    cmd.status().ok().and_then(|s| s.code()).unwrap_or(1)
}
