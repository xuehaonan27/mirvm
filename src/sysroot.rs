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

/// mirvm 本地仓库根（2026-07-18 由 `$XDG_CACHE_HOME/mirvm` 迁址，decision-history
/// §7.14）：内容不是随手可弃的 cache——scripts/<hash> 是不打包世界的本地依赖库、
/// sysroot 是 MIR-rich std 唯一来源、deps/base/ir 是降低加速器、*.{so} 是运行期
/// dlopen 对象——与未来 `.mirvmar` 预分发制品同族，统一放 `$HOME/.mirvm` 管理。
/// `MIRVM_HOME` 环境变量可整体改址（测试/隔离用）。全组件自愈，可整根手删。
pub fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("MIRVM_HOME") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").expect("HOME 未设置");
    PathBuf::from(home).join(".mirvm")
}

/// 确保 MIR-rich sysroot 存在，返回其路径。
///
/// V1 stamp 快路径（S1a，coldstart-research §6）：全仪式每跑要 spawn 两个 rustc 子进程
/// （--print sysroot / -vV）+ 递归 stat 整棵 rust-src 树（builder 判新），实测 ~40–55ms
/// 且 warm 也付。stamp 记 (MIRVM_BUILD_ID, rustc 二进制 len+mtime_ns, builder hash 文件
/// 内容)，三者齐合即免仪式。失效轴对照 rustc-build-sysroot 0.5.13 sysroot_compute_hash：
/// config/mode/rustflags/crate 版本 → BUILD_ID；rustc_version/换 toolchain → rustc 二进制
/// stat；建成与否 → builder hash 文件（兼作存在标记）。**不在防护面**：同一 toolchain 内
/// 手改 rust-src 源树（builder 的全树 stat 走查才抓得到）——逃生门 = 删 stamp 或 sysroot
/// 目录，走全仪式自愈重建。stamp 读写任何失败都只回退全仪式，不引入新错误路径。
pub fn ensure_sysroot() -> anyhow::Result<PathBuf> {
    let target = env!("MIRVM_HOST");
    let sysroot_dir = cache_dir().join(format!("sysroot-{target}"));
    let rustc = toolchain_root().join("bin/rustc");
    let cargo = toolchain_root().join("bin/cargo");

    let builder_hash_file = sysroot_dir
        .join("lib/rustlib")
        .join(target)
        .join(".rustc-build-sysroot-hash");
    let stamp_path = cache_dir().join(format!("sysroot-{target}.stamp"));
    if let Some(want) = stamp_value(&rustc, &builder_hash_file)
        && std::fs::read_to_string(&stamp_path).is_ok_and(|have| have == want)
    {
        return Ok(sysroot_dir);
    }

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

    // 仪式通过后落 stamp（builder hash 文件此刻已是最新）。临时名+rename 原子发布
    // （物化缓存同款纪律）；写失败不致命——下跑走全仪式。
    if let Some(want) = stamp_value(&rustc, &builder_hash_file) {
        let tmp = stamp_path.with_extension(format!("stamp.tmp-{}", std::process::id()));
        if std::fs::write(&tmp, &want).is_ok() {
            let _ = std::fs::rename(&tmp, &stamp_path);
        }
    }
    Ok(sysroot_dir)
}

/// 当前 toolchain/sysroot 的 stamp 值（S4 底座键复用；sysroot 未建成 ⇒ None）。
pub(crate) fn current_stamp_value() -> Option<String> {
    let target = env!("MIRVM_HOST");
    let rustc = toolchain_root().join("bin/rustc");
    let builder_hash_file = cache_dir()
        .join(format!("sysroot-{target}"))
        .join("lib/rustlib")
        .join(target)
        .join(".rustc-build-sysroot-hash");
    stamp_value(&rustc, &builder_hash_file)
}

/// stamp 内容；任一构件缺失（rustc 不在 / sysroot 未建成）→ None（走全仪式）。
fn stamp_value(rustc: &Path, builder_hash_file: &Path) -> Option<String> {
    let md = std::fs::metadata(rustc).ok()?;
    if !md.is_file() {
        return None;
    }
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let builder_hash = std::fs::read_to_string(builder_hash_file).ok()?;
    Some(format!(
        "v1\n{}\n{}\n{}\n{}\n",
        env!("MIRVM_BUILD_ID"),
        md.len(),
        mtime_ns,
        builder_hash.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::stamp_value;

    #[test]
    fn stamp_tracks_rustc_stat_and_builder_hash() {
        let dir = std::env::temp_dir().join(format!("mirvm-sysroot-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rustc = dir.join("rustc");
        let hash = dir.join("hash");

        // 构件缺失 → None（全仪式）
        assert_eq!(stamp_value(&rustc, &hash), None);
        std::fs::write(&rustc, b"fake-rustc").unwrap();
        assert_eq!(stamp_value(&rustc, &hash), None);

        std::fs::write(&hash, "12345\n").unwrap();
        let s0 = stamp_value(&rustc, &hash).expect("齐备即有值");

        // builder hash 变（sysroot 重建/换代）→ stamp 变
        std::fs::write(&hash, "67890\n").unwrap();
        let s1 = stamp_value(&rustc, &hash).unwrap();
        assert_ne!(s0, s1);
        std::fs::write(&hash, "12345\n").unwrap();

        // rustc 二进制内容长度变 → stamp 变
        std::fs::write(&rustc, b"fake-rustc-v2").unwrap();
        assert_ne!(stamp_value(&rustc, &hash).unwrap(), s0);

        // 同长但 mtime 后移（原地换版本）→ stamp 变
        std::fs::write(&rustc, b"fake-rustc").unwrap();
        let s2 = stamp_value(&rustc, &hash).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(7);
        std::fs::File::options()
            .write(true)
            .open(&rustc)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_ne!(stamp_value(&rustc, &hash).unwrap(), s2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
