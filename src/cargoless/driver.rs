//! `cargoless/driver.rs` —— `mirvm run` 的零 cargo 新路径（D15 P2 切①/②/③，
//! 设计档 §3.6/§5 P2），替代 cargo_shim::phase_cargo 的三阶段（cargo run +
//! RUSTC_WRAPPER + runner 协议）：
//!
//! ```text
//! resolve（P1 求解器）→ links 互斥校验 → unit 级 Kahn 就绪队列并行调度
//! （P3 切⑤c：N 个 worker，MIRVM_CLESS_JOBS 覆盖，缺省 available_parallelism；
//! =1 与旧串行 topo 序逐位一致——对拍调试锚。unit 的全部依赖完成即就绪，
//! unit 内部阶段保持串行）：
//!   build.rs 全生命周期（切③ + P3 切⑤b 精细增量）：host 真编译 build script
//!     （fp 命中跳过）→ rerun 判定（buildrs::should_rerun，cargo 同语义——
//!     存档在 build/<pkg>-<fp>/{output.txt,rerun.txt}，跳过则从 output.txt
//!     重解析回放 BuildOutput，warning 门控同款）→ 以 cargo 兼容 env 执行 →
//!     指令解析 → BuildOutput 回传主线程入完成表（本 unit 记入 re_ran，
//!     links 传递判定用）
//!   host 集（proc-macro 闭包 ∪ build-deps 闭包）→ spawn 真 rustc 真 codegen
//!   target 集 → 起 `__cless-dep` 子进程（cli::run_dep_compiler：
//!   in-process rustc_driver + global_asm 抽取）
//!   （双侧编译都吃本 unit BuildOutput 修正：cfg/check-cfg/link 旗进 argv，
//!   OUT_DIR/rustc-env 进子进程 env——proc-macro2 的 build.rs cfg 进其 host
//!   编译，serde_derive 类全链解锁的关键）
//! → 全 unit 汇合后回主线程：根包 build.rs 同生命周期 → bin 走既有
//!   MirvmCallbacks 会话（OUT_DIR/rustc-env/cfg 修正同样进 bin 会话）
//! ```
//!
//! 传播规则（-l 只进本包、-L 进传递依赖者、metadata 只给直接依赖者的
//! build script、无自动 DEP_*_ROOT、无自动 check-cfg 补钉）全是切③ 实证
//! 结论，明细在 buildrs.rs 文件头。

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

/// `mirvm run <目录|Cargo.toml> [--bin <名>]`（MIRVM_DEPS=self）。
/// bin_sel = --bin 选定的 bin 名（D15 P4 切⑥b，cargo run --bin 语义）。
pub fn run_project(
    dir: &Path,
    program_args: &[String],
    bin_sel: Option<&str>,
    ignore_rust_version: bool,
) -> ExitCode {
    let mut manifest = match PackageManifest::read_dir(dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mirvm: 读取项目 {} 失败: {e}", dir.display());
            std::process::exit(1);
        }
    };
    manifest.ignore_rust_version = ignore_rust_version;
    drive(&manifest, program_args, bin_sel, None)
}

/// `mirvm pack <目录|Cargo.toml>` 的默认 self 路径。依赖、build.rs、
/// proc-macro 和根包编译与 `run_project` 完全共用，只在最终 rustc 会话把
/// “执行”换成写出自包含包。
pub fn pack_project(dir: &Path, out: &Path) -> ExitCode {
    let manifest = match PackageManifest::read_dir(dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("mirvm: 读取项目 {} 失败: {error}", dir.display());
            std::process::exit(1);
        }
    };
    drive(&manifest, &[], None, Some(out))
}

/// 一个根测试目标的独立执行配方。Cargo 每个测试 artifact 各起一进程；self
/// 路径也把 rustc 参数和编译期环境封进配方，再由 `__cless-run-root` 子进程执行。
#[derive(serde::Serialize, serde::Deserialize)]
struct RootRunRecipe {
    rustc_args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    argv0: String,
}

/// `mirvm test` 的 self 路径。workspace 先归约为完整成员清单，再统一多包
/// feature 与编译键；所有选中包准备完成后才开始执行测试。
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
            eprintln!("mirvm: 读取项目 {} 失败: {e}", dir.display());
            return ExitCode::from(1);
        }
    };
    if request.offline {
        // SAFETY: CLI 启动相，尚未启动 worker/rustc/guest 线程。
        unsafe { std::env::set_var("MIRVM_OFFLINE", "1") };
    }
    if request.ignore_rust_version {
        for member in &mut workspace.members {
            member.ignore_rust_version = true;
        }
    }
    if request.locked && !workspace.root.join("Cargo.lock").is_file() {
        eprintln!("mirvm test: --locked 要求 workspace 根已有 Cargo.lock");
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
            eprintln!("mirvm test: 生成 workspace Cargo.lock 失败: {e}");
            return ExitCode::from(1);
        }
    }
    let plans = match resolve_workspace_plans(&manifests, &known) {
        Ok(plans) => plans,
        Err(e) => {
            eprintln!("mirvm: 依赖解析失败: {e}");
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

/// Cargo resolver v2 会把同一次 workspace 命令中到达同一包、同一版本、同一
/// normal/build 类别的 feature 求并集。各根先独立求图，再把结果反灌，直到所有
/// 根得到同一个并集；中间结果不编译，因此不会把未收敛图发布到缓存。
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
    Err("workspace feature 统一 64 轮未收敛（图异常）".into())
}

fn prepare_test_package(
    mut manifest: PackageManifest,
    plan: ResolvePlan,
    request: &TestRequest,
) -> Result<PreparedTests, ExitCode> {
    manifest.profile = manifest.test_profile;
    if request.locked && !manifest.lock_root.join("Cargo.lock").is_file() {
        eprintln!("mirvm test: --locked 要求现有 Cargo.lock");
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
            eprintln!("mirvm test: --doc 要求包有启用 doctest 的 lib target");
            return Err(ExitCode::from(101));
        }
        _ => None,
    };

    let sysroot = match std::env::var_os("MIRVM_SYSROOT") {
        Some(p) => PathBuf::from(p),
        None => match crate::sysroot::ensure_sysroot() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mirvm: 构建 sysroot 失败: {e}");
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
            eprintln!("mirvm: rustflags 解析失败: {e}");
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
    let self_exe = std::env::current_exe().expect("current_exe 失败");
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
            eprintln!("mirvm: 根包指纹计算失败: {e}");
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

    // Cargo 先编完全部 test artifacts 再开始执行。预检静默告警，真实执行
    // 会通过 runner 会话发一次；若预检失败，再用真实会话重放诊断。
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
                    .ok_or_else(|| format!("{name} 需要目标名"))
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
                    return Err(format!("不支持的 Cargo test 参数 `{arg}`，不会静默吞掉"));
                }
                _ => {
                    if out.filter.replace(arg.clone()).is_some() {
                        return Err("Cargo test 只接受一个 TESTNAME 过滤串".into());
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
            return Err("--workspace 与 --package 不能同时使用".into());
        }
        if !self.workspace && !self.excludes.is_empty() {
            return Err("--exclude 只能与 --workspace 一起使用".into());
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
            return Err("package 选择结果为空".into());
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
                        return Err(format!("包 `{}` 没有 feature `{feature}`", manifest.name));
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
                        "feature `{spec}` 既不指向选中包，也不指向其直接依赖"
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
                return Err(format!("选中的包均没有 feature `{feature}`"));
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
                    return Err(format!("没有名为 `{name}` 的 {kind:?} 目标"));
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
            return Err("目标选择结果为空".into());
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
                    TargetKind::Example => true, // 默认至少编译；test=true 才运行
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
                        "目标 `{}` 需要未启用 features: {}",
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
            return Err("没有可测试目标".into());
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

/// 无锁 workspace 的一次性全成员求解。Cargo 的 workspace lock 覆盖所有成员的
/// 全部 feature 可达依赖，与本次实际编译选择分开；合成根只负责形成这张最大解析图。
/// 落盘前删除它，并把成员的非可选 Dev 边补回各自 lock 行。
fn generate_workspace_lock(workspace: &WorkspaceManifest) -> Result<(), String> {
    let mut synthetic = workspace
        .members
        .first()
        .cloned()
        .ok_or_else(|| "workspace 没有成员".to_string())?;
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
    // 非可选 Dev 包也必须进入版本选择；成员 lock 行在下方补依赖引用。
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
            return Err(format!("求解结果缺 workspace 成员 {}", member.name));
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
            .ok_or_else(|| format!("Dev 依赖 {} 没有已解版本", dep.package))?;
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
        .map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("发布 {} 失败: {e}", path.display()))
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
            // Cargo 会在后续依赖解析响亮报告环；这里保持确定顺序，不伪造诊断。
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
                "mirvm: lib 编译失败：根包 {} {}",
                manifest.name, manifest.version
            );
            Err(ExitCode::from(1))
        }
        Err(e) => {
            eprintln!("mirvm: lib 编译子进程启动失败：{e}");
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
                "mirvm: proc-macro 编译失败：根包 {} {}",
                manifest.name, manifest.version
            );
            Err(ExitCode::from(1))
        }
        Err(error) => {
            eprintln!("mirvm: proc-macro 编译子进程启动失败：{error}");
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
        eprintln!("mirvm: 创建测试配方目录 {} 失败: {e}", dir.display());
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
    let bytes = serde_json::to_vec(&recipe).expect("测试配方序列化失败");
    if std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&path, bytes).unwrap_or_else(|e| {
            eprintln!("mirvm: 写测试配方 {} 失败: {e}", path.display());
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

/// rustdoc `--test-builder` 入口。库 bundle 用真 rustc 生成带 MIR 的
/// metadata-only rlib；bin 先做真 rustc 类型检查，再把目标路径发布为
/// mirvm 启动器。rustdoc 因而仍能自己判断 compile_fail。
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
                eprintln!("mirvm doctest builder: 启动 rustc 失败: {error}");
                ExitCode::from(1)
            }
        };
    }
    if crate_type != "bin" {
        eprintln!("mirvm doctest builder: 不支持的 crate type `{crate_type}`");
        return ExitCode::from(1);
    }
    let Some(output) = arg_value(&args, "-o").map(PathBuf::from) else {
        eprintln!("mirvm doctest builder: bin 编译缺少 -o");
        return ExitCode::from(1);
    };
    let check_dir = output
        .parent()
        .unwrap_or(Path::new("."))
        .join("mirvm-check");
    if let Err(error) = std::fs::create_dir_all(&check_dir) {
        eprintln!(
            "mirvm doctest builder: 创建检查目录 {} 失败: {error}",
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
            eprintln!("mirvm doctest builder: 启动 rustc 检查失败: {error}");
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
            eprintln!("mirvm doctest builder: 配方序列化失败: {error}");
            return ExitCode::from(1);
        }
    };
    let tmp = recipe_path.with_extension(format!("tmp-{}", std::process::id()));
    if let Err(error) =
        std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &recipe_path))
    {
        eprintln!(
            "mirvm doctest builder: 发布配方 {} 失败: {error}",
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
                    "mirvm doctest builder: 替换输出 {} 失败: {error}",
                    output.display()
                );
                return ExitCode::from(1);
            }
        }
        let self_exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("mirvm doctest builder: current_exe 失败: {error}");
                return ExitCode::from(1);
            }
        };
        if let Err(error) = symlink(self_exe, &output) {
            eprintln!(
                "mirvm doctest builder: 创建启动器 {} 失败: {error}",
                output.display()
            );
            return ExitCode::from(1);
        }
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        eprintln!("mirvm doctest builder: runner 当前只支持 Unix 主机");
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

/// CLI 启动最早期用 argv[0] 识别 `CARGO_BIN_EXE_*` 启动器。
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
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建 bin 启动器目录 {} 失败: {e}", dir.display()))?;
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
    let bytes = serde_json::to_vec(&recipe).map_err(|e| format!("bin 配方序列化失败: {e}"))?;
    if std::fs::read(&recipe_path).ok().as_deref() != Some(bytes.as_slice()) {
        std::fs::write(&recipe_path, bytes)
            .map_err(|e| format!("写 bin 配方 {} 失败: {e}", recipe_path.display()))?;
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
                Err(e) => return Err(format!("替换 bin 启动器 {} 失败: {e}", launcher.display())),
            }
            symlink(self_exe, &launcher)
                .map_err(|e| format!("创建 bin 启动器 {} 失败: {e}", launcher.display()))?;
        }
        Ok(launcher)
    }
    #[cfg(not(unix))]
    {
        let _ = self_exe;
        Err("CARGO_BIN_EXE 启动器当前只支持 Unix 主机".into())
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
    cmd.arg("__cless-run-root")
        .arg(recipe)
        .args(args)
        .current_dir(cwd)
        .env("MIRVM_SYSROOT", sysroot);
    cmd.status().ok().and_then(|s| s.code()).unwrap_or(1)
}

/// `__cless-run-root <recipe> [program args]` 子进程入口。
pub fn run_root_recipe(mut argv: impl Iterator<Item = String>) -> ExitCode {
    let Some(path) = argv.next() else {
        eprintln!("mirvm: __cless-run-root 缺配方路径");
        return ExitCode::from(2);
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("mirvm: 读取测试配方 {path} 失败: {e}");
            return ExitCode::from(1);
        }
    };
    let recipe: RootRunRecipe = match serde_json::from_slice(&data) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mirvm: 测试配方 {path} 损坏: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = std::env::set_current_dir(&recipe.cwd) {
        eprintln!("mirvm: 测试工作目录 {} 不可进入: {e}", recipe.cwd.display());
        return ExitCode::from(1);
    }
    for (key, value) in recipe.env {
        // SAFETY: 独立子进程启动相，rustc/guest 线程尚未创建。
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

/// `mirvm run <frontmatter 脚本>`（MIRVM_DEPS=self）：正文物化到脚本缓存目录
/// （audit::script_cache_dir 同口径键），伪包 manifest 走同一 drive。
pub fn run_script(file: &Path, program_args: &[String], ignore_rust_version: bool) -> ExitCode {
    let mut manifest = script_manifest(file);
    manifest.ignore_rust_version = ignore_rust_version;
    drive(&manifest, program_args, None, None)
}

/// `mirvm pack <frontmatter 脚本>` 的默认 self 路径。
pub fn pack_script(file: &Path, out: &Path) -> ExitCode {
    let manifest = script_manifest(file);
    drive(&manifest, &[], None, Some(out))
}

fn script_manifest(file: &Path) -> PackageManifest {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mirvm: 读取脚本 {} 失败: {e}", file.display());
            std::process::exit(1);
        }
    };
    let Some((manifest_text, body)) = crate::cli::parse_frontmatter_pub(&text) else {
        // 路由层（cli.rs run_main）保证只在有 frontmatter 时进来；
        // 裸单文件是形态 3 快路径，不经此
        eprintln!("mirvm: {} 无 frontmatter（内部路由错误）", file.display());
        std::process::exit(2);
    };
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    let cache = super::audit::script_cache_dir(file);
    let src_dir = cache.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        eprintln!("mirvm: 创建脚本缓存目录 {} 失败: {e}", src_dir.display());
        std::process::exit(1);
    }
    // 布局与 cargo 腿物化项目同形（cli.rs materialize_script：正文在
    // <cache>/src/main.rs）——file!()/panic Location remap 后与 cargo 腿的
    // "src/main.rs" 逐字节同（redb_kv/gix_pure 实锤）；root 仍 = <cache>
    // （CARGO_MANIFEST_DIR 与 cargo 腿一致）。
    let main_rs = src_dir.join("main.rs");
    // write-if-changed：内容相同不重写——mtime 稳定是 cargo 指纹/L2 的共同前提
    // （cli.rs materialize_script 同款纪律）
    if std::fs::read(&main_rs)
        .ok()
        .is_none_or(|old| old != body.as_bytes())
        && let Err(e) = std::fs::write(&main_rs, &body)
    {
        eprintln!("mirvm: 写入 {} 失败: {e}", main_rs.display());
        std::process::exit(1);
    }
    match PackageManifest::from_frontmatter_at(stem, &manifest_text, &cache, &main_rs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mirvm: 解析 {} 的 frontmatter 失败: {e}", file.display());
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
    // 1. P1 求解器：lock 在按 lock（闭合），lock 缺席 pubgrub fresh 解
    let mut registry = match Registry::open_for(&manifest.lock_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mirvm: registry 打开失败: {e}");
            std::process::exit(1);
        }
    };
    let plan = match resolve(manifest, &mut registry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mirvm: 依赖解析失败: {e}");
            std::process::exit(1);
        }
    };
    let lock_path = manifest.lock_root.join("Cargo.lock");
    if !lock_path.is_file()
        && let Err(error) = write_lock_atomic(&lock_path, &plan.lock)
    {
        eprintln!("mirvm: 写入 {} 失败: {error}", lock_path.display());
        std::process::exit(1);
    }

    // 2. links 互斥（cargo 同：同一 links 值至多一个包；根包也参查）
    if let Err(e) =
        buildrs::check_links_unique(Some((&manifest.name, manifest.links.as_deref())), &plan)
    {
        eprintln!("mirvm: {e}");
        std::process::exit(1);
    }

    // 3. sysroot：MIRVM_SYSROOT 环境优先，否则自产（与 cli.rs run 路径同口径）
    let sysroot = match std::env::var_os("MIRVM_SYSROOT") {
        Some(p) => PathBuf::from(p),
        None => match crate::sysroot::ensure_sysroot() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mirvm: 构建 sysroot 失败: {e}");
                std::process::exit(1);
            }
        },
    };

    // 4. 指纹 + 编译段（compile_plan 抽取件，D15 P4 切⑥a——drive 与
    // sysroot 自管构建共用同一流水线；本段的 stamp/sysroot/rustflags 三
    // 输入在 drive 侧的来源注释见下行各段）
    let layout = Layout::new();
    // sysroot stamp 进指纹（sysroot 换代 ⇒ 全量重编）；ensure 之后必有值，
    // 缺值回退字面量不致命（后果只是 fp 粗一档，不引入新错误路径）
    let stamp = crate::sysroot::current_stamp_value()
        .unwrap_or_else(|| "sysroot-stamp-unknown".to_string());
    // rustflags（D15 P3 切⑤a）解析一次穿线到底：只进 target 侧参数
    // （dep/bin 末尾追加），指纹全 unit 统一吃（host 侧跟随失效无害，
    // v1 从简；解析/优先级/边界见 rustflags.rs 头注）
    let rustflags = match super::rustflags::from_env_and_disk(&manifest.root) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mirvm: rustflags 解析失败: {e}");
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
        // 第一枚编译错误（worker 回传原文）——与串行同形响亮点名后退出
        Err(e) => {
            eprintln!("mirvm: {e}");
            std::process::exit(1);
        }
    };
    let UnitTables { outputs, re_ran } = compiled.tables;
    let fps = compiled.fps;

    // 5b 起根 lib/bin 会话还需自家 exe（__cless-dep 通道）；unit 编译段的
    // self_exe 在 compile_plan 内部，这里单取
    let self_exe = std::env::current_exe().expect("current_exe 失败");

    // 5. 根包 build.rs 同生命周期（根不是 unit：边表取 plan.root_deps，
    // fp 单算；OUT_DIR/rustc-env/cfg 修正进 bin 会话）
    // 根 lib target（切⑤a full 层迁移面，hexyl 实锤）：[lib]+[[bin]] 双
    // target 时 bin 隐式依赖同名 lib——cargo 先把根 lib 编成 target rlib
    // 再让 bin --extern 它。fp 与根 build.rs 共用 root_fingerprint（同包
    // 同配方），故 fp 计算条件 = has_build_script || 有 lib target。
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
                eprintln!("mirvm: 根包指纹计算失败: {e}");
                std::process::exit(1);
            }
        };
        root_fp = Some(fp);
    }
    if manifest.has_build_script {
        let fp = root_fp.clone().expect("上一步已算");
        // 根的 ran 无人消费（根无下游，links 传不到它头上），只走观测行
        let (bo, _ran) = run_build_lifecycle_root(
            manifest, &plan, &fps, &layout, &fp, &outputs, &re_ran, false,
        );
        root_bo = Some(bo);
    }

    // 5b. 根 lib target 编译（__cless-dep 通道，fp 命中跳过；根 build.rs
    // 的 bo 修正与 OUT_DIR/rustc-env 同款注入——必须在根 build.rs 之后）
    if let Some((lib_name, lib_path, lib_pm)) = &root_lib {
        if *lib_pm {
            // proc-macro 根 lib + bin 组合（cargo 编 dylib 再 --extern）v1
            // 未接——响亮拒绝记档，不静默错编
            eprintln!(
                "mirvm: 根包 {} 是 proc-macro lib 且带 bin，组合未接（P5 边界）",
                manifest.name
            );
            std::process::exit(1);
        }
        let fp = root_fp.as_ref().expect("root_lib 在场必已算 fp");
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
            // 根包编译期 env（CARGO_PKG_* 全集 + manifest 两员，cargo 同）
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
                        "mirvm: lib 编译子进程启动失败（根包 {} {}）: {e}",
                        manifest.name, manifest.version
                    );
                    std::process::exit(1);
                }
            };
            if !status.success() {
                eprintln!(
                    "mirvm: lib 编译失败：根包 {} {}",
                    manifest.name, manifest.version
                );
                std::process::exit(1);
            }
        }
    }

    // 6. bin：根 crate 走既有 MirvmCallbacks 会话（after_analysis 停，零产物）
    let (bin_name, bin_path) = match manifest.runnable_bin_opt(bin_sel) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("mirvm: {e}");
            std::process::exit(1);
        }
    };
    // SAFETY: 单线程启动相（rustc 会话未起、引擎未跑），env 写入无并发读者。
    // bin 会话在同进程内（runner_main 的 env 回放同款 pattern）。
    unsafe {
        for (k, v) in &manifest.pkg_env {
            std::env::set_var(k, v);
        }
        std::env::set_var("CARGO_CRATE_NAME", bin_name.replace('-', "_"));
        std::env::set_var("CARGO_BIN_NAME", bin_name);
        std::env::set_var("CARGO_MANIFEST_DIR", &manifest.root);
        std::env::set_var("CARGO_MANIFEST_PATH", manifest.root.join("Cargo.toml"));
        if let (Some(bo), Some(fp)) = (&root_bo, &root_fp) {
            // 根 build.rs 的 rustc-env + OUT_DIR 进 bin 会话（env! 可读，cargo 同）
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
            root_fp.as_deref().expect("root_lib 在场必已算 fp"),
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
    // argv0 = 合成产物路径（cargo run 的 argv0 语义 = 最终二进制路径；本会话
    // 零产物，用 deps/<bin> 占位——guest 只见 argv 字符串，不读文件）
    let mut program_argv = vec![layout.deps.join(bin_name).display().to_string()];
    program_argv.extend(program_args.iter().cloned());
    // 全程不 chdir：guest cwd = 调用者 cwd，与 cargo run 语义一致（E36 闭合）
    if let Some(out) = pack_out {
        crate::cli::pack_driver(args, program_argv, out.to_path_buf())
    } else {
        crate::cli::run_driver(args, program_argv, false, None, false, true, None)
    }
}

/// compile_plan 的返回件：完成表 + unit 指纹表（drive 的根包阶段还要拿
/// fps 算根指纹——root_fingerprint 的 dep fp 成分；sysroot 构建不消费）。
pub struct CompiledPlan {
    pub tables: UnitTables,
    pub fps: Vec<String>,
}

/// unit 编译段（drive 原第 4 段，D15 P4 切⑥a 抽成共用件）：指纹 +
/// host/target/build 集合 + unit 级 Kahn 就绪队列并行调度（run_scheduler）
/// 跑完全部 unit 流水线。drive 与 sysroot 自管构建共用：
///
/// - drive 传 MIR sysroot 与其 stamp（本跑编译的消费底座）；
/// - sysroot 构建传 **toolchain sysroot** 与其盖戳——产出物不能当自己的
///   编译输入（编译 std 的 --sysroot 只能是发行版工具链，鸡生蛋）。
///
/// 失败 = 第一枚编译错误原文（调用方补「mirvm: 」前缀响亮退出，
/// 与抽取前逐字节同形）。
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
    // unit 级 Kahn 就绪队列并行调度（D15 P3 切⑤c）：unit 的全部依赖
    // 「完成」（build.rs 生命周期 + host/target 编译按集合归属全结束）
    // 即就绪；N 个 worker 各把领到的 unit 的完整流水线（build.rs 判定/
    // 执行 → 编译）跑完，完成表只在主线程汇集。
    for d in [&layout.deps, &layout.host_deps, &layout.build_root] {
        std::fs::create_dir_all(d).map_err(|e| format!("创建 {} 失败: {e}", d.display()))?;
    }
    let fps = schedule::fingerprints(plan, profile, stamp, rustflags)
        .map_err(|e| format!("依赖指纹计算失败: {e}"))?;
    let host_set = schedule::host_closure_for_root(plan, root_proc_macro);
    let target_set = schedule::target_units(plan);
    let build_set = schedule::build_closure(plan, root_has_build_script);
    let self_exe = std::env::current_exe().expect("current_exe 失败");
    let jobs = cless_jobs();
    let (dependents, mut indeg) = schedule::dep_graph(plan);
    // worker 共享的只读上下文（thread::scope 借用，调度期全程不可变——完成
    // 表不跨线程，无锁必要）。线程安全核对（切⑤c 设计钉）：
    // - driver 进程内**没有** rustc 会话——编译全在 __cless-dep/真 rustc
    //   子进程，worker 间无编译器全局状态；
    // - env 写入全在 Command 实例上（per-child，线程安全）；worker 内禁止
    //   std::env::set_var（全 crate 核对：set_var 只在 bin 阶段 = 汇合后
    //   主线程；build script 的 env 全走 Command.envs）。std::env::var
    //   读取（rerun 门 env_get、build_script_env 的 CARGO_HOME 等）与
    //   set_var 不并发，安全；
    // - 目录创建 create_dir_all 幂等；产物内容寻址（fp 盖戳），不同 unit
    //   不同 stem 无两名冲突；同 fp 重复 unit（同包同版本同 feature 的
    //   Normal/Build 双 unit——fp 不含 class 会撞名）由 FpLocks 把整个
    //   流水线互斥：后到者开工时产物已齐、rerun 门读存档跳过，与串行
    //   「先行者跑、后到者全跳过」逐字节同效；
    // - MIRVM_DEBUG_BLDRS 观测行与子进程诊断在 jobs>1 时允许交错（debug
    //   旋钮；对拍轴 = jobs=1 与串行同序 + 默认 N 的 corpus 判官——
    //   program 输出在汇合后的 bin 会话，天然串行）。
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

/// 并发度（切⑤c）：MIRVM_CLESS_JOBS 覆盖，缺省 available_parallelism
/// （拿不到回退 1）。**=1 时派发序与旧串行 topo 序逐位一致——对拍调试锚，
/// 钉**。非法值（非正整数）响亮拒绝退出。
fn cless_jobs() -> usize {
    match std::env::var("MIRVM_CLESS_JOBS") {
        Ok(raw) => match raw.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                eprintln!("mirvm: MIRVM_CLESS_JOBS={raw} 无效（应为正整数）");
                std::process::exit(1);
            }
        },
        Err(_) => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    }
}

/// worker 共享的只读上下文（thread::scope 借用；调度期全程不可变——完成表
/// 不跨线程，无锁必要）。线程安全核对明细见 drive() 第 4 段头注。
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
    /// 同 fp 重复 unit 的流水线互斥锁表（drive() 头注第三条）。
    fp_locks: FpLocks,
}

/// fp → 互斥锁的懒建表：同包同版本同 feature 的 Normal/Build 双 unit 的
/// fp 相同（fp 不含 class）会撞产物名/build 目录——整个流水线按 fp 互斥，
/// 后到者开工时产物已齐、rerun 门读存档跳过，与串行「先行者跑、后到者
/// 全跳过」同效。锁表本体只在取锁瞬间持有；唯一 fp 的锁零竞争。
#[derive(Default)]
struct FpLocks(std::sync::Mutex<BTreeMap<String, std::sync::Arc<std::sync::Mutex<()>>>>);

impl FpLocks {
    fn lock_for(&self, fp: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
        self.0
            .lock()
            .expect("fp 锁表中毒（内部错误）")
            .entry(fp.to_string())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
            .clone()
    }
}

/// 完成表（只归主线程所有：worker 开工所需的依赖侧输入——DEP_* env、
/// 传递 -L 汇集、links 重跑名单——由主线程在**派发时**从此表算好随
/// WorkMsg 带走；此刻全部依赖必已完成，取值与串行版在 unit 开头算的
/// 逐位相等）。compile_plan 的返回件（D15 P4 切⑥a 起 pub——drive 的
/// 根包阶段消费；sysroot 构建取 Ok 即罢不读字段）。
#[derive(Default)]
pub struct UnitTables {
    /// unit 下标 → 已执行的 BuildOutput（本 unit 编译修正 + 依赖者 -L
    /// 汇集 + 直接依赖者 build script 的 DEP_* 三处消费）。
    pub outputs: BTreeMap<usize, BuildOutput>,
    /// 本次会话真正重跑了 build.rs 的 unit（切⑤b 条件 4 links 传递：
    /// 直接依赖中带 links 的包在 re_ran ⇒ 依赖者也重跑，DEP_* 输入可能变）。
    pub re_ran: BTreeSet<usize>,
}

/// 一个 unit 的开工令（主线程派发时算好全部依赖侧输入，见 UnitTables 注；
/// fp 命中的 unit 也照算——纯计算无输出，换来 worker 零访问完成表）。
struct WorkMsg {
    ix: usize,
    /// 跑 build.rs 生命周期（has_build_script ∧ 在任一编译集；孤儿
    /// build-dep 不跑——父包没 build.rs 的那种 cargo 本不编译，跑它的
    /// build.rs 是越权执行）
    run_build: bool,
    /// 直接依赖的 DEP_* env（dep_metadata_env 同口径；run_build=false 时空）
    dep_env: BTreeMap<String, String>,
    /// 直接依赖中带 links 且本会话已重跑的包名（rerun 门条件 4）
    dep_links_reran: Vec<String>,
    /// 传递 -L 汇集（aggregate_link_searches 同口径；不在任何编译集时空）
    searches: Vec<String>,
}

/// 一个 unit 的完成回执（worker → 主线程）。
struct PerUnitDone {
    /// build.rs 产物（没跑 build.rs 的 unit 为 None）
    bo: Option<BuildOutput>,
    /// 本次是否真重跑了 build.rs
    ran: bool,
}

/// 派发令构造（主线程）：集合归属判定 + 依赖侧输入计算。
fn build_work_msg(ctx: &SharedCtx, t: &UnitTables, ix: usize) -> WorkMsg {
    let u = &ctx.plan.units[ix];
    let in_host = ctx.host_set.contains(&ix) || ctx.build_set.contains(&ix);
    let in_target = ctx.target_set.contains(&ix);
    let run_build = u.has_build_script && (in_host || in_target);
    let (dep_env, dep_links_reran) = if run_build {
        (
            buildrs::dep_metadata_env(ctx.plan, &u.deps, &t.outputs),
            // 条件 4 links 传递：直接依赖中带 links 且本次重跑了的包
            // （DEP_* 只给直接依赖者——传递再远一层由各层自己判定覆盖）
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

/// 一个 unit 的完整流水线（worker 线程）：fp 锁互斥（同 fp 重复 unit）→
/// build.rs 生命周期 → host 侧编译 → target 侧编译；命中的阶段照旧跳过
/// （**锁内**查盘——同 fp 先行者的产物必须可见才算命中）。失败回传错误
/// 原文（「mirvm: 」前缀由主线程汇合后补，与串行文案逐字节同形）。
fn run_unit_pipeline(msg: WorkMsg, ctx: &SharedCtx) -> Result<PerUnitDone, String> {
    let ix = msg.ix;
    let u = &ctx.plan.units[ix];
    let fp = &ctx.fps[ix];
    // 同 fp 重复 unit 互斥（锁中毒只可能来自先行 worker 恐慌——内部错误已
    // 在收尾，取内层继续，不叠加失败）
    let fp_mutex = ctx.fp_locks.lock_for(fp);
    let _fp_guard = fp_mutex.lock().unwrap_or_else(|e| e.into_inner());
    let stem = format!("lib{}-{}", u.lib_name, fp);
    let mut done = PerUnitDone {
        bo: None,
        ran: false,
    };
    // build.rs 生命周期（就绪判定保证其 build-deps 及其 build.rs 都已完成）
    if msg.run_build {
        let (bo, ran) = run_build_lifecycle(u, ix, ctx, msg.dep_env, msg.dep_links_reran)?;
        done.ran = ran;
        done.bo = Some(bo);
    }
    let bo = done.bo.as_ref();
    // host 侧：proc-macro 本体产 dylib；闭包普通单元（含 build-deps 闭包）产 host rlib
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
    // target 侧：照旧 __cless-dep（-Zno-codegen rlib）
    if ctx.target_set.contains(&ix) {
        // 指纹命中：内容寻址，同名产物即同内容，跳过
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

/// 一个 unit 的 build.rs 全生命周期：build script 编译（fp 命中跳过）→
/// rerun 判定（切⑤b：buildrs::should_rerun，cargo 同语义）——跳过则读
/// output.txt 重解析回放 BuildOutput，跑则以 cargo 兼容 env 执行并写存档
/// （执行成功后——失败本函数已回传错误，无半存档）→ (BuildOutput, 是否真跑)。
/// `dep_env`/`dep_links_reran` 由主线程派发时算好捎来（WorkMsg 注）。
/// 任何一步失败回传错误原文（主线程汇合后响亮点名）。
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
            "创建 build 目录 {} 失败（{} {}）: {e}",
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
        // 被编译的 crate 是 build script 本体（cargo 同：CARGO_CRATE_NAME
        // 跟着被编译 crate 走，不是所属包 lib 名）
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

/// 根包 build.rs 生命周期（根不是 unit：pkg_env/features/profile 由
/// manifest/plan 直供；根是本地 path 包，warning 照常显示）。
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
            "mirvm: 创建 build 目录 {} 失败（根包 {}）: {e}",
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
        // 根包编译期 env（CARGO_PKG_* 全集 + manifest 两员，cargo 同）
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
                    "mirvm: build script 编译子进程启动失败（根包 {} {}）: {e}",
                    manifest.name, manifest.version
                );
                std::process::exit(1);
            }
        };
        if !status.success() {
            eprintln!(
                "mirvm: build script 编译失败：根包 {} {}",
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
    // 根阶段在汇合后主线程跑——错误照旧响亮退出（文案与 worker 回传同形）
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

/// rerun 门（切⑤b）：判定（buildrs::should_rerun）→ 跳过则读存档
/// output.txt 重解析回放（指令流零序列化失真，warning 按同门控从缓存
/// 回放——cargo 同）；跑则执行 + 写 output.txt/rerun.txt 两份存档。
/// MIRVM_DEBUG_BLDRS=1 时向 stderr 打 `bldrs run|skip <pkg> <原因>` 观测行
/// （切⑤c：jobs>1 时各 worker 的观测行允许交错——debug 旋钮，非对拍面）。
/// 返回 (BuildOutput, 本次是否真跑)；失败回传错误原文（调用方补「mirvm: 」
/// 前缀响亮退出——主线程的根路径就地补，worker 路径汇合后补）。
// 平铺参数先例同 run_build_lifecycle
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
        // 跳过执行：output.txt 重解析即 BuildOutput（回放失败按损坏自愈落跑）
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
    // 存档写失败不致命——下次 no-record 重跑自愈（磁盘层故障前序编译写已
    // 先炸）；静默，不惊扰对拍 stderr
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

/// 执行 + 指令解析 + warning 回吐，返回 (BuildOutput, 原始 stdout)
/// （原始 stdout 供调用方写 output.txt 存档——回放靠重解析，零序列化失真）。
/// 失败回传错误原文（调用方补「mirvm: 」前缀响亮退出）。
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
        .map_err(|e| format!("build script 执行失败（{pkg} {ver}）: {e}"))?;
    let bo = buildrs::parse_instructions(&stdout)
        .map_err(|e| format!("build script 指令解析失败（{pkg} {ver}）: {e}"))?;
    show_warnings(pkg, ver, from_registry, &bo, quiet_build_warnings);
    Ok((bo, stdout))
}

/// cargo 编译期 env 契约（源码 env! 可读）：CARGO_PKG_* 全集 + crate/manifest
/// 三员（cargo 对每次 rustc 调用都设；host 真 rustc 与 __cless-dep 两侧同款）。
fn apply_unit_env(cmd: &mut std::process::Command, u: &Unit) {
    cmd.envs(u.pkg_env.iter());
    cmd.env("CARGO_CRATE_NAME", &u.lib_name);
    cmd.env("CARGO_MANIFEST_DIR", &u.source_dir);
    cmd.env(
        "CARGO_MANIFEST_PATH",
        u.source_dir.join("Cargo.toml").display().to_string(),
    );
}

/// 本 unit build script 的编译期 env 注入：OUT_DIR + rustc-env（cargo 对
/// 有 build script 的包编译时设；env! 可读）。
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

/// 编译子进程同步跑到底；启动/编译失败回传错误原文（what = 产物类别，
/// 点名 crate——主线程汇合后补「mirvm: 」前缀响亮退出，与串行文案同形）。
fn run_compile(cmd: &mut std::process::Command, u: &Unit, what: &str) -> Result<(), String> {
    let status = cmd.status().map_err(|e| {
        format!(
            "{what} 编译子进程启动失败（{} {}）: {e}",
            u.package, u.version
        )
    })?;
    if !status.success() {
        return Err(format!("{what} 编译失败：{} {}", u.package, u.version));
    }
    Ok(())
}
