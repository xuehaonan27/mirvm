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

fn toolchain_cargo() -> PathBuf {
    PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/cargo")
}

fn configured_cargo_wrapper(
    project_dir: &std::path::Path,
    key: &str,
) -> Result<Option<String>, String> {
    let output = Command::new(toolchain_cargo())
        .current_dir(project_dir)
        .args([
            "-Z",
            "unstable-options",
            "config",
            "get",
            key,
            "--format=json-value",
        ])
        .output()
        .map_err(|error| format!("无法查询 Cargo config `{key}`：{error}"))?;
    if output.status.success() {
        let value: String = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("Cargo config `{key}` 不是字符串：{error}"))?;
        return Ok((!value.is_empty()).then_some(value));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains(&format!("config value `{key}` is not set")) {
        return Ok(None);
    }
    let detail = stderr.lines().next().unwrap_or("unknown Cargo error");
    Err(format!("查询 Cargo config `{key}` 失败：{detail}"))
}

fn reject_custom_rustc_wrappers(project_dir: &std::path::Path) -> Result<(), String> {
    for key in ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"] {
        if std::env::var_os(key).is_some_and(|value| !value.is_empty()) {
            return Err(format!(
                "不支持 Cargo wrapper `{key}`：尚不能与 MIR capture 组合"
            ));
        }
    }
    for key in ["build.rustc-wrapper", "build.rustc-workspace-wrapper"] {
        if configured_cargo_wrapper(project_dir, key)?.is_some() {
            return Err(format!(
                "不支持 Cargo wrapper `{key}`：尚不能与 MIR capture 组合"
            ));
        }
    }
    Ok(())
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

fn cargo_project_command(
    project_dir: &std::path::Path,
    program_args: &[String],
    sysroot: &std::path::Path,
    self_exe: &std::path::Path,
    locked: bool,
) -> Command {
    let self_str = self_exe.to_str().expect("mirvm 路径非 UTF-8");
    let mut cmd = Command::new(toolchain_cargo());
    cmd.current_dir(project_dir);
    cmd.arg("run");
    if locked {
        cmd.arg("--locked");
    }
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
    // Preflight rejects effective custom wrappers. Remove the ambient form so
    // Cargo cannot nest it ahead of rustc (an explicit empty value changes
    // Cargo artifact fingerprints, so it is deliberately not used here).
    cmd.env_remove("RUSTC_WORKSPACE_WRAPPER");
    // Cargo also exposes the same build settings through environment-form
    // config keys. Empty values disable wrappers but still perturb Cargo's
    // effective config/fingerprint, so remove those aliases after preflight.
    cmd.env_remove("CARGO_BUILD_RUSTC_WRAPPER");
    cmd.env_remove("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER");
    cmd.env("MIRVM_CARGO_SESSION", "1");
    cmd.env("MIRVM_SYSROOT", sysroot);
    cmd
}

/// 阶段 1：在 `project_dir` 里驱动 cargo。program_args 传给最终被解释的程序。
pub fn phase_cargo(project_dir: &std::path::Path, program_args: &[String]) -> ! {
    if let Err(error) = reject_custom_rustc_wrappers(project_dir) {
        eprintln!("mirvm: {error}");
        exit(1);
    }
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mirvm: 构建 sysroot 失败: {e}");
            exit(1);
        }
    };
    let self_exe = std::env::current_exe().expect("current_exe 失败");
    let locked = std::env::var_os("MIRVM_CARGO_LOCKED").is_some();
    let cmd = cargo_project_command(project_dir, program_args, &sysroot, &self_exe, locked);
    exec(cmd)
}

/// 阶段 2：RUSTC_WRAPPER。argv = [<rustc 名字>, <rustc 参数...>]。
/// 注意：忽略 cargo 传来的 rustc 名字（裸 "rustc" 会被 rustup 按 cwd 解析到错误
/// toolchain），一律用 pinned toolchain 的 rustc——proc-macro dylib 与 rlib 元数据
/// 都必须和解释会话的编译器版本严格一致。
pub fn phase_wrapper(mut argv: impl Iterator<Item = String>) -> ! {
    let _rustc_name = argv.next();
    let rustc = toolchain_rustc();
    let mut args: Vec<String> = argv.collect();
    if looks_like_nested_rustc_wrapper(&args) {
        eprintln!("mirvm: 不支持嵌套 Cargo rustc wrapper：尚不能与 MIR capture 组合");
        exit(1);
    }
    append_encoded_rustflags(
        &mut args,
        std::env::var("MIRVM_ENCODED_RUSTFLAGS_APPEND")
            .ok()
            .as_deref(),
    );

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

    // target 依赖：注入 MIR sysroot（保证与解释会话同一套 std）+ 全量 MIR。
    // S2（D9d，coldstart-research §3/§4.1）：-Zno-codegen 剪掉 LLVM codegen+目标码
    // ——runner 只消费 rmeta 里的 MIR，目标码纯白烧（实测 ecosystem 22 个 rlib 全带
    // .rcgu.o 共 ~100MB）。metadata-only rlib 由 rustc 默认 link 路径照常产出；
    // post-mono const-eval 错误面由 DepCallbacks 显式补齐（cli.rs）。
    let sysroot = std::env::var("MIRVM_SYSROOT").expect("wrapper 阶段缺少 MIRVM_SYSROOT");
    let mut dep_args = Vec::with_capacity(args.len() + 5);
    dep_args.push("mirvm-dep-rustc".to_string()); // argv[0] 占位（driver 跳过）
    dep_args.extend(args);
    dep_args.push("--sysroot".into());
    dep_args.push(sysroot);
    dep_args.push("-Zalways-encode-mir".into());
    dep_args.push("-Zno-codegen".into());
    crate::cli::run_dep_compiler(dep_args)
}

fn looks_like_nested_rustc_wrapper(args: &[String]) -> bool {
    args.first().is_some_and(|arg| !arg.starts_with('-'))
        && args.get(1).is_some_and(|arg| arg.starts_with('-'))
}

fn append_encoded_rustflags(args: &mut Vec<String>, encoded: Option<&str>) {
    let Some(encoded) = encoded else {
        return;
    };
    args.extend(
        encoded
            .split('\u{1f}')
            .filter(|arg| !arg.is_empty())
            .map(str::to_owned),
    );
}

fn write_fake_outputs(rustc: &std::path::Path, args: &[String], info: &CrateRunInfo) {
    let out_dir = arg_flag_value(args, "--out-dir").unwrap_or_default();
    let crate_name = arg_flag_value(args, "--crate-name").unwrap_or_default();

    // P2（coldstart-research §5）：dep-info 必须**真实**。旧空 stub 让 cargo 没有 bin 的
    // 源文件清单——cargo 在 rustc 调用结束时就把 dep-info 快照进 .fingerprint（事后补写
    // 不可见，实测），于是源码编辑永不触发假二进制重录，录制 env/参数化石永生。这里用
    // 真 rustc 只发 dep-info 拿精确清单（含 mod/include!/env! 追踪；deps rlib 此刻已就绪，
    // 只在 bin 指纹脏时发生）。stderr 静默：诊断由 runner 会话响亮重演，保持与 native
    // "单次告警"同口径。失败退回 crate 根单行清单（.d 只影响重录频率；运行语义永远由
    // runner 现读源码保证，宁可少重录不可错语义）。
    if arg_flag_value(args, "--emit")
        .unwrap_or_default()
        .split(',')
        .any(|e| e == "dep-info")
    {
        let mut cmd = Command::new(rustc);
        let mut it = args.iter().peekable();
        while let Some(a) = it.next() {
            if a == "--emit" {
                it.next();
                cmd.arg("--emit=dep-info");
            } else if a.starts_with("--emit=") {
                cmd.arg("--emit=dep-info");
            } else {
                cmd.arg(a);
            }
        }
        // 与 target 依赖同一套 MIR sysroot（use std::* 的解析必需）
        if let Ok(sysroot) = std::env::var("MIRVM_SYSROOT") {
            cmd.arg("--sysroot").arg(sysroot);
        }
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        if !cmd.status().is_ok_and(|s| s.success()) {
            let extra = arg_flag_value(args, "extra-filename").unwrap_or_default();
            let d = PathBuf::from(&out_dir).join(format!("{crate_name}{extra}.d"));
            let root = args
                .iter()
                .find(|a| !a.starts_with('-') && a.ends_with(".rs"))
                .cloned()
                .unwrap_or_default();
            let _ = std::fs::write(d, format!("{crate_name}{extra}.d: {root}\n\n{root}:\n"));
        }
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    use super::{
        append_encoded_rustflags, cargo_project_command, configured_cargo_wrapper,
        looks_like_nested_rustc_wrapper, reject_custom_rustc_wrappers,
    };

    #[test]
    fn harness_rustflags_are_appended_without_replacing_cargo_flags() {
        let mut args = vec!["--cfg=from-project-config".to_string()];
        append_encoded_rustflags(
            &mut args,
            Some(
                "--remap-path-prefix=/run/mirvm-project-side=/mirvm-project\u{1f}\
                 --remap-path-scope=diagnostics",
            ),
        );
        assert_eq!(
            args,
            [
                "--cfg=from-project-config",
                "--remap-path-prefix=/run/mirvm-project-side=/mirvm-project",
                "--remap-path-scope=diagnostics",
            ]
        );
    }

    #[test]
    fn cargo_project_command_is_locked() {
        let command = cargo_project_command(
            Path::new("/tmp/project"),
            &[],
            Path::new("/tmp/sysroot"),
            Path::new("/tmp/mirvm"),
            true,
        );
        let args: Vec<_> = command.get_args().collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == [OsStr::new("run"), OsStr::new("--locked")])
        );
    }

    #[test]
    fn ordinary_cargo_project_command_can_create_a_lockfile() {
        let command = cargo_project_command(
            Path::new("/tmp/project"),
            &[],
            Path::new("/tmp/sysroot"),
            Path::new("/tmp/mirvm"),
            false,
        );
        assert!(command.get_args().all(|arg| arg != OsStr::new("--locked")));
    }

    #[test]
    fn cargo_project_command_removes_ambient_wrapper_overrides() {
        let command = cargo_project_command(
            Path::new("/tmp/project"),
            &[],
            Path::new("/tmp/sysroot"),
            Path::new("/tmp/mirvm"),
            true,
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == OsStr::new("RUSTC_WORKSPACE_WRAPPER") && value.is_none()
        }));
        for key in [
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        ] {
            assert!(
                command
                    .get_envs()
                    .any(|(candidate, value)| candidate == OsStr::new(key) && value.is_none())
            );
        }
    }

    #[test]
    fn nested_wrapper_argv_is_rejected_before_rustc_sees_a_compiler_as_input() {
        assert!(looks_like_nested_rustc_wrapper(&[
            "/path/to/rustc-proxy".to_string(),
            "--crate-name".to_string(),
            "demo".to_string(),
        ]));
        assert!(!looks_like_nested_rustc_wrapper(&[
            "--crate-name".to_string(),
            "demo".to_string(),
        ]));
    }

    #[test]
    fn configured_rustc_wrappers_are_rejected_before_cargo_nests_them() {
        let root = std::env::temp_dir().join(format!(
            "mirvm-cargo-wrapper-config-test-{}",
            std::process::id()
        ));
        let cargo_dir = root.join(".cargo");
        std::fs::create_dir_all(&cargo_dir).unwrap();

        for key in ["rustc-wrapper", "rustc-workspace-wrapper"] {
            std::fs::write(
                cargo_dir.join("config.toml"),
                format!("[build]\n{key} = \"/tmp/custom-wrapper\"\n"),
            )
            .unwrap();
            let full_key = format!("build.{key}");
            assert_eq!(
                configured_cargo_wrapper(&root, &full_key).unwrap(),
                Some("/tmp/custom-wrapper".to_string())
            );
            assert_eq!(
                reject_custom_rustc_wrappers(&root).unwrap_err(),
                format!("不支持 Cargo wrapper `{full_key}`：尚不能与 MIR capture 组合")
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }
}
