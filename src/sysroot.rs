//! 构建并缓存"携带全量 MIR"的 sysroot（DESIGN.md D5）。
//!
//! 发行版 std 的 rlib 只给泛型/#[inline] 函数编码 MIR，解释非泛型 std 函数
//! 必须用 `-Zalways-encode-mir` 从 rust-src 重建 std（Miri sysroot 同款做法，
//! 通过 rustc-build-sysroot crate 完成，内部按内容 hash 缓存判新）。

use std::path::{Path, PathBuf};
use std::process::Command;

use rustc_build_sysroot::{BuildMode, SysrootBuilder, SysrootConfig, SysrootStatus};

/// 编译期烘焙的 toolchain sysroot（见 build.rs），rustc/cargo 从这里取。
fn toolchain_root() -> &'static Path {
    Path::new(env!("MIRVM_DEFAULT_SYSROOT"))
}

pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").expect("HOME 未设置");
            PathBuf::from(home).join(".cache")
        })
        .join("mirvm")
}

/// 确保 MIR-rich sysroot 存在，返回其路径。已缓存时开销可忽略。
pub fn ensure_sysroot() -> anyhow::Result<PathBuf> {
    let target = env!("MIRVM_HOST");
    let sysroot_dir = cache_dir().join(format!("sysroot-{target}"));
    let rustc = toolchain_root().join("bin/rustc");
    let cargo = toolchain_root().join("bin/cargo");

    let src_dir = rustc_build_sysroot::rustc_sysroot_src(Command::new(&rustc))?;

    // 显式钉死版本信息：builder 默认用 PATH 上的 rustc 算缓存哈希，
    // 而 rustup 代理按 cwd 解析 toolchain——项目外运行会解析到别的版本，
    // 造成缓存哈希乒乓、sysroot 反复重建。
    let version = rustc_version::VersionMeta::for_command(Command::new(&rustc))?;

    let status = SysrootBuilder::new(&sysroot_dir, target)
        .build_mode(BuildMode::Build)
        .rustc_version(version)
        .sysroot_config(SysrootConfig::WithStd {
            std_features: ["panic-unwind", "backtrace"]
                .into_iter()
                .map(Into::into)
                .collect(),
        })
        // 与发行版 std 对齐：debug-assertions 关、overflow-checks 开；外加全量 MIR
        .rustflags([
            "-Zalways-encode-mir",
            "-Cdebug-assertions=off",
            "-Coverflow-checks=on",
        ])
        .cargo({
            let mut cmd = Command::new(&cargo);
            cmd.env("RUSTC", &rustc);
            cmd
        })
        .when_build_required(|| {
            eprintln!("mirvm: 正在构建带全量 MIR 的 sysroot（一次性，需几分钟）...");
        })
        .build_from_source(&src_dir)?;

    if status == SysrootStatus::SysrootBuilt {
        eprintln!("mirvm: sysroot 构建完成: {}", sysroot_dir.display());
    }
    Ok(sysroot_dir)
}
