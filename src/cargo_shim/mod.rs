//! Cargo integration in three phases (mechanism ported from cargo-miri, MIT/Apache-2.0):
//!
//! 1. `phase_cargo`: drive the whole dependency graph build with `cargo run`, but point
//!    Cargo's RUSTC at mirvm and inject target.runner=["mirvm","runner"] plus a separate
//!    target dir. Cargo-configured RUSTC_WRAPPER / RUSTC_WORKSPACE_WRAPPER are left as they
//!    are, so Cargo still decides which wrappers ordinary dependencies and workspace members
//!    each pass through. `--target <host>` is forced: it is the switch that separates host
//!    crates (build scripts/proc-macros, compiled normally) from target crates (to be
//!    interpreted, with the MIR sysroot injected).
//! 2. `phase_wrapper`: every cargo rustc invocation goes through here.
//!    - info query/host crate -> pass through to real rustc
//!    - target dependency -> real rustc + `--sysroot <MIR sysroot>` + `-Zalways-encode-mir`
//!    - final runnable bin -> do not compile: write an executable launcher with the full
//!      rustc arguments + environment in a sidecar JSON (plus a real .d to stop cargo from
//!      rebuilding)
//! 3. `phase_runner`: when cargo "runs" the launcher, control returns to us -- read the
//!    sidecar JSON and drive the interpreter with cargo's original arguments.
//!
//! The launcher itself -- what replaces the compiled bin, and the two ends that write and read its
//! recipe -- is [`fake`].

use std::path::{Path, PathBuf};
use std::process::{Command, exit};

mod fake;

pub use fake::parse_runner_invocation;

/// Why the self launchers cargo needs could not be published.
///
/// Publishing one is a symlink into the shared target directory, so the two classes are "the path
/// cannot hold a file at all" (a programming error) and "a step of publishing failed" (a filesystem
/// failure whose `detail` names the step).
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    #[error("internal tool path has no parent directory: {}", path.display())]
    NoParent { path: PathBuf },

    #[error("{detail}: {source}")]
    Io {
        detail: String,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },
}

crate::diag_codes! {
    Error: Runner => {
        NoParent => "cargo_shim.no_parent",
        Io => "cargo_shim.io",
    }
}

impl Error {
    fn io(detail: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            detail: detail.into(),
            source,
        }
    }
}

fn toolchain_rustc() -> PathBuf {
    PathBuf::from(crate::options::build::DEFAULT_SYSROOT).join("bin/rustc")
}

fn toolchain_cargo() -> PathBuf {
    PathBuf::from(crate::options::build::DEFAULT_SYSROOT).join("bin/cargo")
}

fn toolchain_rustdoc() -> PathBuf {
    PathBuf::from(crate::options::build::DEFAULT_SYSROOT).join("bin/rustdoc")
}

pub(crate) fn ensure_self_symlink(self_exe: &Path, path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        if let (Ok(actual), Ok(expected)) =
            (std::fs::canonicalize(path), std::fs::canonicalize(self_exe))
            && actual == expected
        {
            return Ok(());
        }
        let parent = path.parent().ok_or_else(|| Error::NoParent {
            path: path.to_path_buf(),
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::io(
                format!(
                    "cannot create the internal tool directory {}",
                    parent.display()
                ),
                error,
            )
        })?;
        // A link is published the same way an artifact is: fill a staging name, then rename.
        let tmp = crate::store::staging_path(path);
        symlink(self_exe, &tmp).map_err(|error| {
            Error::io(
                format!("cannot create the internal tool {}", tmp.display()),
                error,
            )
        })?;
        crate::store::publish(path, &tmp).map_err(|error| {
            Error::io(
                format!("cannot publish the internal tool {}", path.display()),
                error,
            )
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (self_exe, path);
        Err("Cargo doctest currently supports Unix hosts only".into())
    }
}

pub(super) fn arg_flag_value(args: &[String], flag: &str) -> Option<String> {
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
        eprintln!("mirvm: cannot execute {cmd:?}: {e}");
        exit(1);
    });
    exit(status.code().unwrap_or(1));
}

fn cargo_target_dir() -> PathBuf {
    let mut target_dir = crate::options::get().target_dir.clone();
    if let Some(encoded) = crate::options::get().encoded_rustflags_append.as_deref() {
        let hash = crate::utils::content::fnv1a(encoded.as_bytes());
        target_dir = target_dir
            .join("mirvm-append-rustflags")
            .join(format!("{hash:016x}"));
    }
    target_dir
}

fn cargo_runner_config(self_exe: &Path, capture_directory: Option<&Path>) -> String {
    let quote = |value: &Path| {
        value
            .to_str()
            .expect("mirvm runner path is not UTF-8")
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
    };
    let mut config = format!(
        "target.'cfg(all())'.runner=['{}', 'runner'",
        quote(self_exe)
    );
    if let Some(directory) = capture_directory {
        config.push_str(&format!(
            ", '{}', '{}'",
            crate::cli::INTERNAL_CAPTURE_DIRECTORY_ARG,
            quote(directory)
        ));
    }
    config.push(']');
    config
}

fn ensure_cargo_doctest_tools(
    project_dir: &Path,
    self_exe: &Path,
) -> Result<(PathBuf, PathBuf), Error> {
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
    let self_str = self_exe.to_str().expect("mirvm path is not UTF-8");
    let mut cmd = Command::new(toolchain_cargo());
    cmd.current_dir(project_dir);
    cmd.arg(action.subcommand());
    if locked {
        cmd.arg("--locked");
    }
    action.append_args(&mut cmd);
    // Force the host target: it makes host and target crates distinguishable and activates
    // target.runner
    cmd.arg("--target").arg(crate::options::build::HOST);
    // Every "run a binary" action is redirected to us
    cmd.arg("--config").arg(cargo_runner_config(
        self_exe,
        crate::cli::capture_directory(),
    ));
    // Shared dependency store: every script/project mirvm build uses the same target dir.
    // The cargo fingerprint is a content-addressed compile key (version x features x
    // dependency closure x flags x toolchain), so one crate compilation unit exists once per
    // machine. The final artifact is located through the runner protocol (cargo passes the
    // fake binary path to the runner), never by scanning directories.
    // MIRVM_TARGET_DIR relocates the whole store (for isolation/tests; default
    // $MIRVM_HOME/build/target/mirvm).
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
    crate::options::protocol::set_cargo_session(&mut cmd);
    crate::options::protocol::set_cargo_compiler(&mut cmd);
    cmd.env(crate::options::env_var_name("sysroot"), sysroot);
    // Cargo run keeps the directory from which the user invoked Cargo even
    // when --manifest-path points elsewhere.  We drive Cargo from project_dir
    // for config/workspace discovery, so carry the original directory to the
    // runner and apply it only when guest execution begins.
    crate::options::protocol::set_guest_cwd(&mut cmd, guest_cwd);
    match crate::options::get().sysroot.as_deref() {
        Some(value) => {
            crate::options::protocol::set_caller_sysroot_present(&mut cmd, true);
            crate::options::protocol::set_caller_sysroot(&mut cmd, value);
        }
        None => {
            crate::options::protocol::set_caller_sysroot_present(&mut cmd, false);
            crate::options::protocol::clear_caller_sysroot(&mut cmd);
        }
    }
    cmd
}

/// A user action on the Cargo-compatible track. Both actions share one wrapper/runner
/// protocol; they differ only in whether Cargo picks one run bin or picks and launches
/// several test harnesses in sequence.
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

/// Phase 1: drive cargo in `project_dir`. program_args go to the program that is finally
/// interpreted.
pub fn phase_cargo(
    project_dir: &std::path::Path,
    program_args: &[String],
    bin_sel: Option<&str>,
    ignore_rust_version: bool,
) -> ! {
    let guest_cwd = std::env::current_dir().unwrap_or_else(|error| {
        eprintln!("mirvm: cannot read the caller's current directory: {error}");
        exit(1);
    });
    // Absolutize: a relative project_dir plus current_dir plus join(target/mirvm) nests the
    // target dir as project/project/target, and makes one project's rlib paths drift with the
    // invocation form (relative vs absolute), destabilizing deps-image keys.
    let project_dir =
        &std::path::absolute(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let sysroot = match crate::sysroot::ensure_sysroot() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mirvm: building the sysroot failed: {e}");
            exit(1);
        }
    };
    let self_exe = std::env::current_exe().expect("current_exe failed");
    let locked = crate::options::get().cargo_locked;
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

/// The Cargo-compatible track of `mirvm test`. `cargo_args` are selection arguments that
/// Cargo interprets itself (`--lib/--test/.../TESTNAME`); `harness_args` are the arguments
/// after `--`, passed verbatim to libtest.
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
            eprintln!("mirvm: building the sysroot failed: {e}");
            exit(1);
        }
    };
    let self_exe = std::env::current_exe().expect("current_exe failed");
    let locked = crate::options::get().cargo_locked;
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
    crate::options::protocol::set_doctest_builder(&mut cmd, &doctest_builder);
    crate::options::protocol::set_doctest_run_dir(&mut cmd, project_dir);
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

/// Cargo still selects doctest targets and assembles rustdoc arguments; this only ensures
/// that the temporary crate extracted by rustdoc and the library produced by the Cargo
/// wrapper use the same MIR sysroot, and hands the temporary executable to MIRVM.
pub fn phase_cargo_rustdoc(argv: impl Iterator<Item = String>) -> ! {
    let mut args: Vec<String> = argv.collect();
    if args.iter().any(|arg| arg == "--test") {
        let sysroot = crate::options::get()
            .sysroot
            .as_deref()
            .expect("Cargo rustdoc phase is missing the sysroot option")
            .display()
            .to_string();
        let builder = crate::options::protocol::doctest_builder()
            .expect("Cargo rustdoc phase is missing the doctest builder option");
        args = cargo_doctest_rustdoc_args(args, sysroot, builder);
    }
    let mut command = Command::new(toolchain_rustdoc());
    command.args(args);
    exec(command)
}

/// When Cargo's RUSTC slot invokes mirvm directly there is no `<rustc name>` argument;
/// prepend the pinned toolchain's rustc and continue on the same path as the traditional
/// wrapper.
pub fn phase_compiler(argv: impl Iterator<Item = String>) -> ! {
    phase_wrapper(std::iter::once(toolchain_rustc().display().to_string()).chain(argv))
}

/// Phase 2: compile capture. argv = [<rustc name>, <rustc args...>].
/// The rustc name passed by cargo is ignored (a bare "rustc" would be resolved by rustup
/// against the cwd to the wrong toolchain); the pinned toolchain's rustc is always used,
/// because proc-macro dylibs and rlib metadata must match the interpreting session's compiler
/// version exactly.
pub fn phase_wrapper(mut argv: impl Iterator<Item = String>) -> ! {
    let _rustc_name = argv.next();
    let rustc = toolchain_rustc();
    let mut args: Vec<String> = argv.collect();
    append_encoded_rustflags(
        &mut args,
        crate::options::get().encoded_rustflags_append.as_deref(),
    );

    let is_info_query =
        arg_flag_value(&args, "--print").is_some() || args.iter().any(|a| a == "-vV");
    let is_target = arg_flag_value(&args, "--target").is_some();
    // A missing crate-type means bin (matching cargo-miri); --test is a test harness bin
    let is_runnable = !is_info_query
        && (arg_flag_value(&args, "--crate-type")
            .as_deref()
            .unwrap_or("bin")
            == "bin"
            || args.iter().any(|a| a == "--test"));

    if is_info_query || !is_target {
        // Version query / host crate (build script, proc-macro): compile as is
        let mut cmd = Command::new(&rustc);
        cmd.args(&args);
        exec(cmd);
    }

    // Temporary probe compilations outside cargo's drive: build.rs reads RUSTC_WRAPPER and
    // spawns `$WRAPPER $RUSTC --crate-type=rlib --emit=metadata -o <f> -` to probe toolchain
    // features. The criterion is a missing --out-dir (cargo's dep compilations always pass
    // one) or a `-` stdin source (cargo always uses a file path). Exec real rustc as is: the
    // probe asks whether this toolchain accepts X, which only real rustc can answer. Routing
    // it into the dep channel would panic in run_dep_compiler on the missing --out-dir and
    // race the probe's writeln into EPIPE (build.rs explodes under load, and the probe
    // falsely answers no when idle -- both states are wrong).
    if arg_flag_value(&args, "--out-dir").is_none() || args.iter().any(|a| a == "-") {
        let mut cmd = Command::new(&rustc);
        cmd.args(&args);
        exec(cmd);
    }

    if is_runnable {
        // Final bin: do not compile; write an executable launcher + JSON recipe + real .d
        let info = fake::CrateRunInfo {
            args: fake::runner_args_with_stable_paths(&args),
            env: std::env::vars().collect(),
        };
        fake::write_fake_outputs(&rustc, &args, &info);
        exit(0);
    }

    // Target dependency: inject the MIR sysroot (guaranteeing the same std as the interpreting
    // session) plus full MIR. -Zno-codegen cuts LLVM codegen and object code: the runner
    // consumes only the MIR in rmeta, so object code is pure waste (the ecosystem's 22 rlibs
    // all carried .rcgu.o, ~100MB total). metadata-only rlibs are still produced through
    // rustc's default link path, and DepCallbacks supplies the post-mono const-eval error
    // surface explicitly (cli.rs).
    let sysroot = crate::options::get()
        .sysroot
        .as_deref()
        .expect("wrapper phase is missing the sysroot option")
        .display()
        .to_string();
    let mut dep_args = Vec::with_capacity(args.len() + 5);
    dep_args.push("mirvm-dep-rustc".to_string()); // argv[0] placeholder (the driver skips it)
    dep_args.extend(args);
    dep_args.push("--sysroot".into());
    dep_args.push(sysroot);
    dep_args.push("-Zalways-encode-mir".into());
    dep_args.push("-Zno-codegen".into());
    crate::cli::run_dep_compiler(dep_args)
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    use super::{
        CargoAction, append_encoded_rustflags, cargo_doctest_rustdoc_args, cargo_project_command,
        cargo_runner_config,
    };

    #[test]
    fn capture_directory_is_carried_only_by_the_runner_argv() {
        let config = cargo_runner_config(
            Path::new("/tmp/mirvm"),
            Some(Path::new("/tmp/capture output")),
        );
        assert_eq!(
            config,
            "target.'cfg(all())'.runner=['/tmp/mirvm', 'runner', \
             '--mirvm-capture-directory', '/tmp/capture output']"
        );
    }

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
            key == OsStr::new(crate::options::protocol::CARGO_COMPILER)
                && value == Some(OsStr::new("1"))
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
}
