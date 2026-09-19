//! `cargoless/manifest/mod.rs` -- Cargo.toml parsing and model.
//!
//! Supported subset; anything beyond it is rejected loudly by name rather than
//! silently ignored:
//! - `[package]` (name/version/edition/autobins/default-run/links)
//! - `[lib]` / `[[bin]]` / `[[test]]` / `[[example]]` plus Cargo auto-discovery
//! - `[dependencies]` / `[build-dependencies]` / `[dev-dependencies]`: version req,
//!   features, optional, default-features, path, git (default branch/branch/tag/rev);
//!   private registries are not supported yet and are rejected loudly
//! - `[features]` in three forms: `"foo"` (a feature, or an implicit optional
//!   dependency), `"dep:foo"` (explicit dependency activation), `"foo?/bar"` (weak
//!   activation)
//! - `[profile.*]`: only debug-assertions / overflow-checks / opt-level are read (the
//!   first two feed MIR semantics)
//! - `target.'cfg()'.dependencies` platform evaluation: target atoms (target_os/
//!   target_arch/target_family/unix/target_vendor/target_env/target_abi/
//!   target_pointer_width/target_endian) combined with any/all/not;
//!   `cfg(feature=..)` is not a platform evaluation (as in cargo), and
//!   `cfg(target_feature=..)` is rejected loudly
//! - `[workspace]`: `workspace.rs` first discovers the resolver=1/2/3 multi-package
//!   graph and materializes workspace.package/workspace.dependencies/root profile;
//!   this file only parses the materialized package
//! - doctests are still not built; test and bench targets are consumed by
//!   `mirvm test`.

// Some model fields are consumed by only a subset of commands; they stay for now
// because they belong to this module's boundary.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use cfg::validate_cfg_expr;
use deps::{parse_dep_table, parse_patches, parse_replacements};
use targets::{discover_targets, profiles_from};

mod cfg;
mod deps;
mod targets;

#[cfg(test)]
mod tests;

pub use cfg::eval_cfg;
pub(crate) use cfg::host_cfg_atoms;
pub(crate) use deps::parse_feature_value;

/// Cargo's global dependency resolution rule version. A dependency package's own
/// value is overridden by the top-level package/workspace, but it is still kept in
/// the manifest model for when that package is the top-level one itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolverVersion {
    V1,
    V2,
    V3,
}

impl ResolverVersion {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "1" => Ok(Self::V1),
            "2" => Ok(Self::V2),
            "3" => Ok(Self::V3),
            other => Err(format!("resolver must be 1, 2 or 3, got `{other}`")),
        }
    }

    pub fn inferred(edition: &str) -> Self {
        match edition {
            "2021" => Self::V2,
            "2024" => Self::V3,
            _ => Self::V1,
        }
    }
}

/// How Cargo chooses when a dependency's declared minimum Rust version is
/// incompatible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncompatibleRustVersions {
    /// Keep the usual "highest version wins" order.
    Allow,
    /// Prefer a compatible version; if there is none, still fall back to the highest
    /// incompatible one.
    Fallback,
}

impl IncompatibleRustVersions {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "allow" => Ok(Self::Allow),
            "fallback" => Ok(Self::Fallback),
            other => Err(format!(
                "resolver.incompatible-rust-versions accepts only `allow` or `fallback`, got `{other}`"
            )),
        }
    }
}

/// Cargo's rust-version allows 1, 2 or 3 bare numeric segments and rejects semver
/// operators, prerelease and build metadata. Internally it is padded to three
/// segments so comparisons are stable.
pub fn parse_rust_version(value: &str, field: &str) -> Result<semver::Version, String> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.is_empty()
        || parts.len() > 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!(
            "{field} must be 1, 2 or 3 bare version segments, got `{value}`"
        ));
    }
    let normalized = match parts.len() {
        1 => format!("{}.0.0", parts[0]),
        2 => format!("{}.{}.0", parts[0], parts[1]),
        3 => value.to_string(),
        _ => unreachable!(),
    };
    semver::Version::parse(&normalized)
        .map_err(|error| format!("invalid {field} `{value}`: {error}"))
}

/// The version of the rustc mirvm actually embeds. The sysroot rustc is used at
/// build time so another toolchain on PATH cannot affect dependency selection.
pub fn current_rust_version() -> Result<semver::Version, String> {
    static VERSION: std::sync::OnceLock<semver::Version> = std::sync::OnceLock::new();
    if let Some(version) = VERSION.get() {
        return Ok(version.clone());
    }
    let rustc = PathBuf::from(crate::options::build::DEFAULT_SYSROOT).join("bin/rustc");
    let command = std::process::Command::new(&rustc);
    let mut version = rustc_version::VersionMeta::for_command(command)
        .map_err(|error| format!("failed to read version of {}: {error}", rustc.display()))?
        .semver;
    version.pre = semver::Prerelease::EMPTY;
    version.build = semver::BuildMetadata::EMPTY;
    let _ = VERSION.set(version.clone());
    Ok(version)
}

/// Where a dependency comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepSource {
    /// Registry dependency: a semver requirement plus the registry named in the manifest.
    Registry(semver::VersionReq, RegistryReference),
    /// Local path dependency (already made absolute).
    Path(PathBuf),
    /// Git repository dependency; a movable reference is resolved to a precise commit
    /// during fetch, and the lock records only the precise result.
    Git(GitSpec),
}

/// How a registry is written in a manifest. The name and any explicit index are
/// reduced through Cargo config into the stable URL used by lock/source only when
/// dependency resolution starts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RegistryReference {
    CratesIo,
    Named(String),
    Index(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitReference {
    DefaultBranch,
    Branch(String),
    Tag(String),
    Rev(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSpec {
    pub url: String,
    pub reference: GitReference,
    pub version: semver::VersionReq,
}

impl GitSpec {
    /// The source id before the `#<commit>` in Cargo.lock.
    pub fn source_id(&self) -> String {
        let query = match &self.reference {
            GitReference::DefaultBranch => None,
            GitReference::Branch(value) => Some(("branch", value.as_str())),
            GitReference::Tag(value) => Some(("tag", value.as_str())),
            GitReference::Rev(value) => Some(("rev", value.as_str())),
        };
        match query {
            Some((key, value)) => format!("git+{}?{key}={}", self.url, percent_encode(value)),
            None => format!("git+{}", self.url),
        }
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Dependency kind (dev-deps are not built).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    Normal,
    Build,
    /// Enters the build graph only when the root package is a test target; dev edges of
    /// path/registry dependencies do not propagate.
    Dev,
}

/// One dependency declaration. The platform cfg expression travels with the row
/// instead of being filtered at parse time: version resolution is the union over all
/// platforms (cargo lock semantics), and filtering happens only when the host build
/// graph is assembled.
#[derive(Clone, Debug)]
pub struct DepDecl {
    /// Key as written in the manifest (used for feature references and `--extern`
    /// naming unless renamed).
    pub key: String,
    /// Real crate name (the `real` of `package = "real"`, otherwise equal to `key`).
    pub package: String,
    pub source: DepSource,
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    pub kind: DepKind,
    /// cfg expression when the entry came from a `target.'cfg(...)'` table
    /// (`None` for a plain table).
    pub platform_cfg: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PatchDecl {
    pub registry: RegistryReference,
    pub dependency: DepDecl,
}

#[derive(Clone, Debug)]
pub struct ReplaceDecl {
    pub package: String,
    pub version: semver::Version,
    pub source: Option<String>,
    pub dependency: DepDecl,
}

impl DepDecl {
    /// Feature reference name (cargo semantics: the implicit feature name is the key).
    pub fn feature_name(&self) -> &str {
        &self.key
    }
}

/// The value form of one `[features]` table entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeatureValue {
    /// `"foo"`: another feature, or the implicit activation of an optional dependency
    /// of that name.
    Simple(String),
    /// `"dep:foo"`: explicitly activate an optional dependency (no feature of the same
    /// name is created).
    DepActivation(String),
    /// `"foo/bar"`: strong activation -- activate foo and turn on its bar.
    StrongDep { dep: String, feature: String },
    /// `"foo?/bar"`: turn on foo's bar if foo is activated (weak activation, does not
    /// activate foo itself).
    WeakDep { dep: String, feature: String },
}

/// Cargo target kind. The build script is still modeled separately via
/// package.build/links.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetKind {
    Lib,
    Bin,
    Test,
    Example,
    Bench,
}

/// One buildable target and the manifest attributes that affect how tests select and
/// compile it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub kind: TargetKind,
    pub name: String,
    pub path: PathBuf,
    pub proc_macro: bool,
    /// Whether Cargo's default `test` selection includes this target.
    pub test: bool,
    /// true = rustc `--test` injects libtest; false = the target keeps its own main.
    pub harness: bool,
    pub doctest: bool,
    pub required_features: Vec<String>,
}

impl Target {
    pub fn is_lib(&self) -> bool {
        self.kind == TargetKind::Lib
    }

    pub fn is_bin(&self) -> bool {
        self.kind == TargetKind::Bin
    }
}

/// Profile semantic flags: the ones that affect MIR semantics, plus the opt-level
/// that is passed through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileFlags {
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub opt_level: OptLevel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Os,
    Oz,
}

impl OptLevel {
    pub fn is_zero(self) -> bool {
        self == Self::O0
    }
}

impl std::fmt::Display for OptLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::O0 => "0",
            Self::O1 => "1",
            Self::O2 => "2",
            Self::O3 => "3",
            Self::Os => "s",
            Self::Oz => "z",
        })
    }
}

impl Default for ProfileFlags {
    /// Equivalent to cargo's dev profile.
    fn default() -> Self {
        Self {
            debug_assertions: true,
            overflow_checks: true,
            opt_level: OptLevel::O0,
        }
    }
}

/// The complete model of one package.
#[derive(Clone, Debug)]
pub struct PackageManifest {
    pub name: String,
    pub version: semver::Version,
    pub edition: String,
    /// The global resolver adopted by the top-level package/workspace; the value on a
    /// path/registry dependency itself does not take effect.
    pub resolver: ResolverVersion,
    /// Minimum Rust version declared by this package.
    pub rust_version: Option<semver::Version>,
    /// The workspace comparison baseline for resolver 3. A workspace writes the lowest
    /// member value; a single package uses its own rust-version, and when absent the
    /// resolver uses the current rustc.
    pub resolver_rust_version: Option<semver::Version>,
    /// `--ignore-rust-version` disables both candidate preference and compiler version
    /// rejection.
    pub ignore_rust_version: bool,
    pub root: PathBuf,
    /// Directory Cargo.lock belongs to. For a single package it equals root; a workspace
    /// member points at the workspace root.
    pub lock_root: PathBuf,
    pub targets: Vec<Target>,
    pub deps: Vec<DepDecl>,
    /// Only overrides at the top-level package/workspace root take effect; the workspace
    /// discovery layer materializes the root values onto members.
    pub patches: Vec<PatchDecl>,
    pub replacements: Vec<ReplaceDecl>,
    pub features: BTreeMap<String, Vec<FeatureValue>>,
    /// Features the CLI explicitly requests for this test root; dependency-edge features
    /// are still propagated by the resolver.
    pub requested_features: BTreeSet<String>,
    /// CLI `dependency/feature` requests; propagated to dependency edges, not root-package
    /// cfg features.
    pub dependency_features: BTreeMap<String, BTreeSet<String>>,
    pub default_features_enabled: bool,
    pub profile: ProfileFlags,
    pub test_profile: ProfileFlags,
    /// Has a build script (a `build` key, a `links` key, or a real build.rs under root).
    pub has_build_script: bool,
    /// Custom build script path from `[package] build = "custom.rs"`; `None` means the
    /// default `<root>/build.rs` (used by build.rs scheduling).
    pub build_script_path: Option<PathBuf>,
    /// `[package] links` (the -sys link key; used to derive the native library name and
    /// to schedule build.rs).
    pub links: Option<String>,
    pub default_run: Option<String>,
    /// The full set of compile-time CARGO_PKG_* env vars (a missing key is the empty
    /// string, the same contract as cargo; computed by `pkg_env_map`).
    pub pkg_env: BTreeMap<String, String>,
    /// rustc arguments reduced from Cargo `[lints]`: first the level arguments sorted by
    /// priority, then the two `--check-cfg` slots for `unexpected_cfgs`.
    pub rustc_lint_flags: Vec<String>,
    /// The source of a non-registry package in Cargo.lock. `None` for the root and plain
    /// path packages; `git+URL?...#commit` for a Git package.
    pub lock_source: Option<String>,
    /// Git checkout root, so in-repo path dependencies inherit the same Git source.
    pub git_checkout_root: Option<PathBuf>,
}

// ---------- serde raw form (lenient: unknown keys ignored, known-unsupported keys
// checked afterwards) ----------

#[derive(serde::Deserialize, Default)]
struct RawManifest {
    package: Option<RawPackage>,
    workspace: Option<RawWorkspace>,
    patch: Option<toml::Value>,
    replace: Option<toml::Value>,
    lints: Option<toml::Value>,
    lib: Option<RawLib>,
    bin: Option<Vec<RawBin>>,
    test: Option<Vec<RawTarget>>,
    example: Option<Vec<RawTarget>>,
    bench: Option<Vec<RawTarget>>,
    dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<BTreeMap<String, toml::Value>>,
    features: Option<BTreeMap<String, Vec<String>>>,
    profile: Option<RawProfiles>,
    target: Option<BTreeMap<String, RawTargetDeps>>,
}

#[derive(serde::Deserialize, Default)]
struct RawPackage {
    name: Option<String>,
    version: Option<toml::Value>,
    edition: Option<toml::Value>,
    resolver: Option<String>,
    autobins: Option<bool>,
    autoexamples: Option<bool>,
    autotests: Option<bool>,
    autobenches: Option<bool>,
    links: Option<String>,
    build: Option<toml::Value>,
    #[serde(rename = "default-run")]
    default_run: Option<String>,
    // All of the following are CARGO_PKG_* env inputs (consumed by pkg_env_map; a
    // missing key is the empty string, as in cargo)
    authors: Option<Vec<String>>,
    description: Option<String>,
    homepage: Option<String>,
    repository: Option<String>,
    license: Option<String>,
    #[serde(rename = "license-file")]
    license_file: Option<toml::Value>,
    readme: Option<toml::Value>,
    #[serde(rename = "rust-version")]
    rust_version: Option<toml::Value>,
}

#[derive(serde::Deserialize, Default)]
struct RawWorkspace {
    package: Option<RawWorkspacePackage>,
    members: Option<Vec<String>>,
    resolver: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RawWorkspacePackage {
    version: Option<String>,
    edition: Option<String>,
    #[serde(rename = "rust-version")]
    rust_version: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct RawLib {
    name: Option<String>,
    path: Option<String>,
    // Both spellings are accepted: the hyphen is the cargo documentation form (written
    // by hand) and the underscore is what newer cargo normalizes to (confirmed by
    // derive_arbitrary 1.3.2; cargo accepts both).
    #[serde(rename = "proc-macro", alias = "proc_macro")]
    proc_macro: Option<bool>,
    test: Option<bool>,
    harness: Option<bool>,
    doctest: Option<bool>,
    #[serde(rename = "required-features")]
    required_features: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct RawBin {
    name: Option<String>,
    path: Option<String>,
    test: Option<bool>,
    harness: Option<bool>,
    #[serde(rename = "required-features")]
    required_features: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct RawTarget {
    name: Option<String>,
    path: Option<String>,
    test: Option<bool>,
    harness: Option<bool>,
    #[serde(rename = "required-features")]
    required_features: Option<Vec<String>>,
}

#[derive(serde::Deserialize, Default)]
struct RawProfiles {
    dev: Option<RawProfile>,
    test: Option<RawProfile>,
    release: Option<RawProfile>,
}

#[derive(serde::Deserialize, Default)]
struct RawProfile {
    #[serde(rename = "debug-assertions")]
    debug_assertions: Option<bool>,
    #[serde(rename = "overflow-checks")]
    overflow_checks: Option<bool>,
    #[serde(rename = "opt-level")]
    opt_level: Option<toml::Value>,
}

#[derive(serde::Deserialize, Default)]
struct RawTargetDeps {
    dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "build-dependencies")]
    build_dependencies: Option<BTreeMap<String, toml::Value>>,
    #[serde(rename = "dev-dependencies")]
    dev_dependencies: Option<BTreeMap<String, toml::Value>>,
}

// ---------- errors ----------

type MErr = String;

fn unsupported(what: impl Into<String>) -> MErr {
    format!(
        "manifest construct outside the supported subset (rejected loudly): {}",
        what.into()
    )
}

// ---------- public entry points ----------

impl PackageManifest {
    /// Read from a project directory (directory/Cargo.toml).
    pub fn read_dir(dir: &Path) -> Result<Self, MErr> {
        let dir = std::path::absolute(dir)
            .map_err(|e| format!("failed to absolutize project dir {}: {e}", dir.display()))?;
        let file = dir.join("Cargo.toml");
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
        Self::parse(&text, &dir)
    }

    /// Parse from manifest text (root = package root directory).
    pub fn parse(text: &str, root: &Path) -> Result<Self, MErr> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|e| format!("failed to parse Cargo.toml: {e}"))?;
        let rustc_lint_flags = parse_lints(raw.lints.as_ref())?;
        if raw.package.is_none() && raw.workspace.is_some() {
            return Err(unsupported(
                "virtual manifest ([workspace] without [package])",
            ));
        }
        let pkg = raw
            .package
            .ok_or_else(|| "manifest has no [package]".to_string())?;
        let name = pkg
            .name
            .ok_or_else(|| "package.name is missing".to_string())?;

        // version/edition support workspace inheritance (workspace.package.*)
        let ws_pkg = raw.workspace.as_ref().and_then(|w| w.package.as_ref());
        let version = match pkg.version {
            Some(toml::Value::String(v)) => semver::Version::parse(&v)
                .map_err(|e| format!("invalid package.version {v}: {e}"))?,
            Some(toml::Value::Table(t)) if t.get("workspace").is_some() => ws_pkg
                .and_then(|w| w.version.clone())
                .and_then(|v| semver::Version::parse(&v).ok())
                .ok_or_else(|| {
                    "package.version inherits from workspace but the root has no version"
                        .to_string()
                })?,
            Some(_) => return Err("unsupported package.version form".into()),
            None => semver::Version::new(0, 0, 0),
        };
        let edition = match pkg.edition {
            Some(toml::Value::String(e)) => e,
            Some(toml::Value::Table(t)) if t.get("workspace").is_some() => {
                ws_pkg.and_then(|w| w.edition.clone()).ok_or_else(|| {
                    "package.edition inherits from workspace but the root has no edition"
                        .to_string()
                })?
            }
            Some(_) => return Err("unsupported package.edition form".into()),
            None => "2015".to_string(),
        };
        if !matches!(edition.as_str(), "2015" | "2018" | "2021" | "2024") {
            return Err(format!("package.edition `{edition}` is not supported"));
        }
        let resolver = raw
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.resolver.as_deref())
            .or(pkg.resolver.as_deref())
            .map(ResolverVersion::parse)
            .transpose()?
            .unwrap_or_else(|| ResolverVersion::inferred(&edition));
        // The full CARGO_PKG_* env set (cargo contract: readable via env! at compile
        // time; a missing key is the empty string). `readme = true` reduces to
        // "README.md" (as in cargo); license-file only accepts the string form.
        let rust_version_text = match pkg.rust_version {
            Some(toml::Value::String(v)) => Some(v),
            Some(toml::Value::Table(t)) if t.get("workspace").is_some() => {
                ws_pkg.and_then(|w| w.rust_version.clone())
            }
            Some(_) => {
                return Err(
                    "package.rust-version must be a string or a workspace inheritance".into(),
                );
            }
            None => None,
        };
        let rust_version = rust_version_text
            .as_deref()
            .map(|version| parse_rust_version(version, "package.rust-version"))
            .transpose()?;
        if let Some(version) = &rust_version {
            let minimum = match edition.as_str() {
                "2015" => semver::Version::new(1, 0, 0),
                "2018" => semver::Version::new(1, 31, 0),
                "2021" => semver::Version::new(1, 56, 0),
                "2024" => semver::Version::new(1, 85, 0),
                _ => unreachable!(),
            };
            if version < &minimum {
                return Err(format!(
                    "package.rust-version {} is incompatible with Rust {minimum} required by edition {edition}",
                    rust_version_text.as_deref().unwrap_or("")
                ));
            }
        }
        let readme = match &pkg.readme {
            Some(toml::Value::String(s)) => Some(s.clone()),
            Some(toml::Value::Boolean(true)) => Some("README.md".to_string()),
            _ => None,
        };
        let license_file = match &pkg.license_file {
            Some(toml::Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        let pkg_env = pkg_env_map(
            &name,
            &version,
            pkg.authors.as_deref(),
            pkg.description.as_deref(),
            pkg.homepage.as_deref(),
            pkg.repository.as_deref(),
            pkg.license.as_deref(),
            license_file.as_deref(),
            readme.as_deref(),
            rust_version_text.as_deref(),
        );
        if raw
            .workspace
            .as_ref()
            .and_then(|w| w.members.as_ref())
            .is_some()
            && raw
                .workspace
                .as_ref()
                .and_then(|w| w.package.as_ref())
                .is_none()
        {
            // members without workspace.package: this may be a multi-package root. A
            // single package rarely uses members itself, and this file does not tell the
            // cases apart, so the parse commitment for a multi-package graph is refused
            // loudly.
            return Err(unsupported(
                "workspace.members multi-package graph (a single-package project can drop the key)",
            ));
        }

        let mut deps = Vec::new();
        parse_dep_table(&raw.dependencies, DepKind::Normal, root, None, &mut deps)?;
        parse_dep_table(
            &raw.build_dependencies,
            DepKind::Build,
            root,
            None,
            &mut deps,
        )?;
        parse_dep_table(&raw.dev_dependencies, DepKind::Dev, root, None, &mut deps)?;
        // target.'cfg()'.dependencies: the expression travels with the row into the
        // model (union over all platforms, no filtering)
        for (cfg_expr, tdeps) in raw.target.iter().flatten() {
            // The expression is validated here (a typo must be loud); semantic evaluation
            // happens at use time
            validate_cfg_expr(cfg_expr).map_err(|e| format!("target.{cfg_expr}: {e}"))?;
            parse_dep_table(
                &tdeps.dependencies,
                DepKind::Normal,
                root,
                Some(cfg_expr.clone()),
                &mut deps,
            )?;
            parse_dep_table(
                &tdeps.build_dependencies,
                DepKind::Build,
                root,
                Some(cfg_expr.clone()),
                &mut deps,
            )?;
            parse_dep_table(
                &tdeps.dev_dependencies,
                DepKind::Dev,
                root,
                Some(cfg_expr.clone()),
                &mut deps,
            )?;
        }
        let patches = parse_patches(raw.patch.as_ref(), root)?;
        let replacements = parse_replacements(raw.replace.as_ref(), root)?;

        let features = raw
            .features
            .as_ref()
            .map(|fs| {
                fs.iter()
                    .map(|(k, vs)| {
                        vs.iter()
                            .map(|v| parse_feature_value(v))
                            .collect::<Result<Vec<_>, _>>()
                            .map(|vals| (k.clone(), vals))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()
            })
            .transpose()?
            .unwrap_or_default();

        let autobins = pkg.autobins.unwrap_or(true);
        let autoexamples = pkg.autoexamples.unwrap_or(true);
        let autotests = pkg.autotests.unwrap_or(true);
        let autobenches = pkg.autobenches.unwrap_or(true);
        let targets = discover_targets(
            raw.lib.as_ref(),
            raw.bin.as_ref(),
            raw.test.as_ref(),
            raw.example.as_ref(),
            raw.bench.as_ref(),
            autobins,
            autotests,
            autoexamples,
            autobenches,
            &name,
            root,
        )?;
        let (profile, test_profile) = profiles_from(raw.profile)?;
        // cargo semantics: `build = false` explicitly disables the build script (the key
        // being present is not the same as having build.rs -- confirmed by cfg-if); a
        // string value is a custom path; absent means a real build.rs under root.
        let has_build_script = pkg.links.is_some()
            || match &pkg.build {
                Some(toml::Value::Boolean(false)) => false,
                Some(_) => true,
                None => root.join("build.rs").is_file(),
            };
        let build_script_path = match &pkg.build {
            Some(toml::Value::String(p)) => Some(root.join(p)),
            _ => None,
        };

        Ok(Self {
            name,
            version,
            edition,
            resolver,
            rust_version: rust_version.clone(),
            resolver_rust_version: rust_version,
            ignore_rust_version: false,
            root: root.to_path_buf(),
            lock_root: root.to_path_buf(),
            targets,
            deps,
            patches,
            replacements,
            features,
            requested_features: BTreeSet::new(),
            dependency_features: BTreeMap::new(),
            default_features_enabled: true,
            profile,
            test_profile,
            has_build_script,
            build_script_path,
            links: pkg.links,
            default_run: pkg.default_run,
            pkg_env,
            rustc_lint_flags,
            lock_source: None,
            git_checkout_root: None,
        })
    }

    /// A frontmatter pseudo-package: the script stem is the package name and the
    /// dependency section is fed verbatim to the same parser.
    /// The bin name carries a short hash suffix so target directories do not collide
    /// with the materialized script cache.
    pub fn from_frontmatter(
        stem: &str,
        manifest_text: &str,
        body_path: &Path,
    ) -> Result<Self, MErr> {
        let root = body_path.parent().unwrap_or(Path::new("."));
        Self::from_frontmatter_at(stem, manifest_text, root, body_path)
    }

    /// Root-explicit form of `from_frontmatter`: the script cache layout matches the
    /// project cargo materializes (`materialize_script` in cli.rs puts Cargo.toml in
    /// `<cache>` and the body in `<cache>/src/main.rs`). root = `<cache>` keeps
    /// CARGO_MANIFEST_DIR identical to the cargo path, and the bin path
    /// `<cache>/src/main.rs` keeps `file!()` equal to "src/main.rs" after remapping.
    pub fn from_frontmatter_at(
        stem: &str,
        manifest_text: &str,
        root: &Path,
        body_path: &Path,
    ) -> Result<Self, MErr> {
        let pseudo = format!(
            "[package]\nname = \"{stem}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             [[bin]]\nname = \"{stem}\"\npath = \"{}\"\n{manifest_text}",
            body_path.display()
        );
        Self::parse(&pseudo, root)
    }

    /// Pick the bin to run (the cargo run semantics subset): default-run > the only bin
    /// > reject loudly when there are several.
    pub fn runnable_bin(&self) -> Result<(&str, &Path), MErr> {
        self.runnable_bin_opt(None)
    }

    /// Bin selection with a `--bin` choice (cargo run --bin semantics):
    /// `Some(name)` selects by exact name (not in [[bin]] = a loud error listing the
    /// available names, with cargo's wording); `None` follows `runnable_bin`'s
    /// default-run > only-one > reject chain.
    pub fn runnable_bin_opt(&self, sel: Option<&str>) -> Result<(&str, &Path), MErr> {
        let bins: Vec<_> = self
            .targets
            .iter()
            .filter(|t| t.is_bin())
            .map(|t| (t.name.as_str(), t.path.as_path()))
            .collect();
        if let Some(want) = sel {
            if let Some(b) = bins.iter().find(|(n, _)| *n == want) {
                return Ok(*b);
            }
            return Err(format!(
                "no bin target named `{want}` (available: {})",
                bins.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            ));
        }
        if let Some(dr) = &self.default_run {
            if let Some(b) = bins.iter().find(|(n, _)| *n == dr) {
                return Ok(*b);
            }
            return Err(format!("default-run={dr} does not exist in [[bin]]"));
        }
        match bins.len() {
            0 => Err(format!("package {} has no bin target", self.name)),
            1 => Ok(bins[0]),
            _ => Err(format!(
                "multiple bin targets ({}) -- choose one with --bin or pin it with default-run",
                bins.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// The legal value table for `--check-cfg cfg(feature, values(...))` (same criterion
    /// as the cargo bin path): `[features]` table keys plus implicit optional dependency
    /// keys. When an optional dep key is not named by any `dep:key` feature value, an
    /// implicit feature of the same name exists -- the same rule as the `hidden` logic in
    /// `expand_node` in resolve.rs.
    pub fn check_cfg_feature_values(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self.features.keys().cloned().collect();
        let hidden: BTreeSet<String> = self
            .features
            .values()
            .flatten()
            .filter_map(|v| match v {
                FeatureValue::DepActivation(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        for d in &self.deps {
            if d.optional && !hidden.contains(&d.key) {
                out.insert(d.key.clone());
            }
        }
        out
    }
}

#[derive(Debug)]
struct ParsedLint {
    priority: i64,
    flag: String,
    check_cfg: Vec<String>,
}

/// Reduction of Cargo `[lints]` into rustc argv. Cargo preserves list order for equal
/// priorities; the TOML map enables preserve_order, so a stable sort by priority alone
/// reproduces it.
pub(super) fn parse_lints(value: Option<&toml::Value>) -> Result<Vec<String>, MErr> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let tools = value
        .as_table()
        .ok_or_else(|| "[lints] must be a table".to_string())?;
    if tools.contains_key("workspace") {
        return Err(unsupported(
            "[lints] workspace inheritance (the workspace layer must materialize it first)",
        ));
    }

    let mut parsed = Vec::new();
    for (tool, lints) in tools {
        let lints = lints
            .as_table()
            .ok_or_else(|| format!("[lints.{tool}] must be a table"))?;
        for (name, spec) in lints {
            let (level, priority, check_cfg) = match spec {
                toml::Value::String(level) => (level.as_str(), 0, Vec::new()),
                toml::Value::Table(table) => {
                    if let Some(key) = table
                        .keys()
                        .find(|key| !matches!(key.as_str(), "level" | "priority" | "check-cfg"))
                    {
                        return Err(format!(
                            "lints.{tool}.{name} has a key `{key}` that Cargo does not know"
                        ));
                    }
                    let level = table
                        .get("level")
                        .and_then(toml::Value::as_str)
                        .ok_or_else(|| format!("lints.{tool}.{name}.level must be a string"))?;
                    let priority = match table.get("priority") {
                        Some(value) => value.as_integer().ok_or_else(|| {
                            format!("lints.{tool}.{name}.priority must be an integer")
                        })?,
                        None => 0,
                    };
                    let check_cfg = match table.get("check-cfg") {
                        Some(value) => {
                            if tool != "rust" || name != "unexpected_cfgs" {
                                return Err(
                                    "check-cfg is only allowed under lints.rust.unexpected_cfgs"
                                        .to_string(),
                                );
                            }
                            value
                                .as_array()
                                .ok_or_else(|| {
                                    "lints.rust.unexpected_cfgs.check-cfg must be an array of strings"
                                        .to_string()
                                })?
                                .iter()
                                .map(|item| {
                                    item.as_str().map(str::to_string).ok_or_else(|| {
                                        "lints.rust.unexpected_cfgs.check-cfg contains a non-string member"
                                            .to_string()
                                    })
                                })
                                .collect::<Result<Vec<_>, _>>()?
                        }
                        None => Vec::new(),
                    };
                    (level, priority, check_cfg)
                }
                _ => {
                    return Err(format!(
                        "lints.{tool}.{name} must be a level string or a config table"
                    ));
                }
            };
            if !matches!(level, "allow" | "warn" | "deny" | "forbid") {
                return Err(format!(
                    "lints.{tool}.{name}.level accepts only allow/warn/deny/forbid, got `{level}`"
                ));
            }
            let qualified = if tool == "rust" {
                name.clone()
            } else {
                format!("{tool}::{name}")
            };
            parsed.push(ParsedLint {
                priority,
                flag: format!("--{level}={qualified}"),
                check_cfg,
            });
        }
    }
    parsed.sort_by_key(|lint| lint.priority);
    let mut flags = parsed
        .iter()
        .map(|lint| lint.flag.clone())
        .collect::<Vec<_>>();
    for check_cfg in parsed.into_iter().flat_map(|lint| lint.check_cfg) {
        flags.push("--check-cfg".into());
        flags.push(check_cfg);
    }
    Ok(flags)
}

/// The full CARGO_PKG_* env set (the cargo compile-time env contract, readable by
/// env!/option_env!). A missing key is the empty string (that is what cargo sets);
/// VERSION_MAJOR/MINOR/PATCH/PRE are split out of the semver (PRE is the prerelease
/// string, empty when there is none); the AUTHORS array is joined with ":".
/// manifest.rs (root/path packages) and resolve.rs (minimal read of registry packages)
/// both key off this one function.
// The flat parameter list mirrors cargo's flat env key set one for one; bundling it
// into a struct would lose the visual correspondence with the manifest keys.
#[allow(clippy::too_many_arguments)]
pub fn pkg_env_map(
    name: &str,
    version: &semver::Version,
    authors: Option<&[String]>,
    description: Option<&str>,
    homepage: Option<&str>,
    repository: Option<&str>,
    license: Option<&str>,
    license_file: Option<&str>,
    readme: Option<&str>,
    rust_version: Option<&str>,
) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: &str| {
        m.insert(k.to_string(), v.to_string());
    };
    put("CARGO_PKG_NAME", name);
    put("CARGO_PKG_VERSION", &version.to_string());
    put("CARGO_PKG_VERSION_MAJOR", &version.major.to_string());
    put("CARGO_PKG_VERSION_MINOR", &version.minor.to_string());
    put("CARGO_PKG_VERSION_PATCH", &version.patch.to_string());
    put("CARGO_PKG_VERSION_PRE", version.pre.as_str());
    put("CARGO_PKG_AUTHORS", &authors.unwrap_or(&[]).join(":"));
    put("CARGO_PKG_DESCRIPTION", description.unwrap_or(""));
    put("CARGO_PKG_HOMEPAGE", homepage.unwrap_or(""));
    put("CARGO_PKG_LICENSE", license.unwrap_or(""));
    put("CARGO_PKG_LICENSE_FILE", license_file.unwrap_or(""));
    put("CARGO_PKG_README", readme.unwrap_or(""));
    put("CARGO_PKG_REPOSITORY", repository.unwrap_or(""));
    put("CARGO_PKG_RUST_VERSION", rust_version.unwrap_or(""));
    m
}
