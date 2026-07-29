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
use super::manifest::PackageManifest;
use super::registry::Registry;
use super::resolve::{ResolvePlan, Unit, resolve};
use super::schedule::{self, Layout};

/// `mirvm run <目录|Cargo.toml> [--bin <名>]`（MIRVM_DEPS=self）。
/// bin_sel = --bin 选定的 bin 名（D15 P4 切⑥b，cargo run --bin 语义）。
pub fn run_project(dir: &Path, program_args: &[String], bin_sel: Option<&str>) -> ExitCode {
    let manifest = match PackageManifest::read_dir(dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mirvm: 读取项目 {} 失败: {e}", dir.display());
            std::process::exit(1);
        }
    };
    drive(&manifest, program_args, bin_sel)
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
    let manifest =
        match PackageManifest::from_frontmatter_at(stem, &manifest_text, &cache, &main_rs) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("mirvm: 解析 {} 的 frontmatter 失败: {e}", file.display());
                std::process::exit(1);
            }
        };
    drive(&manifest, program_args, None)
}

fn drive(manifest: &PackageManifest, program_args: &[String], bin_sel: Option<&str>) -> ExitCode {
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
        // 根的 ran 无人消费（根无下游，links 传不到它头上），只走观测行
        let (bo, _ran) =
            run_build_lifecycle_root(manifest, &plan, &fps, &layout, &fp, &outputs, &re_ran);
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
    crate::cli::run_driver(args, program_argv, false, None, false, true)
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
    let host_set = schedule::host_closure(plan);
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
    re_ran: &BTreeSet<usize>,
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
            show_warnings(pkg, ver, from_registry, &bo);
            return Ok((bo, false));
        }
    }
    let (bo, stdout) = exec_and_parse(pkg, ver, from_registry, bexe, pkg_root, env)?;
    // 存档写失败不致命——下次 no-record 重跑自愈（磁盘层故障前序编译写已
    // 先炸）；静默，不惊扰对拍 stderr
    let _ = buildrs::write_record(bdir, &stdout, &bo, from_registry, pkg, pkg_root, &env_get);
    Ok((bo, true))
}

/// build script warning 回吐门控（cargo 同格式同口径：`warning: <pkg>@<ver>:
/// <msg>`；registry 包默认吞，path 包显示；执行与存档回放两路同款）。
fn show_warnings(pkg: &str, ver: &str, from_registry: bool, bo: &BuildOutput) {
    if !from_registry {
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
) -> Result<(BuildOutput, String), String> {
    let stdout = buildrs::run_build_script(bexe, cwd, env)
        .map_err(|e| format!("build script 执行失败（{pkg} {ver}）: {e}"))?;
    let bo = buildrs::parse_instructions(&stdout)
        .map_err(|e| format!("build script 指令解析失败（{pkg} {ver}）: {e}"))?;
    show_warnings(pkg, ver, from_registry, &bo);
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
