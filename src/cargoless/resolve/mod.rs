//! `cargoless/resolve/mod.rs` -- version resolution + feature unification -> compilation
//! unit graph.
//!
//! Two modes:
//! - **lock mode** (the project has a Cargo.lock): every version comes from the lock
//!   (yanked crates included, as in cargo), with an integrity check -- a manifest
//!   requirement that the locked version does not satisfy is a loud error (the lock is
//!   stale).
//! - **fresh mode** (frontmatter script / no lock): pubgrub resolves against the sparse
//!   index (yanked skipped; prereleases follow an approximation of cargo's rule -- a
//!   prerelease is admitted only when some dependent's requirement carries a pre
//!   comparator, one notch looser than cargo's per-major.minor.patch rule), and the
//!   result is written out as a canonical Cargo.lock for reproducibility and for
//!   `cargo --locked` refutation.
//!
//! Feature unification = the resolver v2 semantics subset: **normal and build edges are
//! listed separately** (the same crate with different feature sets for the two classes
//! is two compilation units), dev edges are not resolved at all, the three optional
//! forms (implicit feature / explicit dep: / weak ?/ plus strong /), and the
//! default_features edge rule. Feature/dependency metadata for registry crates comes
//! from the sparse index (as in cargo); lib name/proc-macro/links/build.rs presence come
//! from a **minimal manifest read** of the unpacked source (no full subset parse --
//! registry crate manifests make no promises).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use semver::Version;

use super::lockfile::{LockedPkg, Lockfile};
use super::manifest::{
    DepKind, DepSource, GitSpec, IncompatibleRustVersions, PackageManifest, RegistryReference,
    current_rust_version,
};
use super::registry::{IndexEntry, Registry};

use features::unify_features;
use fresh::{FreshSolveContext, solve_fresh};
use lockfill::{fill_lock_dependency_lines, fill_unused_patches, lock_dependency_source};
use units::{assemble_units, validate_compiler_rust_version};

pub use fresh::EdgeVersions;

mod features;
mod fresh;
mod lockfill;
mod units;

#[cfg(test)]
mod tests;

// ---------- package source abstraction (production = Registry; tests = in-memory fake) ----------

pub trait PkgSource {
    fn registry_source(&mut self, reference: &RegistryReference) -> Result<String, String>;
    fn index_entry(&mut self, source: &str, name: &str) -> Result<IndexEntry, String>;
    fn ensure_source(
        &mut self,
        source: &str,
        name: &str,
        version: &Version,
        cksum: Option<&str>,
    ) -> Result<PathBuf, String>;
    fn ensure_git_package(
        &mut self,
        spec: &GitSpec,
        package: &str,
        locked_source: Option<&str>,
    ) -> Result<PackageManifest, String> {
        let _ = (spec, package, locked_source);
        Err("the current package source does not support Git dependencies".into())
    }
}

impl PkgSource for Registry {
    fn registry_source(&mut self, reference: &RegistryReference) -> Result<String, String> {
        Registry::registry_source(self, reference)
    }
    fn index_entry(&mut self, source: &str, name: &str) -> Result<IndexEntry, String> {
        Registry::index_entry(self, source, name)
    }
    fn ensure_source(
        &mut self,
        source: &str,
        name: &str,
        version: &Version,
        cksum: Option<&str>,
    ) -> Result<PathBuf, String> {
        Registry::ensure_source(self, source, name, version, cksum)
    }
    fn ensure_git_package(
        &mut self,
        spec: &GitSpec,
        package: &str,
        locked_source: Option<&str>,
    ) -> Result<PackageManifest, String> {
        Registry::ensure_git_package(self, spec, package, locked_source)
    }
}

// ---------- output model ----------

/// Compilation unit class (the resolver v2 split between normal and build).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnitClass {
    Normal,
    Build,
}

/// One resolved dependency edge (pointing at an index into `units`).
#[derive(Clone, Debug)]
pub struct UnitDep {
    /// The `--extern` naming key: the rename key when renamed (`package = "..."`),
    /// otherwise the dep package's lib target name (see `extern_key` for the cargo
    /// semantics behind it).
    pub key: String,
    pub unit: usize,
    /// Edge class (normal/build); consumers filter by it (extern filtering, build
    /// closure, build script --extern).
    pub class: UnitClass,
    pub kind: DepKind,
}

/// One compilation unit (package x class x feature set).
#[derive(Clone, Debug)]
pub struct Unit {
    pub package: String,
    /// Lib target name (the file-name side of --extern; equals package when there is no
    /// lib).
    pub lib_name: String,
    pub version: Version,
    pub source_dir: PathBuf,
    pub from_registry: bool,
    /// A precise source identity that is immutable like Git but cannot be told apart by
    /// package/version alone. A registry package is pinned by version + checksum and
    /// keeps this `None`; a path package goes through source-tree snapshotting.
    pub immutable_source_id: Option<String>,
    /// This unit's class (a node key component of the resolver v2 normal/build split;
    /// edge class filtering uses `UnitDep.class`, so this field is kept for the audit
    /// and model surface).
    #[allow(dead_code)]
    pub class: UnitClass,
    pub features: BTreeSet<String>,
    /// The full declaration set Cargo passes to rustc as
    /// `--check-cfg cfg(feature, values(...))`; distinct from the enabled set above.
    pub declared_features: BTreeSet<String>,
    pub proc_macro: bool,
    pub has_build_script: bool,
    /// Custom build script path from `[package] build = "custom.rs"`; `None` means the
    /// default `<source_dir>/build.rs` (used by build.rs scheduling).
    pub build_script_path: Option<PathBuf>,
    /// The -sys link key (used for DEP_* propagation keys and the links mutual-exclusion
    /// check during build.rs scheduling).
    pub links: Option<String>,
    pub deps: Vec<UnitDep>,
    /// package.edition ("2015" by default for registry packages) -- used for dep rustc
    /// arguments.
    pub edition: String,
    /// Absolute path of the lib root file ([lib] path or the default src/lib.rs) -- used
    /// for dep rustc arguments.
    pub lib_path: PathBuf,
    /// The full CARGO_PKG_* env set (computed by `manifest::pkg_env_map`; env for the dep
    /// compilation subprocess).
    pub pkg_env: BTreeMap<String, String>,
    /// rustc arguments generated from this package's `[lints]`; every target (build.rs
    /// included) must consume them.
    pub rustc_lint_flags: Vec<String>,
}

/// Resolution result.
#[derive(Clone, Debug)]
pub struct ResolvePlan {
    pub root_name: String,
    pub root_version: Version,
    /// Audit-surface field kept for the reconciliation tool; the driver takes the root
    /// directory from the manifest instead.
    #[allow(dead_code)]
    pub root_dir: PathBuf,
    pub root_features: BTreeSet<String>,
    pub units: Vec<Unit>,
    /// The root (bin) --extern edge table: the root is not a unit itself, but a bin
    /// session needs the same dependency edges (through the same gates as the unit edges;
    /// produced by assemble_units).
    pub root_deps: Vec<UnitDep>,
    /// name -> resolved version set (audit surface: used to reconcile against Cargo.lock).
    pub version_map: BTreeMap<String, Vec<Version>>,
    /// fresh mode = the generated canonical lock; lock mode = the input lock echoed back.
    pub lock: Lockfile,
}

/// The union of resolver v2 features propagated from other roots in the same workspace
/// command. The key carries the version and the normal/build class so that compilation
/// units Cargo deliberately keeps apart are not merged.
pub type FeatureOverrides = BTreeMap<(String, Version, UnitClass), BTreeSet<String>>;

/// The root package's current purpose only changes whether dev dependencies enter the
/// build graph; version resolution and Cargo.lock always see the root dev dependencies,
/// matching Cargo locking dev dependencies on `cargo build` too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvePurpose {
    Run,
    Test,
}

impl ResolvePurpose {
    fn includes_dev(self) -> bool {
        self == Self::Test
    }
}

// ---------- main entry points ----------

pub fn resolve(root: &PackageManifest, src: &mut impl PkgSource) -> Result<ResolvePlan, String> {
    resolve_for(root, src, ResolvePurpose::Run)
}

pub fn resolve_for(
    root: &PackageManifest,
    src: &mut impl PkgSource,
    purpose: ResolvePurpose,
) -> Result<ResolvePlan, String> {
    resolve_for_known(root, src, purpose, &[])
}

/// Workspace entry point: `known_paths` are the members whose workspace inheritance has
/// already been materialized. When a path edge lands on a member directory it must reuse
/// that manifest rather than re-reading the original, which still says
/// `workspace = true`.
pub fn resolve_for_known(
    root: &PackageManifest,
    src: &mut impl PkgSource,
    purpose: ResolvePurpose,
    known_paths: &[PackageManifest],
) -> Result<ResolvePlan, String> {
    resolve_for_known_with_features(root, src, purpose, known_paths, &FeatureOverrides::new())
}

pub fn resolve_for_known_with_features(
    root: &PackageManifest,
    src: &mut impl PkgSource,
    purpose: ResolvePurpose,
    known_paths: &[PackageManifest],
    workspace_features: &FeatureOverrides,
) -> Result<ResolvePlan, String> {
    let compiler_rust_version = current_rust_version()?;
    let rust_version_policy = if root.ignore_rust_version {
        IncompatibleRustVersions::Allow
    } else {
        super::config::CargoConfig::load_at(
            &root.lock_root,
            super::config::cargo_home().as_deref(),
        )?
        .incompatible_rust_versions(root.resolver)?
    };
    let resolver_rust_version = root
        .resolver_rust_version
        .clone()
        .unwrap_or_else(|| compiler_rust_version.clone());
    let lock_path = root.lock_root.join("Cargo.lock");
    let input_lock = if lock_path.is_file() {
        Some(Lockfile::read(&lock_path)?)
    } else {
        None
    };

    let mut overrides = SourceOverrides::default();
    let mut override_manifests = Vec::new();
    for patch in &root.patches {
        let logical_source = src.registry_source(&patch.registry)?;
        let logical = registry_identity(&patch.dependency.package, &logical_source);
        if let Some(mut manifest) =
            load_override_manifest(&patch.dependency, input_lock.as_ref(), src)?
        {
            if manifest.name != patch.dependency.package {
                return Err(format!(
                    "[patch] key {} points at package.name {}",
                    patch.dependency.package, manifest.name
                ));
            }
            manifest.patches.clear();
            manifest.replacements.clear();
            let selected = local_manifest_key(&manifest);
            overrides
                .patches
                .insert((logical, manifest.version.clone()), selected);
            override_manifests.push(manifest);
        } else if let DepSource::Registry(req, reference) = &patch.dependency.source {
            let source = src.registry_source(reference)?;
            let selected = registry_identity(&patch.dependency.package, &source);
            let candidates = registry_entry(src, &selected)?;
            let mut matched = false;
            for candidate in candidates
                .iter()
                .filter(|candidate| req.matches(&candidate.version))
            {
                matched = true;
                overrides.patches.insert(
                    (logical.clone(), candidate.version.clone()),
                    selected.clone(),
                );
            }
            if !matched {
                return Err(format!(
                    "[patch] registry source has no version of {} matching {}",
                    patch.dependency.package, req
                ));
            }
        }
    }
    for replacement in &root.replacements {
        let logical_source = replacement
            .source
            .as_deref()
            .map(|source| {
                if source.starts_with("registry+") || source.starts_with("sparse+") {
                    source.to_string()
                } else {
                    format!("registry+{source}")
                }
            })
            .unwrap_or_else(|| CRATES_IO_LOCK_SOURCE.to_string());
        let logical = registry_identity(&replacement.package, &logical_source);
        let selected = if let Some(mut manifest) =
            load_override_manifest(&replacement.dependency, input_lock.as_ref(), src)?
        {
            if manifest.name != replacement.package || manifest.version != replacement.version {
                return Err(format!(
                    "[replace] {}:{} must be replaced by a package of the same name and version, got {}:{}",
                    replacement.package, replacement.version, manifest.name, manifest.version
                ));
            }
            manifest.patches.clear();
            manifest.replacements.clear();
            let selected = local_manifest_key(&manifest);
            override_manifests.push(manifest);
            selected
        } else if let DepSource::Registry(req, reference) = &replacement.dependency.source {
            let source = src.registry_source(reference)?;
            let selected = registry_identity(&replacement.package, &source);
            if !req.matches(&replacement.version)
                || !registry_entry(src, &selected)?
                    .iter()
                    .any(|candidate| candidate.version == replacement.version)
            {
                return Err(format!(
                    "[replace] source has no {} {}",
                    replacement.package, replacement.version
                ));
            }
            selected
        } else {
            unreachable!()
        };
        if overrides
            .replacements
            .insert((logical, replacement.version.clone()), selected)
            .is_some()
        {
            return Err(format!(
                "[replace] {} {} is specified twice",
                replacement.package, replacement.version
            ));
        }
    }

    // BFS over path/Git dependencies. A movable Git reference is resolved to a precise
    // commit here; an existing lock only supplies the commit for the lock source and does
    // not reinterpret branch/tag/default HEAD.
    let mut path_manifests: BTreeMap<String, PackageManifest> = BTreeMap::new();
    let mut queue: VecDeque<(PackageManifest, bool)> = VecDeque::new();
    for manifest in override_manifests {
        let identity = local_manifest_key(&manifest);
        path_manifests.insert(identity, manifest.clone());
        queue.push_back((manifest, false));
    }
    queue.push_back((clone_root_shallow(root), true));
    while let Some((m, is_root)) = queue.pop_front() {
        for d in m.deps.iter().filter(|d| is_root || d.kind != DepKind::Dev) {
            let next = match &d.source {
                DepSource::Path(path) => {
                    let absolute = std::fs::canonicalize(path)
                        .or_else(|_| std::path::absolute(path))
                        .unwrap_or_else(|_| path.clone());
                    let mut manifest = known_paths
                        .iter()
                        .find(|known| known.root == absolute)
                        .cloned()
                        .map(Ok)
                        .unwrap_or_else(|| read_path_manifest(&absolute))
                        .map_err(|error| {
                            format!(
                                "path dependency {} ({}): {error}",
                                d.package,
                                absolute.display()
                            )
                        })?;
                    if let Some(checkout) = &m.git_checkout_root {
                        if !absolute.starts_with(checkout) {
                            return Err(format!(
                                "path dependency {} of Git package {} escapes the repository checkout; a Cargo Git source may not reference a path outside the repository",
                                m.name,
                                absolute.display()
                            ));
                        }
                        manifest.lock_source = m.lock_source.clone();
                        manifest.git_checkout_root = m.git_checkout_root.clone();
                    }
                    Some(manifest)
                }
                DepSource::Git(spec) => {
                    let locked = input_lock
                        .as_ref()
                        .map(|lock| locked_git_source(lock, spec, &d.package))
                        .transpose()?
                        .flatten();
                    Some(src.ensure_git_package(spec, &d.package, locked.as_deref())?)
                }
                DepSource::Registry(..) => None,
            };
            if let Some(manifest) = next {
                let identity = local_manifest_key(&manifest);
                if let Some(existing) = path_manifests.get(&identity) {
                    if existing.root != manifest.root {
                        return Err(format!(
                            "local/Git package identity collision on `{}`: {} and {}",
                            d.package,
                            existing.root.display(),
                            manifest.root.display()
                        ));
                    }
                    continue;
                }
                path_manifests.insert(identity.clone(), manifest);
                queue.push_back((
                    clone_root_shallow(path_manifests.get(&identity).unwrap()),
                    false,
                ));
            }
        }
    }

    // Version resolution + feature unification (two passes: the resolve graph = strong
    // and weak references together (the lock/resolution gate), the build graph = strong
    // edges only (unit features and buildability, matching cargo's build graph))
    let (version_map, out_lock, _lock_nodes, build_nodes, edge_versions) = match &input_lock {
        Some(lf) => {
            let (vm, ev) = versions_from_lock(root, &path_manifests, lf)?;
            let (nodes, _) = unify_features(
                root,
                &path_manifests,
                &ev,
                src,
                true,
                true,
                workspace_features,
            )?;
            let (build_nodes, _) = unify_features(
                root,
                &path_manifests,
                &ev,
                src,
                false,
                purpose.includes_dev(),
                workspace_features,
            )?;
            (vm, lf.clone(), nodes, build_nodes, ev)
        }
        None => {
            // Iteration to a fixed point: an optional dependency enters version
            // resolution only when activated by a (parent package, dependency key) pair
            // or weakly referenced (cargo semantics -- a global package-name gate would
            // misattribute zerovec's yoke to litemap). The activation set grows
            // monotonically, so this converges.
            let mut activated: BTreeSet<(String, Version, String)> = BTreeSet::new();
            let mut preferred_exact_versions = BTreeMap::new();
            let fresh_context = FreshSolveContext {
                rust_version_policy,
                resolver_rust_version: &resolver_rust_version,
                overrides: &overrides,
            };
            loop {
                let (vm, lf, ev) = solve_fresh(
                    root,
                    &path_manifests,
                    src,
                    &activated,
                    &mut preferred_exact_versions,
                    &fresh_context,
                )?;
                let (nodes, mut new_activated) = unify_features(
                    root,
                    &path_manifests,
                    &ev,
                    src,
                    true,
                    true,
                    workspace_features,
                )?;
                new_activated.extend(activated.iter().cloned());
                if new_activated == activated {
                    // Lock dependency lines are filled in only after convergence: the
                    // optional gate is judged per (parent package, dependency key), since
                    // a global set would misattribute cipher's zeroize to generic-array.
                    let mut lf = lf;
                    fill_lock_dependency_lines(
                        &mut lf,
                        root,
                        &path_manifests,
                        &ev,
                        &nodes,
                        src,
                        &overrides,
                    )?;
                    fill_unused_patches(&mut lf, &path_manifests, src, &overrides)?;
                    let (build_nodes, _) = unify_features(
                        root,
                        &path_manifests,
                        &ev,
                        src,
                        false,
                        purpose.includes_dev(),
                        workspace_features,
                    )?;
                    break (vm, lf, nodes, build_nodes, ev);
                }
                activated = new_activated;
            }
        }
    };

    // Compilation unit assembly (build graph nodes: only the strong-edge activation
    // surface) plus the root's --extern edge table
    let (units, root_deps) = assemble_units(
        root,
        &path_manifests,
        &edge_versions,
        &build_nodes,
        src,
        purpose.includes_dev(),
    )?;
    if !root.ignore_rust_version {
        validate_compiler_rust_version(root, &path_manifests, &units, src, &compiler_rust_version)?;
    }

    Ok(ResolvePlan {
        root_name: root.name.clone(),
        root_version: root.version.clone(),
        root_dir: root.root.clone(),
        root_features: build_nodes
            .get(&(root.name.clone(), root.version.clone(), UnitClass::Normal))
            .map(|n| n.features.clone())
            .unwrap_or_default(),
        units,
        root_deps,
        version_map,
        lock: out_lock,
    })
}

fn clone_root_shallow(m: &PackageManifest) -> PackageManifest {
    m.clone()
}

const LOCAL_ID_SEPARATOR: char = '\u{1f}';
const CRATES_IO_LOCK_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn registry_identity(name: &str, source: &str) -> String {
    format!("{name}{LOCAL_ID_SEPARATOR}{source}")
}

fn identity_package_name(identity: &str) -> &str {
    identity
        .split_once(LOCAL_ID_SEPARATOR)
        .map(|(name, _)| name)
        .unwrap_or(identity)
}

fn identity_source(identity: &str) -> Option<&str> {
    identity
        .split_once(LOCAL_ID_SEPARATOR)
        .map(|(_, source)| source)
}

fn registry_entry(src: &mut impl PkgSource, identity: &str) -> Result<IndexEntry, String> {
    let source = identity_source(identity).unwrap_or(CRATES_IO_LOCK_SOURCE);
    src.index_entry(source, identity_package_name(identity))
}

#[derive(Clone, Debug, Default)]
struct SourceOverrides {
    /// (original registry identity, version) -> patch source identity.
    patches: BTreeMap<(String, Version), String>,
    /// (replaced registry identity, exact version) -> replacement source identity.
    replacements: BTreeMap<(String, Version), String>,
}

impl SourceOverrides {
    fn selected_identity(&self, identity: &str, version: &Version) -> String {
        self.replacements
            .get(&(identity.to_string(), version.clone()))
            .or_else(|| self.patches.get(&(identity.to_string(), version.clone())))
            .cloned()
            .unwrap_or_else(|| identity.to_string())
    }

    fn patched_entry(
        &self,
        src: &mut impl PkgSource,
        manifests: &BTreeMap<String, PackageManifest>,
        identity: &str,
    ) -> Result<IndexEntry, String> {
        let mut versions = registry_entry(src, identity)?.to_vec();
        for ((logical, version), selected) in &self.patches {
            if logical != identity {
                continue;
            }
            let candidate = if let Some(manifest) = manifests.get(selected) {
                super::vendor::VendorDir::entry_from_manifest(manifest)?
            } else {
                registry_entry(src, selected)?
                    .iter()
                    .find(|candidate| candidate.version == *version)
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "[patch] source {} has no {} {version}",
                            selected,
                            identity_package_name(identity)
                        )
                    })?
            };
            versions.retain(|existing| existing.version != *version);
            versions.push(candidate);
        }
        versions.sort_by(|left, right| left.version.cmp(&right.version));
        Ok(versions.into())
    }
}

fn load_override_manifest(
    dependency: &super::manifest::DepDecl,
    input_lock: Option<&Lockfile>,
    src: &mut impl PkgSource,
) -> Result<Option<PackageManifest>, String> {
    match &dependency.source {
        DepSource::Path(path) => {
            let absolute = std::fs::canonicalize(path)
                .or_else(|_| std::path::absolute(path))
                .unwrap_or_else(|_| path.clone());
            read_path_manifest(&absolute).map(Some)
        }
        DepSource::Git(spec) => {
            let locked = input_lock
                .map(|lock| locked_git_source(lock, spec, &dependency.package))
                .transpose()?
                .flatten();
            src.ensure_git_package(spec, &dependency.package, locked.as_deref())
                .map(Some)
        }
        DepSource::Registry(..) => Ok(None),
    }
}

fn local_manifest_key(manifest: &PackageManifest) -> String {
    let source = manifest
        .lock_source
        .clone()
        .unwrap_or_else(|| format!("path+{}", manifest.root.display()));
    format!("{}{LOCAL_ID_SEPARATOR}{source}", manifest.name)
}

fn local_package_name(identity: &str, manifests: &BTreeMap<String, PackageManifest>) -> String {
    manifests
        .get(identity)
        .map(|manifest| manifest.name.clone())
        .unwrap_or_else(|| identity_package_name(identity).to_string())
}

fn local_dep_identity(
    dependency: &super::manifest::DepDecl,
    manifests: &BTreeMap<String, PackageManifest>,
) -> Result<String, String> {
    let mut matches = manifests.iter().filter(|(_, manifest)| {
        if manifest.name != dependency.package {
            return false;
        }
        match &dependency.source {
            DepSource::Path(path) => {
                let absolute = std::fs::canonicalize(path)
                    .or_else(|_| std::path::absolute(path))
                    .unwrap_or_else(|_| path.clone());
                manifest.root == absolute
            }
            DepSource::Git(spec) => manifest
                .lock_source
                .as_deref()
                .is_some_and(|source| source.starts_with(&format!("{}#", spec.source_id()))),
            DepSource::Registry(..) => false,
        }
    });
    let first = matches.next().map(|(identity, _)| identity.clone());
    if matches.next().is_some() {
        return Err(format!(
            "dependency {} matches several local/Git packages with the same source",
            dependency.package
        ));
    }
    first.ok_or_else(|| {
        format!(
            "the local/Git package of dependency {} was fetched but cannot be located by source",
            dependency.package
        )
    })
}

fn read_path_manifest(path: &Path) -> Result<PackageManifest, String> {
    let workspace = super::workspace::WorkspaceManifest::read_dependency(path)?;
    workspace
        .members
        .into_iter()
        .find(|member| member.root == path)
        .or_else(|| {
            std::fs::canonicalize(path).ok().and_then(|canonical| {
                super::workspace::WorkspaceManifest::read_dependency(&canonical)
                    .ok()?
                    .members
                    .into_iter()
                    .find(|member| member.root == canonical)
            })
        })
        .ok_or_else(|| format!("{} is not a resolvable Cargo package", path.display()))
}

fn locked_git_source(
    lock: &Lockfile,
    spec: &GitSpec,
    package: &str,
) -> Result<Option<String>, String> {
    let source_id = spec.source_id();
    let mut matches = lock.packages.iter().filter(|candidate| {
        candidate.name == package
            && spec.version.matches(&candidate.version)
            && candidate
                .source
                .as_deref()
                .is_some_and(|source| source.starts_with(&format!("{source_id}#")))
    });
    let first = matches.next().and_then(|package| package.source.clone());
    if matches.next().is_some() {
        return Err(format!(
            "Cargo.lock has several precise commits for Git dependency {package} / {source_id}; cannot disambiguate"
        ));
    }
    if first.is_none() {
        return Err(format!(
            "stale Cargo.lock: Git dependency {package} has no locked package matching {source_id}"
        ));
    }
    Ok(first)
}

// ---------- lock mode ----------

fn versions_from_lock(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    lf: &Lockfile,
) -> Result<(BTreeMap<String, Vec<Version>>, EdgeVersions), String> {
    let mut map: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    let mut edges: EdgeVersions = BTreeMap::new();
    // Walk the graph from the root's lock row (the root package is always in the lock)
    let root_locked = lf
        .packages
        .iter()
        .find(|p| p.name == root.name && p.version == root.version)
        .ok_or_else(|| {
            format!(
                "Cargo.lock has no root package {} {} -- the lock and the manifest are out of sync (re-resolve or delete the lock)",
                root.name, root.version
            )
        })?;
    let mut visited: BTreeSet<(String, Version)> = BTreeSet::new();
    let mut stack: Vec<(&LockedPkg, String)> = vec![(root_locked, root.name.clone())];
    while let Some((pkg, parent_identity)) = stack.pop() {
        for dependency in &pkg.dependencies {
            let candidates: Vec<&LockedPkg> = lf
                .find(&dependency.name)
                .into_iter()
                .filter(|candidate| {
                    dependency
                        .version
                        .as_ref()
                        .is_none_or(|version| candidate.version == *version)
                        && dependency.source.as_ref().is_none_or(|source| {
                            candidate.source.as_deref().is_some_and(|candidate| {
                                lock_dependency_source(candidate) == *source
                            })
                        })
                })
                .collect();
            let child = match candidates.as_slice() {
                [child] => *child,
                [] => {
                    return Err(format!(
                        "broken lock graph: no package row for {} {:?} {:?}",
                        dependency.name, dependency.version, dependency.source
                    ));
                }
                _ => {
                    return Err(format!(
                        "ambiguous lock graph: {} matches {} package rows and the dependency line lacks version/source disambiguation",
                        dependency.name,
                        candidates.len()
                    ));
                }
            };
            let effective_child = effective_locked_package(lf, child)?;
            let child_identity = locked_package_identity(effective_child, path_manifests)?;
            // Edge version record: lock dependency lines carry no kind information, so
            // register under both Normal and Build. The disambiguator keeps both the
            // chosen child's source and version: registry versions of one source must not
            // overwrite each other, while a Git package must keep its URL/commit for
            // manifest source matching.
            let disambiguator = child.source.as_ref().map_or_else(
                || child.version.to_string(),
                |source| format!("{source} {}", child.version),
            );
            for class in [UnitClass::Normal, UnitClass::Build] {
                edges.insert(
                    (
                        parent_identity.clone(),
                        pkg.version.clone(),
                        dependency.name.clone(),
                        disambiguator.clone(),
                        class,
                    ),
                    (child_identity.clone(), child.version.clone()),
                );
            }
            if visited.insert((child_identity.clone(), child.version.clone())) {
                stack.push((effective_child, child_identity));
            }
        }
    }
    for (identity, v) in visited {
        let name = local_package_name(&identity, path_manifests);
        let vs = map.entry(name).or_default();
        if !vs.contains(&v) {
            vs.push(v);
        }
    }
    // Integrity check: every registry req in a manifest must be satisfied by a locked
    // version (the guard against a stale lock)
    let check = |m: &PackageManifest, include_dev: bool| -> Result<(), String> {
        for d in m
            .deps
            .iter()
            .filter(|d| include_dev || d.kind != DepKind::Dev)
        {
            let req = match &d.source {
                DepSource::Registry(req, _) => Some(req),
                DepSource::Git(spec) => Some(&spec.version),
                DepSource::Path(_) => None,
            };
            if let Some(req) = req
                && !map
                    .get(&d.package)
                    .is_some_and(|versions| versions.iter().any(|version| req.matches(version)))
            {
                return Err(format!(
                    "stale Cargo.lock: the locked version of {} does not satisfy req {req} (the manifest changed; re-resolve or delete the lock)",
                    d.package
                ));
            }
        }
        Ok(())
    };
    check(root, true)?;
    for m in path_manifests.values() {
        check(m, false)?;
    }
    Ok((map, edges))
}

fn effective_locked_package<'a>(
    lock: &'a Lockfile,
    package: &'a LockedPkg,
) -> Result<&'a LockedPkg, String> {
    let Some(replace) = package.replace.as_deref() else {
        return Ok(package);
    };
    let mut parts = replace.split_whitespace();
    let name = parts
        .next()
        .ok_or_else(|| format!("invalid Cargo.lock replace line: {replace}"))?;
    let version = parts
        .next()
        .ok_or_else(|| format!("Cargo.lock replace line lacks a version: {replace}"))
        .and_then(|version| {
            Version::parse(version)
                .map_err(|error| format!("invalid version on Cargo.lock replace line: {error}"))
        })?;
    let candidates = lock
        .packages
        .iter()
        .filter(|candidate| {
            candidate.name == name
                && candidate.version == version
                && !std::ptr::eq(*candidate, package)
        })
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [replacement] => Ok(*replacement),
        [] => Err(format!(
            "Cargo.lock replace `{replace}` has no replacement package row"
        )),
        _ => Err(format!(
            "Cargo.lock replace `{replace}` matches several replacement package rows"
        )),
    }
}

fn locked_package_identity(
    package: &LockedPkg,
    manifests: &BTreeMap<String, PackageManifest>,
) -> Result<String, String> {
    let mut matches = manifests.iter().filter(|(_, manifest)| {
        manifest.name == package.name
            && manifest.version == package.version
            && manifest.lock_source == package.source
    });
    let first = matches.next().map(|(identity, _)| identity.clone());
    if matches.next().is_some() {
        return Err(format!(
            "Cargo.lock package {} {} {:?} matches several local/Git packages",
            package.name, package.version, package.source
        ));
    }
    Ok(first.unwrap_or_else(|| match package.source.as_deref() {
        Some(source) if source.starts_with("registry+") || source.starts_with("sparse+") => {
            registry_identity(&package.name, source)
        }
        _ => package.name.clone(),
    }))
}
