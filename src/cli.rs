//! CLI 与 rustc 驱动薄壳。三种运行形态：
//! - `mirvm run <脚本|项目>`：用户入口
//! - `mirvm <rustc> <args...>`（MIRVM_CARGO_SESSION 下）：cargo 的 RUSTC_WRAPPER
//! - `mirvm runner <假二进制> <args...>`：cargo 的 target runner，真正的解释入口
//!
//! 引擎 = M4 字节码 VM（加载相 lower + 执行相 engine）。tier-0（rustc InterpCx）已于
//! 2026-07-09 移除——代码在 git 历史（tag 前缀 feat: M4.3 之前），差分 oracle 一直是
//! native 编译直跑（tests/diff_vm.sh）。

use std::path::{Path, PathBuf};
use std::process::{ExitCode, exit};

use rustc_data_structures::AtomicRef;
use rustc_driver::{Callbacks, Compilation};
use rustc_errors::{DiagInner, ErrorGuaranteed, Level};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::cargo_shim;

const USAGE: &str = "\
mirvm — a Rust runtime with its own execution engine

USAGE:
    mirvm run <file.rs>  [OPTIONS] [-- <program args>]   # 单文件（可带 frontmatter 依赖）
    mirvm run <dir | Cargo.toml> [-- <program args>]     # cargo 项目（依赖自动构建为 MIR rlib）

OPTIONS:
    --dump-mir        打印 entry fn 的 MIR 后退出（仅单文件直通模式）
    --edition <ED>    默认 2024（仅单文件直通模式）
    --sysroot <PATH>  使用指定 sysroot（默认：自动构建带全量 MIR 的缓存 sysroot）
    --vm-call <SPEC>  直接调导出函数（gate/调试入口），如 'fib(25)'；缺省跑 main 启动链
    --vm-stats        打印 Trap 债务统计（每期开工前的调研仪器）后退出
    --stack-size <N>  guest 主执行栈虚拟保留（默认 1g；接受 k/m/g 后缀，JVM -Xss 同位）
    --jit <on|off>    方法级 JIT 分层（默认 on；M5.3a 期 = 计数基座，尚无编译）

ENV:
    MIRVM_SYSROOT     等价于 --sysroot
    MIRVM_STACK_SIZE  等价于 --stack-size（cargo 项目形态经环境传给 runner）
    MIRVM_JIT         等价于 --jit（off/0 = 纯解释对拍口径）
    MIRVM_JIT_THRESHOLD 编译触发阈值（默认 1000；诊断用）
    MIRVM_TIMING      =1 时向 stderr 输出相位账本（frontend/lower/engine/total）
    MIRVM_NO_IR_CACHE =1 时旁路 L2 engine-IR 缓存（读写全禁；诊断/对拍用）
    MIRVM_NO_BASE_IMAGE =1 时旁路 std 预降低底座（全量冷降低；诊断/对拍用）

DEV:
    mirvm spike1..5   跑已冻结的 M4 前置 spike（回归自检；见 docs/spike*.md）
";

pub fn main() -> ExitCode {
    let mut argv = std::env::args();
    argv.next(); // 跳过自身

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
    if std::env::var_os("MIRVM_CARGO_SESSION").is_some() {
        // RUSTC_WRAPPER：first = 真 rustc 路径
        cargo_shim::phase_wrapper(std::iter::once(first).chain(argv));
    }

    match first.as_str() {
        "run" => run_main(argv),
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

// ===== 用户入口 =====

fn run_main(args: impl Iterator<Item = String>) -> ExitCode {
    let mut args = args.peekable();
    let mut input = None;
    let mut dump_mir = false;
    let mut edition = "2024".to_string();
    let mut sysroot = None;
    let mut vm_call: Option<String> = None;
    let mut vm_stats = false;
    let mut program_args: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        let mut next = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("mirvm: {name} 需要参数");
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
                    eprintln!("mirvm: --jit 只接受 on|off（收到 `{v}`）");
                    exit(2);
                }
                // 同 --stack-size：落 env 使 cargo 形态经 runner 生效
                unsafe { std::env::set_var("MIRVM_JIT", v) };
            }
            _ if input.is_none() && !arg.starts_with('-') => input = Some(arg),
            _ => {
                eprintln!("mirvm: 未知参数 `{arg}`\n{USAGE}");
                exit(2);
            }
        }
    }
    let Some(input) = input else {
        eprint!("{USAGE}");
        exit(2);
    };
    let input_path = PathBuf::from(&input);

    // 形态 1：cargo 项目（目录或 Cargo.toml）
    if input_path.is_dir() {
        cargo_shim::phase_cargo(&input_path, &program_args);
    }
    if input_path.file_name().is_some_and(|f| f == "Cargo.toml") {
        cargo_shim::phase_cargo(input_path.parent().unwrap_or(Path::new(".")), &program_args);
    }

    let src = std::fs::read_to_string(&input_path).unwrap_or_else(|e| {
        eprintln!("mirvm: 读取 {input} 失败: {e}");
        exit(1);
    });

    // 形态 2：带 frontmatter 依赖声明的单文件脚本 → 物化成 cargo 项目
    if let Some((manifest, body)) = parse_frontmatter(&src) {
        let dir = materialize_script(&input_path, &manifest, &body);
        cargo_shim::phase_cargo(&dir, &program_args);
    }

    // 形态 3：纯单文件，零 cargo 快路径（M1 同款）
    let sysroot = sysroot
        .or_else(|| std::env::var("MIRVM_SYSROOT").ok())
        .unwrap_or_else(|| match crate::sysroot::ensure_sysroot() {
            Ok(p) => p.display().to_string(),
            Err(e) => {
                eprintln!("mirvm: 构建 sysroot 失败: {e}");
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
    run_driver(rustc_args, program_argv, dump_mir, vm_call, vm_stats, false)
}

// ===== cargo runner 回调 =====

fn runner_main(argv: impl Iterator<Item = String>) -> ExitCode {
    let (rustc_args, program_argv, env) = cargo_shim::parse_runner_invocation(argv);
    // 构建期环境优先（env!() 展开、CARGO_* 等在编译会话里要可见）。
    // CARGO_MAKEFLAGS 指向已消亡的 jobserver，透传会招警告（cargo-miri 同款处理）。
    for (k, v) in env {
        if k == "CARGO_MAKEFLAGS" {
            continue;
        }
        // MIRVM_* 是引擎控制面，永远取活环境（P1，coldstart-research §5）：录制回放
        // 会把构建期旋钮化石化进假二进制——实证 MIRVM_NO_IR_CACHE 被化石化后 L2 对
        // 该项目永久旁路且无迹象；MIRVM_TIMING 化石化则永久污染 stderr 差分。
        // 编译语义变量（env!/CARGO_*）维持录制优先不变。
        if k.starts_with("MIRVM_") {
            continue;
        }
        // SAFETY: 单线程阶段，尚未启动解释
        unsafe { std::env::set_var(k, v) };
    }
    run_driver(rustc_args, program_argv, false, None, false, true)
}

// ===== cargo wrapper：target 依赖的 in-process 编译（S2 / D9d）=====

/// 依赖编译回调：分析后显式跑 mono 收集再放行。native 构建在 codegen 期做
/// post-mono const-eval（собирает时求值 required consts），`-Zno-codegen` 跳过 codegen
/// 会连这层构建期错误一起漏掉（依赖里死代码的 const 恐慌等，native cargo build 会红）
/// ——cargo-miri 的 dummy backend 同款显式补齐，保住"依赖构建错误面与 native 一致"。
struct DepCallbacks;

impl Callbacks for DepCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let _ = tcx.collect_and_partition_mono_items(());
        Compilation::Continue
    }
}

/// target 依赖：真 rustc 语义 + `-Zno-codegen`。rustc_interface::start_codegen 对
/// no-codegen 有树内现成空转（空 CompiledModules；rmeta 编码在 backend 之外不受影响；
/// Linker::link 照走默认 link_binary 产出 metadata-only rlib，cargo 与下游 --extern
/// 无感）。in-process 驱动（进程本就链着 librustc_driver）顺带省一次 rustc exec。
pub(crate) fn run_dep_compiler(rustc_args: Vec<String>) -> ! {
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut DepCallbacks)
    });
    exit(if code == ExitCode::SUCCESS { 0 } else { 1 })
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
    /// S4 底座：after_analysis 验降低指纹后供 lower 查找；run_driver 尾部 absorb。
    base: Option<crate::baseimage::BaseImage>,
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
            // S4 降低指纹会话内验证：底座烤入构建会话的 (ub/overflow/contract) checks，
            // 本会话不一致（cargo runner 自定义 profile 旗标等）即弃用底座走全量降低
            // ——错指纹复用 = 底座函数带着另一套检查语义（错值级）。
            if let Some(b) = &self.base {
                let sess = tcx.sess;
                let fp = (
                    sess.ub_checks(),
                    sess.overflow_checks(),
                    sess.contract_checks(),
                );
                if b.lowering_fp != fp {
                    self.base = None;
                }
            }
            // callback 只做加载相；执行相必须等 tcx.finish、诊断收尾和 compiler drop 全部完成。
            let t_lower = std::time::Instant::now();
            self.module = Some(crate::lower::lower_program(tcx, self.base.as_ref()));
            self.timing.lower = Some(t_lower.elapsed());
            // L2 入账：guest 运行前的洁净快照（argv 尚未终结化）。
            // 会话有任何告警/错误即不入账——warm 路径无法重演诊断（见 SESSION_WARNINGS）。
            // S4：delta 条目携带底座键（装载时双验证，防错配底座）。
            let t_store = std::time::Instant::now();
            if session_diagnostics_clean()
                && tcx.sess.dcx().has_errors().is_none()
                && crate::ircache::store(
                    tcx,
                    &self.rustc_args,
                    self.module.as_ref().expect("刚设置"),
                    self.base.as_ref().map(|b| b.key.as_str()),
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
    if vm_stats {
        print!("{}", crate::vm::engine::stats::report(&module));
        return 0;
    }
    // argv 终结化（M6 片2）：运行期输入在快照语义之后布置，冷/热单一路径
    module.finalize_entry_argv(program_argv);
    // Shared 提升进程级 &'static（M4.4：thunk/多线程要求 Ctx 可在任意线程随时引用它）
    let shared: &'static _ = Box::leak(Box::new(crate::vm::engine::ctx::Shared::new(module)));
    // M5.3b：编译服务（--jit off / feature 关 = 不启动，纯解释）
    #[cfg(feature = "cranelift")]
    crate::vm::engine::jit_compile::start(shared);
    let Some(spec) = vm_call else {
        // main 启动链：lang_start 照常解释，退出码 = Termination 产物
        return on_guest_stack(move || crate::vm::engine::interp::run_main(shared));
    };
    let (name, args) = match parse_vm_call(spec) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mirvm: --vm-call 解析失败: {e}");
            return 2;
        }
    };
    match on_guest_stack(move || crate::vm::engine::interp::run_export(shared, &name, &args)) {
        Ok(r) => {
            println!("{r}");
            0
        }
        Err(e) => {
            eprintln!("mirvm: {e}");
            1
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

fn run_driver(
    rustc_args: Vec<String>,
    program_argv: Vec<String>,
    dump_mir: bool,
    vm_call: Option<String>,
    vm_stats: bool,
    suppress_runner_warning_summary: bool,
) -> ExitCode {
    let t_start = std::time::Instant::now();
    // S4 底座：装载或子进程构建（失败/旁路 = None，全量冷路径自愈）。
    // 降低指纹（ub/overflow/contract checks）要到会话内才能验证——after_analysis 复核。
    let base = if dump_mir {
        None
    } else {
        crate::baseimage::ensure()
    };
    let base_key = base.as_ref().map(|b| b.key.clone());
    // L2 热路径（M6 片2）：命中即跳过整个 rustc 会话（前端+metadata+mono+lower）。
    // dump-mir 需要 tcx，强制冷路径。S4：delta 条目与底座键双验证（ircache）。
    if !dump_mir && let Some(mut module) = crate::ircache::lookup(&rustc_args, base_key.as_deref())
    {
        let timing = PhaseTiming {
            cache_load: Some(t_start.elapsed()),
            ..PhaseTiming::default()
        };
        if let Some(b) = base {
            crate::baseimage::absorb(&mut module, b.module); // 内含 asm 合并重物化
        } else {
            // asm-stub 真地址是进程级活体：以配方幂等重物化覆写陈旧地址
            module.asm_stub_addrs = crate::lower::asm::materialize(&module.asm_sites);
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
        base,
    };
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
        // S4 冷路径合并（store 已在 after_analysis 落盘 delta；引擎吃合并模块）
        if let Some(b) = callbacks.base.take() {
            crate::baseimage::absorb(&mut module, b.module);
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
    let dir = crate::sysroot::cache_dir()
        .join("scripts")
        .join(format!("{:016x}", hasher.finish()));
    std::fs::create_dir_all(dir.join("src")).expect("创建脚本缓存目录失败");

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

    let cargo_toml = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"{name}\"\npath = \"src/main.rs\"\n\n{manifest}"
    );
    // 幂等物化：内容未变不落盘——mtime 稳定是 L2 IR 缓存清单与 cargo 指纹共同的前提
    write_if_changed(&dir.join("Cargo.toml"), &cargo_toml);
    write_if_changed(&dir.join("src/main.rs"), body);
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
