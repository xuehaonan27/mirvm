//! `cargoless/audit.rs` —— P1 闭合契约的审计工具（设计档 §5 P1）：
//! 对给定目标（项目目录 / frontmatter 脚本）跑完整 resolve，并与对照
//! Cargo.lock 逐条对账（项目自带 lock；脚本取 `~/.mirvm/scripts/<hash>/`
//! 下 cargo 时代物化的 lock——哈希口径与 materialize_script 一致）。
//!
//! 对账语义：lock 中每个非根包（registry 与 path）的 (name, version)
//! 必须与自解 version_map 精确互含（自解 == lock；多版本并存在集合级对账）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::Version;

use super::lockfile::Lockfile;
use super::manifest::PackageManifest;
use super::registry::Registry;
use super::resolve::{ResolvePlan, resolve};

pub struct AuditReport {
    pub name: String,
    pub mode: &'static str,
    pub units: usize,
    pub plan: ResolvePlan,
    /// (对照 lock 描述, 失配清单)；无对照 = None。
    /// 项目 = 等值判据（失败计 FAIL）；脚本 = 信息级（时间漂移，不判负）。
    pub lock_check: Option<(String, Vec<String>)>,
    /// 脚本验收：生成的 lock 被 cargo --locked --offline 接受与否。
    pub acceptance: Option<Result<(), String>>,
}

/// 项目目录（含 Cargo.toml）审计。
pub fn audit_project(dir: &Path) -> Result<AuditReport, String> {
    let manifest = PackageManifest::read_dir(dir)?;
    let mut registry = Registry::open()?;
    let plan = resolve(&manifest, &mut registry)?;
    let lock_path = manifest.root.join("Cargo.lock");
    let lock_check = lock_path
        .is_file()
        .then(|| {
            Lockfile::read(&lock_path)
                .map(|lf| (lock_path.display().to_string(), diff_versions(&plan, &lf)))
        })
        .transpose()?;
    Ok(AuditReport {
        name: plan.root_name.clone(),
        mode: if lock_path.is_file() { "lock" } else { "fresh" },
        units: plan.units.len(),
        plan,
        lock_check,
        acceptance: None,
    })
}

/// frontmatter 脚本审计（fresh 求解；验收 = 生成的 lock 被 cargo
/// `--locked --offline` 原样接受——先 `cargo fetch --locked`（在线补取）
/// 再 `--offline` 构建；历史物化 lock 的失配只作信息备注（时间漂移非分叉）。
/// 与 tests/corpus.manifest 联动：条目带 needs= 且路径缺席时记 SKIP
/// （与 gate 同口径，不算失败）。
pub fn audit_script(file: &Path) -> Result<AuditReport, String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("读取脚本 {} 失败: {e}", file.display()))?;
    let stem_owned;
    let stem = match file.file_stem().and_then(|s| s.to_str()) {
        Some(s) => {
            stem_owned = s.to_string();
            stem_owned.as_str()
        }
        None => return Err(format!("{} 文件名非法", file.display())),
    };
    // needs=/env= 联动（tests/corpus.manifest 唯一真源）
    let (needs, manifest_env) = manifest_fields(stem);
    if let Some(needs) = needs
        && !std::path::Path::new(&needs).exists()
    {
        return Ok(AuditReport {
            name: stem.to_string(),
            mode: "skip",
            units: 0,
            plan: empty_plan(file),
            lock_check: None,
            acceptance: None,
        });
    }
    let Some((manifest_text, body)) = crate::cli::parse_frontmatter_pub(&text) else {
        // 无 frontmatter = 零依赖单文件——平凡通过（diff.sh 族，不属 cargo 形态）
        return Ok(AuditReport {
            name: stem.to_string(),
            mode: "fresh",
            units: 0,
            plan: empty_plan(file),
            lock_check: None,
            acceptance: None,
        });
    };
    let manifest = PackageManifest::from_frontmatter(stem, &manifest_text, file)?;
    let mut registry = Registry::open()?;
    let plan = resolve(&manifest, &mut registry)?;

    // 历史对照（信息级）：materialize_script 同口径哈希定位
    let lock_dir = script_cache_dir(file);
    let lock_path = lock_dir.join("Cargo.lock");
    let lock_check = lock_path
        .is_file()
        .then(|| {
            Lockfile::read(&lock_path)
                .map(|lf| (lock_path.display().to_string(), diff_versions(&plan, &lf)))
        })
        .transpose()?;

    // 验收：生成的 lock 被 cargo --locked --offline 原样接受
    let acceptance = cargo_accepts_lock(
        &manifest,
        &manifest_text,
        &body,
        &plan.lock,
        manifest_env.as_deref(),
    )?;

    Ok(AuditReport {
        name: plan.root_name.clone(),
        mode: "fresh",
        units: plan.units.len(),
        plan,
        lock_check,
        acceptance: Some(acceptance),
    })
}

/// 物化伪项目并跑 cargo 验收链（fetch 在线 + build 离线）。
/// 返回 Ok(()) 或 Err(失败诊断)。
fn cargo_accepts_lock(
    manifest: &PackageManifest,
    manifest_text: &str,
    body: &str,
    lock: &Lockfile,
    manifest_env: Option<&str>,
) -> Result<Result<(), String>, String> {
    let dir = std::env::temp_dir().join(format!(
        "mirvm-deps-audit-{}-{}",
        manifest.name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).map_err(|e| format!("审计目录创建失败: {e}"))?;
    let cargo_toml = format!(
        "[package]\nname = \"{}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"{}\"\npath = \"src/main.rs\"\n\n{manifest_text}",
        manifest.name, manifest.name
    );
    std::fs::write(dir.join("Cargo.toml"), cargo_toml).unwrap();
    std::fs::write(dir.join("src/main.rs"), body).unwrap();
    std::fs::write(dir.join("Cargo.lock"), lock.serialize()).unwrap();

    let toolchain_root = std::path::PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT"));
    let cargo = toolchain_root.join("bin/cargo");
    let rustc = toolchain_root.join("bin/rustc");
    let target = crate::sysroot::cache_dir().join("target/native");
    let run = |extra: &[&str]| {
        let mut cmd = std::process::Command::new(&cargo);
        cmd.current_dir(&dir)
            .args(extra)
            .arg("--quiet")
            .env("RUSTC", &rustc)
            .env("CARGO_TARGET_DIR", &target);
        // manifest env= 列（K=V;K=V，%20 解码空格；opencc 类机侧前缀依赖）
        if let Some(envs) = manifest_env {
            for pair in envs.split(';') {
                if let Some((k, v)) = pair.split_once('=') {
                    cmd.env(k, v.replace("%20", " "));
                }
            }
        }
        cmd.output()
    };
    // ① fetch --locked（在线补取；lock 完整性 + 可得性验证）
    let fetch = run(&["fetch", "--locked"]).map_err(|e| format!("cargo fetch 执行失败: {e}"))?;
    if !fetch.status.success() {
        let tail = String::from_utf8_lossy(&fetch.stderr);
        let tail = tail.lines().last().unwrap_or("").to_string();
        if std::env::var_os("MIRVM_DEPS_AUDIT_KEEP").is_some() {
            eprintln!("audit 现场保留: {}", dir.display());
        } else {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Ok(Err(format!("cargo fetch --locked 拒绝: {tail}")));
    }
    // ② build --locked --offline（离线可复现验证）
    let build = run(&["build", "--locked", "--offline"])
        .map_err(|e| format!("cargo build 执行失败: {e}"))?;
    let ok = build.status.success();
    let diag = if ok {
        String::new()
    } else {
        let tail = String::from_utf8_lossy(&build.stderr);
        format!(
            "cargo build --locked --offline 拒绝: {}",
            tail.lines().last().unwrap_or("")
        )
    };
    if std::env::var_os("MIRVM_DEPS_AUDIT_KEEP").is_some() {
        eprintln!("audit 现场保留: {}", dir.display());
    } else {
        let _ = std::fs::remove_dir_all(&dir);
    }
    if ok { Ok(Ok(())) } else { Ok(Err(diag)) }
}

/// materialize_script 同口径：DefaultHasher(脚本绝对路径) → scripts/<16hex>。
fn script_cache_dir(script: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let abs = std::path::absolute(script).unwrap_or_else(|_| script.to_path_buf());
    let mut hasher = std::hash::DefaultHasher::new();
    abs.hash(&mut hasher);
    crate::sysroot::cache_dir()
        .join("scripts")
        .join(format!("{:016x}", hasher.finish()))
}

/// 零依赖脚本的平凡 plan（无 frontmatter 形态）。
fn empty_plan(file: &Path) -> ResolvePlan {
    ResolvePlan {
        root_name: file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
        root_version: Version::new(0, 0, 0),
        root_dir: file.parent().unwrap_or(Path::new(".")).to_path_buf(),
        root_features: Default::default(),
        units: vec![],
        version_map: Default::default(),
        lock: Default::default(),
    }
}

/// tests/corpus.manifest 里该条目的 needs= 路径与 env= 串（无登记 = (None, None)）。
/// 脚本文件是 c_<name>.rs 而 manifest 行名是 <name>——双键查询。
fn manifest_fields(stem: &str) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string("tests/corpus.manifest") else {
        return (None, None);
    };
    let bare = stem.strip_prefix("c_").unwrap_or(stem);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let row = it.next().unwrap_or("");
        if row != stem && row != bare {
            continue;
        }
        let mut needs = None;
        let mut envs = None;
        for field in it {
            if let Some(p) = field.strip_prefix("needs=") {
                needs = Some(p.to_string());
            } else if let Some(e) = field.strip_prefix("env=") {
                envs = Some(e.to_string());
            }
        }
        return (needs, envs);
    }
    (None, None)
}

/// 对账：lock 非根包集合 vs 自解 version_map（互含性）。
fn diff_versions(plan: &ResolvePlan, lf: &Lockfile) -> Vec<String> {
    let mut mismatches = Vec::new();
    let mut locked: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    for p in &lf.packages {
        if p.name == plan.root_name && p.version == plan.root_version {
            continue;
        }
        locked
            .entry(p.name.clone())
            .or_default()
            .push(p.version.clone());
    }
    for (name, versions) in &locked {
        let ours = plan.version_map.get(name).cloned().unwrap_or_default();
        let mut a = versions.clone();
        a.sort();
        let mut b = ours.clone();
        b.sort();
        if a != b {
            mismatches.push(format!(
                "{name}: lock={} ours={}",
                a.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                b.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
            ));
        }
    }
    for name in plan.version_map.keys() {
        if !locked.contains_key(name) {
            mismatches.push(format!("{name}: lock 缺席，ours 有"));
        }
    }
    mismatches
}
