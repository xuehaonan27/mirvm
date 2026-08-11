//! CLI 与 rustc 驱动薄壳。三种运行形态：
//! - `mirvm run <脚本|项目>`：用户入口
//! - `mirvm <rustc> <args...>`（MIRVM_CARGO_SESSION 下）：cargo 的 RUSTC_WRAPPER
//! - `mirvm runner <假二进制> <args...>`：cargo 的 target runner，真正的解释入口
//!
//! 引擎 = M4 字节码 VM（加载相 lower + 执行相 engine）。tier-0（rustc InterpCx）已于
//! 2026-07-09 移除——代码在 git 历史（tag 前缀 feat: M4.3 之前），差分 oracle 一直是
//! native 编译直跑（`differential.programs`）。

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
    mirvm run <file.rs>  [OPTIONS] [-- <program args>]   # 单文件（可带 frontmatter 依赖）
    mirvm run <x.mirvm>  [OPTIONS] [-- <program args>]   # 跑 .mirvm 包（mode B 片②）
    mirvm pack <target>  [-o out.mirvm]                  # cargo 项目 / 脚本 / 单文件 → .mirvm 包
    mirvm run <dir | Cargo.toml> [-- <program args>]     # cargo 项目（依赖自动构建为 MIR rlib）
    mirvm test [dir | Cargo.toml] [OPTIONS] [TESTNAME] [-- <libtest args>]
    mirvm cache status                                   # 本地仓库各组件体量 + 陈代体量
    mirvm cache purge [--dry-run]                        # 默认 = 清陈代（deps/base/ir 非本 build 代）
    mirvm cache purge --deps|--base|--ir                 # 对应族全清（所有代）
    mirvm cache purge --scripts                          # scripts/ 全清（物化项目清单）
    mirvm cache purge --target                           # 统一 target dir 全清（共享依赖存储，最大件）
    mirvm cache purge --all [--sysroot]                  # 除 sysroot 外全清；加旗连 sysroot（完全冷启动）

OPTIONS:
    --dump-mir        打印 entry fn 的 MIR 后退出（仅单文件直通模式）
    --edition <ED>    默认 2024（仅单文件直通模式）
    --sysroot <PATH>  使用指定 sysroot（默认：自动构建带全量 MIR 的缓存 sysroot）
    --vm-call <SPEC>  直接调导出函数（gate/调试入口），如 'fib(25)'；缺省跑 main 启动链
    --vm-stats        打印 Trap 债务统计（每期开工前的调研仪器）后退出
    --stack-size <N>  guest 主执行栈虚拟保留（默认 1g；接受 k/m/g 后缀，JVM -Xss 同位）
    --jit <on|off>    方法级 JIT（M5.3–M5.5 全收，默认 on；off = 纯解释对拍口径）
    --ignore-rust-version  忽略 package.rust-version（项目/依赖脚本，Cargo 同名语义）

ENV:
    MIRVM_HOME        本地仓库根（默认 $HOME/.mirvm；sysroot/scripts/target/各缓存族所在）
    MIRVM_TARGET_DIR  mirvm 构建统一 target dir 改址（默认 $MIRVM_HOME/target/mirvm）
    MIRVM_SYSROOT     等价于 --sysroot
    MIRVM_STACK_SIZE  等价于 --stack-size（cargo 项目形态经环境传给 runner）
    MIRVM_JIT         等价于 --jit（off/0 = 纯解释对拍口径）
    MIRVM_JIT_THRESHOLD 编译触发阈值（默认 1000；诊断用）
    MIRVM_JIT_SYNC    =1 时 JIT 验证模式：投递后等待发布/失败，可准入编译失败响亮
                      终止（gate 用；证明 threshold=1 差分真跑机器码）
    MIRVM_JIT_STATS   =1 时进程退出经 atexit 打 JIT 助手频度统计（诊断用）
    MIRVM_CARGO_LOCKED 置位时 frontmatter/脚本项目按 --locked 构建（依赖锁定；
                      未置位 = clean 环境可重解析，见 open-issues G7）
    MIRVM_DEPS        =cargo 时项目/脚本走长期保留的 cargo 三阶段 compat 轨
                      （用户回退 + 行为对拍）；**缺省/=self 走零 cargo 自有
                      调度**（D15 cargoless driver，P4 默认翻转：依赖解析/编译
                      调度/build.rs/proc-macro/rustflags/rerun-if 增量/并行调度
                      全生命周期；mirvm test 已支持 resolver=2/3 常见 workspace，
                      替代 registry、常见 source replacement/patch/replace 与
                      pack 共用该路径；resolver 1 等边界响亮拒绝）
    MIRVM_CLESS_JOBS  =N 时 cargoless 编译调度并发度（缺省 = 核数；=1 退化为
                      拓扑序串行，对拍调试用）
    MIRVM_TIMING      =1 时向 stderr 输出相位账本（frontend/lower/engine/total）
    MIRVM_NO_IR_CACHE =1 时旁路 L2 engine-IR 缓存（读写全禁；诊断/对拍用）
    MIRVM_NO_BASE_IMAGE =1 时旁路 std 预降低底座（全量冷降低；诊断/对拍用）

DEV:
    mirvm spike1..5   跑已冻结的 M4 前置 spike（回归自检；见 docs/history/spike*.md）
    MIRVM_JIT_DEBUG   =1 时 JIT 编译线程打 收到/发布 流水（刻意的诊断旋钮）
    MIRVM_JIT_DEBUG_DUMP =1 时转储编译失败函数的 CLIF（叠加 MIRVM_JIT_DEBUG）
    MIRVM_SEGV_DUMP   =1 时 SIGSEGV 打印 fault RIP（JIT 码崩点定位）
";

pub fn main() -> ExitCode {
    // 排障旋钮（M5.4b）：SIGSEGV 时打印 fault RIP，用于 JIT 码崩点定位。
    if std::env::var_os("MIRVM_SEGV_DUMP").is_some() {
        crate::os::signal::install_segv_dump();
    }
    let mut argv = std::env::args();
    let argv0 = argv.next().unwrap_or_default();

    // `CARGO_BIN_EXE_*` 的 self 启动器是指向 mirvm 的符号链接，旁边带根 bin
    // 配方。必须在普通命令分派前识别，否则会把 guest 参数误当成 mirvm 命令。
    if let Some(recipe) = crate::cargoless::driver::root_launcher_recipe(Path::new(&argv0)) {
        return crate::cargoless::driver::run_root_recipe(
            std::iter::once(recipe.display().to_string()).chain(argv),
        );
    }
    let Some(first) = argv.next() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };

    // cargo 会话中的两个回调形态
    if first == "runner" {
        return runner_main(argv);
    }
    // S4 底座构建子进程（必须先于 MIRVM_CARGO_SESSION 分流：runner 里触发的
    // 构建子进程带着 cargo 会话环境，不能被误路由进 phase_wrapper）
    if first == "__build-base-image" {
        return crate::baseimage::build_main(argv);
    }
    // D15：cargoless dep 编译子进程（cargoless::driver 的调度落点；
    // 同样必须先于 MIRVM_CARGO_SESSION 分流）
    if first == "__cless-dep" {
        return run_cless_dep(argv.collect());
    }
    if first == "__cless-run-root" {
        return crate::cargoless::driver::run_root_recipe(argv);
    }
    if std::env::var_os("MIRVM_CARGO_SESSION").is_some() {
        // RUSTC_WRAPPER：first = 真 rustc 路径
        cargo_shim::phase_wrapper(std::iter::once(first).chain(argv));
    }

    match first.as_str() {
        "run" => run_main(argv),
        "test" => test_main(argv),
        "pack" => pack_main(argv),
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

/// `mirvm test [项目] [Cargo 选择参数/TESTNAME] [-- libtest 参数]`。
/// 项目参数只在第一槽识别；缺省当前目录。Cargo 兼容轨保留原参数逐字解释，
/// self 轨在 cargoless::driver 内按同一合同解析。
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

// ===== 用户入口 =====

/// `mirvm pack <target> [-o out.mirvm]`（mode B 片②，designs/modeb-mirvmar-design.md）：
/// cargo 项目（目录/Cargo.toml）、frontmatter 脚本、纯单文件 → .mirvm 包。
/// 项目/frontmatter 缺省走 cargoless；`MIRVM_DEPS=cargo` 经 MIRVM_PACK
/// 传入 runner。两条路径都强制全量冷路径，保证包自包含。
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

    // 项目形态：缺省走自有调度；Cargo 轨只在显式回退时进入 runner。
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
        // SAFETY: 单线程启动相。
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
        // SAFETY: 单线程启动相。
        unsafe { set_cargo_pack_env(&out_abs) };
        cargo_shim::phase_cargo(&dir, &[], None, false);
    }

    // 纯单文件：直接 pack_driver（与 run 的形态 3 同参）
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
    // SAFETY: caller 保证仍处于 CLI 单线程启动相。
    unsafe {
        std::env::set_var("MIRVM_PACK", out);
        std::env::set_var("MIRVM_NO_BASE_IMAGE", "1");
        std::env::set_var("MIRVM_NO_DEPS_IMAGE", "1");
    }
}

/// `mirvm cache status|purge …`：本地仓库（$HOME/.mirvm，MIRVM_HOME 可改址）管理。
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
            // 无旗默认 = 清陈代（保守面）；任何目标旗在场则按旗走
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

/// `mirvm deps audit <目标...>`（D15 P1 审计工具）：目标 = 项目目录（含
/// Cargo.toml）或 frontmatter 脚本；逐目标 resolve 并与对照 lock 对账，
/// 任一目标解析失败或对账失配即非零退出。
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
                    println!("SKIP {}（needs 缺席，与 gate 同口径不算失败）", report.name);
                    continue;
                }
                let head = format!(
                    "{}（{} 模式，{} 单元，{} 包版本）",
                    report.name,
                    report.mode,
                    report.units,
                    report.plan.version_map.len()
                );
                // 判负条件：项目 = 对账等值；脚本 = cargo 验收链
                let mut fail: Option<String> = None;
                if report.mode == "lock"
                    && let Some((lock_desc, mismatches)) = &report.lock_check
                    && !mismatches.is_empty()
                {
                    fail = Some(format!("对账失配 {} 条 vs {lock_desc}", mismatches.len()));
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
                        println!("FAIL {head}：{why}");
                        failures += 1;
                    }
                    None => {
                        print!("OK   {head}");
                        if let Some((lock_desc, mismatches)) = &report.lock_check {
                            if mismatches.is_empty() {
                                print!("；对账 == {lock_desc}");
                            } else if report.mode == "fresh" {
                                print!(
                                    "；历史对照 {} 条时间漂移（信息级，非判负）",
                                    mismatches.len()
                                );
                            }
                        }
                        if report.acceptance.is_some() {
                            print!("；cargo --locked --offline 接受");
                        }
                        println!();
                    }
                }
            }
            Err(e) => {
                // 仍明确归入 P5 的响亮拒绝是事先明说的边界，不算普通解析失败。
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
    println!("deps audit: {} 目标，{} 失败", targets.len(), failures);
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
                eprintln!("mirvm: {name} needs argument(s)");
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
            // 兼容旧 gate 脚本：--engine vm 是唯一引擎，吞掉参数即可
            "--engine" => {
                let e = next("--engine");
                if e != "vm" {
                    eprintln!("mirvm: 引擎 `{e}` 已不存在（tier-0 已移除；唯一引擎 = vm）");
                    exit(2);
                }
            }
            "--vm-call" => vm_call = Some(next("--vm-call")),
            "--vm-stats" => vm_stats = true,
            // D15 P4 切⑥b：cargo run --bin 语义（项目形态；脚本/单文件无此概念）
            "--bin" => bin_sel = Some(next("--bin")),
            "--ignore-rust-version" => ignore_rust_version = true,
            "--stack-size" => {
                let v = next("--stack-size");
                parse_stack_size(&v); // 先验证再落 env（错在入口就响）
                // 落 env 让 cargo 形态（wrapper→runner 子进程）同一旋钮生效。
                // 此刻仍是单线程启动相（rustc 会话尚未开始）。
                unsafe { std::env::set_var("MIRVM_STACK_SIZE", v) };
            }
            "--jit" => {
                let v = next("--jit");
                if v != "on" && v != "off" {
                    // TODO: tiered JIT?
                    eprintln!("mirvm: --jit only accepts on|off (got `{v}`)");
                    exit(2);
                }
                // 同 --stack-size：落 env 使 cargo 形态经 runner 生效
                unsafe { std::env::set_var("MIRVM_JIT", v) };
            }
            _ if input.is_none() && !arg.starts_with('-') => input = Some(arg),
            _ => {
                eprintln!("mirvm: unknown argument `{arg}`\n{USAGE}");
                exit(2);
            }
        }
    }
    let Some(input) = input else {
        eprint!("{USAGE}");
        exit(2);
    };
    let input_path = PathBuf::from(&input);

    // D15 P4 默认翻转：缺省 = self 零 cargo 自有调度（cargoless::driver）；
    // =cargo 显式走长期保留的 cargo 三阶段 compat 轨（用户回退 + 行为对拍）；
    // 两轨各自完整，其他值响亮报错
    let deps_self = match std::env::var("MIRVM_DEPS").as_deref() {
        Err(_) | Ok("self") => true,
        Ok("cargo") => false,
        Ok(other) => {
            eprintln!("mirvm: MIRVM_DEPS only accepts `cargo` or `self` (got `{other}`)");
            exit(2);
        }
    };

    // 形态 1：cargo 项目（目录或 Cargo.toml）
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
        // 脚本/单文件/包形态无 --bin 概念（cargo script 同）——响亮拒绝不静默吞
        eprintln!("mirvm: --bin {b} 只适用于 cargo 项目形态（目录/Cargo.toml）");
        exit(2);
    }

    // mode B 片②：.mirvm 包嗅探（先于文本读取——包是二进制）
    if crate::pack::is_package(&input_path) {
        let module = match crate::pack::load_package(&input_path) {
            Ok(p) => p.module,
            Err(reason) => {
                eprintln!("mirvm: fail to load {}: {reason}", input_path.display());
                exit(70);
            }
        };
        // warm 后半段与 run_driver 热路径同形（空 image 栈：asm 配方幂等重物化）
        let mut module = module;
        module.asm_stub_addrs = crate::lower::asm::materialize(&module.asm_sites);
        let mut program_argv = vec![input];
        program_argv.extend(program_args);
        let code = run_vm_engine(module, &program_argv, vm_call.as_deref(), vm_stats);
        exit(code);
    }

    let src = std::fs::read_to_string(&input_path).unwrap_or_else(|e| {
        eprintln!("mirvm: fail to read {input}: {e}");
        exit(1);
    });

    // 形态 2：带 frontmatter 依赖声明的单文件脚本 → 物化成 cargo 项目
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

    // 形态 3：纯单文件，零 cargo 快路径（M1 同款）
    let sysroot = sysroot
        .or_else(|| std::env::var("MIRVM_SYSROOT").ok())
        .unwrap_or_else(|| match crate::sysroot::ensure_sysroot() {
            Ok(p) => p.display().to_string(),
            Err(e) => {
                eprintln!("mirvm: fail to build sysroot: {e}");
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

// ===== cargo runner 回调 =====

fn runner_main(argv: impl Iterator<Item = String>) -> ExitCode {
    let guest_process = GuestProcessState::from_cargo_runner();
    let (rustc_args, program_argv, env) = cargo_shim::parse_runner_invocation(argv);
    // rustc 前端必须重演 wrapper 录下的构建环境；来宾执行前会完整恢复
    // runner 刚启动时的运行环境，不能让这层覆盖进入 guest。
    install_recorded_build_environment(env);
    // 构建期环境优先（env!() 展开、CARGO_* 等在编译会话里要可见）。
    // CARGO_MAKEFLAGS 指向已消亡的 jobserver，透传会招警告（cargo-miri 同款处理）。
    // mode B 片②：pack 会话（mirvm pack 经 phase_cargo 以 MIRVM_PACK 传入
    // 输出路径）——强制全量冷路径保包自包含（空 image 栈 + 旁路 L2/deps-image
    // 由 mirvm pack 以 MIRVM_NO_BASE_IMAGE/MIRVM_NO_DEPS_IMAGE 同进 env）
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

    fn enter(&self) {
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
            eprintln!(
                "mirvm: 无法进入 Cargo 调用者目录 {}: {error}",
                cwd.display()
            );
            exit(1);
        }
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

/// mode B 片②：pack 驱动——run_driver 冷半的同构（空 image 栈、无 L2 查询），
/// 岔口在 callbacks.pack_out：after_analysis 末尾落 .mirvm 包代替执行。
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

// ===== cargo wrapper：target 依赖的 in-process 编译（S2 / D9d）=====

/// 依赖编译回调：分析后显式跑 mono 收集再放行。native 构建在 codegen 期做
/// post-mono const-eval（собирает时求值 required consts），`-Zno-codegen` 跳过 codegen
/// 会连这层构建期错误一起漏掉（依赖里死代码的 const 恐慌等，native cargo build 会红）
/// ——cargo-miri 的 dummy backend 同款显式补齐，保住"依赖构建错误面与 native 一致"。
///
/// C4（decision-history §7.22）：同一次 mono 收集顺带抽取 dep crate 的
/// global_asm/naked 文本落 rlib 旁挂清单（`<rlib 主名>.mirasm.s`）——mirvm
/// 就是 dep crate 的编译器（after_analysis 的 HIR 在手），无需从 rmeta/rlib
/// 抠模板；汇编动作留 bin 加载相同一 assemble 通道（缓存自愈随之免费）。
/// 无 asm 的 crate（99%）纯 mono 扫描，边际零成本。
struct DepCallbacks {
    /// `<out-dir>`（rlib 所在目录）
    out_dir: String,
    /// rlib 主名 = `lib<crate_name><extra-filename>`（extra-filename 含前导 `-`）
    rlib_stem: String,
}

impl Callbacks for DepCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let _ = tcx.collect_and_partition_mono_items(());
        match crate::lower::global_asm::materialize_dep_text(tcx) {
            Ok(crate::lower::global_asm::DepAsmText::Text(text)) => {
                let path = format!("{}/{}.mirasm.s", self.out_dir, self.rlib_stem);
                // 原子发布（全仓同款纪律）
                let tmp = format!("{path}.tmp{}", std::process::id());
                std::fs::write(&tmp, text)
                    .unwrap_or_else(|e| panic!("fail to write dep global_asm list: {e}"));
                std::fs::rename(&tmp, &path)
                    .unwrap_or_else(|e| panic!("fail to release dep global_asm list: {e}"));
            }
            // UnsupportedSym：跳过清单（C4 片①语义边界，见 global_asm.rs
            // DepAsmText 文档——不因「可能不用」拖垮整个 dep 构建）
            Ok(crate::lower::global_asm::DepAsmText::UnsupportedSym)
            | Ok(crate::lower::global_asm::DepAsmText::None) => {}
            Err(reason) => panic!("fail to extract dep global_asm: {reason}"),
        }
        Compilation::Continue
    }
}

/// target 依赖：真 rustc 语义 + `-Zno-codegen`。rustc_interface::start_codegen 对
/// no-codegen 有树内现成空转（空 CompiledModules；rmeta 编码在 backend 之外不受影响；
/// Linker::link 照走默认 link_binary 产出 metadata-only rlib，cargo 与下游 --extern
/// 无感）。in-process 驱动（进程本就链着 librustc_driver）顺带省一次 rustc exec。
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

/// D15：cargoless dep 编译子进程入口——补回 argv0 后喂 run_dep_compiler
/// （参数由 cargoless::schedule::dep_rustc_args 计算，cargoless::driver 调度；
/// 与 cargo_shim wrapper 段共用同一 DepCallbacks/global_asm 抽取通道）。
fn run_cless_dep(rest: Vec<String>) -> ExitCode {
    let mut args = Vec::with_capacity(rest.len() + 1);
    args.push("mirvm-cless-rustc".to_string());
    args.extend(rest);
    run_dep_compiler(args)
}

// ===== 共享驱动 =====

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

/// 会话 guest 可见告警计数（M6 片2）：**有告警的编译不入 L2 缓存**。warm 路径跳过
/// rustc 会话，无法重演诊断——静默吞告警违反 run-from-source 语义（native 差分口径
/// = 每次新鲜编译必发告警）。告警程序每跑冷路径重演；零告警程序才享受缓存。
///
/// 安装时机 = `psess_created`（Session 建成、任何解析之前）：rustc_interface 的
/// setup_callbacks 会覆写 TRACK_DIAGNOSTIC 喂增量查询系统，psess_created 在其后触发，
/// 此处链式保存并委派前钩（与 runner 收尾过滤器同一机制，三钩可叠）。
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
    /// 相位计时（M6 片1，D9f①）：t_start = run_driver 进入时刻
    t_start: std::time::Instant,
    timing: PhaseTiming,
    /// L2 缓存键素材（M6 片2）：与 run_compiler 所见完全一致的参数
    rustc_args: Vec<String>,
    /// S4/S3′ image 栈：after_analysis 验降低指纹后供 lower 并集查找；run_driver 尾部 absorb。
    stack: crate::baseimage::ImageStack,
    /// A2 split 产物（s3b-a2-design；`MIRVM_DEPS_IMAGE=1` 且底座在场时由 lower_program 产出）；
    /// run_driver 尾部 push 上栈再 absorb。
    split_image: Option<crate::lower::SplitImage>,
    /// 本会话降低指纹（after_analysis 记录；split_image 包装栈层时用）
    session_fp: Option<(bool, bool, bool)>,
    /// A2：本会话起手是否已装载 deps-image（已装载 ⇒ 不再 split 重建）
    deps_image_loaded: bool,
    /// mode B 片②：pack 输出路径（Some ⇒ after_analysis 末尾落 .mirvm 包
    /// 代替执行；None = 常规 run 语义）
    pack_out: Option<std::path::PathBuf>,
}

/// 加载相计时账本（M6 片1）。frontend = 驱动进入→analysis 完成（含依赖 metadata 加载），
/// lower = mono 收集+降低+冻结物化。engine 段由 run_driver 在解释结束后补记。
/// M6 片2：cache_load = L2 命中反序列化+校验（热路径整体替代 frontend+lower）；
/// cache_store = 冷路径洁净快照入账。
#[derive(Default)]
struct PhaseTiming {
    frontend: Option<std::time::Duration>,
    lower: Option<std::time::Duration>,
    cache_load: Option<std::time::Duration>,
    cache_store: Option<std::time::Duration>,
}

/// `MIRVM_TIMING=1`（或 --vm-stats 仪器）时输出单行相位账本到 stderr。
/// 默认关闭——stderr 参与 native 差分逐字节比对，不能引入噪声。
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
    eprintln!("{line}");
}

impl Callbacks for MirvmCallbacks {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        // 告警计数钩（L2 入账前提）：psess_created 在 interface 覆写 TRACK_DIAGNOSTIC
        // 之后、首次解析之前触发——全会话诊断零缺口。
        config.psess_created = Some(Box::new(|_psess| install_warning_counter()));
    }

    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        self.timing.frontend = Some(self.t_start.elapsed());
        let Some((def_id, entry_ty)) = tcx.entry_fn(()) else {
            eprintln!("mirvm: 未找到 entry fn（需要 `fn main`）");
            self.exit_code = Some(1);
            return Compilation::Stop;
        };
        if !matches!(entry_ty, rustc_session::config::EntryFnType::Main { .. }) {
            eprintln!("mirvm: 暂不支持 #![no_main]/start 类型的入口");
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
            // S4/S3′ 降低指纹会话内验证：image 栈烤入构建会话的 (ub/overflow/contract)
            // checks，本会话不一致（cargo runner 自定义 profile 旗标等）即弃整栈走全量
            // 降低——错指纹复用 = image 函数带着另一套检查语义（错值级）。
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
            // callback 只做加载相；执行相必须等 tcx.finish、诊断收尾和 compiler drop 全部完成。
            let t_lower = std::time::Instant::now();
            // A2 split 判定（s3b-a2-design）：非旁路 + 本会话未装载 image +
            // 底座在场（fp 截断后栈可能已空——无底座不 split，Q2）。
            // 无 --extern（纯 std 程序）⇒ deps pre_key 恒 None（v1 边界：std 残余
            // 归 S4 底座地盘，不建共享 image）——此时 split 只会产写不了盘的内存
            // image（键退化进程占位），把 L2 键链永久打断；不 split 全量入 delta，
            // 语义不变（单 id 空间 = 经典非 split 路），L2 对纯 std 程序转活。
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
            // mode B 片②：pack 岔口——落 .mirvm 包代替执行（与 L2 store 同一
            // 洁净快照时机；失败 = 响亮终止，不静默退化）
            if let Some(out) = &self.pack_out {
                match crate::pack::write_package(
                    tcx,
                    &self.rustc_args,
                    self.module.as_ref().expect("刚设置"),
                    out,
                ) {
                    Ok(()) => {
                        eprintln!("mirvm: 包已写出 {}", out.display());
                    }
                    Err(reason) => {
                        eprintln!("mirvm: 打包失败: {reason}");
                        self.exit_code = Some(1);
                    }
                }
            }
            // A2：split 产物先写盘再上栈——栈键链自此含 image 键，L2 delta 条目带
            // 完整链（delta 内嵌 image 绝对量，错链入账 = 后续错配装载）。
            if let Some(img) = self.split_image.take() {
                let base_key = self.stack.key().expect("split 必在底座在场时").to_string();
                let bi = crate::depsimage::store_and_wrap(&self.rustc_args, &base_key, fp, img);
                self.stack.push(bi);
            }
            // L2 入账：guest 运行前的洁净快照（argv 尚未终结化）。
            // 会话有任何告警/错误即不入账——warm 路径无法重演诊断（见 SESSION_WARNINGS）。
            // S4/S3′：delta 条目携带键链（装载时双验证，防错配 image 栈）。
            let t_store = std::time::Instant::now();
            if session_diagnostics_clean()
                && tcx.sess.dcx().has_errors().is_none()
                && crate::ircache::store(
                    tcx,
                    &self.rustc_args,
                    self.module.as_ref().expect("刚设置"),
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

/// 引擎入口：缺省跑 main 启动链；`--vm-call 'name(args…)'` 直调导出函数（gate 入口）；
/// `--vm-stats` = Trap 债务统计（各期开工前的调研仪器）。
/// `--stack-size` / `MIRVM_STACK_SIZE` 解析：字节数，可带 k/m/g 后缀。非法即诊断退出。
fn parse_stack_size(s: &str) -> usize {
    let t = s.trim();
    let (num, mult): (&str, usize) = match t.as_bytes().last() {
        Some(b'k' | b'K') => (&t[..t.len() - 1], 1 << 10),
        Some(b'm' | b'M') => (&t[..t.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    let Ok(n) = num.trim().parse::<usize>() else {
        eprintln!("mirvm: 无法解析栈尺寸 `{s}`（例：8m、1g、67108864）");
        exit(2);
    };
    let bytes = n.saturating_mul(mult);
    // 下限护住引擎自身序言 + 边距；上限防笔误（虚拟保留也别要 128T）
    if !(1 << 20..=1 << 40).contains(&bytes) {
        eprintln!("mirvm: 栈尺寸 {s} 超出 [1m, 1t] 合理区间");
        exit(2);
    }
    bytes
}

fn run_vm_engine(
    mut module: crate::vm::engine::ir::Module,
    program_argv: &[String],
    vm_call: Option<&str>,
    vm_stats: bool,
) -> i32 {
    if let Err(e) = crate::vm::engine::verify::module(&module) {
        eprintln!("mirvm: bytecode verification failed: {e}");
        return 70;
    }
    if vm_stats {
        print!("{}", crate::vm::engine::stats::report(&module));
        return 0;
    }
    // argv 终结化（M6 片2）：运行期输入在快照语义之后布置，冷/热单一路径
    module.finalize_entry_argv(program_argv);
    // P2 启动相 GOT 重填（decision-history §7.5c）：foreign 符号值 = 本进程
    // 真地址；冷路径与 lower 初填一致（幂等），热路径换掉上进程陈旧地址
    if let Err(e) = crate::vm::engine::ffi::resolve_got_fixups(&mut module) {
        eprintln!("mirvm: {e}");
        exit(70);
    }
    let mut shared = crate::vm::engine::ctx::Shared::new(module);
    // P1 条目可执行化（decision-history §7.6）：配方 → closure → stub 字节 →
    // 整域 RX（与上两道并列的全相工序；域被占 = 装载失败）
    if let Err(e) =
        crate::vm::engine::thunks::materialize_all_entry_stubs(&mut shared.module, shared.id)
    {
        eprintln!("mirvm: {e}");
        exit(70);
    }
    let engine = crate::vm::engine::ctx::Engine::new(shared);
    let Some(spec) = vm_call else {
        // main 启动链：lang_start 照常解释，退出码 = Termination 产物
        let shared = std::sync::Arc::clone(engine.shared());
        return match on_guest_stack(move || crate::vm::engine::interp::run_main(&shared)) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("mirvm[m4-engine]: {e}");
                e.exit_code
            }
        };
    };
    let (name, args) = match parse_vm_call(spec) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mirvm: fail to resolve `--vm-call`: {e}");
            return 2;
        }
    };
    let shared = std::sync::Arc::clone(engine.shared());
    match on_guest_stack(move || crate::vm::engine::interp::run_export(&shared, &name, &args)) {
        Ok(r) => {
            println!("{r}");
            0
        }
        Err(e) => {
            eprintln!("mirvm[m4-engine]: {e}");
            e.exit_code
        }
    }
}

/// D8a：guest 主执行迁到专用大栈线程（默认 1 GiB 虚拟保留，Linux 按需提交）。
/// 解释帧宿主成本数十倍于 native 帧，借调用方线程的 8–16 MiB 栈只能容 ~8k 帧，
/// 对 native 栈界严重失真。guest panic 已在 run_main/run_export 内消化；穿出
/// join 的是宿主 panic（VM bug）——原样续传，绝不吞。
fn on_guest_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let reserve = match std::env::var("MIRVM_STACK_SIZE") {
        Ok(s) => parse_stack_size(&s),
        Err(_) => 1 << 30,
    };
    let spawned = std::thread::Builder::new()
        .name("mirvm-guest".into())
        .stack_size(reserve)
        .spawn(f);
    match spawned {
        Ok(h) => match h.join() {
            Ok(r) => r,
            Err(host_panic) => std::panic::resume_unwind(host_panic),
        },
        Err(e) => {
            // 不静默降级到调用方小栈（栈语义会悄悄变差）——响亮退出并给旋钮。
            // 典型触发：vm.overcommit_memory=2 的严格提交环境。
            eprintln!(
                "mirvm: guest 执行线程创建失败（stack 保留 {reserve} 字节）：{e}；\
                 请用 --stack-size / MIRVM_STACK_SIZE 调小后重试"
            );
            exit(70)
        }
    }
}

/// 解析 `name(1,2,…)`（或裸 `name` = 无参）。
fn parse_vm_call(spec: &str) -> Result<(String, Vec<u64>), String> {
    let spec = spec.trim();
    let Some(open) = spec.find('(') else {
        return Ok((spec.to_string(), Vec::new()));
    };
    let name = spec[..open].trim().to_string();
    let inner = spec[open + 1..].strip_suffix(')').ok_or("缺少右括号")?;
    let mut args = Vec::new();
    for part in inner.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        args.push(p.parse::<u64>().map_err(|e| format!("参数 `{p}`: {e}"))?);
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
    let t_start = std::time::Instant::now();
    // S4/S3′ image 栈：装载底座 + 依赖 image 链（失败/旁路 = 空栈，全量冷路径自愈）。
    // 降低指纹（ub/overflow/contract checks）要到会话内才能验证——after_analysis 复核。
    let mut stack = if dump_mir {
        crate::baseimage::ImageStack::empty()
    } else {
        crate::baseimage::ensure()
    };
    // A2 deps-image 装载（pre-compiler，s3b-a2-design §3）：非旁路 + 底座在场。
    // 命中即 push 上栈——栈键链自此含 image 键，L2 delta 条目可恢复入账。
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
    // L2 热路径（M6 片2）：命中即跳过整个 rustc 会话（前端+metadata+mono+lower）。
    // dump-mir 需要 tcx，强制冷路径。S4/S3′：delta 条目与键链双验证（ircache）。
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
        let timing = PhaseTiming {
            cache_load: Some(t_start.elapsed()),
            ..PhaseTiming::default()
        };
        if stack.is_empty() {
            // asm-stub 真地址是进程级活体：以配方幂等重物化覆写陈旧地址
            module.asm_stub_addrs = crate::lower::asm::materialize(&module.asm_sites);
        } else {
            crate::baseimage::absorb_stack(&mut module, stack); // 内含 asm 合并重物化
        }
        if let Some(guest) = &guest_process {
            guest.enter();
        }
        let t_engine = std::time::Instant::now();
        let code = run_vm_engine(module, &program_argv, vm_call.as_deref(), vm_stats);
        let engine = (!vm_stats).then(|| t_engine.elapsed());
        print_phase_timing(&timing, engine, t_start.elapsed(), vm_stats);
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
    if compiler_code != ExitCode::SUCCESS {
        return compiler_code;
    }
    if let Some(code) = callbacks.exit_code {
        exit(code);
    }
    if let Some(mut module) = callbacks.module.take() {
        // S4/S3′ 冷路径合并（store 已在 after_analysis 落盘 delta；引擎吃合并模块）。
        // 空栈（无 image）跳过——module 的 asm_stub_addrs 已在 lower 会话内物化。
        // A2：split 产物已在 after_analysis 写盘并 push 上栈，此处统一 absorb。
        let stack = std::mem::replace(&mut callbacks.stack, crate::baseimage::ImageStack::empty());
        if !stack.is_empty() {
            crate::baseimage::absorb_stack(&mut module, stack);
        }
        if let Some(guest) = &guest_process {
            guest.enter();
        }
        let t_engine = std::time::Instant::now();
        let code = run_vm_engine(
            module,
            &callbacks.program_argv,
            callbacks.vm_call.as_deref(),
            callbacks.vm_stats,
        );
        // vm-stats 分支不跑 guest，engine 段无意义则不报
        let engine = (!callbacks.vm_stats).then(|| t_engine.elapsed());
        print_phase_timing(
            &callbacks.timing,
            engine,
            t_start.elapsed(),
            callbacks.vm_stats,
        );
        exit(code);
    }
    compiler_code
}

// ===== frontmatter（cargo script RFC 3424 语法）=====

/// 解析 `---` 围栏的内嵌 manifest。返回 (manifest, 替换为空行保持行号的正文)。
/// cargoless::audit 的脚本入口（D15 P1）——本体保持私有。
pub(crate) fn parse_frontmatter_pub(src: &str) -> Option<(String, String)> {
    parse_frontmatter(src)
}

fn parse_frontmatter(src: &str) -> Option<(String, String)> {
    let mut lines = src.lines().enumerate().peekable();
    // 跳过 shebang
    if lines.peek().is_some_and(|(_, l)| l.starts_with("#!")) {
        lines.next();
    }
    // 跳过空行
    while lines.peek().is_some_and(|(_, l)| l.trim().is_empty()) {
        lines.next();
    }
    let (_open_idx, open) = lines.next()?;
    let fence = open.trim_end();
    if !fence.starts_with("---") {
        return None;
    }
    // infostring（如 `---cargo`）允许，忽略内容
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
    let close_idx = close_idx?; // 没有闭合围栏 → 不是 frontmatter
    // 正文 = 原文件，但 [0, close_idx] 行替换为空行（保持诊断行号）
    let body: String = src
        .lines()
        .enumerate()
        .map(|(i, l)| if i <= close_idx { "" } else { l })
        .collect::<Vec<_>>()
        .join("\n");
    Some((manifest, body))
}

/// 把脚本物化成缓存里的 cargo 项目，返回项目目录。
fn materialize_script(script: &Path, manifest: &str, body: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};

    let abs = std::path::absolute(script).unwrap_or_else(|_| script.to_path_buf());
    let mut hasher = std::hash::DefaultHasher::new();
    abs.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let dir = crate::sysroot::cache_dir().join("scripts").join(&hash);
    std::fs::create_dir_all(dir.join("src")).expect("创建脚本缓存目录失败");
    std::fs::create_dir_all(dir.join(".cargo")).expect("创建脚本 .cargo 目录失败");

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

    // bin 名带路径哈希短缀：共享 target dir（D14）下最终二进制落在无指纹的
    // debug/<binname>——同 stem 不同路径的脚本（/tmp 草变体等）不互相覆盖；
    // package 名保持 stem（onboarding 的 grep recipe 按名找 script dir）。
    let bin_name = format!("{name}-{}", &hash[..8]);
    let cargo_toml = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"{bin_name}\"\npath = \"src/main.rs\"\n\n{manifest}"
    );
    // 幂等物化：内容未变不落盘——mtime 稳定是 L2 IR 缓存清单与 cargo 指纹共同的前提
    write_if_changed(&dir.join("Cargo.toml"), &cargo_toml);
    write_if_changed(&dir.join("src/main.rs"), body);
    // B 维 native 对拍构建同样进统一存储（D14）：shim 构建走显式 --target-dir
    // 覆盖本键，native cargo run 吃文件配置——两族分目录（sysroot/rustflags
    // 不同，fingerprint 本也互斥，分目录只为 purge 语义清晰）。
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
    std::fs::write(path, contents).unwrap_or_else(|e| panic!("写 {} 失败: {e}", path.display()));
}

#[cfg(test)]
mod tests {
    use rustc_errors::{DiagInner, Level};

    use super::is_runner_warning_summary;

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
