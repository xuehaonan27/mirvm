//! Build and cache the sysroot that "carries full MIR" (DESIGN.md D5).
//!
//! Release std rlibs only encode MIR for generic/#[inline] functions; interpreting non-generic
//! std functions requires rebuilding std from rust-src with `-Zalways-encode-mir`. Since D15 P4
//! cut ⑥a, this is done directly by **cargoless's own scheduler** (previously driven by the
//! rustc-build-sysroot crate, which ran cargo — std's backtrace closure resolved via crates.io,
//! .d referenced `~/.cargo/registry`, and the whole process needed a cargo process; this slice
//! removes that: zero cargo processes, zero crates.io / zero ~/.cargo dependencies):
//!
//! - Pseudo-root package: materialized synthetic manifest at `cache_dir()/sysroot-build/root/`
//!   (path edges point to `library/{std,test,proc_macro}`, std with panic-unwind+backtrace
//!   features — aligned with the old build's std_features) + augmented library/Cargo.lock
//!   (original text + pseudo-root row), resolve uses lock mode with all versions pinned;
//! - Supply side = `VendorDir` (`library/vendor/` + four `[patch.crates-io]` overrides —
//!   rustc-std-workspace trio and windows-sys point back into library/);
//! - Compile = same driver::compile_plan pipeline (Layout::at points to sysroot lib flat dir
//!   + separate staging for host artifacts; --sysroot passed **toolchain** — outputs cannot be
//!     used as their own compile input); flags aligned with the old build:
//!   - debug-assertions off, overflow-checks on;
//!   - `-Zalways-encode-mir` carried by dep_rustc_args;
//!   - `-Zforce-unstable-if-unmarked` on the rustflags channel (target units only).
//!
//!   Dependency artifacts still use -Zno-codegen metadata-only — mirvm only consumes MIR,
//!   object code is pure waste (old sysroot carrying object code was a cargo historical shape,
//!   not a requirement).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::cargoless::driver::compile_plan;
use crate::cargoless::manifest::{OptLevel, PackageManifest, ProfileFlags};
use crate::cargoless::schedule::Layout;
use crate::cargoless::vendor::VendorDir;
use crate::cargoless::{buildrs, resolve};

/// Toolchain sysroot baked at compile time (see build.rs); rustc takes it from here.
fn toolchain_root() -> &'static Path {
    Path::new(env!("MIRVM_DEFAULT_SYSROOT"))
}

/// rust-src 的 library/ 树（std 系 workspace crate 与 vendor/ 的家乡）。
fn library_dir() -> PathBuf {
    toolchain_root().join("lib/rustlib/src/rust/library")
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

/// stamp 文件（内容键）在 sysroot 内的位置。
fn stamp_file(sysroot_dir: &Path) -> PathBuf {
    sysroot_dir
        .join("lib/rustlib")
        .join(env!("MIRVM_HOST"))
        .join(".mirvm-sysroot-hash")
}

/// 确保 MIR-rich sysroot 存在，返回其路径。
///
/// 快路径 = 重算内容键与 stamp 文件比对；任一不符（或 stamp 缺席）走重建。
/// 重建全量在 staging + tmp 目录进行，原子 rename 发布——旧 sysroot 全程
/// 可用到被换下的最后一刻（中间崩溃：tmp/旧目残留，下跑 stamp 缺席自愈）。
pub fn ensure_sysroot() -> anyhow::Result<PathBuf> {
    let target = env!("MIRVM_HOST");
    let sysroot_dir = cache_dir().join(format!("sysroot-{target}"));
    if let Some(want) = stamp_value()
        && std::fs::read_to_string(stamp_file(&sysroot_dir)).is_ok_and(|have| have == want)
    {
        return Ok(sysroot_dir);
    }
    build_sysroot(&sysroot_dir)?;
    Ok(sysroot_dir)
}

/// 当前已建 sysroot 的 stamp 值（S4 底座键复用；sysroot 未建成 ⇒ None）。
pub(crate) fn current_stamp_value() -> Option<String> {
    let target = env!("MIRVM_HOST");
    std::fs::read_to_string(stamp_file(&cache_dir().join(format!("sysroot-{target}")))).ok()
}

/// 内容键（换代判据）：rustc 二进制 stat + library/ 顶层哨兵 + 构建配方
/// 序列化。toolchain 换（rustc stat + 哨兵双抓——rustup 换装会动顶层目录
/// mtime）、构建配方变（序列化段），都抓得到。
/// **不含 MIRVM_BUILD_ID**：mirvm 换版不重建 sysroot——旧设计亦然
/// （rustc-build-sysroot 的 builder hash 不含 BUILD_ID，stamp 里的
/// BUILD_ID 只强制「仪式」（一次 no-op 新鲜度核对），不强制重建）。
/// cargo 轨 dep 缓存的指纹看不见 sysroot 内容，BUILD_ID 级重建会把
/// 旧 rmeta 混进新 sysroot（E0463 实证）；mirvm 侧的失效面由各自键
/// 里的 BUILD_ID 成分兜（cargoless fp/baseimage key 均自带）。
/// **不做全树 stat**：2348 文件的递归盖戳实测 ~20ms/跑，fib(32) JIT
/// 硬门（<80ms）直接被打红（103ms 实锤）——每跑一次的快路径付不起。
/// 防护面与旧 V1 stamp 对齐：toolchain 内手改 rust-src 叶子文件不在
/// 防护面，逃生门 = 删 stamp 或 sysroot 目录，下跑自愈重建。
/// rust-src 缺席（哨兵失败）⇒ None ⇒ 走重建，重建处在 rust-src 检查响亮。
fn stamp_value() -> Option<String> {
    let mut key = rustc_stat()?;
    key.push('\n');
    key.push_str(&library_sentinel().ok()?);
    key.push('\n');
    // 配方序列化：与 build_sysroot 实际使用的 profile/rustflags 同源
    let p = sysroot_profile();
    key.push_str(&format!(
        "da{} oc{} opt{}\n",
        p.debug_assertions as u8, p.overflow_checks as u8, p.opt_level
    ));
    for f in sysroot_rustflags() {
        key.push_str(&f);
        key.push('\n');
    }
    Some(key)
}

/// rustc 二进制 stat 串（len + mtime_ns；缺席 ⇒ None）。
fn rustc_stat() -> Option<String> {
    let md = std::fs::metadata(toolchain_root().join("bin/rustc")).ok()?;
    if !md.is_file() {
        return None;
    }
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}\n{}", md.len(), mtime_ns))
}

/// sysroot 构建配方（对齐发行版 std，decision-history 同款钉：
/// debug-assertions 关、overflow-checks 开；全量 MIR 由 dep_rustc_args
/// 自带 -Zalways-encode-mir，不在此列）。
fn sysroot_profile() -> ProfileFlags {
    ProfileFlags {
        debug_assertions: false,
        overflow_checks: true,
        opt_level: OptLevel::O0,
    }
}

/// sysroot 构建的 rustflags（走切⑤a 通道，实证只落 target 单元——build.rs
/// 编译不吃，与 cargo 行为一致）。std 系 crate 的 unstable 特性按
///「未标记也放行」处理（rustbuild/cargo -Zbuild-std 同枚旗）。
fn sysroot_rustflags() -> Vec<String> {
    vec!["-Zforce-unstable-if-unmarked".to_string()]
}

/// library/ 顶层哨兵：library/ 本体与各一级条目（子目录/文件）的
/// (名字, len, mtime_ns) 排序折叠（~40 次 stat，亚毫秒级）。换代语义：
/// rustup 装/换 rust-src 会替换顶层条目（目录 mtime 动）；同版重装 =
/// 同内容 = 本不需重建，哨兵不变正合意。叶子文件手改抓不到（见
/// stamp_value 头注的防护面说明与逃生门）。
fn library_sentinel() -> Result<String, String> {
    let root = library_dir();
    let mut rows: Vec<String> = Vec::new();
    let mut put = |p: &Path, name: String| -> Result<(), String> {
        let md = std::fs::metadata(p)
            .map_err(|e| format!("rust-src 哨兵 stat 失败 {}: {e}", p.display()))?;
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        rows.push(format!("{name}:{}:{}", md.len(), mtime_ns));
        Ok(())
    };
    put(&root, ".".to_string())?;
    let rd = std::fs::read_dir(&root)
        .map_err(|e| format!("rust-src library 目录读取失败 {}: {e}", root.display()))?;
    for ent in rd {
        let ent = ent.map_err(|e| format!("rust-src library 条目读取失败: {e}"))?;
        put(&ent.path(), ent.file_name().to_string_lossy().into_owned())?;
    }
    rows.sort();
    Ok(rows.join("\u{1e}"))
}

/// fp 的 sysroot 成分（schedule::fingerprints 第三料）：编译用 toolchain
/// 的盖戳（BUILD_ID + rustc 二进制 len+mtime_ns）。toolchain 换 ⇒ fp 变 ⇒
/// staging 的 host 侧缓存（跨重建复用）正确失效。
fn toolchain_stamp() -> String {
    match rustc_stat() {
        Some(stat) => format!("{}\n{}", env!("MIRVM_BUILD_ID"), stat),
        // 拿不到不致命：fp 粗一档（BUILD_ID 仍在），不引入新错误路径
        None => format!("{}\nrustc-stat-missing", env!("MIRVM_BUILD_ID")),
    }
}

/// [patch.crates-io] 四件（library/Cargo.toml 实锤）：registry 名、本地身。
fn workspace_overrides(library: &Path) -> BTreeMap<String, PathBuf> {
    [
        "rustc-std-workspace-core",
        "rustc-std-workspace-alloc",
        "rustc-std-workspace-std",
        "windows-sys",
    ]
    .iter()
    .map(|n| (n.to_string(), library.join(n)))
    .collect()
}

/// 伪根物化（write-if-changed——mtime 稳定是 fp/增量前提，driver.rs 同款纪律）：
/// - Cargo.toml：path 边指 library/{std,test,proc_macro}，std 带
///   panic-unwind+backtrace（与旧构建 std_features 对齐）。proc_macro 必须
///   在——它是 proc-macro crate 的桥，标准 sysroot 必有（旧构建同）；它不在
///   std+test 依赖闭包里，单列。
/// - Cargo.lock：library/Cargo.lock 原文 + 伪根包行——resolve 的 lock 模式
///   从根行出发走图，全图版本钉死（root=library/ 的话伪根不在 lock 会响亮，
///   且 toolchain 目录不可写——故伪根物化在自家 staging）。
fn materialize_pseudo_root(library: &Path, root_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root_dir)?;
    let toml = format!(
        "[package]\nname = \"mirvm-mir-sysroot\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
         \n[dependencies]\n\
         std = {{ path = '{}', features = [\"panic-unwind\", \"backtrace\"] }}\n\
         test = {{ path = '{}' }}\n\
         proc_macro = {{ path = '{}' }}\n",
        library.join("std").display(),
        library.join("test").display(),
        library.join("proc_macro").display(),
    );
    write_if_changed(&root_dir.join("Cargo.toml"), toml.as_bytes())?;
    let lock_src = std::fs::read_to_string(library.join("Cargo.lock"))
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", library.join("Cargo.lock").display()))?;
    let lock = format!(
        "{lock_src}\n[[package]]\nname = \"mirvm-mir-sysroot\"\nversion = \"0.0.0\"\n\
         dependencies = [\n \"proc_macro\",\n \"std\",\n \"test\",\n]\n"
    );
    write_if_changed(&root_dir.join("Cargo.lock"), lock.as_bytes())?;
    Ok(())
}

/// 内容相同不重写（mtime 稳定是指纹/增量前提）。
fn write_if_changed(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if std::fs::read(path).ok().is_none_or(|old| old != bytes) {
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

/// 全量重建：伪根 → resolve（VendorDir 供给）→ compile_plan（toolchain 做
/// 编译底座）→ stamp 落 tmp → 原子 rename 发布。
fn build_sysroot(sysroot_dir: &Path) -> anyhow::Result<()> {
    let target = env!("MIRVM_HOST");
    let library = library_dir();
    if !library.join("std/Cargo.toml").is_file() {
        anyhow::bail!(
            "rust-src 不在（{} 缺 std/Cargo.toml）——MIR sysroot 从 rust-src 构建，\
             toolchain 需带 rust-src component",
            library.display()
        );
    }
    eprintln!("mirvm: 正在构建带全量 MIR 的 sysroot（一次性，需几分钟）...");

    // staging（持久，跨重建复用 host 产物/build script 缓存）与伪根
    let staging = cache_dir().join("sysroot-build");
    let root_dir = staging.join("root");
    materialize_pseudo_root(&library, &root_dir)?;
    let manifest = PackageManifest::read_dir(&root_dir)
        .map_err(|e| anyhow::anyhow!("伪根 manifest 解析失败: {e}"))?;
    let mut src = VendorDir::new(vec![library.join("vendor")], workspace_overrides(&library));
    let plan = resolve::resolve(&manifest, &mut src)
        .map_err(|e| anyhow::anyhow!("sysroot 依赖解析失败: {e}"))?;
    buildrs::check_links_unique(Some((&manifest.name, manifest.links.as_deref())), &plan)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // 产物落 tmp 目（同盘，rename 才原子），发布前旧 sysroot 全程可用
    let dir_name = format!("sysroot-{target}");
    let tmp = sysroot_dir.with_file_name(format!("{dir_name}.tmp-{}", std::process::id()));
    if tmp.exists() {
        // 上次构建崩溃残留（tmp 从不发布，删之无损）
        std::fs::remove_dir_all(&tmp)?;
    }
    let layout = Layout::at(
        tmp.join("lib/rustlib").join(target).join("lib"),
        staging.join("host-deps"),
        staging.join("build"),
    );
    compile_plan(
        &plan,
        &layout,
        &sysroot_profile(),
        &sysroot_rustflags(),
        toolchain_root(),
        &toolchain_stamp(),
        manifest.has_build_script,
        false,
        false,
    )
    .map_err(|e| anyhow::anyhow!("sysroot 编译失败: {e}"))?;

    // stamp 落 tmp 内（随 rename 一起发布；内容键此刻重算——与快路径同源）
    let want = stamp_value()
        .ok_or_else(|| anyhow::anyhow!("sysroot 构建后 stamp 计算失败（rust-src 树读取异常）"))?;
    let stamp_in_tmp = stamp_file(&tmp);
    std::fs::write(&stamp_in_tmp, &want)?;

    // 原子发布：旧目 rename 让位（rename(2) 不能盖非空目录），tmp 就位，
    // 再清旧目。让位到就位之间旧目缺席——窗口两枚 rename 之间，微秒级；
    // 崩溃则 stamp 缺席，下跑自愈重建。
    let old = sysroot_dir.with_file_name(format!("{dir_name}.old-{}", std::process::id()));
    if old.exists() {
        std::fs::remove_dir_all(&old)?;
    }
    if sysroot_dir.exists() {
        std::fs::rename(sysroot_dir, &old)?;
    }
    std::fs::rename(&tmp, sysroot_dir)?;
    if old.exists() {
        let _ = std::fs::remove_dir_all(&old);
    }
    // cargo 轨 dep 缓存连坐 purge：sysroot 内容已换代，而 cargo 的指纹
    // 看不见它（--sysroot 路径同串）——留着旧 rmeta 会被 cargo 当新鲜，
    // 混进新 sysroot 报 E0463/E0460（实证）。cargoless 轨与各 image
    // 的键自带 stamp/BUILD_ID 成分，自愈无需动。purge 失败按 FS 故障
    // 同处理（响亮——留着必然后续编译炸，不如现在点名）。
    let cargo_deps = cache_dir().join("target/mirvm");
    if cargo_deps.exists() {
        std::fs::remove_dir_all(&cargo_deps).map_err(|e| {
            anyhow::anyhow!(
                "sysroot 换代后 purge cargo 轨 dep 缓存 {} 失败（手工删除即可）: {e}",
                cargo_deps.display()
            )
        })?;
    }
    // V1 旧 stamp（rustc-build-sysroot 时代）扫墓——新 stamp 在 sysroot 内，
    // 旧文件无人再读
    let _ = std::fs::remove_file(cache_dir().join(format!("sysroot-{target}.stamp")));
    eprintln!("mirvm: sysroot 构建完成: {}", sysroot_dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("mirvm-sysroot-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stamp_file_lives_inside_sysroot_lib() {
        let f = stamp_file(Path::new("/x/sysroot-x86_64-unknown-linux-gnu"));
        assert_eq!(
            f,
            Path::new(
                "/x/sysroot-x86_64-unknown-linux-gnu/lib/rustlib/\
                 x86_64-unknown-linux-gnu/.mirvm-sysroot-hash"
            )
        );
    }

    #[test]
    fn pseudo_root_reparses_and_lock_gains_root_row() {
        let tmp = tmpdir("pseudo-root");
        let library = tmp.join("library");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::write(
            library.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n\
             # It is not intended for manual editing.\nversion = 4\n\n\
             [[package]]\nname = \"std\"\nversion = \"0.0.0\"\n\n\
             [[package]]\nname = \"test\"\nversion = \"0.0.0\"\n\
             dependencies = [\n \"std\",\n]\n",
        )
        .unwrap();
        let root = tmp.join("root");
        materialize_pseudo_root(&library, &root).unwrap();

        // 伪 manifest 可解析：三 path 边，std 带两 feature
        let m = PackageManifest::read_dir(&root).unwrap();
        assert_eq!(m.name, "mirvm-mir-sysroot");
        assert_eq!(m.deps.len(), 3);
        let std_dep = m.deps.iter().find(|d| d.package == "std").unwrap();
        assert_eq!(std_dep.features, vec!["panic-unwind", "backtrace"]);
        assert!(m.deps.iter().any(|d| d.package == "proc_macro"));

        // 增广 lock 可解析：原行保留 + 伪根行（lock 模式从根行走图的前提）
        let lf = crate::cargoless::lockfile::Lockfile::read(&root.join("Cargo.lock")).unwrap();
        let root_pkg = lf
            .packages
            .iter()
            .find(|p| p.name == "mirvm-mir-sysroot")
            .expect("伪根包行必须在");
        assert_eq!(root_pkg.version.to_string(), "0.0.0");
        assert_eq!(root_pkg.dependencies.len(), 3);
        assert_eq!(lf.find("std").len(), 1);
        assert_eq!(lf.find("test").len(), 1);

        // write-if-changed：二跑内容同 → mtime 不动
        let first = std::fs::metadata(root.join("Cargo.toml"))
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        materialize_pseudo_root(&library, &root).unwrap();
        let second = std::fs::metadata(root.join("Cargo.toml"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(first, second);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn stamp_value_tracks_recipe_and_tree() {
        // 本仓 toolchain 带 rust-src（开发前置），stamp 必有值且含配方行
        let v = stamp_value().expect("rust-src 在场必有 stamp");
        // 内容键不含 MIRVM_BUILD_ID（mirvm 换版不重建 sysroot——换代轴 =
        // rustc/rust-src/配方；见 stamp_value 头注）
        assert!(!v.starts_with(env!("MIRVM_BUILD_ID")));
        assert!(v.contains("da0 oc1 opt0"));
        assert!(v.contains("-Zforce-unstable-if-unmarked"));
        // 同源同树 ⇒ 确定性
        assert_eq!(stamp_value().unwrap(), v);
    }
}
