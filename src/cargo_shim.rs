//! cargo 集成三阶段（机制移植自 cargo-miri，MIT/Apache-2.0）：
//!
//! 1. `phase_cargo`：以 `cargo run` 驱动整个依赖图构建，但把 Cargo 的
//!    RUSTC 指向 mirvm，并注入 target.runner=["mirvm","runner"] + 独立 target dir。
//!    Cargo 自己配置的 RUSTC_WRAPPER / RUSTC_WORKSPACE_WRAPPER 原样保留；因此
//!    Cargo 仍负责决定普通依赖与 workspace 成员分别经过哪些 wrapper。
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

fn toolchain_rustdoc() -> PathBuf {
    PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT")).join("bin/rustdoc")
}

pub(crate) fn ensure_self_symlink(self_exe: &Path, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        if let (Ok(actual), Ok(expected)) =
            (std::fs::canonicalize(path), std::fs::canonicalize(self_exe))
            && actual == expected
        {
            return Ok(());
        }
        let parent = path
            .parent()
            .ok_or_else(|| format!("内部工具路径没有父目录: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建内部工具目录 {} 失败: {error}", parent.display()))?;
        let tmp = parent.join(format!(
            ".mirvm-tool-{}-{}.tmp",
            std::process::id(),
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("清理内部工具 {} 失败: {error}", tmp.display())),
        }
        symlink(self_exe, &tmp)
            .map_err(|error| format!("创建内部工具 {} 失败: {error}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|error| format!("发布内部工具 {} 失败: {error}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (self_exe, path);
        Err("Cargo doctest 当前只支持 Unix 主机".into())
    }
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

fn cargo_target_dir() -> PathBuf {
    let mut target_dir = std::env::var_os("MIRVM_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::sysroot::cache_dir().join("target/mirvm"));
    if let Some(encoded) =
        std::env::var_os("MIRVM_ENCODED_RUSTFLAGS_APPEND").filter(|value| !value.is_empty())
    {
        let hash = crate::lower::asm::fnv1a(encoded.as_encoded_bytes());
        target_dir = target_dir
            .join("mirvm-append-rustflags")
            .join(format!("{hash:016x}"));
    }
    target_dir
}

fn ensure_cargo_doctest_tools(
    project_dir: &Path,
    self_exe: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let target_dir = cargo_target_dir();
    let tools_root = if target_dir.is_absolute() {
        target_dir
    } else {
        project_dir.join(target_dir)
    };
    let tools_dir = tools_root.join(".mirvm-tools");
    let rustdoc = tools_dir.join("mirvm-rustdoc");
    let doctest_builder = tools_dir.join("mirvm-doctest-builder");
    ensure_self_symlink(self_exe, &rustdoc)?;
    ensure_self_symlink(self_exe, &doctest_builder)?;
    Ok((rustdoc, doctest_builder))
}

fn cargo_project_command(
    project_dir: &std::path::Path,
    guest_cwd: &std::path::Path,
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
    let target_dir = cargo_target_dir();
    // These flags are appended inside our rustc wrapper, after Cargo has
    // computed its normal fingerprint.  Partition only this exceptional
    // channel by content so a changed value cannot reuse a fake binary (or a
    // dependency rlib) recorded with the old flags.  Cargo-visible flags keep
    // using Cargo's own fingerprints and the ordinary shared target store.
    cmd.arg("--target-dir").arg(&target_dir);
    if matches!(action, CargoAction::Run { .. }) {
        cmd.arg("--quiet");
    }
    if !program_args.is_empty() {
        cmd.arg("--");
        cmd.args(program_args);
    }

    // MIRVM occupies Cargo's compiler slot rather than either wrapper slot.
    // Cargo can therefore apply its ordinary wrapper outside the workspace
    // wrapper exactly as it normally would, including wrappers supplied by
    // config files and the CARGO_BUILD_* environment aliases. MIRVM remains
    // innermost and sees the final argument vector after both wrappers.
    cmd.env("RUSTC", self_str);
    cmd.env("MIRVM_CARGO_SESSION", "1");
    cmd.env("MIRVM_CARGO_COMPILER", "1");
    cmd.env("MIRVM_SYSROOT", sysroot);
    // Cargo run keeps the directory from which the user invoked Cargo even
    // when --manifest-path points elsewhere.  We drive Cargo from project_dir
    // for config/workspace discovery, so carry the original directory to the
    // runner and apply it only when guest execution begins.
    cmd.env("MIRVM_GUEST_CWD", guest_cwd);
    match std::env::var_os("MIRVM_SYSROOT") {
        Some(value) => {
            cmd.env("MIRVM_CALLER_SYSROOT_PRESENT", "1");
            cmd.env("MIRVM_CALLER_SYSROOT", value);
        }
        None => {
            cmd.env("MIRVM_CALLER_SYSROOT_PRESENT", "0");
            cmd.env_remove("MIRVM_CALLER_SYSROOT");
        }
    }
    cmd
}

/// Cargo 兼容轨的用户动作。两种动作共用同一 wrapper/runner 协议；区别只在
/// Cargo 负责选择一个 run bin，还是选择并串行启动若干 test harness。
#[derive(Clone, Copy)]
enum CargoAction<'a> {
    Run {
        bin_sel: Option<&'a str>,
        ignore_rust_version: bool,
    },
    Test {
        cargo_args: &'a [String],
    },
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
            Self::Run {
                bin_sel: Some(bin),
                ignore_rust_version,
            } => {
                cmd.arg("--bin").arg(bin);
                if ignore_rust_version {
                    cmd.arg("--ignore-rust-version");
                }
            }
            Self::Run {
                bin_sel: None,
                ignore_rust_version,
            } => {
                if ignore_rust_version {
                    cmd.arg("--ignore-rust-version");
                }
            }
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
    ignore_rust_version: bool,
) -> ! {
    let guest_cwd = std::env::current_dir().unwrap_or_else(|error| {
        eprintln!("mirvm: 无法读取调用者当前目录: {error}");
        exit(1);
    });
    // 绝对化：relative project_dir + current_dir + join(target/mirvm) 会把 target 目录
    // 拼成 project/project/target 的重复嵌套（A2 gate 实测）——且使同一项目的 rlib 路径
    // 随调用形态（相对/绝对）漂移，deps-image 键失稳。
    let project_dir =
        &std::path::absolute(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
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
        &guest_cwd,
        CargoAction::Run {
            bin_sel,
            ignore_rust_version,
        },
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
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mirvm: 构建 sysroot 失败: {e}");
            exit(1);
        }
    };
    let self_exe = std::env::current_exe().expect("current_exe 失败");
    let locked = std::env::var_os("MIRVM_CARGO_LOCKED").is_some();
    let (rustdoc, doctest_builder) = ensure_cargo_doctest_tools(project_dir, &self_exe)
        .unwrap_or_else(|error| {
            eprintln!("mirvm: {error}");
            exit(1);
        });
    let mut cmd = cargo_project_command(
        project_dir,
        project_dir,
        CargoAction::Test { cargo_args },
        harness_args,
        &sysroot,
        &self_exe,
        locked,
    );
    cmd.env("RUSTDOC", rustdoc);
    cmd.env("MIRVM_DOCTEST_BUILDER", doctest_builder);
    cmd.env("MIRVM_DOCTEST_RUN_DIR", project_dir);
    exec(cmd)
}

pub fn is_cargo_rustdoc(argv0: &Path) -> bool {
    argv0
        .file_name()
        .is_some_and(|name| name == "mirvm-rustdoc")
}

fn remove_value_arg(args: &mut Vec<String>, flag: &str) {
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut index = 0;
    while index < args.len() {
        if args[index] == flag {
            index += 2;
            continue;
        }
        if args[index].starts_with(&format!("{flag}=")) {
            index += 1;
            continue;
        }
        out.push(args[index].clone());
        index += 1;
    }
    *args = out;
}

fn replace_value_arg(args: &mut Vec<String>, flag: &str, value: String) {
    remove_value_arg(args, flag);
    args.push(flag.to_string());
    args.push(value);
}

fn cargo_doctest_rustdoc_args(
    mut args: Vec<String>,
    sysroot: String,
    builder: String,
) -> Vec<String> {
    remove_value_arg(&mut args, "--test-runtool");
    remove_value_arg(&mut args, "--test-runtool-arg");
    replace_value_arg(&mut args, "--sysroot", sysroot);
    replace_value_arg(&mut args, "--test-builder", builder);
    if !args
        .windows(2)
        .any(|pair| pair == ["-Z", "unstable-options"])
        && !args.iter().any(|arg| arg == "-Zunstable-options")
    {
        args.push("-Z".into());
        args.push("unstable-options".into());
    }
    args
}

/// Cargo 仍负责选择 doctest target 和组织 rustdoc 参数；这里只保证 rustdoc
/// 提取出的临时 crate 与 Cargo wrapper 产出的库使用同一套 MIR sysroot，并把
/// 临时可执行文件交给 MIRVM。
pub fn phase_cargo_rustdoc(argv: impl Iterator<Item = String>) -> ! {
    let mut args: Vec<String> = argv.collect();
    if args.iter().any(|arg| arg == "--test") {
        let sysroot = std::env::var("MIRVM_SYSROOT").expect("Cargo rustdoc 阶段缺少 MIRVM_SYSROOT");
        let builder = std::env::var("MIRVM_DOCTEST_BUILDER")
            .expect("Cargo rustdoc 阶段缺少 MIRVM_DOCTEST_BUILDER");
        args = cargo_doctest_rustdoc_args(args, sysroot, builder);
    }
    let mut command = Command::new(toolchain_rustdoc());
    command.args(args);
    exec(command)
}

/// Cargo 的 RUSTC 槽直接调用 mirvm 时没有 `<rustc 名字>` 参数；补上固定
/// toolchain 的 rustc 后进入与传统 wrapper 相同的处理路径。
pub fn phase_compiler(argv: impl Iterator<Item = String>) -> ! {
    phase_wrapper(std::iter::once(toolchain_rustc().display().to_string()).chain(argv))
}

/// 阶段 2：编译捕获。argv = [<rustc 名字>, <rustc 参数...>]。
/// 注意：忽略 cargo 传来的 rustc 名字（裸 "rustc" 会被 rustup 按 cwd 解析到错误
/// toolchain），一律用 pinned toolchain 的 rustc——proc-macro dylib 与 rlib 元数据
/// 都必须和解释会话的编译器版本严格一致。
pub fn phase_wrapper(mut argv: impl Iterator<Item = String>) -> ! {
    let _rustc_name = argv.next();
    let rustc = toolchain_rustc();
    let mut args: Vec<String> = argv.collect();
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
    // Cargo 直接调用 runner 时环境里有内部 sysroot；CARGO_BIN_EXE_* 启动器却可能
    // 被 guest 再次执行，此时用户运行环境已经按合同去掉了所有内部变量。旁置配方
    // 保存了生成该启动器时的构建环境，因此它是两条路径共同、无需用户介入的后备。
    let sysroot = std::env::var("MIRVM_SYSROOT")
        .ok()
        .or_else(|| {
            info.env
                .iter()
                .find_map(|(key, value)| (key == "MIRVM_SYSROOT").then(|| value.clone()))
        })
        .unwrap_or_else(|| {
            eprintln!("mirvm runner: 启动器配方缺少 MIRVM_SYSROOT（请清理对应 target 后重建）");
            exit(1);
        });
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
        CargoAction, append_encoded_rustflags, cargo_doctest_rustdoc_args, cargo_project_command,
        read_fake_info, runner_args_with_stable_paths,
    };

    #[test]
    fn cargo_doctest_replaces_the_native_runner_and_sysroot() {
        let args = cargo_doctest_rustdoc_args(
            [
                "--test",
                "src/lib.rs",
                "--test-runtool=/tmp/mirvm",
                "--test-runtool-arg",
                "runner",
                "--sysroot",
                "/old/sysroot",
                "--test-builder=/old/builder",
                "--cfg",
                "kept",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            "/mirvm/sysroot".into(),
            "/mirvm/builder".into(),
        );
        assert!(!args.iter().any(|arg| arg.starts_with("--test-runtool")));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sysroot", "/mirvm/sysroot"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--test-builder", "/mirvm/builder"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--cfg", "kept"]));
        assert_eq!(args.iter().filter(|arg| *arg == "--sysroot").count(), 1);
    }

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
            Path::new("/tmp/caller"),
            CargoAction::Run {
                bin_sel: None,
                ignore_rust_version: false,
            },
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
            Path::new("/tmp/caller"),
            CargoAction::Run {
                bin_sel: None,
                ignore_rust_version: false,
            },
            &[],
            Path::new("/tmp/sysroot"),
            Path::new("/tmp/mirvm"),
            false,
        );
        assert!(command.get_args().all(|arg| arg != OsStr::new("--locked")));
    }

    #[test]
    fn cargo_project_command_occupies_rustc_and_leaves_wrapper_slots_to_cargo() {
        let command = cargo_project_command(
            Path::new("/tmp/project"),
            Path::new("/tmp/caller"),
            CargoAction::Run {
                bin_sel: None,
                ignore_rust_version: false,
            },
            &[],
            Path::new("/tmp/sysroot"),
            Path::new("/tmp/mirvm"),
            true,
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == OsStr::new("RUSTC") && value == Some(OsStr::new("/tmp/mirvm"))
        }));
        assert!(command.get_envs().any(|(key, value)| {
            key == OsStr::new("MIRVM_CARGO_COMPILER") && value == Some(OsStr::new("1"))
        }));
        for key in [
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        ] {
            assert!(
                command
                    .get_envs()
                    .all(|(candidate, _)| candidate != OsStr::new(key))
            );
        }
    }

    #[test]
    fn cargo_test_command_keeps_selection_and_harness_arguments_separate() {
        let cargo_args = vec!["--lib".to_string(), "needle".to_string()];
        let harness_args = vec!["--nocapture".to_string(), "--test-threads=1".to_string()];
        let command = cargo_project_command(
            Path::new("/tmp/project"),
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
}
