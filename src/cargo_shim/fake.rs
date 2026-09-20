//! The launcher recipe: what replaces the compiled binary, and the two ends that write and read it.
//!
//! [`write_fake_outputs`] does not produce a bin. It writes a shell script that re-enters mirvm
//! (`mirvm runner <path>`), a sidecar JSON holding the rustc arguments and the environment cargo had
//! at that moment, and a real `.d` so cargo's fingerprint stops asking for the bin — a fingerprint
//! that stayed dirty would re-record the recipe, and cargo snapshots dep-info when the rustc
//! invocation ends, so a source edit is what has to make the recorded environment observable again.
//!
//! [`parse_runner_invocation`] is the other end: it reads the recipe back and rebuilds the
//! interpreting session's arguments, the program argv and the environment to install. The recipe is
//! the only place cargo's build-time environment survives — by the time the runner starts, cargo has
//! exited and the internal protocol variables are gone, so a `CARGO_BIN_EXE_*` launcher that the
//! guest re-executes falls back to what the recipe recorded.

use std::path::{Path, PathBuf};
use std::process::{Command, exit};

use serde::{Deserialize, Serialize};

use super::arg_flag_value;

#[derive(Serialize, Deserialize)]
pub(super) struct CrateRunInfo {
    pub(super) args: Vec<String>,
    pub(super) env: Vec<(String, String)>,
}

/// In a Cargo workspace, rustc receives `member/src/lib.rs` from the workspace root while the
/// runner starts in the member directory. Absolutize the crate root and remap it back to the
/// original relative spelling: compilation is then independent of the runner cwd, while
/// diagnostics and file!() still match Cargo's original invocation.
pub(super) fn runner_args_with_stable_paths(args: &[String]) -> Vec<String> {
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

pub(super) fn write_fake_outputs(rustc: &Path, args: &[String], info: &CrateRunInfo) {
    let out_dir = arg_flag_value(args, "--out-dir").unwrap_or_default();
    let crate_name = arg_flag_value(args, "--crate-name").unwrap_or_default();

    // dep-info must be **real**. An empty stub used to leave cargo without the bin's source
    // file list, and cargo snapshots dep-info into .fingerprint when the rustc invocation
    // ends (writing it afterwards is invisible), so a source edit never triggered
    // re-recording of the fake binary and the recorded env/arguments fossilized. Real rustc
    // emits only dep-info here to get the exact list (including mod/include!/env! tracking;
    // deps rlibs are ready by now, and this only happens when the bin fingerprint is dirty).
    // stderr is silenced: the runner session replays diagnostics loudly, keeping the same
    // single-warning behavior as native. On failure, fall back to a one-line crate-root list
    // (.d only affects re-record frequency; run semantics always come from the runner reading
    // the source, so under-recording beats wrong semantics).
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
        // The same MIR sysroot as target dependencies (required to resolve use std::*)
        if let Some(sysroot) = crate::options::get().sysroot.as_deref() {
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

    // Let rustc tell us the artifact file names (they depend on the target's suffix rules)
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
        let output = cmd.output().expect("rustc --print file-names failed");
        assert!(
            output.status.success(),
            "rustc --print file-names failed: {output:?}"
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
            eprintln!(
                "mirvm: failed to write the fake binary recipe {}: {e}",
                info_path.display()
            );
            exit(1);
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let self_exe = std::env::current_exe().expect("current_exe failed");
            let quote =
                |value: &Path| format!("'{}'", value.display().to_string().replace('\'', "'\\''"));
            let script = format!(
                "#!/bin/sh\n# MIRVM_RUN_INFO {}\nexec {} runner {} \"$@\"\n",
                serde_json::to_string(&info_path.display().to_string()).unwrap(),
                quote(&self_exe),
                quote(&f)
            );
            std::fs::write(&f, script).unwrap_or_else(|e| {
                eprintln!(
                    "mirvm: failed to write the fake binary launcher {}: {e}",
                    f.display()
                );
                exit(1);
            });
            let mut permissions = std::fs::metadata(&f).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&f, permissions).unwrap_or_else(|e| {
                eprintln!(
                    "mirvm: failed to set fake binary permissions {}: {e}",
                    f.display()
                );
                exit(1);
            });
        }
        #[cfg(not(unix))]
        std::fs::write(&f, &json).unwrap_or_else(|e| {
            eprintln!(
                "mirvm: failed to write the fake binary {}: {e}",
                f.display()
            );
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
        // Compatibility with caches written before the update, which stored the JSON in the
        // artifact itself.
        return Ok(launcher);
    };
    let path: String = serde_json::from_str(encoded).map_err(std::io::Error::other)?;
    std::fs::read_to_string(path)
}

/// Phase 3: runner. argv = [<fake binary path>, <program args...>].
/// Returns (the interpreting session's rustc args, the program argv, the environment to set).
pub fn parse_runner_invocation(
    mut argv: impl Iterator<Item = String>,
) -> (Vec<String>, Vec<String>, Vec<(String, String)>) {
    let fake_bin = argv.next().unwrap_or_else(|| {
        crate::cli::diagnostics::control(format_args!(
            "mirvm runner: missing binary path argument"
        ));
        exit(2);
    });
    let program_args: Vec<String> = argv.collect();

    let data = read_fake_info(Path::new(&fake_bin)).unwrap_or_else(|e| {
        crate::cli::diagnostics::control(format_args!(
            "mirvm runner: failed to read {fake_bin}: {e}"
        ));
        exit(1);
    });
    let info: CrateRunInfo = serde_json::from_str(&data).unwrap_or_else(|_| {
        crate::cli::diagnostics::control(format_args!(
            "mirvm runner: {fake_bin} is not a mirvm fake binary (try deleting target/mirvm and rerunning)"
        ));
        exit(1);
    });

    // Assemble the interpreting session's arguments: argv[0] placeholder + cargo's original
    // arguments + our sysroot. Strip JSON diagnostic/artifact notifications (they were for
    // cargo, which has now exited). When Cargo invokes the runner directly, the internal
    // sysroot is in the environment; a CARGO_BIN_EXE_* launcher, however, may be re-executed
    // by the guest, at which point the user's environment has already dropped every internal
    // variable by contract. The sidecar recipe recorded the build environment that produced
    // the launcher, so it is the fallback both paths share without user intervention.
    let sysroot = crate::options::get()
        .sysroot
        .as_deref()
        .map(|path| path.display().to_string())
        .or_else(|| {
            info.env.iter().find_map(|(key, value)| {
                (key == crate::options::env_var_name("sysroot")).then(|| value.clone())
            })
        })
        .unwrap_or_else(|| {
            crate::cli::diagnostics::control(format_args!(
                "mirvm runner: the launcher recipe lacks MIRVM_SYSROOT (clean the matching target and rebuild)"
            ));
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

    // Program argv: argv[0] is the fake binary path (matching cargo run)
    let mut prog_argv = vec![fake_bin];
    prog_argv.extend(program_args);

    (rustc_args, prog_argv, info.env)
}

#[cfg(test)]
mod tests {
    use super::*;

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
