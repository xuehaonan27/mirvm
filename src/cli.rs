//! CLI 与 rustc 驱动薄壳。

use std::process::{ExitCode, exit};

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::interp;

const USAGE: &str = "\
mirvm — a Rust runtime with its own execution engine

USAGE:
    mirvm run <file.rs> [OPTIONS]

OPTIONS:
    --dump-mir        打印 entry fn 的 MIR 后退出（不执行）
    --edition <ED>    默认 2024
    --sysroot <PATH>  使用指定 sysroot（默认：自动构建带全量 MIR 的缓存 sysroot）
    --engine <E>      执行引擎：interp（默认；JIT 见 M5）

ENV:
    MIRVM_SYSROOT     等价于 --sysroot
";

struct RunConfig {
    input: String,
    dump_mir: bool,
    edition: String,
    sysroot: Option<String>,
}

struct MirvmCallbacks {
    dump_mir: bool,
    /// 解释执行得到的进程退出码（None = 未执行到）
    exit_code: Option<i32>,
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
            self.exit_code = Some(interp::eval::eval_main(tcx, def_id));
        }

        Compilation::Stop
    }
}

fn parse_args() -> RunConfig {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => {}
        _ => {
            eprint!("{USAGE}");
            exit(2);
        }
    }

    let mut input = None;
    let mut dump_mir = false;
    let mut edition = "2024".to_string();
    let mut sysroot = None;
    while let Some(arg) = args.next() {
        let mut next = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("mirvm: {name} 需要参数");
                exit(2);
            })
        };
        match arg.as_str() {
            "--dump-mir" => dump_mir = true,
            "--edition" => edition = next("--edition"),
            "--sysroot" => sysroot = Some(next("--sysroot")),
            "--engine" => {
                let e = next("--engine");
                if e != "interp" {
                    eprintln!("mirvm: 引擎 `{e}` 尚未实现（当前仅 interp；JIT 见 DESIGN.md M5）");
                    exit(2);
                }
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
    RunConfig { input, dump_mir, edition, sysroot }
}

pub fn main() -> ExitCode {
    let cfg = parse_args();

    let sysroot = cfg
        .sysroot
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
        cfg.input.clone(),
        format!("--edition={}", cfg.edition),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot,
    ];

    let mut callbacks = MirvmCallbacks { dump_mir: cfg.dump_mir, exit_code: None };
    let compiler_code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    });
    match callbacks.exit_code {
        Some(code) => exit(code),
        // 编译期出错（诊断已输出），透传 rustc 的退出码
        None => compiler_code,
    }
}
