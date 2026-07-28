//! `cargoless/driver.rs` —— `mirvm run` 的零 cargo 新路径（D15 P2 切①/②/③，
//! 设计档 §3.6/§5 P2），替代 cargo_shim::phase_cargo 的三阶段（cargo run +
//! RUSTC_WRAPPER + runner 协议）：
//!
//! ```text
//! resolve（P1 求解器）→ links 互斥校验 → 逐 unit 按 topo 序调度：
//!   build.rs 全生命周期（切③）：host 真编译 build script（fp 命中跳过）
//!     → 以 cargo 兼容 env 执行（v1 粗指纹**每次都重跑**，rerun-if 精细化归
//!     P3，设计档 §5 P2 行）→ 指令解析 → BuildOutput 入表
//!   host 集（proc-macro 闭包 ∪ build-deps 闭包）→ spawn 真 rustc 真 codegen
//!   target 集 → 起 `__cless-dep` 子进程（cli::run_dep_compiler：
//!   in-process rustc_driver + global_asm 抽取）
//!   （双侧编译都吃本 unit BuildOutput 修正：cfg/check-cfg/link 旗进 argv，
//!   OUT_DIR/rustc-env 进子进程 env——proc-macro2 的 build.rs cfg 进其 host
//!   编译，serde_derive 类全链解锁的关键）
//! → 根包 build.rs 同生命周期 → bin 走既有 MirvmCallbacks 会话
//!   （OUT_DIR/rustc-env/cfg 修正同样进 bin 会话）
//! ```
//!
//! 传播规则（-l 只进本包、-L 进传递依赖者、metadata 只给直接依赖者的
//! build script、无自动 DEP_*_ROOT、无自动 check-cfg 补钉）全是切③ 实证
//! 结论，明细在 buildrs.rs 文件头。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::buildrs::{self, BuildOutput};
use super::manifest::PackageManifest;
use super::registry::Registry;
use super::resolve::{ResolvePlan, Unit, resolve};
use super::schedule::{self, Layout};

/// `mirvm run <目录|Cargo.toml>`（MIRVM_DEPS=self）。
pub fn run_project(dir: &Path, program_args: &[String]) -> ExitCode {
    let manifest = match PackageManifest::read_dir(dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mirvm: 读取项目 {} 失败: {e}", dir.display());
            std::process::exit(1);
        }
    };
    drive(&manifest, program_args)
}

/// `mirvm run <frontmatter 脚本>`（MIRVM_DEPS=self）：正文物化到脚本缓存目录
/// （audit::script_cache_dir 同口径键），伪包 manifest 走同一 drive。
pub fn run_script(file: &Path, program_args: &[String]) -> ExitCode {
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
    if let Err(e) = std::fs::create_dir_all(&cache) {
        eprintln!("mirvm: 创建脚本缓存目录 {} 失败: {e}", cache.display());
        std::process::exit(1);
    }
    let main_rs = cache.join("main.rs");
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
    let manifest = match PackageManifest::from_frontmatter(stem, &manifest_text, &main_rs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mirvm: 解析 {} 的 frontmatter 失败: {e}", file.display());
            std::process::exit(1);
        }
    };
    drive(&manifest, program_args)
}

fn drive(manifest: &PackageManifest, program_args: &[String]) -> ExitCode {
    // 1. P1 求解器：lock 在按 lock（闭合），lock 缺席 pubgrub fresh 解
    let mut registry = match Registry::open() {
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

    // 4. 指纹 + 拓扑序，逐 unit 按双侧编译集 + build.rs 生命周期调度（串行
    // v1；并行调度归后续切片）
    let layout = Layout::new();
    for d in [&layout.deps, &layout.host_deps, &layout.build_root] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("mirvm: 创建 {} 失败: {e}", d.display());
            std::process::exit(1);
        }
    }
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
    let fps = match schedule::fingerprints(&plan, &manifest.profile, &stamp, &rustflags) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mirvm: 依赖指纹计算失败: {e}");
            std::process::exit(1);
        }
    };
    let order = match schedule::topo_order(&plan) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("mirvm: {e}");
            std::process::exit(1);
        }
    };
    let host_set = schedule::host_closure(&plan);
    let target_set = schedule::target_units(&plan);
    let build_set = schedule::build_closure(&plan, manifest.has_build_script);
    let self_exe = std::env::current_exe().expect("current_exe 失败");
    // unit 下标 → 已执行的 BuildOutput（本 unit 编译修正 + 依赖者 -L 汇集 +
    // 直接依赖者 build script 的 DEP_* 三处消费）
    let mut outputs: BTreeMap<usize, BuildOutput> = BTreeMap::new();
    for ix in order {
        let u = &plan.units[ix];
        let stem = format!("lib{}-{}", u.lib_name, fps[ix]);
        // 4a. build.rs 生命周期：参与构建图的 unit 才跑（孤儿 build-dep——
        // 父包没 build.rs 的那种——cargo 本不编译，跑它的 build.rs 是越权
        // 执行）。topo 序保证其 build-deps（及其 build.rs）都已完成。
        if u.has_build_script
            && (host_set.contains(&ix) || target_set.contains(&ix) || build_set.contains(&ix))
        {
            let bo = run_build_lifecycle(u, ix, &plan, &manifest.profile, &fps, &outputs, &layout);
            outputs.insert(ix, bo);
        }
        let bo = outputs.get(&ix);
        // 4b. host 侧：proc-macro 本体产 dylib；闭包普通单元（含 build-deps
        // 闭包）产 host rlib
        if host_set.contains(&ix) || build_set.contains(&ix) {
            let hit = if u.proc_macro {
                layout
                    .host_deps
                    .join(format!("{stem}{}", std::env::consts::DLL_SUFFIX))
                    .is_file()
            } else {
                layout.host_deps.join(format!("{stem}.rmeta")).is_file()
                    && layout.host_deps.join(format!("{stem}.rlib")).is_file()
            };
            if !hit {
                let searches = buildrs::aggregate_link_searches(&plan, &u.deps, &outputs);
                let (args, what) = if u.proc_macro {
                    (
                        schedule::proc_macro_rustc_args(
                            &plan,
                            ix,
                            &manifest.profile,
                            &fps,
                            &layout,
                            bo,
                            &searches,
                        ),
                        "proc-macro",
                    )
                } else {
                    (
                        schedule::host_rustc_args(
                            &plan,
                            ix,
                            &manifest.profile,
                            &fps,
                            &layout,
                            bo,
                            &searches,
                        ),
                        "host dep",
                    )
                };
                let mut cmd = std::process::Command::new(&args[0]);
                cmd.args(&args[1..]);
                apply_unit_env(&mut cmd, u);
                apply_build_env(&mut cmd, &layout, u, &fps[ix], bo);
                run_compile(&mut cmd, u, what);
            }
        }
        // 4c. target 侧：照旧 __cless-dep（-Zno-codegen rlib）
        if target_set.contains(&ix) {
            if layout.deps.join(format!("{stem}.rmeta")).is_file()
                && layout.deps.join(format!("{stem}.rlib")).is_file()
            {
                // 指纹命中：内容寻址，同名产物即同内容，跳过
            } else {
                let searches = buildrs::aggregate_link_searches(&plan, &u.deps, &outputs);
                let args = schedule::dep_rustc_args(
                    &plan,
                    ix,
                    &manifest.profile,
                    &fps,
                    &sysroot,
                    &layout,
                    bo,
                    &searches,
                    &rustflags,
                );
                let mut cmd = std::process::Command::new(&self_exe);
                cmd.arg("__cless-dep").args(&args[1..]);
                apply_unit_env(&mut cmd, u);
                apply_build_env(&mut cmd, &layout, u, &fps[ix], bo);
                run_compile(&mut cmd, u, "dep");
            }
        }
    }

    // 5. 根包 build.rs 同生命周期（根不是 unit：边表取 plan.root_deps，
    // fp 单算；OUT_DIR/rustc-env/cfg 修正进 bin 会话）
    // 根 lib target（切⑤a full 层迁移面，hexyl 实锤）：[lib]+[[bin]] 双
    // target 时 bin 隐式依赖同名 lib——cargo 先把根 lib 编成 target rlib
    // 再让 bin --extern 它。fp 与根 build.rs 共用 root_fingerprint（同包
    // 同配方），故 fp 计算条件 = has_build_script || 有 lib target。
    let root_lib = manifest.targets.iter().find_map(|t| match t {
        super::manifest::Target::Lib {
            name,
            path,
            proc_macro,
        } => Some((name.clone(), path.clone(), *proc_macro)),
        _ => None,
    });
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
        let bo = run_build_lifecycle_root(manifest, &plan, &fps, &layout, &fp, &outputs);
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
    let (bin_name, bin_path) = match manifest.runnable_bin() {
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
    crate::cli::run_driver(args, program_argv, false, None, false, true)
}

/// 一个 unit 的 build.rs 全生命周期：build script 编译（fp 命中跳过）→
/// 以 cargo 兼容 env 执行（v1 粗指纹**每次都重跑**，rerun-if 精细化归 P3，
/// 设计档 §5 P2 行）→ 指令解析 → BuildOutput。任何一步失败响亮报错点名
/// crate 后退出。
fn run_build_lifecycle(
    u: &Unit,
    ix: usize,
    plan: &ResolvePlan,
    profile: &super::manifest::ProfileFlags,
    fps: &[String],
    outputs: &BTreeMap<usize, BuildOutput>,
    layout: &Layout,
) -> BuildOutput {
    let fp = &fps[ix];
    let bdir = layout.build_dir(&u.package, fp);
    if let Err(e) = std::fs::create_dir_all(bdir.join("out")) {
        eprintln!(
            "mirvm: 创建 build 目录 {} 失败（{} {}）: {e}",
            bdir.display(),
            u.package,
            u.version
        );
        std::process::exit(1);
    }
    let bexe = bdir.join(format!("build_script_build-{fp}"));
    if !bexe.is_file() {
        let args = schedule::build_script_rustc_args(plan, ix, profile, fps, layout);
        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        apply_unit_env(&mut cmd, u);
        // 被编译的 crate 是 build script 本体（cargo 同：CARGO_CRATE_NAME
        // 跟着被编译 crate 走，不是所属包 lib 名）
        cmd.env("CARGO_CRATE_NAME", "build_script_build");
        run_compile(&mut cmd, u, "build script");
    }
    let env = buildrs::build_script_env(&buildrs::ExecCtx {
        pkg_env: &u.pkg_env,
        source_dir: &u.source_dir,
        features: &u.features,
        profile,
        out_dir: &bdir.join("out"),
        dep_env: buildrs::dep_metadata_env(plan, &u.deps, outputs),
        ld_dirs: &[layout.host_deps.clone(), layout.deps.clone()],
    });
    exec_and_parse(
        &u.package,
        &u.version.to_string(),
        u.from_registry,
        &bexe,
        &u.source_dir,
        &env,
    )
}

/// 根包 build.rs 生命周期（根不是 unit：pkg_env/features/profile 由
/// manifest/plan 直供；根是本地 path 包，warning 照常显示）。
fn run_build_lifecycle_root(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    root_fp: &str,
    outputs: &BTreeMap<usize, BuildOutput>,
) -> BuildOutput {
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
        ld_dirs: &[layout.host_deps.clone(), layout.deps.clone()],
    });
    exec_and_parse(
        &manifest.name,
        &manifest.version.to_string(),
        false,
        &bexe,
        &manifest.root,
        &env,
    )
}

/// 执行 + 指令解析 + warning 回吐（cargo 同格式同口径：`warning: <pkg>@<ver>:
/// <msg>`；registry 包的 build.rs warning 默认吞，path 包显示）。
fn exec_and_parse(
    pkg: &str,
    ver: &str,
    from_registry: bool,
    bexe: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> BuildOutput {
    let stdout = match buildrs::run_build_script(bexe, cwd, env) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mirvm: build script 执行失败（{pkg} {ver}）: {e}");
            std::process::exit(1);
        }
    };
    let bo = match buildrs::parse_instructions(&stdout) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("mirvm: build script 指令解析失败（{pkg} {ver}）: {e}");
            std::process::exit(1);
        }
    };
    if !from_registry {
        for w in &bo.warnings {
            eprintln!("warning: {pkg}@{ver}: {w}");
        }
    }
    bo
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

/// 编译子进程同步跑到底；启动/编译失败响亮点名构造（what = 产物类别）后退出。
fn run_compile(cmd: &mut std::process::Command, u: &Unit, what: &str) {
    let status = match cmd.status() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "mirvm: {what} 编译子进程启动失败（{} {}）: {e}",
                u.package, u.version
            );
            std::process::exit(1);
        }
    };
    if !status.success() {
        eprintln!("mirvm: {what} 编译失败：{} {}", u.package, u.version);
        std::process::exit(1);
    }
}
