//! cargo 集成三阶段（机制移植自 cargo-miri，MIT/Apache-2.0）：
//!
//! 1. `phase_cargo`：以 `cargo run` 驱动整个依赖图构建，但注入
//!    RUSTC_WRAPPER=mirvm + target.runner=["mirvm","runner"] + 独立 target dir。
//!    强制 `--target <host>`——这是区分 host crate（build script/proc-macro，
//!    正常编译）与 target crate（要被解释，注入 MIR sysroot）的开关。
//! 2. `phase_wrapper`：cargo 的每次 rustc 调用都经过这里。
//!    - 信息查询/host crate → 透传真 rustc
//!    - target 依赖 → 真 rustc + `--sysroot <MIR sysroot>` + `-Zalways-encode-mir`
//!    - 最终可运行 bin → 不编译：把完整 rustc 参数 + 环境写成 JSON"假二进制"
//!      （外加 stub .d 防 cargo 重建）
//! 3. `phase_runner`：cargo "运行"假二进制时回到我们手里——读 JSON，
//!    用 cargo 的原始参数驱动解释器。

use std::path::PathBuf;
use std::process::{Command, exit};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct CrateRunInfo {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn toolchain_rustc() -> PathBuf {
    PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/rustc")
}

fn arg_flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    None
}

fn exec(mut cmd: Command) -> ! {
    let status = cmd.status().unwrap_or_else(|e| {
        eprintln!("mirvm: 无法执行 {cmd:?}: {e}");
        exit(1);
    });
    exit(status.code().unwrap_or(1));
}

/// 阶段 1：在 `project_dir` 里驱动 cargo。program_args 传给最终被解释的程序。
pub fn phase_cargo(project_dir: &std::path::Path, program_args: &[String]) -> ! {
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mirvm: 构建 sysroot 失败: {e}");
            exit(1);
        }
    };
    let self_exe = std::env::current_exe().expect("current_exe 失败");
    let self_str = self_exe.to_str().expect("mirvm 路径非 UTF-8");

    let mut cmd = Command::new(PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/cargo"));
    cmd.current_dir(project_dir);
    cmd.arg("run");
    // 强制 host target：让 host/target crate 可区分，且激活 target.runner
    cmd.arg("--target").arg(env!("MIRVM_HOST"));
    // 所有"运行二进制"的动作转给我们
    let runner_toml = self_str.replace('\\', "\\\\").replace('\'', "\\'");
    cmd.arg("--config").arg(format!(
        "target.'cfg(all())'.runner=['{runner_toml}', 'runner']"
    ));
    // 独立 target dir，避免与用户正常构建的指纹互相踩踏
    cmd.arg("--target-dir")
        .arg(project_dir.join("target/mirvm"));
    cmd.arg("--quiet");
    if !program_args.is_empty() {
        cmd.arg("--");
        cmd.args(program_args);
    }

    cmd.env("RUSTC_WRAPPER", self_str);
    cmd.env_remove("RUSTC_WORKSPACE_WRAPPER");
    cmd.env("MIRVM_CARGO_SESSION", "1");
    cmd.env("MIRVM_SYSROOT", &sysroot);
    exec(cmd)
}

/// 阶段 2：RUSTC_WRAPPER。argv = [<rustc 名字>, <rustc 参数...>]。
/// 注意：忽略 cargo 传来的 rustc 名字（裸 "rustc" 会被 rustup 按 cwd 解析到错误
/// toolchain），一律用 pinned toolchain 的 rustc——proc-macro dylib 与 rlib 元数据
/// 都必须和解释会话的编译器版本严格一致。
pub fn phase_wrapper(mut argv: impl Iterator<Item = String>) -> ! {
    let _rustc_name = argv.next();
    let rustc = toolchain_rustc();
    let args: Vec<String> = argv.collect();

    let is_info_query =
        arg_flag_value(&args, "--print").is_some() || args.iter().any(|a| a == "-vV");
    let is_target = arg_flag_value(&args, "--target").is_some();
    // crate-type 缺省即 bin（与 cargo-miri 的判定一致）；--test 是 test harness bin
    let is_runnable = !is_info_query
        && (arg_flag_value(&args, "--crate-type")
            .as_deref()
            .unwrap_or("bin")
            == "bin"
            || args.iter().any(|a| a == "--test"));

    if is_info_query || !is_target {
        // 版本查询 / host crate（build script、proc-macro）：原样编译
        let mut cmd = Command::new(&rustc);
        cmd.args(&args);
        exec(cmd);
    }

    if is_runnable {
        // 最终 bin：不编译，写 JSON 假二进制 + stub .d
        let info = CrateRunInfo {
            args: args.clone(),
            env: std::env::vars().collect(),
        };
        write_fake_outputs(&rustc, &args, &info);
        exit(0);
    }

    // target 依赖：注入 MIR sysroot（保证与解释会话同一套 std）+ 全量 MIR
    let sysroot = std::env::var("MIRVM_SYSROOT").expect("wrapper 阶段缺少 MIRVM_SYSROOT");
    let mut cmd = Command::new(&rustc);
    cmd.args(&args);
    cmd.arg("--sysroot").arg(sysroot);
    cmd.arg("-Zalways-encode-mir");
    exec(cmd)
}

fn write_fake_outputs(rustc: &std::path::Path, args: &[String], info: &CrateRunInfo) {
    let out_dir = arg_flag_value(args, "--out-dir").unwrap_or_default();
    let crate_name = arg_flag_value(args, "--crate-name").unwrap_or_default();

    // stub dep-info：阻止 cargo 每次都认为需要重建
    if arg_flag_value(args, "--emit")
        .unwrap_or_default()
        .split(',')
        .any(|e| e == "dep-info")
    {
        let extra = arg_flag_value(args, "extra-filename").unwrap_or_default();
        let d = PathBuf::from(&out_dir).join(format!("{crate_name}{extra}.d"));
        let _ = std::fs::write(d, "");
    }

    // 让 rustc 告诉我们产物文件名（依赖 target 的后缀规则）
    let out_files: Vec<PathBuf> = if let Some(o) = arg_flag_value(args, "-o") {
        vec![PathBuf::from(o)]
    } else {
        let mut cmd = Command::new(rustc);
        cmd.args(["--print", "file-names"]);
        for flag in ["--crate-name", "--crate-type", "--target"] {
            if let Some(v) = arg_flag_value(args, flag) {
                cmd.arg(flag).arg(v);
            }
        }
        if let Some(extra) = arg_flag_value(args, "extra-filename") {
            cmd.arg("-C").arg(format!("extra-filename={extra}"));
        }
        cmd.arg("-");
        let output = cmd.output().expect("rustc --print file-names 失败");
        assert!(
            output.status.success(),
            "rustc --print file-names 失败: {output:?}"
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| PathBuf::from(&out_dir).join(l))
            .collect()
    };

    let json = serde_json::to_string(info).unwrap();
    for f in out_files {
        std::fs::write(&f, &json).unwrap_or_else(|e| {
            eprintln!("mirvm: 写假二进制 {} 失败: {e}", f.display());
            exit(1);
        });
    }
}

/// 阶段 3：runner。argv = [<假二进制路径>, <程序参数...>]。
/// 返回 (解释会话的 rustc 参数, 程序 argv, 需设置的环境)。
pub fn parse_runner_invocation(
    mut argv: impl Iterator<Item = String>,
) -> (Vec<String>, Vec<String>, Vec<(String, String)>) {
    let fake_bin = argv.next().unwrap_or_else(|| {
        eprintln!("mirvm runner: 缺少二进制路径参数");
        exit(2);
    });
    let program_args: Vec<String> = argv.collect();

    let data = std::fs::read_to_string(&fake_bin).unwrap_or_else(|e| {
        eprintln!("mirvm runner: 读取 {fake_bin} 失败: {e}");
        exit(1);
    });
    let info: CrateRunInfo = serde_json::from_str(&data).unwrap_or_else(|_| {
        eprintln!("mirvm runner: {fake_bin} 不是 mirvm 的假二进制（试试删掉 target/mirvm 重跑）");
        exit(1);
    });

    // 组装解释会话参数：argv[0] 占位 + cargo 的原始参数 + 我们的 sysroot。
    // 剥掉 JSON 诊断/artifact 通知（那是给 cargo 消费的，现在 cargo 已退场）。
    let sysroot = std::env::var("MIRVM_SYSROOT").expect("runner 阶段缺少 MIRVM_SYSROOT");
    let mut rustc_args = vec!["mirvm".to_string()];
    let mut it = info.args.iter().peekable();
    while let Some(a) = it.next() {
        if a == "--error-format" || a == "--json" {
            it.next();
            continue;
        }
        if a.starts_with("--error-format=") || a.starts_with("--json=") {
            continue;
        }
        rustc_args.push(a.clone());
    }
    rustc_args.push("--sysroot".into());
    rustc_args.push(sysroot);

    // 程序 argv：argv[0] 用假二进制路径（与 cargo run 一致）
    let mut prog_argv = vec![fake_bin];
    prog_argv.extend(program_args);

    (rustc_args, prog_argv, info.env)
}
