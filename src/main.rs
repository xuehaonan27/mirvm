//! mirvm — M0: rustc_private 驱动骨架。
//!
//! 当前能力：把源文件交给 rustc 前端（宏展开/名字解析/typeck/trait 求解），
//! 在 analysis 完成后定位 entry fn 并读取其 MIR，随后停止（不 codegen、不链接）。
//! 这是解释器（M1 fast Machine）挂接的位置：拿到 `TyCtxt` 就拥有了一切。

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;

use std::process::exit;

use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

const USAGE: &str = "\
mirvm — a Rust runtime with its own execution engine (M0 skeleton)

USAGE:
    mirvm run <file.rs> [OPTIONS]

OPTIONS:
    --dump-mir        打印 entry fn 的完整 MIR
    --edition <ED>    默认 2024
";

struct RunConfig {
    input: String,
    dump_mir: bool,
    edition: String,
}

struct MirvmCallbacks {
    dump_mir: bool,
}

impl Callbacks for MirvmCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let Some((def_id, _entry_ty)) = tcx.entry_fn(()) else {
            eprintln!("mirvm: 未找到 entry fn（需要 `fn main`）");
            return Compilation::Stop;
        };

        let body = tcx.optimized_mir(def_id);
        eprintln!(
            "mirvm[M0]: entry fn `{}` — {} basic blocks, {} locals",
            tcx.def_path_str(def_id),
            body.basic_blocks.len(),
            body.local_decls.len(),
        );

        if self.dump_mir {
            let mut buf = Vec::new();
            rustc_middle::mir::pretty::MirWriter::new(tcx)
                .write_mir_fn(body, &mut buf)
                .expect("write_mir_fn failed");
            print!("{}", String::from_utf8_lossy(&buf));
        } else {
            // M1: 在这里构造 fast Machine (InterpCx) 并从 entry fn 开始解释执行。
            eprintln!("mirvm[M0]: 解释器尚未实现（M1）。用 --dump-mir 查看 MIR。");
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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dump-mir" => dump_mir = true,
            "--edition" => edition = args.next().unwrap_or_else(|| {
                eprintln!("mirvm: --edition 需要参数");
                exit(2);
            }),
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
    RunConfig { input, dump_mir, edition }
}

fn sysroot() -> String {
    std::env::var("MIRVM_SYSROOT")
        .unwrap_or_else(|_| env!("MIRVM_DEFAULT_SYSROOT").to_string())
}

fn main() -> std::process::ExitCode {
    let cfg = parse_args();

    // 伪装成 rustc 的 argv。args[0] 占位即可。
    let rustc_args = vec![
        "mirvm".to_string(),
        cfg.input.clone(),
        format!("--edition={}", cfg.edition),
        "--crate-type=bin".to_string(),
        "--sysroot".to_string(),
        sysroot(),
    ];

    let mut callbacks = MirvmCallbacks { dump_mir: cfg.dump_mir };
    rustc_driver::catch_with_exit_code(move || {
        rustc_driver::run_compiler(&rustc_args, &mut callbacks)
    })
}
