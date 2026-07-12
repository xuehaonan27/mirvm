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

use rustc_driver::{Callbacks, Compilation};
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

ENV:
    MIRVM_SYSROOT     等价于 --sysroot

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
    run_driver(rustc_args, program_argv, dump_mir, vm_call, vm_stats)
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
        // SAFETY: 单线程阶段，尚未启动解释
        unsafe { std::env::set_var(k, v) };
    }
    run_driver(rustc_args, program_argv, false, None, false)
}

// ===== 共享驱动 =====

struct MirvmCallbacks {
    dump_mir: bool,
    program_argv: Vec<String>,
    exit_code: Option<i32>,
    vm_call: Option<String>,
    vm_stats: bool,
}

impl Callbacks for MirvmCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
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
            // 加载相（lower，tcx 关在此）→ 执行相（纯 Rust）
            self.exit_code = Some(run_vm_engine(
                tcx,
                self.vm_call.as_deref(),
                self.vm_stats,
                std::mem::take(&mut self.program_argv),
            ));
        }

        Compilation::Stop
    }
}

/// 引擎入口：缺省跑 main 启动链；`--vm-call 'name(args…)'` 直调导出函数（gate 入口）；
/// `--vm-stats` = Trap 债务统计（各期开工前的调研仪器）。
fn run_vm_engine(
    tcx: TyCtxt<'_>,
    vm_call: Option<&str>,
    vm_stats: bool,
    program_argv: Vec<String>,
) -> i32 {
    let module = crate::lower::lower_program(tcx, &program_argv);
    if vm_stats {
        print!("{}", crate::vm::engine::stats::report(&module));
        return 0;
    }
    // Shared 提升进程级 &'static（M4.4：thunk/多线程要求 Ctx 可在任意线程随时引用它）
    let shared: &'static _ = Box::leak(Box::new(crate::vm::engine::ctx::Shared::new(module)));
    let Some(spec) = vm_call else {
        // main 启动链：lang_start 照常解释，退出码 = Termination 产物
        return crate::vm::engine::interp::run_main(shared);
    };
    let (name, args) = match parse_vm_call(spec) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mirvm: --vm-call 解析失败: {e}");
            return 2;
        }
    };
    match crate::vm::engine::interp::run_export(shared, &name, &args) {
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
) -> ExitCode {
    let mut callbacks = MirvmCallbacks {
        dump_mir,
        program_argv,
        exit_code: None,
        vm_call,
        vm_stats,
    };
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    });
    match callbacks.exit_code {
        Some(code) => exit(code),
        None => compiler_code, // 编译期出错，透传 rustc 退出码
    }
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
    std::fs::write(dir.join("Cargo.toml"), cargo_toml).expect("写 Cargo.toml 失败");
    std::fs::write(dir.join("src/main.rs"), body).expect("写 main.rs 失败");
    dir
}
