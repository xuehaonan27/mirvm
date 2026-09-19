//! `cargoless/audit.rs` -- audit tool for the closed cargoless contract:
//! run a full resolve for a target (project directory or frontmatter script)
//! and reconcile it entry by entry against a reference Cargo.lock (the project's
//! own lock; for a script, the lock materialized by cargo under
//! `~/.mirvm/scripts/<hash>/`, hashed the same way `materialize_script` does).
//!
//! Reconciliation semantics: for every non-root package in the lock (registry
//! and path alike), the set of (name, version) pairs must equal the one derived
//! from resolve's `version_map`; coexisting multiple versions are compared at
//! the set level.

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
    /// (reference lock description, mismatch list); `None` when there is no reference.
    /// A project uses the equality criterion (a mismatch counts as FAIL); a script
    /// uses the informational one (time drift, not a failure).
    pub lock_check: Option<(String, Vec<String>)>,
    /// Script acceptance: whether cargo `--locked --offline` accepts the generated lock.
    pub acceptance: Option<Result<(), String>>,
}

/// Audit a project directory (containing Cargo.toml).
pub fn audit_project(dir: &Path) -> Result<AuditReport, String> {
    let manifest = PackageManifest::read_dir(dir)?;
    let mut registry = Registry::open_for(&manifest.lock_root)?;
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

/// Audit a frontmatter script (fresh resolve; acceptance means cargo
/// `--locked --offline` accepts the generated lock verbatim -- first
/// `cargo fetch --locked` (fetching online) then a `--offline` build; a mismatch
/// against a historically materialized lock is only an informational note, since
/// time drift is not a fork).
/// Tied to `tests/suites/corpus/cases.manifest`: an entry carrying `needs=` whose
/// path is absent is recorded as SKIP (same criterion as the gate, not a failure).
pub fn audit_script(file: &Path) -> Result<AuditReport, String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("failed to read script {}: {e}", file.display()))?;
    let stem_owned;
    let stem = match file.file_stem().and_then(|s| s.to_str()) {
        Some(s) => {
            stem_owned = s.to_string();
            stem_owned.as_str()
        }
        None => return Err(format!("{} has no valid file name", file.display())),
    };
    // needs=/env= linkage (corpus cases.manifest is the single source of truth)
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
        // No frontmatter means a zero-dependency single file: trivially accepted
        // (the diff.sh family, which is not a cargo-shaped target).
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
    let mut registry = Registry::open_for(&manifest.lock_root)?;
    let plan = resolve(&manifest, &mut registry)?;

    // Historical reference (informational): located with materialize_script's hash
    let lock_dir = script_cache_dir(file);
    let lock_path = lock_dir.join("Cargo.lock");
    let lock_check = lock_path
        .is_file()
        .then(|| {
            Lockfile::read(&lock_path)
                .map(|lf| (lock_path.display().to_string(), diff_versions(&plan, &lf)))
        })
        .transpose()?;

    // Acceptance: cargo --locked --offline accepts the generated lock verbatim
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

/// Materialize a pseudo-project and run the cargo acceptance chain (fetch online
/// + build offline). Returns Ok(()) or Err(failure diagnostic).
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
    std::fs::create_dir_all(dir.join("src"))
        .map_err(|e| format!("failed to create audit dir: {e}"))?;
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
        // manifest env= list (K=V;K=V, %20 decodes to a space); machine-side prefix
        // dependencies such as opencc rely on it
        if let Some(envs) = manifest_env {
            for pair in envs.split(';') {
                if let Some((k, v)) = pair.split_once('=') {
                    cmd.env(k, v.replace("%20", " "));
                }
            }
        }
        cmd.output()
    };
    // (1) fetch --locked (fetches online; verifies lock integrity and availability)
    let fetch =
        run(&["fetch", "--locked"]).map_err(|e| format!("failed to run cargo fetch: {e}"))?;
    if !fetch.status.success() {
        let tail = String::from_utf8_lossy(&fetch.stderr);
        let tail = tail.lines().last().unwrap_or("").to_string();
        if std::env::var_os("MIRVM_DEPS_AUDIT_KEEP").is_some() {
            eprintln!("audit scratch dir kept: {}", dir.display());
        } else {
            let _ = std::fs::remove_dir_all(&dir);
        }
        return Ok(Err(format!("cargo fetch --locked rejected: {tail}")));
    }
    // (2) build --locked --offline (verifies offline reproducibility)
    let build = run(&["build", "--locked", "--offline"])
        .map_err(|e| format!("failed to run cargo build: {e}"))?;
    let ok = build.status.success();
    let diag = if ok {
        String::new()
    } else {
        let tail = String::from_utf8_lossy(&build.stderr);
        format!(
            "cargo build --locked --offline rejected: {}",
            tail.lines().last().unwrap_or("")
        )
    };
    if std::env::var_os("MIRVM_DEPS_AUDIT_KEEP").is_some() {
        eprintln!("audit scratch dir kept: {}", dir.display());
    } else {
        let _ = std::fs::remove_dir_all(&dir);
    }
    if ok { Ok(Ok(())) } else { Ok(Err(diag)) }
}

/// Same key as `materialize_script`: DefaultHasher(absolute script path) ->
/// `scripts/<16hex>`. The script materialization directory in `cargoless::driver`
/// uses this key too.
pub(crate) fn script_cache_dir(script: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let abs = std::path::absolute(script).unwrap_or_else(|_| script.to_path_buf());
    let mut hasher = std::hash::DefaultHasher::new();
    abs.hash(&mut hasher);
    crate::sysroot::cache_dir()
        .join("scripts")
        .join(format!("{:016x}", hasher.finish()))
}

/// Trivial plan for a zero-dependency script (no frontmatter).
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
        root_deps: vec![],
        version_map: Default::default(),
        lock: Default::default(),
    }
}

/// The `needs=` path and `env=` string for this entry in corpus
/// `cases.manifest` (no registration = `(None, None)`).
/// Script files are named `c_<name>.rs` while manifest rows use `<name>`, so
/// both keys are looked up.
fn manifest_fields(stem: &str) -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string("tests/suites/corpus/cases.manifest") else {
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

/// Reconcile the lock's non-root package set against resolve's `version_map`
/// (mutual containment).
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
            mismatches.push(format!("{name}: absent from lock, present in ours"));
        }
    }
    mismatches
}
