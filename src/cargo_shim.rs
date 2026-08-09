//! cargo 集成三阶段（机制移植自 cargo-miri，MIT/Apache-2.0）：
//!
//! 1. `phase_cargo`：以 `cargo run` 驱动整个依赖图构建，但注入
//!    RUSTC_WRAPPER=mirvm + target.runner=["mirvm","runner"] + 独立 target dir。
//!    强制 `--target <host>`——这是区分 host crate（build script/proc-macro，
//!    正常编译）与 target crate（要被解释，注入 MIR sysroot）的开关。
//! 2. `phase_wrapper`：cargo 的每次 rustc 调用都经过这里。
//!    - 信息查询/host crate → 透传真 rustc
//!    - target 依赖 → 真 rustc + `--sysroot <MIR sysroot>` + `-Zalways-encode-mir`
//!    - 最终可运行 bin → 不编译：写可执行启动器，完整 rustc 参数 + 环境放在
//!      旁置 JSON（外加真实 .d 防 cargo 重建）
//! 3. `phase_runner`：cargo "运行"启动器时回到我们手里——读旁置 JSON，
//!    用 cargo 的原始参数驱动解释器。

use std::path::{Path, PathBuf};
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
    action: CargoAction<'_>,
    program_args: &[String],
    sysroot: &std::path::Path,
    self_exe: &std::path::Path,
    locked: bool,
) -> Command {
    let self_str = self_exe.to_str().expect("mirvm 路径非 UTF-8");
    let mut cmd = Command::new(toolchain_cargo());
    cmd.current_dir(project_dir);
    cmd.arg(action.subcommand());
    if locked {
        cmd.arg("--locked");
    }
    action.append_args(&mut cmd);
    // 强制 host target：让 host/target crate 可区分，且激活 target.runner
    cmd.arg("--target").arg(env!("MIRVM_HOST"));
    // 所有"运行二进制"的动作转给我们
    let runner_toml = self_str.replace('\\', "\\\\").replace('\'', "\\'");
    cmd.arg("--config").arg(format!(
        "target.'cfg(all())'.runner=['{runner_toml}', 'runner']"
    ));
    // 统一依赖存储（D14 近期片，2026-07-18 裁定）：所有脚本/项目的 mirvm 构建
    // 共享同一 target dir——cargo fingerprint 即编译键（版本×features×依赖闭包
    // ×flags×toolchain）内容寻址，同一 crate 编译单元全机唯一一份；最终产物
    // 定位由 runner 协议供给（cargo 把假二进制路径传给 runner），不扫目录。
    // MIRVM_TARGET_DIR 可整体改址（隔离/测试用；默认 $MIRVM_HOME/target/mirvm）。
    let target_dir = std::env::var_os("MIRVM_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::sysroot::cache_dir().join("target/mirvm"));
    cmd.arg("--target-dir").arg(target_dir);
    if matches!(action, CargoAction::Run { .. }) {
        cmd.arg("--quiet");
    }
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

/// Cargo 兼容轨的用户动作。两种动作共用同一 wrapper/runner 协议；区别只在
/// Cargo 负责选择一个 run bin，还是选择并串行启动若干 test harness。
#[derive(Clone, Copy)]
enum CargoAction<'a> {
    Run { bin_sel: Option<&'a str> },
    Test { cargo_args: &'a [String] },
}

impl CargoAction<'_> {
    fn subcommand(self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::Test { .. } => "test",
        }
    }

    fn append_args(self, cmd: &mut Command) {
        match self {
            Self::Run { bin_sel: Some(bin) } => {
                cmd.arg("--bin").arg(bin);
            }
            Self::Run { bin_sel: None } => {}
            Self::Test { cargo_args } => {
                cmd.args(cargo_args);
            }
        }
    }
}

/// 阶段 1：在 `project_dir` 里驱动 cargo。program_args 传给最终被解释的程序。
pub fn phase_cargo(
    project_dir: &std::path::Path,
    program_args: &[String],
    bin_sel: Option<&str>,
) -> ! {
    // 绝对化：relative project_dir + current_dir + join(target/mirvm) 会把 target 目录
    // 拼成 project/project/target 的重复嵌套（A2 gate 实测）——且使同一项目的 rlib 路径
    // 随调用形态（相对/绝对）漂移，deps-image 键失稳。
    let project_dir =
        &std::path::absolute(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
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
    let cmd = cargo_project_command(
        project_dir,
        CargoAction::Run { bin_sel },
        program_args,
        &sysroot,
        &self_exe,
        locked,
    );
    exec(cmd)
}

/// `mirvm test` 的 Cargo 兼容轨。`cargo_args` 是 `--lib/--test/.../TESTNAME`
/// 等 Cargo 自己解释的选择参数；`harness_args` 是 `--` 后逐字透给 libtest 的参数。
pub fn phase_cargo_test(
    project_dir: &std::path::Path,
    cargo_args: &[String],
    harness_args: &[String],
) -> ! {
    let project_dir =
        &std::path::absolute(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
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
    let cmd = cargo_project_command(
        project_dir,
        CargoAction::Test { cargo_args },
        harness_args,
        &sysroot,
        &self_exe,
        locked,
    );
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

    // cargo 驱动之外的临时探测编译：build.rs 读 RUSTC_WRAPPER 后 spawn
    // `$WRAPPER $RUSTC --crate-type=rlib --emit=metadata -o <f> -` 探测工具链
    // 特性（rustix 1.1.4 实锤）。判据：无 --out-dir（cargo 的 dep 编译恒带）
    // 或 stdin 源 `-`（cargo 恒为文件路径）。原样 exec 真 rustc——探针问的
    // 是「这套工具链认不认 X」，只能由真 rustc 回答；劫持进 dep 通道则
    // run_dep_compiler 缺 --out-dir 即 panic，与探针 writeln 竞态成 EPIPE
    // （负载高时 build.rs 炸，空载时探针假否——两态都错）。
    if arg_flag_value(&args, "--out-dir").is_none() || args.iter().any(|a| a == "-") {
        let mut cmd = Command::new(&rustc);
        cmd.args(&args);
        exec(cmd);
    }

    if is_runnable {
        // 最终 bin：不编译，写可执行启动器 + JSON 配方 + 真实 .d
        let recorded_args = runner_args_with_stable_paths(&args);
        let info = CrateRunInfo {
            args: recorded_args,
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

/// Cargo workspace 中 rustc 从 workspace 根接收 `member/src/lib.rs`，runner
/// 却从成员目录启动。把 crate 根绝对化，并重映射回原相对拼写：编译不受 runner
/// cwd 影响，诊断/file!() 仍与 Cargo 原调用一致。
fn runner_args_with_stable_paths(args: &[String]) -> Vec<String> {
    let Ok(cwd) = std::env::current_dir() else {
        return args.to_vec();
    };
    let mut out = args.to_vec();
    let mut changed = false;
    for arg in &mut out {
        let path = Path::new(arg);
        if !arg.starts_with('-') && arg.ends_with(".rs") && path.is_relative() {
            *arg = cwd.join(path).display().to_string();
            changed = true;
            break;
        }
    }
    if changed {
        out.push(format!("--remap-path-prefix={}/=", cwd.display()));
    }
    out
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
        let info_path = fake_info_path(&f);
        std::fs::write(&info_path, &json).unwrap_or_else(|e| {
            eprintln!("mirvm: 写假二进制配方 {} 失败: {e}", info_path.display());
            exit(1);
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let self_exe = std::env::current_exe().expect("current_exe 失败");
            let quote =
                |value: &Path| format!("'{}'", value.display().to_string().replace('\'', "'\\''"));
            let script = format!(
                "#!/bin/sh\n# MIRVM_RUN_INFO {}\nexec {} runner {} \"$@\"\n",
                serde_json::to_string(&info_path.display().to_string()).unwrap(),
                quote(&self_exe),
                quote(&f)
            );
            std::fs::write(&f, script).unwrap_or_else(|e| {
                eprintln!("mirvm: 写假二进制启动器 {} 失败: {e}", f.display());
                exit(1);
            });
            let mut permissions = std::fs::metadata(&f).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&f, permissions).unwrap_or_else(|e| {
                eprintln!("mirvm: 设置假二进制权限 {} 失败: {e}", f.display());
                exit(1);
            });
        }
        #[cfg(not(unix))]
        std::fs::write(&f, &json).unwrap_or_else(|e| {
            eprintln!("mirvm: 写假二进制 {} 失败: {e}", f.display());
            exit(1);
        });
    }
}

fn fake_info_path(fake_bin: &Path) -> PathBuf {
    let mut path = fake_bin.as_os_str().to_os_string();
    path.push(".mirvm-run.json");
    PathBuf::from(path)
}

fn read_fake_info(fake_bin: &Path) -> std::io::Result<String> {
    let info_path = fake_info_path(fake_bin);
    if info_path.is_file() {
        return std::fs::read_to_string(info_path);
    }
    let launcher = std::fs::read_to_string(fake_bin)?;
    let Some(encoded) = launcher
        .lines()
        .find_map(|line| line.strip_prefix("# MIRVM_RUN_INFO "))
    else {
        // 兼容更新前直接把 JSON 写在 artifact 本体里的缓存。
        return Ok(launcher);
    };
    let path: String = serde_json::from_str(encoded).map_err(std::io::Error::other)?;
    std::fs::read_to_string(path)
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

    let data = read_fake_info(Path::new(&fake_bin)).unwrap_or_else(|e| {
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
        CargoAction, append_encoded_rustflags, cargo_project_command, configured_cargo_wrapper,
        looks_like_nested_rustc_wrapper, read_fake_info, reject_custom_rustc_wrappers,
        runner_args_with_stable_paths,
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
            CargoAction::Run { bin_sel: None },
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
            CargoAction::Run { bin_sel: None },
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
            CargoAction::Run { bin_sel: None },
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
    fn cargo_test_command_keeps_selection_and_harness_arguments_separate() {
        let cargo_args = vec!["--lib".to_string(), "needle".to_string()];
        let harness_args = vec!["--nocapture".to_string(), "--test-threads=1".to_string()];
        let command = cargo_project_command(
            Path::new("/tmp/project"),
            CargoAction::Test {
                cargo_args: &cargo_args,
            },
            &harness_args,
            Path::new("/tmp/sysroot"),
            Path::new("/tmp/mirvm"),
            true,
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args[0], OsStr::new("test"));
        assert!(!args.iter().any(|arg| *arg == OsStr::new("--quiet")));
        assert!(
            args.windows(2)
                .any(|p| p == [OsStr::new("test"), OsStr::new("--locked")])
        );
        assert!(args.windows(3).any(|p| {
            p == [
                OsStr::new("--lib"),
                OsStr::new("needle"),
                OsStr::new("--target"),
            ]
        }));
        assert!(args.windows(3).any(|p| {
            p == [
                OsStr::new("--"),
                OsStr::new("--nocapture"),
                OsStr::new("--test-threads=1"),
            ]
        }));
    }

    #[test]
    fn copied_launcher_still_finds_original_run_info() {
        let root =
            std::env::temp_dir().join(format!("mirvm-cargo-launcher-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let info = root.join("original.mirvm-run.json");
        let copied = root.join("copied-bin");
        std::fs::write(&info, r#"{"args":[],"env":[]}"#).unwrap();
        std::fs::write(
            &copied,
            format!(
                "#!/bin/sh\n# MIRVM_RUN_INFO {}\n",
                serde_json::to_string(&info.display().to_string()).unwrap()
            ),
        )
        .unwrap();
        assert_eq!(read_fake_info(&copied).unwrap(), r#"{"args":[],"env":[]}"#);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_runner_absolutizes_source_and_remaps_diagnostics() {
        let cwd = std::env::current_dir().unwrap();
        let args = runner_args_with_stable_paths(&[
            "--crate-name".into(),
            "demo".into(),
            "member/src/lib.rs".into(),
            "--test".into(),
        ]);
        assert!(args.contains(&cwd.join("member/src/lib.rs").display().to_string()));
        assert!(
            args.contains(&format!("--remap-path-prefix={}/=", cwd.display())),
            "absolute compiler input must still report Cargo's workspace-relative path"
        );
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
