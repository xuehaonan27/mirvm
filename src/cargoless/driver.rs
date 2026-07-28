//! `cargoless/driver.rs` —— `mirvm run` 的零 cargo 新路径（D15 P2 切①/②，
//! 设计档 §3.6/§5 P2），替代 cargo_shim::phase_cargo 的三阶段（cargo run +
//! RUSTC_WRAPPER + runner 协议）：
//!
//! ```text
//! resolve（P1 求解器）→ 子集闸 → 逐 unit 按双侧编译集调度：
//!   host 集（proc-macro 闭包）→ spawn 真 rustc 真 codegen（.so/.rlib）
//!   target 集 → 起 `__cless-dep` 子进程（cli::run_dep_compiler：
//!   in-process rustc_driver + global_asm 抽取）
//! → bin 走既有 MirvmCallbacks 会话（cli::run_driver，after_analysis 停）
//! ```
//!
//! 切② 子集 = **无 build.rs** 的项目/脚本；proc-macro 及其 host 闭包已接
//! （真 rustc host 编译，target 侧 --extern 指 host-deps 的 .so）。子集外
//! 构造响亮拒绝点名（切③ = build.rs），绝不静默回退 cargo（P2 闭合契约，
//! 设计档 §5）。
//!
//! 留给后续切片的接缝：子集闸（buildrs 切片在此放行并接管 build.rs 调度）；
//! strip_build_units（build.rs 切片恢复 Build 类 unit 消费）。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::manifest::PackageManifest;
use super::registry::Registry;
use super::resolve::{ResolvePlan, Unit, UnitClass, resolve};
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

    // 2. 子集闸（切② = 无 build.rs；P2 契约：子集外响亮拒绝点名构造与机制，
    // 不静默回退 cargo）。根与每个 unit 都查——Build 类 unit 也查（宁响不漏：
    // 其父若真无 build.rs，cargo 本不编译它，这里多拒的是「声明了 build-deps
    // 却没有 build.rs」的病理包，误拒面可接受）。proc-macro 闭包同样过此闸：
    // 闭包内撞 build.rs（真实 serde_derive 的 proc-macro2 实锤）照旧响亮点名
    // ——设计如此，build.rs 调度归切③。
    for u in &plan.units {
        if u.has_build_script {
            eprintln!(
                "mirvm: D15 P2 切② 未接 build.rs：{} {}（等切③；可暂用 MIRVM_DEPS=cargo）",
                u.package, u.version
            );
            std::process::exit(1);
        }
    }
    if manifest.has_build_script {
        eprintln!(
            "mirvm: D15 P2 切② 未接 build.rs：根包 {} {}（等切③；可暂用 MIRVM_DEPS=cargo）",
            manifest.name, manifest.version
        );
        std::process::exit(1);
    }

    // 闸过后丢弃全部 Build 类 unit：Build 边的唯一消费者是 build.rs，已被闸掉
    // （cargo 同语义——无 build script 的包其 build-deps 本不编译）
    let plan = strip_build_units(plan);

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

    // 4. 指纹 + 拓扑序，逐 unit 按双侧编译集调度（串行 v1；并行调度归后续
    // 切片）：host 集 spawn 真 rustc 真 codegen，target 集照旧 __cless-dep；
    // 同一 unit 两侧都在就两发（双用 lib，产物分目录互不影响）。
    let layout = Layout::new();
    for d in [&layout.deps, &layout.host_deps] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("mirvm: 创建 {} 失败: {e}", d.display());
            std::process::exit(1);
        }
    }
    // sysroot stamp 进指纹（sysroot 换代 ⇒ 全量重编）；ensure 之后必有值，
    // 缺值回退字面量不致命（后果只是 fp 粗一档，不引入新错误路径）
    let stamp = crate::sysroot::current_stamp_value()
        .unwrap_or_else(|| "sysroot-stamp-unknown".to_string());
    let fps = match schedule::fingerprints(&plan, &manifest.profile, &stamp) {
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
    let self_exe = std::env::current_exe().expect("current_exe 失败");
    for ix in order {
        let u = &plan.units[ix];
        let stem = format!("lib{}-{}", u.lib_name, fps[ix]);
        // host 侧：proc-macro 本体产 dylib；闭包普通单元产 host rlib
        if host_set.contains(&ix) {
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
                let (args, what) = if u.proc_macro {
                    (
                        schedule::proc_macro_rustc_args(
                            &plan,
                            ix,
                            &manifest.profile,
                            &fps,
                            &layout,
                        ),
                        "proc-macro",
                    )
                } else {
                    (
                        schedule::host_rustc_args(&plan, ix, &manifest.profile, &fps, &layout),
                        "host dep",
                    )
                };
                let mut cmd = std::process::Command::new(&args[0]);
                cmd.args(&args[1..]);
                apply_unit_env(&mut cmd, u);
                run_compile(&mut cmd, u, what);
            }
        }
        // target 侧：照旧 __cless-dep（-Zno-codegen rlib）
        if target_set.contains(&ix) {
            if layout.deps.join(format!("{stem}.rmeta")).is_file()
                && layout.deps.join(format!("{stem}.rlib")).is_file()
            {
                // 指纹命中：内容寻址，同名产物即同内容，跳过
            } else {
                let args =
                    schedule::dep_rustc_args(&plan, ix, &manifest.profile, &fps, &sysroot, &layout);
                let mut cmd = std::process::Command::new(&self_exe);
                cmd.arg("__cless-dep").args(&args[1..]);
                apply_unit_env(&mut cmd, u);
                run_compile(&mut cmd, u, "dep");
            }
        }
    }

    // 5+6. bin：根 crate 走既有 MirvmCallbacks 会话（after_analysis 停，零产物）
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
    }
    let args =
        schedule::bin_rustc_args(manifest, &plan, &fps, &sysroot, &layout, bin_name, bin_path);
    // argv0 = 合成产物路径（cargo run 的 argv0 语义 = 最终二进制路径；本会话
    // 零产物，用 deps/<bin> 占位——guest 只见 argv 字符串，不读文件）
    let mut program_argv = vec![layout.deps.join(bin_name).display().to_string()];
    program_argv.extend(program_args.iter().cloned());
    // 全程不 chdir：guest cwd = 调用者 cwd，与 cargo run 语义一致（E36 闭合）
    crate::cli::run_driver(args, program_argv, false, None, false, true)
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

/// 丢弃 Build 类 unit 并重映射所有边下标（drive 子集闸之后调用，理由见闸注释；
/// build.rs 切片恢复 Build 类 unit 的调度时删除本函数）。
fn strip_build_units(mut plan: ResolvePlan) -> ResolvePlan {
    let mut remap: Vec<Option<usize>> = vec![None; plan.units.len()];
    let mut units = Vec::with_capacity(plan.units.len());
    for (old, u) in plan.units.into_iter().enumerate() {
        if u.class == UnitClass::Build {
            continue;
        }
        remap[old] = Some(units.len());
        units.push(u);
    }
    for u in &mut units {
        u.deps.retain_mut(|d| match remap[d.unit] {
            Some(n) => {
                d.unit = n;
                true
            }
            None => false,
        });
    }
    plan.root_deps.retain_mut(|d| match remap[d.unit] {
        Some(n) => {
            d.unit = n;
            true
        }
        None => false,
    });
    plan.units = units;
    plan
}
