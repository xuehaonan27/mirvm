//! `cargoless/resolve.rs` -- version resolution + feature unification -> compilation
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

use semver::{Version, VersionReq};

use super::lockfile::{LockedDep, LockedPkg, Lockfile, UnusedPatch};
use super::manifest::{
    DepKind, DepSource, FeatureValue, GitSpec, IncompatibleRustVersions, PackageManifest,
    RegistryReference, ResolverVersion, current_rust_version, parse_feature_value,
};
use super::registry::{IndexEntry, IndexVersion, Registry};
use pubgrub::Reporter as _;

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

// ---------- fresh mode (pubgrub + lazy-bucket multi-version) ----------
//
// The standard pubgrub model is "one version per package", while cargo allows several
// versions of one name to coexist (hashbrown 0.14/0.15, syn 1/2/3 in one graph).
// Encoding: lazy-bucket -- a package id is (name, bucket). When a dep edge arrives and
// its requirement shares a candidate with an existing bucket's accumulated range (some
// index version satisfies both), it joins that bucket; otherwise it opens a new one.
// That is exactly cargo's "unify when possible, otherwise coexist" semantics, and
// pubgrub does backtracking per bucket independently.

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Pkg {
    Root,
    Registry(String, u32),
    Local(String),
}

impl std::fmt::Display for Pkg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pkg::Root => write!(f, "<root>"),
            Pkg::Registry(n, 0) => write!(f, "{n}"),
            Pkg::Registry(n, k) => write!(f, "{n}#{k}"),
            Pkg::Local(n) => write!(
                f,
                "<local:{}>",
                n.split(LOCAL_ID_SEPARATOR).next().unwrap_or(n)
            ),
        }
    }
}

// pubgrub::Package is satisfied automatically by the blanket impl
// (Clone+Eq+Hash+Debug+Display).

/// The version each dep edge points at after solving: ((parent lock name, parent
/// version), dependency key, disambiguator, class) -> (package, version). The
/// disambiguator is the req string in fresh mode (entries with several reqs for one name
/// must stay separate, as in the four ark-ff crates of ruint) and the child version string
/// in lock mode (the hint on the dep line).
pub type EdgeVersions = BTreeMap<(String, Version, String, String, UnitClass), (String, Version)>;

// (dependency key, package name, req, class, is-registry)
type RawDep = (String, String, VersionReq, UnitClass, bool);

/// Edge assignment record type: (parent, dep key, class) -> (pkg, bucket).
type EdgeAssign = BTreeMap<(String, Version, String, String, UnitClass), (String, u32)>;

fn dep_unit_class(kind: DepKind) -> UnitClass {
    match kind {
        DepKind::Normal | DepKind::Dev => UnitClass::Normal,
        DepKind::Build => UnitClass::Build,
    }
}

/// Value type of the idempotent `get_dependencies` memo.
type DepsRc = std::rc::Rc<Vec<(Pkg, pubgrub::Ranges<Version>)>>;

/// pre comparator record type (a list of (major, minor, patch) triples).
type PreComparators = Vec<(u64, Option<u64>, Option<u64>)>;

struct CratesIo<'a, S: PkgSource> {
    src: std::cell::RefCell<&'a mut S>,
    manifests: &'a BTreeMap<String, PackageManifest>,
    overrides: &'a SourceOverrides,
    root: &'a PackageManifest,
    rust_version_policy: IncompatibleRustVersions,
    resolver_rust_version: &'a Version,
    /// When the first solving pass finds one name split into several versions by exact
    /// pins, the second pass prefers the version established earliest. It is fixed for the
    /// whole pass and must not depend on mutable bucket state during solving.
    preferred_exact_versions: &'a BTreeMap<String, Version>,
    /// pre comparator records (cargo's exact rule: a prerelease is selectable only when
    /// some req of that package names a comparator with the same major/minor/patch and a
    /// pre). Value = (major, minor, patch).
    allow_pre: std::cell::RefCell<BTreeMap<String, PreComparators>>,
    /// The optional dependencies activated by (parent package name, dependency key) in
    /// this round (the fixed-point input; a global package-name set would misattribute
    /// zerovec's yoke to litemap).
    activated: &'a BTreeSet<(String, Version, String)>,
    /// name -> next bucket number (starting at 0).
    buckets: std::cell::RefCell<BTreeMap<String, u32>>,
    /// (name, bucket) -> constraint accumulated so far (used for the merge decision).
    bucket_ranges: std::cell::RefCell<BTreeMap<(String, u32), pubgrub::Ranges<Version>>>,
    /// (parent, dep key, class) -> (pkg, bucket): the edge assignment record (a by-product
    /// of the idempotent dependency memo).
    edge_assign: std::cell::RefCell<EdgeAssign>,
    /// Idempotent `get_dependencies` memo (repeated calls for one (P,V) must return the
    /// same assignment).
    deps_memo: std::cell::RefCell<BTreeMap<(Pkg, Version), DepsRc>>,
    source_error: std::cell::RefCell<Option<String>>,
}

fn req_has_pre(req: &VersionReq) -> bool {
    req.comparators.iter().any(|c| !c.pre.is_empty())
}

fn req_is_full_exact(req: &VersionReq) -> bool {
    req.comparators.len() == 1
        && req.comparators[0].op == semver::Op::Exact
        && req.comparators[0].minor.is_some()
        && req.comparators[0].patch.is_some()
}

fn cargo_versions_compatible(left: &Version, right: &Version) -> bool {
    if !left.pre.is_empty() || !right.pre.is_empty() || left.major != right.major {
        return false;
    }
    left.major > 0 || (left.minor == right.minor && (left.minor > 0 || left.patch == right.patch))
}

impl<'a, S: PkgSource> CratesIo<'a, S> {
    fn index_entry(&self, identity: &str) -> Result<IndexEntry, std::io::Error> {
        let mut source = self.src.borrow_mut();
        self.overrides
            .patched_entry(&mut **source, self.manifests, identity)
            .map_err(io_err)
    }

    fn rust_version_compatible(&self, version: &IndexVersion) -> bool {
        version
            .rust_version
            .as_ref()
            .is_none_or(|required| required <= self.resolver_rust_version)
    }
    /// Whether a prerelease is admitted (cargo's exact rule): only when a comparator
    /// with the same major/minor/patch and a pre names that version.
    fn pre_allowed(&self, name: &str, v: &Version) -> bool {
        if v.pre.is_empty() {
            return true;
        }
        self.allow_pre.borrow().get(name).is_some_and(|cs| {
            cs.iter().any(|(maj, min, pat)| {
                *maj == v.major
                    && min.is_none_or(|m| m == v.minor)
                    && pat.is_none_or(|p| p == v.patch)
            })
        })
    }

    /// Candidate check: a non-yanked index version satisfies the range (the pre rule is
    /// the same as in `choose_version`).
    fn any_candidate(
        &self,
        name: &str,
        range: &pubgrub::Ranges<Version>,
    ) -> Result<bool, std::io::Error> {
        let vs = self.index_entry(name)?;
        Ok(vs
            .iter()
            .any(|v| !v.yanked && range.contains(&v.version) && self.pre_allowed(name, &v.version)))
    }

    /// Edge assignment: if the req shares a candidate with an existing bucket's
    /// accumulated range, join it; otherwise open a new bucket.
    fn assign_bucket(
        &self,
        name: &str,
        range: &pubgrub::Ranges<Version>,
    ) -> Result<u32, std::io::Error> {
        let next = *self.buckets.borrow().get(name).unwrap_or(&0);
        for k in 0..next {
            let acc = self.bucket_ranges.borrow()[&(name.to_string(), k)].intersection(range);
            if self.any_candidate(name, &acc)? {
                self.bucket_ranges
                    .borrow_mut()
                    .insert((name.to_string(), k), acc);
                return Ok(k);
            }
        }
        self.buckets.borrow_mut().insert(name.to_string(), next + 1);
        self.bucket_ranges
            .borrow_mut()
            .insert((name.to_string(), next), range.clone());
        Ok(next)
    }

    /// The second pass only penalizes candidates that would deviate from an already
    /// established exact version. The first pass has no preference, so plain
    /// highest-version-first still applies; ordinary wide-range dependencies do not
    /// participate in the ordering.
    fn dependency_split_cost(&self, candidate: &IndexVersion) -> usize {
        candidate
            .deps
            .iter()
            .filter(|dep| {
                dep.kind.as_deref() != Some("dev")
                    && req_is_full_exact(&dep.req)
                    && (!dep.optional
                        || self.activated.contains(&(
                            candidate.name.clone(),
                            candidate.version.clone(),
                            dep.name.clone(),
                        )))
            })
            .filter(|dep| {
                let name = dep.package.as_deref().unwrap_or(&dep.name);
                self.preferred_exact_versions
                    .get(name)
                    .is_some_and(|version| !dep.req.matches(version))
            })
            .count()
    }

    fn parent_lock_name(&self, package: &Pkg) -> String {
        match package {
            Pkg::Root => self.root.name.clone(),
            Pkg::Local(n) => n.clone(),
            Pkg::Registry(n, _) => n.clone(),
        }
    }

    /// The raw dependency list of one (P,V) (the idempotent memo plus the edge
    /// assignment record).
    fn raw_deps(&self, package: &Pkg, version: &Version) -> Result<DepsRc, std::io::Error> {
        if let Some(hit) = self
            .deps_memo
            .borrow()
            .get(&(package.clone(), version.clone()))
        {
            return Ok(hit.clone());
        }
        let parent_name = self.parent_lock_name(package);
        let mut raw: Vec<RawDep> = Vec::new();
        match package {
            Pkg::Root => {
                for d in &self.root.deps {
                    collect_decl(
                        &self.root.name,
                        &self.root.version,
                        d,
                        self.activated,
                        self,
                        &mut raw,
                    )?;
                }
            }
            Pkg::Local(name) => {
                let m = self.manifests.get(name).ok_or_else(|| {
                    io_err(format!("manifest of path package {name} was not collected"))
                })?;
                for d in m.deps.iter().filter(|d| d.kind != DepKind::Dev) {
                    collect_decl(name, &m.version, d, self.activated, self, &mut raw)?;
                }
            }
            Pkg::Registry(name, _) => {
                let vs = self.index_entry(name)?;
                let Some(iv) = vs.iter().find(|v| v.version == *version) else {
                    return Err(io_err(format!("{name} {version} is not in the index")));
                };
                for d in &iv.deps {
                    if d.kind.as_deref() == Some("dev") {
                        continue; // dev edges are not resolved (stated up front)
                    }
                    let package_name = d.package.clone().unwrap_or_else(|| d.name.clone());
                    let child_source = d
                        .registry
                        .as_deref()
                        .map(|source| {
                            if source.starts_with("sparse+") || source.starts_with("registry+") {
                                source.to_string()
                            } else {
                                format!("registry+{source}")
                            }
                        })
                        .unwrap_or_else(|| {
                            identity_source(name)
                                .unwrap_or(CRATES_IO_LOCK_SOURCE)
                                .to_string()
                        });
                    let pkg_name = registry_identity(&package_name, &child_source);
                    // An unactivated optional dependency is not resolved (gated by
                    // (parent package name, dependency key); the platform cfg is not
                    // evaluated -- union semantics)
                    if d.optional
                        && !self.activated.contains(&(
                            name.to_string(),
                            version.clone(),
                            d.name.clone(),
                        ))
                    {
                        continue;
                    }
                    if req_has_pre(&d.req) {
                        for c in &d.req.comparators {
                            if !c.pre.is_empty() {
                                self.allow_pre
                                    .borrow_mut()
                                    .entry(pkg_name.clone())
                                    .or_default()
                                    .push((c.major, c.minor, c.patch));
                            }
                        }
                    }
                    raw.push((
                        d.name.clone(),
                        pkg_name,
                        d.req.clone(),
                        if d.kind.as_deref() == Some("build") {
                            UnitClass::Build
                        } else {
                            UnitClass::Normal
                        },
                        true,
                    ));
                }
            }
        }
        let mut out: Vec<(Pkg, pubgrub::Ranges<Version>)> = Vec::new();
        for (key, pkg_name, req, class, registry) in raw {
            if registry {
                let range = req_to_ranges(&req);
                let bucket = self.assign_bucket(&pkg_name, &range)?;
                self.edge_assign.borrow_mut().insert(
                    (
                        parent_name.clone(),
                        version.clone(),
                        key,
                        req.to_string(),
                        class,
                    ),
                    (pkg_name.clone(), bucket),
                );
                out.push((Pkg::Registry(pkg_name, bucket), range));
            } else {
                // path dependency: bucket 0 is registered the same way (bucket_versions
                // is filled in by the solve output side)
                self.edge_assign.borrow_mut().insert(
                    (
                        parent_name.clone(),
                        version.clone(),
                        key,
                        req.to_string(),
                        class,
                    ),
                    (pkg_name.clone(), 0),
                );
                out.push((Pkg::Local(pkg_name), pubgrub::Ranges::full()));
            }
        }
        let rc = std::rc::Rc::new(out);
        self.deps_memo
            .borrow_mut()
            .insert((package.clone(), version.clone()), rc.clone());
        Ok(rc)
    }
}

/// Put one manifest dependency into the raw list (shared by the root and path packages;
/// registry vs path is distinguished by the last flag).
fn collect_decl<'a, S: PkgSource>(
    parent_name: &str,
    parent_version: &Version,
    d: &super::manifest::DepDecl,
    activated: &BTreeSet<(String, Version, String)>,
    provider: &CratesIo<'a, S>,
    raw: &mut Vec<RawDep>,
) -> Result<(), std::io::Error> {
    // An unactivated optional dependency is not resolved (gated by (parent package name,
    // dependency key); the platform cfg is likewise not evaluated -- union semantics)
    if d.optional
        && !activated.contains(&(
            parent_name.to_string(),
            parent_version.clone(),
            d.key.clone(),
        ))
    {
        return Ok(());
    }
    match &d.source {
        DepSource::Registry(req, reference) => {
            let source = provider
                .src
                .borrow_mut()
                .registry_source(reference)
                .map_err(|error| {
                    let mut saved = provider.source_error.borrow_mut();
                    if saved.is_none() {
                        *saved = Some(error.clone());
                    }
                    io_err(error)
                })?;
            let identity = registry_identity(&d.package, &source);
            if req_has_pre(req) {
                for c in &req.comparators {
                    if !c.pre.is_empty() {
                        provider
                            .allow_pre
                            .borrow_mut()
                            .entry(identity.clone())
                            .or_default()
                            .push((c.major, c.minor, c.patch));
                    }
                }
            }
            raw.push((
                d.key.clone(),
                identity,
                req.clone(),
                dep_unit_class(d.kind),
                true,
            ));
        }
        DepSource::Path(_) => {
            let identity = local_dep_identity(d, provider.manifests).map_err(io_err)?;
            raw.push((
                d.key.clone(),
                identity,
                VersionReq::STAR,
                dep_unit_class(d.kind),
                false,
            ));
        }
        DepSource::Git(spec) => {
            let identity = local_dep_identity(d, provider.manifests).map_err(io_err)?;
            raw.push((
                d.key.clone(),
                identity,
                spec.version.clone(),
                dep_unit_class(d.kind),
                false,
            ));
        }
    }
    Ok(())
}

impl<'a, S: PkgSource> pubgrub::DependencyProvider for CratesIo<'a, S> {
    type P = Pkg;
    type V = Version;
    type VS = pubgrub::Ranges<Version>;
    type Priority = std::cmp::Reverse<usize>;
    type M = String;
    type Err = std::io::Error;

    fn prioritize(
        &self,
        package: &Self::P,
        range: &Self::VS,
        _stats: &pubgrub::PackageResolutionStatistics,
    ) -> Self::Priority {
        let n = match package {
            Pkg::Registry(name, _) => self
                .index_entry(name)
                .map(|vs| {
                    vs.iter()
                        .filter(|v| !v.yanked && range.contains(&v.version))
                        .count()
                })
                .unwrap_or_else(|error| {
                    let mut saved = self.source_error.borrow_mut();
                    if saved.is_none() {
                        *saved = Some(error.to_string());
                    }
                    0
                }),
            _ => 1,
        };
        std::cmp::Reverse(n)
    }

    fn choose_version(
        &self,
        package: &Self::P,
        range: &Self::VS,
    ) -> Result<Option<Self::V>, Self::Err> {
        match package {
            Pkg::Root => Ok(Some(self.root.version.clone())),
            Pkg::Local(name) => Ok(self
                .manifests
                .get(name)
                .map(|m| m.version.clone())
                .or(Some(Version::new(0, 0, 0)))),
            Pkg::Registry(name, _) => {
                let vs = self.index_entry(name)?;
                let candidates: Vec<&IndexVersion> = vs
                    .iter()
                    .filter(|v| {
                        !v.yanked
                            && range.contains(&v.version)
                            && self.pre_allowed(name, &v.version)
                    })
                    .collect();
                let compatible = candidates
                    .iter()
                    .copied()
                    .filter(|version| self.rust_version_compatible(version))
                    .collect::<Vec<_>>();
                let eligible = match self.rust_version_policy {
                    IncompatibleRustVersions::Fallback if !compatible.is_empty() => compatible,
                    IncompatibleRustVersions::Allow | IncompatibleRustVersions::Fallback => {
                        candidates
                    }
                };
                let preferred = eligible
                    .into_iter()
                    .map(|version| (self.dependency_split_cost(version), version))
                    .min_by(|(left_cost, left), (right_cost, right)| {
                        left_cost
                            .cmp(right_cost)
                            .then_with(|| right.version.cmp(&left.version))
                    });
                Ok(preferred.map(|(_, version)| version.version.clone()))
            }
        }
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<pubgrub::Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        if let Pkg::Registry(name, _) = package {
            let vs = self.index_entry(name)?;
            if !vs.iter().any(|v| v.version == *version) {
                return Ok(pubgrub::Dependencies::Unavailable(format!(
                    "{name} {version} is not in the index"
                )));
            }
        }
        let rc = self.raw_deps(package, version)?;
        Ok(pubgrub::Dependencies::Available(
            rc.iter().cloned().collect(),
        ))
    }
}

fn io_err(e: impl Into<String>) -> std::io::Error {
    std::io::Error::other(e.into())
}

/// req -> Ranges conversion (mirrors the version_ranges::semver algorithm but **keeps
/// the lower bound pre**: `^0.6.0-rc.8` = [0.6.0-rc.8, 0.7.0). `from_req` drops the pre
/// and yields [0.6.0, 0.7.0), and since 0.6.0-rc.x < 0.6.0 in semver order the whole rc
/// family would be wrongly rejected. The upper bound never carries a pre, and every
/// comparator other than a pre comparator is equivalent to `from_req`).
fn req_to_ranges(req: &VersionReq) -> pubgrub::Ranges<Version> {
    use semver::Op;
    type R = pubgrub::Ranges<Version>;
    fn lo(c: &semver::Comparator) -> Version {
        Version {
            major: c.major,
            minor: c.minor.unwrap_or(0),
            patch: c.patch.unwrap_or(0),
            pre: c.pre.clone(),
            build: semver::BuildMetadata::EMPTY,
        }
    }
    fn hi(major: u64, minor: Option<u64>, patch: Option<u64>) -> Version {
        Version::new(major, minor.unwrap_or(0), patch.unwrap_or(0))
    }
    fn exact(major: u64, minor: Option<u64>, patch: Option<u64>, lo: Version) -> R {
        match (minor, patch) {
            (None, None) => R::higher_than(hi(major, Some(0), Some(0)))
                .intersection(&R::strictly_lower_than(hi(major + 1, Some(0), Some(0)))),
            (Some(m), None) => R::higher_than(hi(major, Some(m), Some(0)))
                .intersection(&R::strictly_lower_than(hi(major, Some(m + 1), Some(0)))),
            (Some(m), Some(p)) => {
                // "=M.m.p" = [M.m.p, M.m.(p+1)) with any build metadata. The semver
                // crate's Ord compares build metadata, so a pin with an empty build
                // would push out every candidate that carries one
                // (e.g. 0.18.5+1.9.4).
                R::higher_than(lo).intersection(&R::strictly_lower_than(hi(
                    major,
                    Some(m),
                    Some(p + 1),
                )))
            }
            (None, Some(_)) => unreachable!("invalid version requirement"),
        }
    }
    let mut acc = R::full();
    for c in &req.comparators {
        let (major, minor, patch) = (c.major, c.minor, c.patch);
        let lo = lo(c);
        let r =
            match c.op {
                Op::Exact => exact(major, minor, patch, lo),
                Op::Greater => match (minor, patch) {
                    (None, None) => R::higher_than(hi(major + 1, Some(0), Some(0))),
                    (Some(m), None) => R::higher_than(hi(major, Some(m + 1), Some(0))),
                    (Some(_), Some(_)) => R::strictly_higher_than(lo),
                    (None, Some(_)) => unreachable!("invalid version requirement"),
                },
                Op::GreaterEq => R::higher_than(lo),
                Op::Less => R::strictly_lower_than(lo),
                Op::LessEq => match (minor, patch) {
                    (None, None) => R::strictly_lower_than(hi(major + 1, Some(0), Some(0))),
                    (Some(m), None) => R::strictly_lower_than(hi(major, Some(m + 1), Some(0))),
                    (Some(_), Some(_)) => R::lower_than(lo),
                    (None, Some(_)) => unreachable!("invalid version requirement"),
                },
                Op::Tilde => match (minor, patch) {
                    (None, None) => exact(major, None, None, lo),
                    (Some(_), None) => exact(major, minor, None, lo),
                    (Some(m), Some(_)) => R::higher_than(lo)
                        .intersection(&R::strictly_lower_than(hi(major, Some(m + 1), Some(0)))),
                    (None, Some(_)) => unreachable!("invalid version requirement"),
                },
                Op::Caret => match (major, minor, patch) {
                    (major, Some(_), Some(_)) if major > 0 => R::higher_than(lo)
                        .intersection(&R::strictly_lower_than(hi(major + 1, Some(0), Some(0)))),
                    (0, Some(m), Some(_)) if m > 0 => R::higher_than(lo)
                        .intersection(&R::strictly_lower_than(hi(0, Some(m + 1), Some(0)))),
                    (0, Some(0), Some(_)) => exact(0, Some(0), patch, lo),
                    (major, Some(m), None) if major > 0 || m > 0 => {
                        R::higher_than(hi(major, Some(m), Some(0))).intersection(&{
                            if major > 0 {
                                R::strictly_lower_than(hi(major + 1, Some(0), Some(0)))
                            } else {
                                R::strictly_lower_than(hi(0, Some(m + 1), Some(0)))
                            }
                        })
                    }
                    (0, Some(0), None) => exact(0, Some(0), None, lo),
                    (major, None, None) => exact(major, None, None, lo),
                    _ => unreachable!("invalid version requirement"),
                },
                Op::Wildcard => match minor {
                    Some(m) => R::higher_than(hi(major, Some(m), Some(0)))
                        .intersection(&R::strictly_lower_than(hi(major, Some(m + 1), Some(0)))),
                    None => R::higher_than(hi(major, Some(0), Some(0)))
                        .intersection(&R::strictly_lower_than(hi(major + 1, Some(0), Some(0)))),
                },
                _ => {
                    // A new semver op (unknown to the version pinned in this repo): fall
                    // back to from_req, which drops the pre (noted loudly in accounting)
                    return pubgrub::Ranges::from_req(req.clone());
                }
            };
        acc = acc.intersection(&r);
    }
    acc
}

/// The output of a fresh solve.
type Solved = (
    BTreeMap<String, Vec<Version>>, // name -> version set (several versions may coexist)
    Lockfile,                       // skeleton (dependency lines are filled after convergence)
    EdgeVersions,                   // the version each dep edge points at
);

struct FreshSolveContext<'a> {
    rust_version_policy: IncompatibleRustVersions,
    resolver_rust_version: &'a Version,
    overrides: &'a SourceOverrides,
}

fn solve_fresh(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    src: &mut impl PkgSource,
    activated: &BTreeSet<(String, Version, String)>,
    preferred_exact_versions: &mut BTreeMap<String, Version>,
    context: &FreshSolveContext<'_>,
) -> Result<Solved, String> {
    let (first, discovered) = solve_fresh_pass(
        root,
        path_manifests,
        src,
        activated,
        preferred_exact_versions,
        context,
    )?;
    let before = preferred_exact_versions.clone();
    for (name, version) in discovered {
        preferred_exact_versions.entry(name).or_insert(version);
    }
    if *preferred_exact_versions == before {
        return Ok(first);
    }
    let (unified, _) = solve_fresh_pass(
        root,
        path_manifests,
        src,
        activated,
        preferred_exact_versions,
        context,
    )?;
    Ok(unified)
}

/// A single solving pass. The second return value records only the version established
/// earliest when an exact dependency splits one name into several versions, so the second
/// pass can order candidates stably.
fn solve_fresh_pass(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    src: &mut impl PkgSource,
    activated: &BTreeSet<(String, Version, String)>,
    preferred_exact_versions: &BTreeMap<String, Version>,
    context: &FreshSolveContext<'_>,
) -> Result<(Solved, BTreeMap<String, Version>), String> {
    let provider = CratesIo {
        src: std::cell::RefCell::new(src),
        manifests: path_manifests,
        overrides: context.overrides,
        root,
        rust_version_policy: context.rust_version_policy,
        resolver_rust_version: context.resolver_rust_version,
        preferred_exact_versions,
        allow_pre: std::cell::RefCell::new(BTreeMap::new()),
        activated,
        buckets: std::cell::RefCell::new(BTreeMap::new()),
        bucket_ranges: std::cell::RefCell::new(BTreeMap::new()),
        edge_assign: std::cell::RefCell::new(BTreeMap::new()),
        deps_memo: std::cell::RefCell::new(BTreeMap::new()),
        source_error: std::cell::RefCell::new(None),
    };
    let selected = match pubgrub::resolve(&provider, Pkg::Root, root.version.clone()) {
        Ok(selected) => selected,
        Err(error) => {
            if let Some(source_error) = provider.source_error.borrow().clone() {
                return Err(format!("failed to read dependency source: {source_error}"));
            }
            return Err(format!(
                "dependency resolution failed (pubgrub): {}",
                match error {
                    pubgrub::PubGrubError::NoSolution(derivation) => {
                        pubgrub::DefaultStringReporter::report(&derivation)
                    }
                    other => format!("{other}"),
                }
            ));
        }
    };
    // Solution set (bucket -> version) plus the full edge assignment
    let mut bucket_versions: BTreeMap<(String, u32), Version> = BTreeMap::new();
    for (pkg, version) in selected {
        match pkg {
            Pkg::Root => {}
            Pkg::Local(name) => {
                bucket_versions.insert((name, 0), version);
            }
            Pkg::Registry(name, bucket) => {
                bucket_versions.insert((name, bucket), version);
            }
        }
    }
    // Reachable-set filtering: pubgrub decision history can leave orphan buckets whose
    // parent was backtracked to another version. Only buckets reachable from the root
    // along edges whose parent version is exactly the selected version are kept; orphans
    // enter neither version_map nor lock nor edge_versions.
    let edge_assign = provider.edge_assign.borrow();
    if std::env::var_os("MIRVM_DEBUG_UNIFY").is_some() {
        for ((pn, pver, key, _dis, class), (pkg, bucket)) in edge_assign.iter() {
            if pkg.starts_with("yoke") {
                eprintln!("DBG-ASSIGN ({pn}@{pver}, {key}, {class:?}) -> {pkg}#{bucket}");
            }
        }
    }
    let mut reachable: BTreeSet<(String, u32)> = BTreeSet::new();
    let mut visited: BTreeSet<(String, Version)> = BTreeSet::new();
    let mut queue: VecDeque<(String, Version)> =
        VecDeque::from([(root.name.clone(), root.version.clone())]);
    while let Some(pv) = queue.pop_front() {
        if !visited.insert(pv.clone()) {
            continue;
        }
        for ((pn, pver, _key, _dis, _class), (pkg, bucket)) in edge_assign.iter() {
            if pn == &pv.0 && pver == &pv.1 {
                let child = (pkg.clone(), *bucket);
                if reachable.insert(child.clone())
                    && let Some(cv) = bucket_versions.get(&child)
                {
                    queue.push_back((pkg.clone(), cv.clone()));
                }
            }
        }
    }
    let mut version_map: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    let mut lock = Lockfile {
        // Cargo writes v4 by default since 1.83, but keeps writing v3 -- readable by old
        // toolchains -- for projects with `rust-version <= 1.82`. The workspace's lowest
        // MSRV is used here, matching the baseline of resolver 3 candidate preference;
        // when it is undeclared the baseline is the current rustc.
        format_version: if context.resolver_rust_version >= &Version::new(1, 83, 0) {
            4
        } else {
            3
        },
        packages: vec![LockedPkg {
            name: root.name.clone(),
            version: root.version.clone(),
            source: None,
            checksum: None,
            replace: None,
            dependencies: vec![],
        }],
        unused_patches: vec![],
    };
    for ((name, bucket), version) in &bucket_versions {
        if !reachable.contains(&(name.clone(), *bucket)) {
            continue;
        }
        let selected = context.overrides.selected_identity(name, version);
        let package_name = local_package_name(&selected, path_manifests);
        version_map
            .entry(package_name.clone())
            .or_default()
            .push(version.clone());
        let (source, checksum) = if let Some(manifest) = path_manifests.get(&selected) {
            (manifest.lock_source.clone(), None)
        } else {
            let cksum = registry_entry(&mut **provider.src.borrow_mut(), &selected)
                .ok()
                .and_then(|vs| {
                    vs.iter()
                        .find(|v| v.version == *version)
                        .map(|v| v.cksum.clone())
                });
            (
                Some(
                    identity_source(&selected)
                        .unwrap_or(CRATES_IO_LOCK_SOURCE)
                        .to_string(),
                ),
                cksum,
            )
        };
        lock.packages.push(LockedPkg {
            name: package_name,
            version: version.clone(),
            source,
            checksum,
            replace: None,
            dependencies: vec![],
        });
        if context
            .overrides
            .replacements
            .get(&(name.clone(), version.clone()))
            .is_some_and(|replacement| replacement == &selected)
        {
            let original = registry_entry(&mut **provider.src.borrow_mut(), name)?;
            let checksum = original
                .iter()
                .find(|candidate| candidate.version == *version)
                .map(|candidate| candidate.cksum.clone())
                .ok_or_else(|| {
                    format!(
                        "[replace] original package {} {version} is not in the registry index",
                        identity_package_name(name)
                    )
                })?;
            lock.packages.push(LockedPkg {
                name: identity_package_name(name).to_string(),
                version: version.clone(),
                source: Some(
                    identity_source(name)
                        .unwrap_or(CRATES_IO_LOCK_SOURCE)
                        .to_string(),
                ),
                checksum: Some(checksum),
                replace: Some(format!("{} {version}", identity_package_name(name))),
                dependencies: vec![],
            });
        }
    }
    for vs in version_map.values_mut() {
        vs.sort();
        vs.dedup();
    }
    // Resolve the edge assignment into concrete versions: ((parent name, parent version,
    // dependency key, disambiguator, class) -> (package, version)). Only edges with a
    // reachable parent and a reachable child are kept; the rest are by-products of
    // backtracking/pruning.
    let mut edge_versions: EdgeVersions = BTreeMap::new();
    for ((pname, pver, key, dis, class), (pkg, bucket)) in edge_assign.iter() {
        if !visited.contains(&(pname.clone(), pver.clone())) {
            continue;
        }
        let Some(v) = bucket_versions.get(&(pkg.clone(), *bucket)) else {
            continue;
        };
        if !reachable.contains(&(pkg.clone(), *bucket)) {
            continue;
        }
        let parent_identity = context.overrides.selected_identity(pname, pver);
        let child_identity = context.overrides.selected_identity(pkg, v);
        edge_versions.insert(
            (
                parent_identity,
                pver.clone(),
                key.clone(),
                dis.clone(),
                *class,
            ),
            (child_identity, v.clone()),
        );
    }
    let mut exact_duplicate_names = BTreeSet::new();
    let compatible_duplicate_names = version_map
        .iter()
        .filter(|(_, versions)| {
            versions.iter().enumerate().any(|(index, version)| {
                versions[index + 1..]
                    .iter()
                    .any(|other| cargo_versions_compatible(version, other))
            })
        })
        .map(|(name, _)| name.as_str())
        .collect::<BTreeSet<_>>();
    for ((_pname, _pver, _key, dis, _class), (pkg, bucket)) in edge_assign.iter() {
        if compatible_duplicate_names.contains(identity_package_name(pkg))
            && reachable.contains(&(pkg.clone(), *bucket))
            && VersionReq::parse(dis).is_ok_and(|req| req_is_full_exact(&req))
        {
            exact_duplicate_names.insert(pkg.clone());
        }
    }
    let mut next_preferences = BTreeMap::new();
    for name in exact_duplicate_names {
        if let Some((_, version)) = bucket_versions
            .iter()
            .filter(|((package, bucket), _)| {
                package == &name && reachable.contains(&(package.clone(), *bucket))
            })
            .min_by_key(|((_, bucket), _)| *bucket)
        {
            next_preferences.insert(identity_package_name(&name).to_string(), version.clone());
        }
    }
    // Lock dependency lines are filled in only after feature unification converges (see
    // the iteration loop in resolve()): the optional gate is judged per (parent package,
    // dependency key) and needs the unification output.
    Ok(((version_map, lock, edge_versions), next_preferences))
}

fn fill_unused_patches(
    lock: &mut Lockfile,
    manifests: &BTreeMap<String, PackageManifest>,
    src: &mut impl PkgSource,
    overrides: &SourceOverrides,
) -> Result<(), String> {
    for ((_logical, version), selected) in &overrides.patches {
        let (name, source, checksum) = if let Some(manifest) = manifests.get(selected) {
            (manifest.name.clone(), manifest.lock_source.clone(), None)
        } else {
            let name = identity_package_name(selected).to_string();
            let source = identity_source(selected).map(str::to_string);
            let checksum = registry_entry(src, selected)?
                .iter()
                .find(|candidate| candidate.version == *version)
                .map(|candidate| candidate.cksum.clone())
                .filter(|checksum| !checksum.is_empty());
            (name, source, checksum)
        };
        let used = lock.packages.iter().any(|package| {
            package.name == name && package.version == *version && package.source == source
        });
        if !used
            && !lock.unused_patches.iter().any(|patch| {
                patch.name == name && patch.version == *version && patch.source == source
            })
        {
            lock.unused_patches.push(UnusedPatch {
                name,
                version: version.clone(),
                source,
                checksum,
            });
        }
    }
    Ok(())
}

/// Fill the `dependencies` lines of the generated lock (appending version/source
/// disambiguation as needed); an unactivated optional dependency is not listed (cargo
/// lock semantics).
fn fill_lock_dependency_lines(
    lock: &mut Lockfile,
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    edge_versions: &EdgeVersions,
    nodes: &BTreeMap<NodeKey, FeatNode>,
    src: &mut impl PkgSource,
    overrides: &SourceOverrides,
) -> Result<(), String> {
    // The optional gate is judged per (parent package, parent version, dependency key):
    // a global set would misattribute a dependency activated by package A to package B
    // (cipher/zeroize vs generic-array). A weak reference (?/) is admitted as well (cargo
    // semantics: the yoke/serde?/alloc cascade).
    let activated_keys = |name: &str, version: &Version| -> BTreeSet<&str> {
        [UnitClass::Normal, UnitClass::Build]
            .iter()
            .filter_map(|c| nodes.get(&(name.to_string(), version.clone(), *c)))
            .flat_map(|n| {
                n.activated
                    .iter()
                    .chain(n.weak_refs.iter())
                    .map(|k| k.as_str())
            })
            .collect()
    };
    type LockIdentity = (String, Version, Option<String>);
    type EdgeLines = BTreeMap<LockIdentity, Vec<LockedDep>>;
    let mut edges: EdgeLines = BTreeMap::new();
    let line_for = |parent: &NodeKey,
                    key: &str,
                    pkg_name: &str,
                    req: Option<&VersionReq>,
                    source_id: Option<String>,
                    class: UnitClass|
     -> Result<LockedDep, String> {
        let dep = FeatDep {
            key: key.to_string(),
            package: pkg_name.to_string(),
            class,
            kind: match class {
                UnitClass::Normal => DepKind::Normal,
                UnitClass::Build => DepKind::Build,
            },
            optional: false,
            default_features: false,
            features: vec![],
            registry: true,
            platform_cfg: None,
            req: req.map(|r| r.to_string()),
            source_id,
        };
        let (child_identity, child_version) = edge_version(edge_versions, parent, &dep)
            .ok_or_else(|| {
                format!(
                    "{}@{} has no exact lock edge for dependency {key}",
                    parent.0, parent.1
                )
            })?;
        let replacement_original =
            overrides
                .replacements
                .iter()
                .find_map(|((original, version), replacement)| {
                    (replacement == child_identity && version == child_version).then_some(original)
                });
        let (child_name, child_source) = if let Some(original) = replacement_original {
            (
                identity_package_name(original).to_string(),
                identity_source(original).map(str::to_string),
            )
        } else {
            match path_manifests.get(child_identity) {
                Some(manifest) => (manifest.name.clone(), manifest.lock_source.clone()),
                None => (
                    identity_package_name(child_identity).to_string(),
                    Some(
                        identity_source(child_identity)
                            .unwrap_or(CRATES_IO_LOCK_SOURCE)
                            .to_string(),
                    ),
                ),
            }
        };
        let same_name: Vec<&LockedPkg> = lock
            .packages
            .iter()
            .filter(|package| package.name == child_name)
            .collect();
        if same_name.len() == 1 {
            return Ok(LockedDep {
                name: child_name,
                version: None,
                source: None,
            });
        }
        let same_version: Vec<&LockedPkg> = same_name
            .iter()
            .copied()
            .filter(|package| package.version == *child_version)
            .collect();
        if same_version.len() == 1 {
            return Ok(LockedDep {
                name: child_name,
                version: Some(child_version.clone()),
                source: None,
            });
        }
        let source = child_source
            .as_deref()
            .map(lock_dependency_source)
            .ok_or_else(|| {
                format!(
                    "the path package {} {} in the lock has no source to disambiguate by",
                    child_name, child_version
                )
            })?;
        Ok(LockedDep {
            name: child_name,
            version: Some(child_version.clone()),
            source: Some(source),
        })
    };
    edges.insert(
        (root.name.clone(), root.version.clone(), None),
        root.deps
            .iter()
            .filter(|d| {
                !d.optional || activated_keys(&root.name, &root.version).contains(&d.key.as_str())
            })
            .map(|d| -> Result<LockedDep, String> {
                let class = dep_unit_class(d.kind);
                let req = match &d.source {
                    DepSource::Registry(r, _) => Some(r),
                    DepSource::Git(spec) => Some(&spec.version),
                    DepSource::Path(_) => None,
                };
                let parent = (root.name.clone(), root.version.clone(), class);
                let source_id = match &d.source {
                    DepSource::Git(spec) => Some(spec.source_id()),
                    DepSource::Registry(..) | DepSource::Path(_) => None,
                };
                line_for(&parent, &d.key, &d.package, req, source_id, class)
            })
            .collect::<Result<Vec<_>, _>>()?,
    );
    for (identity, m) in path_manifests {
        edges.insert(
            (m.name.clone(), m.version.clone(), m.lock_source.clone()),
            m.deps
                .iter()
                .filter(|d| d.kind != DepKind::Dev)
                .filter(|d| {
                    !d.optional || activated_keys(identity, &m.version).contains(&d.key.as_str())
                })
                .map(|d| -> Result<LockedDep, String> {
                    let class = dep_unit_class(d.kind);
                    let req = match &d.source {
                        DepSource::Registry(r, _) => Some(r),
                        DepSource::Git(spec) => Some(&spec.version),
                        DepSource::Path(_) => None,
                    };
                    let parent = (identity.clone(), m.version.clone(), class);
                    let source_id = match &d.source {
                        DepSource::Git(spec) => Some(spec.source_id()),
                        DepSource::Registry(..) | DepSource::Path(_) => None,
                    };
                    line_for(&parent, &d.key, &d.package, req, source_id, class)
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    let registry_packages: Vec<(String, Version, String)> = lock
        .packages
        .iter()
        .filter_map(|package| {
            if package.replace.is_some() {
                return None;
            }
            let source = package.source.as_deref()?;
            (source.starts_with("registry+") || source.starts_with("sparse+")).then(|| {
                (
                    package.name.clone(),
                    package.version.clone(),
                    source.to_string(),
                )
            })
        })
        .collect();
    for (name, version, source) in registry_packages {
        if name == root.name {
            continue;
        }
        let identity = registry_identity(&name, &source);
        let vs = registry_entry(src, &identity)?;
        let iv = vs
            .iter()
            .find(|v| v.version == version)
            .ok_or_else(|| format!("{name} {version} is not in the index"))?;
        edges.insert(
            (name.clone(), version.clone(), Some(source)),
            iv.deps
                .iter()
                .filter(|d| d.kind.as_deref() != Some("dev"))
                .filter(|d| {
                    !d.optional || activated_keys(&identity, &version).contains(&d.name.as_str())
                })
                .map(|d| -> Result<LockedDep, String> {
                    let pkg_name = d.package.clone().unwrap_or_else(|| d.name.clone());
                    let class = if d.kind.as_deref() == Some("build") {
                        UnitClass::Build
                    } else {
                        UnitClass::Normal
                    };
                    let parent = (identity.clone(), version.clone(), class);
                    line_for(&parent, &d.name, &pkg_name, Some(&d.req), None, class)
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    for pkg in lock.packages.iter_mut() {
        let mut lines: Vec<LockedDep> = edges
            .get(&(pkg.name.clone(), pkg.version.clone(), pkg.source.clone()))
            .cloned()
            .unwrap_or_default();
        lines.sort();
        lines.dedup();
        pkg.dependencies = lines;
    }
    Ok(())
}

fn lock_dependency_source(source: &str) -> String {
    if source.starts_with("git+") {
        source.rsplit_once('#').map(|(id, _)| id).unwrap_or(source)
    } else {
        source
    }
    .to_string()
}

// ---------- feature unification ----------

#[derive(Clone, Debug, Default)]
struct FeatNode {
    features: BTreeSet<String>,
    /// Optional dependency keys activated inside this package.
    activated: BTreeSet<String>,
    /// Dependency keys weakly referenced (?/) by an enabled feature (not activated, but
    /// they enter resolution and the lock lines).
    weak_refs: BTreeSet<String>,
}

/// One dependency edge that takes part in feature propagation.
/// platform_cfg is the platform cfg expression: **it is not evaluated while parsing or
/// unifying (union over all platforms); it is evaluated against the host only when the
/// build graph is assembled (`assemble_units`)** -- cargo's split between lock semantics
/// and the build graph.
#[derive(Clone, Debug)]
struct FeatDep {
    key: String,
    package: String,
    class: UnitClass,
    kind: DepKind,
    optional: bool,
    default_features: bool,
    features: Vec<String>,
    registry: bool,
    platform_cfg: Option<String>,
    /// req string (the edge disambiguator for entries with several reqs for one name;
    /// `None` for a path dependency)
    req: Option<String>,
    /// Git manifest source id (without the precise commit), so lock mode can tell apart
    /// same-name same-version sources.
    source_id: Option<String>,
}

type FeatTable = BTreeMap<String, Vec<FeatureValue>>;
/// Unification node key: (package, version, class) -- with several versions in play the
/// feature table and edges are distinguished by exact version.
type NodeKey = (String, Version, UnitClass);
type NodeTables = BTreeMap<NodeKey, (FeatTable, Vec<FeatDep>)>;

/// Edge version lookup: an exact class hit first, falling back to the other class on a
/// miss (lock mode registers under both keys, fresh mode registers exactly by kind; the
/// kind from the root and path manifests is always exact).
fn edge_version<'a>(
    ev: &'a EdgeVersions,
    parent: &NodeKey,
    dep: &FeatDep,
) -> Option<&'a (String, Version)> {
    let other = match dep.class {
        UnitClass::Normal => UnitClass::Build,
        UnitClass::Build => UnitClass::Normal,
    };
    // fresh mode: an exact key on the req string (entries with several reqs for one name
    // stay separate by string)
    if let Some(req) = &dep.req {
        if let Some(hit) = ev.get(&(
            parent.0.clone(),
            parent.1.clone(),
            dep.key.clone(),
            req.clone(),
            dep.class,
        )) {
            return Some(hit);
        }
        if let Some(hit) = ev.get(&(
            parent.0.clone(),
            parent.1.clone(),
            dep.key.clone(),
            req.clone(),
            other,
        )) {
            return Some(hit);
        }
    }
    // lock mode: a scan hit -- a child version (the disambiguator string) falling inside
    // the req range matches; the name is matched both as the dependency key and as the
    // real package name (rename and underscore keys: rustix libc_errno -> libc-errno,
    // grep-searcher memmap -> memmap2)
    let range = dep
        .req
        .as_deref()
        .unwrap_or("*")
        .parse::<VersionReq>()
        .ok()
        .map(|r| req_to_ranges(&r));
    ev.iter()
        .find(|((pn, pv, k, dis, c), (_, dv))| {
            pn == &parent.0
                && pv == &parent.1
                && (k == &dep.key || k == &dep.package)
                && (*c == dep.class || *c == other)
                && range.as_ref().is_none_or(|r| r.contains(dv))
                && dep
                    .source_id
                    .as_ref()
                    .is_none_or(|source_id| dis.starts_with(&format!("{source_id}#")))
        })
        .map(|(_, v)| v)
}

fn register_node(
    tables: &mut NodeTables,
    nodes: &mut BTreeMap<NodeKey, FeatNode>,
    key: NodeKey,
    table: FeatTable,
    deps: Vec<FeatDep>,
) {
    tables.insert(key.clone(), (table, deps));
    nodes.entry(key).or_default();
}

fn ensure_registry_node(
    tables: &mut NodeTables,
    nodes: &mut BTreeMap<NodeKey, FeatNode>,
    src: &mut impl PkgSource,
    name: &str,
    version: &Version,
    class: UnitClass,
) -> Result<(), String> {
    if tables.contains_key(&(name.to_string(), version.clone(), class)) {
        return Ok(());
    }
    let vs = registry_entry(src, name)?;
    let iv = vs
        .iter()
        .find(|v| v.version == *version)
        .ok_or_else(|| format!("{name} {version} is not in the index"))?;
    let mut table = FeatTable::new();
    for (f, vals) in &iv.features {
        let parsed = vals
            .iter()
            .map(|v| parse_feature_value(v))
            .collect::<Result<Vec<_>, _>>()?;
        table.insert(f.clone(), parsed);
    }
    let mut deps = Vec::new();
    for d in &iv.deps {
        if d.kind.as_deref() == Some("dev") {
            continue;
        }
        // The platform cfg is not evaluated here (union semantics; the target expression
        // travels with the row and is evaluated at assembly time)
        deps.push(FeatDep {
            key: d.name.clone(),
            package: d.package.clone().unwrap_or_else(|| d.name.clone()),
            class: if d.kind.as_deref() == Some("build") {
                UnitClass::Build
            } else {
                UnitClass::Normal
            },
            kind: if d.kind.as_deref() == Some("build") {
                DepKind::Build
            } else {
                DepKind::Normal
            },
            optional: d.optional,
            default_features: d.default_features,
            features: d.features.clone(),
            registry: true,
            platform_cfg: d.target.clone(),
            req: Some(d.req.to_string()),
            source_id: None,
        });
    }
    register_node(
        tables,
        nodes,
        (name.to_string(), version.clone(), class),
        table,
        deps,
    );
    if std::env::var_os("MIRVM_DEBUG_UNIFY").is_some() {
        eprintln!("DBG-UNIFY register {name} {class:?} v{version}");
    }
    Ok(())
}

/// The product of feature unification: (node table, the set of optional dependencies
/// activated by (parent package name, dependency key)).
type Unified = (
    BTreeMap<NodeKey, FeatNode>,
    BTreeSet<(String, Version, String)>,
);

fn workspace_feature_seed<'a>(
    key: &NodeKey,
    manifests: &BTreeMap<String, PackageManifest>,
    overrides: &'a FeatureOverrides,
) -> Option<&'a BTreeSet<String>> {
    overrides.get(key).or_else(|| {
        let manifest = manifests.get(&key.0)?;
        overrides.get(&(manifest.name.clone(), key.1.clone(), key.2))
    })
}

fn unify_features(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    edge_versions: &EdgeVersions,
    src: &mut impl PkgSource,
    include_weak: bool,
    include_root_dev: bool,
    workspace_features: &FeatureOverrides,
) -> Result<Unified, String> {
    let mut tables: NodeTables = BTreeMap::new();
    let mut nodes: BTreeMap<NodeKey, FeatNode> = BTreeMap::new();

    register_node(
        &mut tables,
        &mut nodes,
        (root.name.clone(), root.version.clone(), UnitClass::Normal),
        root.features.clone(),
        root_featdeps(&root.deps, include_root_dev)?,
    );
    // Root features come from the CLI selection; `default` is enabled only when
    // --no-default-features is absent.
    let root_node = nodes
        .get_mut(&(root.name.clone(), root.version.clone(), UnitClass::Normal))
        .unwrap();
    if root.default_features_enabled && root.features.contains_key("default") {
        root_node.features.insert("default".to_string());
    }
    root_node
        .features
        .extend(root.requested_features.iter().cloned());
    root_node
        .activated
        .extend(root.dependency_features.keys().cloned());
    for (name, m) in path_manifests {
        let deps = decls_to_featdeps(&m.deps)?;
        for class in [UnitClass::Normal, UnitClass::Build] {
            register_node(
                &mut tables,
                &mut nodes,
                (name.clone(), m.version.clone(), class),
                m.features.clone(),
                deps.clone(),
            );
            if class == UnitClass::Normal {
                nodes
                    .get_mut(&(name.clone(), m.version.clone(), class))
                    .unwrap()
                    .activated
                    .extend(m.dependency_features.keys().cloned());
            }
        }
    }
    for key in nodes.keys().cloned().collect::<Vec<_>>() {
        if let Some(features) = workspace_feature_seed(&key, path_manifests, workspace_features)
            && let Some(node) = nodes.get_mut(&key)
        {
            node.features.extend(features.iter().cloned());
        }
    }

    // Global fixed-point iteration (the node and edge counts are bounded and convergence
    // is monotone)
    for _pass in 0..64 {
        let mut changed = false;
        let keys: Vec<NodeKey> = nodes.keys().cloned().collect();
        for key in keys {
            let Some((table, deps)) = tables.get(&key).cloned() else {
                continue;
            };
            let node = nodes.get(&key).cloned().unwrap_or_default();
            let (features, activated, mut edge_adds, weak_refs) =
                expand_node(&table, &deps, &node)?;
            let cli_dependency_features =
                if key.0 == root.name && key.1 == root.version && key.2 == UnitClass::Normal {
                    Some(&root.dependency_features)
                } else if key.2 == UnitClass::Normal {
                    path_manifests
                        .get(&key.0)
                        .filter(|manifest| manifest.version == key.1)
                        .map(|manifest| &manifest.dependency_features)
                } else {
                    None
                };
            if let Some(cli_dependency_features) = cli_dependency_features {
                for (dep, requested) in cli_dependency_features {
                    edge_adds
                        .entry(dep.clone())
                        .or_default()
                        .extend(requested.iter().cloned());
                }
            }
            // A change in any of the three outputs must be recorded -- dropping a weak_ref
            // insert would silently lose a ?/ weak reference.
            if features != node.features
                || activated != node.activated
                || weak_refs != node.weak_refs
            {
                nodes.insert(
                    key.clone(),
                    FeatNode {
                        features: features.clone(),
                        activated: activated.clone(),
                        weak_refs: weak_refs.clone(),
                    },
                );
                changed = true;
            }
            // Edge propagation
            if std::env::var_os("MIRVM_DEBUG_UNIFY").is_some() {
                eprintln!(
                    "DBG-UNIFY expand {}@{} {:?} features={:?} activated={:?} deps={:?}",
                    key.0,
                    key.1,
                    key.2,
                    features,
                    activated,
                    deps.iter().map(|d| d.key.clone()).collect::<Vec<_>>()
                );
            }
            for dep in &deps {
                if dep.optional
                    && !activated.contains(&dep.key)
                    && !(include_weak && weak_refs.contains(&dep.key))
                {
                    continue;
                }
                // Child identity comes from the edge assignment record (with several
                // versions in play, (parent, key, class) pinpoints the version)
                let Some((child_name, child_version)) = edge_version(edge_versions, &key, dep)
                else {
                    // An optional dependency newly activated this round only receives an
                    // edge assignment in the next solve, so skip it now; the activation is
                    // already recorded in activated_pkgs and the fixed point will fill it in.
                    if dep.optional {
                        continue;
                    }
                    return Err(format!(
                        "dependency {} of {}@{} has no edge assignment record (internal inconsistency)",
                        dep.key, key.0, key.1
                    ));
                };
                let child_key = (child_name.clone(), child_version.clone(), dep.class);
                // Register a registry child node on demand
                if dep.registry && !tables.contains_key(&child_key) {
                    ensure_registry_node(
                        &mut tables,
                        &mut nodes,
                        src,
                        child_name,
                        child_version,
                        dep.class,
                    )?;
                    // Registering a new node is itself a change; otherwise the loop would
                    // converge early when adds is empty and its subgraph would never be
                    // expanded.
                    changed = true;
                }
                if let Some(seed) =
                    workspace_feature_seed(&child_key, path_manifests, workspace_features)
                {
                    let child = nodes.entry(child_key.clone()).or_default();
                    let before = child.features.len();
                    child.features.extend(seed.iter().cloned());
                    if child.features.len() != before {
                        changed = true;
                    }
                }
                let mut adds: BTreeSet<String> = dep.features.iter().cloned().collect();
                if dep.default_features {
                    match tables.get(&child_key) {
                        Some((ctable, _)) => {
                            if ctable.contains_key("default") {
                                adds.insert("default".to_string());
                            }
                        }
                        None => {
                            adds.insert("default".to_string());
                        }
                    }
                }
                if let Some(extra) = edge_adds.get(&dep.key) {
                    adds.extend(extra.iter().cloned());
                }
                let child = nodes.entry(child_key).or_default();
                let before = child.features.len();
                child.features.extend(adds);
                if child.features.len() != before {
                    changed = true;
                }
            }
        }
        if root.resolver == ResolverVersion::V1 {
            changed |= unify_resolver_one_classes(&mut nodes);
        }
        if !changed {
            // After convergence: the optional dependency set activated by (parent package
            // name, dependency key) plus the weak-reference set (the fixed-point input;
            // a weak reference likewise enters resolution and the lock lines, cargo
            // semantics)
            let mut activated_parents: BTreeSet<(String, Version, String)> = BTreeSet::new();
            for (key, node) in &nodes {
                for dep_key in &node.activated {
                    activated_parents.insert((key.0.clone(), key.1.clone(), dep_key.clone()));
                }
                for dep_key in &node.weak_refs {
                    activated_parents.insert((key.0.clone(), key.1.clone(), dep_key.clone()));
                }
            }
            return Ok((nodes, activated_parents));
        }
    }
    Err("feature unification did not converge in 64 rounds (abnormal graph)".to_string())
}

/// Resolver 1 unifies features across dependency uses. Compiled artifacts are still kept
/// apart by Normal/Build (host and target must not be mixed), but the three sets that
/// determine `cfg(feature)` and optional dependencies must be identical, and propagation
/// continues on the next round along each side's own dependency graph.
fn unify_resolver_one_classes(nodes: &mut BTreeMap<NodeKey, FeatNode>) -> bool {
    let mut unified: BTreeMap<(String, Version), FeatNode> = BTreeMap::new();
    for ((name, version, _), node) in nodes.iter() {
        let combined = unified.entry((name.clone(), version.clone())).or_default();
        combined.features.extend(node.features.iter().cloned());
        combined.activated.extend(node.activated.iter().cloned());
        combined.weak_refs.extend(node.weak_refs.iter().cloned());
    }
    let mut changed = false;
    for ((name, version, _), node) in nodes.iter_mut() {
        let combined = &unified[&(name.clone(), version.clone())];
        if node.features != combined.features
            || node.activated != combined.activated
            || node.weak_refs != combined.weak_refs
        {
            *node = combined.clone();
            changed = true;
        }
    }
    changed
}

fn decls_to_featdeps(decls: &[super::manifest::DepDecl]) -> Result<Vec<FeatDep>, String> {
    featdeps(decls, false)
}

fn root_featdeps(
    decls: &[super::manifest::DepDecl],
    include_dev: bool,
) -> Result<Vec<FeatDep>, String> {
    featdeps(decls, include_dev)
}

fn featdeps(decls: &[super::manifest::DepDecl], include_dev: bool) -> Result<Vec<FeatDep>, String> {
    Ok(decls
        .iter()
        .filter(|d| d.kind != DepKind::Dev || include_dev)
        .map(|d| FeatDep {
            key: d.key.clone(),
            package: d.package.clone(),
            class: dep_unit_class(d.kind),
            kind: d.kind,
            optional: d.optional,
            default_features: d.default_features,
            features: d.features.clone(),
            registry: matches!(d.source, DepSource::Registry(..)),
            platform_cfg: d.platform_cfg.clone(),
            req: match &d.source {
                DepSource::Registry(req, _) => Some(req.to_string()),
                DepSource::Git(spec) => Some(spec.version.to_string()),
                DepSource::Path(_) => None,
            },
            source_id: match &d.source {
                DepSource::Git(spec) => Some(spec.source_id()),
                DepSource::Registry(..) | DepSource::Path(_) => None,
            },
        })
        .collect())
}

/// Feature expansion inside one package (implicit/explicit activation and the strong and
/// weak edge rules).
/// Returns (features, activated, edge_adds[key -> features], weak_refs[the dependency
/// keys weakly referenced by an enabled feature via ?/]). A weak_ref does not activate the
/// dependency, but that dependency enters version resolution and the lock dependency lines
/// (cargo semantics: a ?/ weak reference from an enabled feature pulls the referenced
/// package into the resolve graph and the lock lines).
type Expanded = (
    BTreeSet<String>,
    BTreeSet<String>,
    BTreeMap<String, BTreeSet<String>>,
    BTreeSet<String>,
);

fn expand_node(
    table: &BTreeMap<String, Vec<FeatureValue>>,
    deps: &[FeatDep],
    node: &FeatNode,
) -> Result<Expanded, String> {
    // Implicit feature rule: an optional dependency key named by any dep: no longer
    // produces an implicit feature
    let hidden: BTreeSet<String> = table
        .values()
        .flatten()
        .filter_map(|v| match v {
            FeatureValue::DepActivation(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    let optional_keys: BTreeSet<String> = deps
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.key.clone())
        .collect();

    if let Some(feature) = node
        .features
        .iter()
        .find(|feature| !table.contains_key(*feature) && !optional_keys.contains(*feature))
    {
        return Err(format!(
            "requested a feature that does not exist: `{feature}`"
        ));
    }
    if let Some(feature) = node.features.iter().find(|feature| {
        !table.contains_key(*feature)
            && optional_keys.contains(*feature)
            && hidden.contains(*feature)
    }) {
        return Err(format!(
            "feature `{feature}` is hidden by dep:{feature} and cannot be enabled as an implicit feature"
        ));
    }

    let mut features = node.features.clone();
    let mut activated = node.activated.clone();
    let mut edge_adds: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut weak_refs: BTreeSet<String> = BTreeSet::new();
    for _ in 0..64 {
        let mut changed = false;
        let snapshot: Vec<String> = features.iter().cloned().collect();
        for f in snapshot {
            let Some(vals) = table.get(&f) else {
                continue;
            };
            for val in vals {
                match val {
                    FeatureValue::Simple(g) => {
                        if table.contains_key(g) {
                            if features.insert(g.clone()) {
                                changed = true;
                            }
                        } else if optional_keys.contains(g) && !hidden.contains(g) {
                            if activated.insert(g.clone()) {
                                changed = true;
                            }
                            // Referencing an implicit feature also turns on the feature
                            // flag of the same name (as in cargo: the serde facade's
                            // `#[cfg(feature = "serde_derive")]`; the explicit `dep:` form
                            // only activates the dependency and sets no flag)
                            if features.insert(g.clone()) {
                                changed = true;
                            }
                        } else {
                            // Same semantics as cargo: a feature reference must point at
                            // another feature or an optional dependency
                            return Err(format!(
                                "feature reference {g} is neither a feature nor an optional dependency (invalid manifest/index data)"
                            ));
                        }
                    }
                    FeatureValue::DepActivation(d) => {
                        if activated.insert(d.clone()) {
                            changed = true;
                        }
                    }
                    FeatureValue::StrongDep { dep, feature } => {
                        if activated.insert(dep.clone()) {
                            changed = true;
                        }
                        // A strong x/y activates the optional dependency and also turns on
                        // the feature flag of the same name:
                        // - when x has an explicit feature definition (the table has the
                        //   key), that explicit feature is enabled even if x is shadowed by
                        //   dep: -- shadowing kills only the implicit feature, not an
                        //   explicit definition (e.g. `serde = [dep:litemap, litemap/serde]`
                        //   with `litemap = [dep:litemap, alloc]` present makes cargo set
                        //   cfg(feature = "litemap") and pull in the
                        //   `#[cfg(feature = "litemap")]` impl block holding
                        //   try_from_serde_litemap);
                        // - with no explicit definition and no dep: shadowing, this is the
                        //   same as naming its implicit feature (as in cargo: k256's
                        //   ecdsa-core/signing sets `#[cfg(feature = "ecdsa-core")]`).
                        if optional_keys.contains(dep)
                            && (table.contains_key(dep) || !hidden.contains(dep))
                            && features.insert(dep.clone())
                        {
                            changed = true;
                        }
                        edge_adds
                            .entry(dep.clone())
                            .or_default()
                            .insert(feature.clone());
                    }
                    FeatureValue::WeakDep { dep, feature } => {
                        // A weak reference (?/): the containing feature being enabled puts
                        // the referenced dependency into the resolve graph (without
                        // activating it) and its features are propagated as usual (the
                        // rust_decimal std -> borsh?/std -> bytes?/std cascade -- cargo's
                        // resolve-graph semantics, unlike the build graph's
                        // "propagate only when activated")
                        weak_refs.insert(dep.clone());
                        edge_adds
                            .entry(dep.clone())
                            .or_default()
                            .insert(feature.clone());
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Final sweep: once a feature flag arrives from an edge or a table, if it is not
    // itself a table key but an unshadowed optional dependency key, activate that
    // dependency (e.g. rand_core's getrandom: the flag arrives along an edge with no table
    // entry to expand, and without this rule the activation is lost).
    for f in features.iter() {
        if !table.contains_key(f) && optional_keys.contains(f) && !hidden.contains(f) {
            activated.insert(f.clone());
        }
    }
    Ok((features, activated, edge_adds, weak_refs))
}

// ---------- unit assembly ----------

/// Minimal manifest read of a registry package (lib name/path/proc-macro/links/build.rs
/// presence/edition/CARGO_PKG_* inputs; no full subset parse, because registry crate
/// manifests make no promises. The Cargo.toml inside a .crate is cargo's normalized
/// output, so inherited keys such as edition already hold concrete values).
struct RegistryMinimal {
    lib_name: String,
    proc_macro: bool,
    links: Option<String>,
    has_build: bool,
    build_script_path: Option<PathBuf>,
    edition: String,
    lib_path: PathBuf,
    pkg_env: BTreeMap<String, String>,
    declared_features: BTreeSet<String>,
    rustc_lint_flags: Vec<String>,
}

fn read_registry_minimal(
    dir: &Path,
    package: &str,
    version: &Version,
) -> Result<RegistryMinimal, String> {
    let file = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
    let v: toml::Value =
        toml::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", file.display()))?;
    let pkg_table = v.get("package");
    let lib = v.get("lib");
    let name = lib
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| package.replace('-', "_"));
    let proc_macro = lib
        .and_then(|l| l.get("proc-macro"))
        .or_else(|| lib.and_then(|l| l.get("proc_macro"))) // the newer normalized underscore form
        .and_then(|p| p.as_bool())
        .unwrap_or(false);
    let lib_path = dir.join(
        lib.and_then(|l| l.get("path"))
            .and_then(|p| p.as_str())
            .unwrap_or("src/lib.rs"),
    );
    let links = pkg_table
        .and_then(|p| p.get("links"))
        .and_then(|l| l.as_str())
        .map(str::to_string);
    // cargo semantics: `build = false` disables it explicitly (the key being present is
    // not the same as having build.rs)
    let has_build = links.is_some()
        || match pkg_table.and_then(|p| p.get("build")) {
            Some(toml::Value::Boolean(false)) => false,
            Some(_) => true,
            None => dir.join("build.rs").is_file(),
        };
    let build_script_path = match pkg_table.and_then(|p| p.get("build")) {
        Some(toml::Value::String(s)) => Some(dir.join(s)),
        _ => None,
    };
    let edition = pkg_table
        .and_then(|p| p.get("edition"))
        .and_then(|e| e.as_str())
        .unwrap_or("2015")
        .to_string();
    // CARGO_PKG_* inputs (computed by the same pkg_env_map as for root/path packages in
    // manifest.rs)
    let str_field = |k: &str| {
        pkg_table
            .and_then(|p| p.get(k))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    let authors: Vec<String> = pkg_table
        .and_then(|p| p.get("authors"))
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let readme = match pkg_table.and_then(|p| p.get("readme")) {
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(toml::Value::Boolean(true)) => Some("README.md".to_string()),
        _ => None,
    };
    let (description, homepage, repository, license, license_file, rust_version) = (
        str_field("description"),
        str_field("homepage"),
        str_field("repository"),
        str_field("license"),
        str_field("license-file"),
        str_field("rust-version"),
    );
    let pkg_env = super::manifest::pkg_env_map(
        package,
        version,
        if authors.is_empty() {
            None
        } else {
            Some(authors.as_slice())
        },
        description.as_deref(),
        homepage.as_deref(),
        repository.as_deref(),
        license.as_deref(),
        license_file.as_deref(),
        readme.as_deref(),
        rust_version.as_deref(),
    );
    let declared_features = v
        .get("features")
        .and_then(toml::Value::as_table)
        .map(|features| features.keys().cloned().collect())
        .unwrap_or_default();
    // The Cargo.toml inside a registry package is the manifest normalized at publish time
    // and may still carry `[lints]`; reuse only the lint parsing here, so the full
    // manifest subset does not constrain registry packages.
    let rustc_lint_flags = super::manifest::parse_lints(v.get("lints")).map_err(|error| {
        format!("failed to parse [lints] of registry package {package} {version}: {error}")
    })?;
    Ok(RegistryMinimal {
        lib_name: name,
        proc_macro,
        links,
        has_build,
        build_script_path,
        edition,
        lib_path,
        pkg_env,
        declared_features,
        rustc_lint_flags,
    })
}

/// Re-fetch a node's dependency edges (manifest for a path package, index re-pulled by
/// exact version for a registry package).
fn node_featdeps(
    name: &str,
    version: &Version,
    path_manifests: &BTreeMap<String, PackageManifest>,
    src: &mut impl PkgSource,
) -> Result<Vec<FeatDep>, String> {
    if let Some(m) = path_manifests.get(name) {
        return decls_to_featdeps(&m.deps);
    }
    let vs = registry_entry(src, name)?;
    let iv = vs
        .iter()
        .find(|v| v.version == *version)
        .ok_or_else(|| format!("{name} {version} is not in the index"))?;
    Ok(iv
        .deps
        .iter()
        .filter(|d| d.kind.as_deref() != Some("dev"))
        .map(|d| FeatDep {
            key: d.name.clone(),
            package: d.package.clone().unwrap_or_else(|| d.name.clone()),
            class: if d.kind.as_deref() == Some("build") {
                UnitClass::Build
            } else {
                UnitClass::Normal
            },
            kind: if d.kind.as_deref() == Some("build") {
                DepKind::Build
            } else {
                DepKind::Normal
            },
            optional: d.optional,
            default_features: d.default_features,
            features: d.features.clone(),
            registry: true,
            platform_cfg: d.target.clone(),
            req: Some(d.req.to_string()),
            source_id: None,
        })
        .collect())
}

/// Whether this edge enters the host build graph (platform_cfg evaluated against the
/// host).
fn host_edge(dep: &FeatDep) -> Result<bool, String> {
    match &dep.platform_cfg {
        None => Ok(true),
        Some(expr) => super::manifest::eval_cfg(expr),
    }
}

/// The --extern naming key (cargo semantics): when there is a rename (toml
/// `package = "..."` / the index `package` key, so key != package) the rename key is
/// used; without a rename the dep package's **lib target name** is used, so when the lib
/// name differs from the package name the extern name follows the lib name. For example
/// tendril depends on new_debug_unreachable, whose `[lib] name = "debug_unreachable"`:
/// `cargo -v` emits `--extern debug_unreachable=.../libdebug_unreachable-....rmeta`. The
/// whole point of the new_debug_unreachable package is to republish under the lib name
/// debug_unreachable.
fn extern_key(d: &FeatDep, child: &Unit) -> String {
    if d.key != d.package {
        d.key.clone()
    } else {
        child.lib_name.clone()
    }
}

fn assemble_units(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    edge_versions: &EdgeVersions,
    nodes: &BTreeMap<NodeKey, FeatNode>,
    src: &mut impl PkgSource,
    include_root_dev: bool,
) -> Result<(Vec<Unit>, Vec<UnitDep>), String> {
    // Buildable set: reachable from the root along edges whose host cfg is true (cargo's
    // build-graph filter -- versions and the lock are the union over all platforms while
    // the build graph is evaluated against the host; a subgraph reached only through a
    // permanently false cfg(any()) edge, as in the serde facade family, enters the lock
    // but not the build graph).
    let root_key = (root.name.clone(), root.version.clone(), UnitClass::Normal);
    let mut buildable: BTreeSet<NodeKey> = BTreeSet::new();
    let mut queue: VecDeque<NodeKey> = VecDeque::new();
    buildable.insert(root_key.clone());
    queue.push_back(root_key.clone());
    while let Some(key) = queue.pop_front() {
        let deps = if key.0 == root.name && key.1 == root.version {
            root_featdeps(&root.deps, include_root_dev)?
        } else {
            node_featdeps(&key.0, &key.1, path_manifests, src)?
        };
        let activated = nodes
            .get(&key)
            .map(|n| n.activated.clone())
            .unwrap_or_default();
        for dep in deps {
            if dep.optional && !activated.contains(&dep.key) {
                continue;
            }
            if !host_edge(&dep)? {
                continue;
            }
            let Some((child_name, child_version)) = edge_version(edge_versions, &key, &dep) else {
                if dep.optional {
                    continue;
                }
                return Err(format!(
                    "dependency {} of {}@{} has no edge assignment record (internal inconsistency)",
                    dep.key, key.0, key.1
                ));
            };
            let child = (child_name.clone(), child_version.clone(), dep.class);
            if buildable.insert(child.clone()) {
                queue.push_back(child);
            }
        }
    }

    let mut units: Vec<Unit> = Vec::new();
    let mut index: BTreeMap<NodeKey, usize> = BTreeMap::new();
    for ((name, version, class), node) in nodes {
        if name == &root.name || !buildable.contains(&(name.clone(), version.clone(), *class)) {
            continue; // the root itself is not a dep unit; an unbuildable subgraph only enters the lock
        }
        if let Some(m) = path_manifests.get(name) {
            let (lib_name, lib_path) = m
                .targets
                .iter()
                .find(|t| t.is_lib())
                .map(|t| (t.name.clone(), t.path.clone()))
                // A path dependency with no lib target is a pathological package (cargo
                // rejects it too); fall back to the default path and let rustc report the
                // missing file at dep compile time (loud, never silently swallowed)
                .unwrap_or_else(|| (m.name.replace('-', "_"), m.root.join("src/lib.rs")));
            let proc_macro = m.targets.iter().any(|t| t.is_lib() && t.proc_macro);
            index.insert((name.clone(), version.clone(), *class), units.len());
            units.push(Unit {
                package: m.name.clone(),
                lib_name,
                version: m.version.clone(),
                source_dir: m.root.clone(),
                // Cargo caps lints for non-path sources such as registry/Git; a Git
                // checkout is likewise treated as immutable by commit and takes no part in
                // path-tree incremental scans.
                from_registry: m.lock_source.is_some(),
                immutable_source_id: m.lock_source.clone(),
                class: *class,
                features: node.features.clone(),
                declared_features: m.check_cfg_feature_values(),
                proc_macro,
                has_build_script: m.has_build_script,
                build_script_path: m.build_script_path.clone(),
                links: m.links.clone(),
                deps: vec![],
                edition: m.edition.clone(),
                lib_path,
                pkg_env: m.pkg_env.clone(),
                rustc_lint_flags: m.rustc_lint_flags.clone(),
            });
            continue;
        }
        let package_name = identity_package_name(name);
        let source = identity_source(name).unwrap_or(CRATES_IO_LOCK_SOURCE);
        let dir = src.ensure_source(source, package_name, version, None)?;
        let rm = read_registry_minimal(&dir, package_name, version)?;
        index.insert((name.clone(), version.clone(), *class), units.len());
        units.push(Unit {
            package: package_name.to_string(),
            lib_name: rm.lib_name,
            version: version.clone(),
            source_dir: dir,
            from_registry: true,
            immutable_source_id: Some(source.to_string()),
            class: *class,
            features: node.features.clone(),
            declared_features: rm.declared_features,
            proc_macro: rm.proc_macro,
            has_build_script: rm.has_build,
            build_script_path: rm.build_script_path,
            links: rm.links,
            deps: vec![],
            edition: rm.edition,
            lib_path: rm.lib_path,
            pkg_env: rm.pkg_env,
            rustc_lint_flags: rm.rustc_lint_flags,
        });
    }
    // Dependency edge filling (buildable node x host-true edge x optional activation gate)
    let mut edge_rows: Vec<(usize, UnitDep)> = Vec::new();
    for ((name, version, class), node) in nodes {
        if name == &root.name {
            continue;
        }
        let key = (name.clone(), version.clone(), *class);
        let Some(&from_idx) = index.get(&key) else {
            continue;
        };
        let deps = node_featdeps(name, version, path_manifests, src)?;
        for d in deps {
            if d.optional && !node.activated.contains(&d.key) {
                continue;
            }
            if !host_edge(&d)? {
                continue;
            }
            let Some((child_name, child_version)) = edge_version(edge_versions, &key, &d) else {
                if d.optional {
                    continue;
                }
                return Err(format!(
                    "dependency {} of {}@{} has no edge assignment record (internal inconsistency)",
                    d.key, key.0, key.1
                ));
            };
            let Some(&to_idx) = index.get(&(child_name.clone(), child_version.clone(), d.class))
            else {
                continue; // the child is not buildable (permanently false edge subgraph), so the edge does not exist
            };
            edge_rows.push((
                from_idx,
                UnitDep {
                    key: extern_key(&d, &units[to_idx]),
                    unit: to_idx,
                    class: d.class,
                    kind: d.kind,
                },
            ));
        }
    }
    for (from, edge) in edge_rows {
        units[from].deps.push(edge);
    }
    // The root's --extern edge table: the root itself is not a unit (see the comment
    // above), but a bin session's --extern closure must go through the same gates as the
    // unit edges (optional activation gate + host platform evaluation + edge-assigned
    // version); only the edge table is filled here, the root never becomes a unit.
    let mut root_deps: Vec<UnitDep> = Vec::new();
    {
        let root_node = nodes.get(&root_key).cloned().unwrap_or_default();
        for d in root_featdeps(&root.deps, include_root_dev)? {
            if d.optional && !root_node.activated.contains(&d.key) {
                continue;
            }
            if !host_edge(&d)? {
                continue;
            }
            let Some((child_name, child_version)) = edge_version(edge_versions, &root_key, &d)
            else {
                if d.optional {
                    continue;
                }
                return Err(format!(
                    "dependency {} of {}@{} has no edge assignment record (internal inconsistency)",
                    d.key, root_key.0, root_key.1
                ));
            };
            let Some(&to_idx) = index.get(&(child_name.clone(), child_version.clone(), d.class))
            else {
                continue; // the child is not buildable (permanently false edge subgraph), so the edge does not exist
            };
            root_deps.push(UnitDep {
                key: extern_key(&d, &units[to_idx]),
                unit: to_idx,
                class: d.class,
                kind: d.kind,
            });
        }
    }
    Ok((units, root_deps))
}

/// Cargo's fallback only changes candidate preference; it does not allow the current rustc
/// to compile a package that explicitly requires a higher version. Only units that will
/// actually be compiled are checked here: `cargo build` does not fail because of a dev
/// dependency that exists only in the lock, while `mirvm test` brings its test-graph units
/// into the check.
fn validate_compiler_rust_version(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    units: &[Unit],
    src: &mut impl PkgSource,
    compiler: &Version,
) -> Result<(), String> {
    let mut incompatible: BTreeSet<(String, Version, Version)> = BTreeSet::new();
    if let Some(required) = &root.rust_version
        && required > compiler
    {
        incompatible.insert((root.name.clone(), root.version.clone(), required.clone()));
    }
    for unit in units {
        let git_source = unit
            .immutable_source_id
            .as_deref()
            .filter(|source| source.starts_with("git+"));
        let required = if let Some(source) = git_source {
            path_manifests
                .values()
                .find(|manifest| {
                    manifest.name == unit.package
                        && manifest.version == unit.version
                        && manifest.lock_source.as_deref() == Some(source)
                })
                .and_then(|manifest| manifest.rust_version.clone())
        } else if unit.from_registry {
            src.index_entry(
                unit.immutable_source_id
                    .as_deref()
                    .unwrap_or(CRATES_IO_LOCK_SOURCE),
                &unit.package,
            )?
            .iter()
            .find(|entry| entry.version == unit.version)
            .and_then(|entry| entry.rust_version.clone())
        } else {
            path_manifests
                .get(&unit.package)
                .filter(|manifest| manifest.version == unit.version)
                .and_then(|manifest| manifest.rust_version.clone())
        };
        if let Some(required) = required
            && required > *compiler
        {
            incompatible.insert((unit.package.clone(), unit.version.clone(), required));
        }
    }
    if incompatible.is_empty() {
        return Ok(());
    }
    let packages = incompatible
        .into_iter()
        .map(|(name, version, required)| {
            format!("  {name}@{version} requires rustc {required} or newer")
        })
        .collect::<Vec<_>>()
        .join("\n");
    Err(format!(
        "the current rustc {compiler} does not satisfy the rust-version of:\n{packages}\nupgrade the toolchain, choose compatible dependency versions, or pass --ignore-rust-version explicitly"
    ))
}

// ---------- tests (offline; FakeSource canned index + tempdir sources) ----------

#[cfg(test)]
mod tests {
    use super::super::manifest::PackageManifest;
    use super::super::registry::IndexDep;
    use super::*;

    struct FakeSource {
        root: PathBuf,
        index: BTreeMap<String, Vec<IndexVersion>>,
        git: BTreeMap<(String, String), PackageManifest>,
        /// package name -> explicit `[lib] name` (canned for the case where the extern name
        /// follows a lib name different from the package name, as with
        /// new_debug_unreachable).
        lib_names: BTreeMap<String, String>,
    }

    impl FakeSource {
        fn new(root: PathBuf) -> Self {
            std::fs::create_dir_all(&root).unwrap();
            Self {
                root,
                index: BTreeMap::new(),
                git: BTreeMap::new(),
                lib_names: BTreeMap::new(),
            }
        }
        fn add(&mut self, name: &str, versions: Vec<IndexVersion>) {
            self.index.insert(name.to_string(), versions);
        }
        fn add_git(&mut self, source_id: &str, manifest: PackageManifest) {
            self.git
                .insert((source_id.to_string(), manifest.name.clone()), manifest);
        }
    }

    impl PkgSource for FakeSource {
        fn registry_source(&mut self, reference: &RegistryReference) -> Result<String, String> {
            Ok(match reference {
                RegistryReference::CratesIo => CRATES_IO_LOCK_SOURCE.to_string(),
                RegistryReference::Named(name) => format!("registry+test://{name}"),
                RegistryReference::Index(index) => {
                    if index.starts_with("sparse+") {
                        index.clone()
                    } else {
                        format!("registry+{index}")
                    }
                }
            })
        }

        fn index_entry(&mut self, _source: &str, name: &str) -> Result<IndexEntry, String> {
            Ok(self.index.get(name).cloned().unwrap_or_default().into())
        }
        fn ensure_source(
            &mut self,
            _source: &str,
            name: &str,
            version: &Version,
            _cksum: Option<&str>,
        ) -> Result<PathBuf, String> {
            let dir = self.root.join(format!("{name}-{version}"));
            std::fs::create_dir_all(&dir).unwrap();
            let lib_section = match self.lib_names.get(name) {
                Some(ln) => format!("\n[lib]\nname = \"{ln}\"\n"),
                None => String::new(),
            };
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n{lib_section}"),
            )
            .unwrap();
            Ok(dir)
        }

        fn ensure_git_package(
            &mut self,
            spec: &GitSpec,
            package: &str,
            locked_source: Option<&str>,
        ) -> Result<PackageManifest, String> {
            let mut manifest = self
                .git
                .get(&(spec.source_id(), package.to_string()))
                .cloned()
                .ok_or_else(|| format!("test Git package does not exist: {package}"))?;
            manifest.lock_source = Some(
                locked_source
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}#{}", spec.source_id(), "1".repeat(40))),
            );
            Ok(manifest)
        }
    }

    fn iv(name: &str, version: &str) -> IndexVersion {
        IndexVersion {
            name: name.to_string(),
            version: Version::parse(version).unwrap(),
            cksum: "0".repeat(64),
            yanked: false,
            deps: vec![],
            features: BTreeMap::new(),
            links: None,
            rust_version: None,
        }
    }

    fn idep(name: &str, req: &str) -> IndexDep {
        IndexDep {
            name: name.to_string(),
            req: VersionReq::parse(req).unwrap(),
            features: vec![],
            optional: false,
            default_features: true,
            target: None,
            kind: None,
            package: None,
            registry: None,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mirvm-cargoless-resolve-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn root_project(d: &Path, manifest: &str) -> PackageManifest {
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
        let manifest = manifest.replacen("[package]", "[package]\nedition=\"2021\"", 1);
        PackageManifest::parse(&manifest, d).unwrap()
    }

    #[test]
    fn lock_mode_pins_versions_and_allows_yanked() {
        let d = tmpdir("lockmode");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\na = \"^1.0\"\nb = \"0.3\"\n",
        );
        std::fs::write(
            d.join("Cargo.lock"),
            "# x\nversion = 4\n\n\
             [[package]]\nname = \"demo\"\nversion = \"0.1.0\"\ndependencies = [\n \"a\",\n \"b\",\n]\n\n\
             [[package]]\nname = \"a\"\nversion = \"1.2.0\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"b\"\nversion = \"0.3.1\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
        let mut src = FakeSource::new(d.join("srcstore"));
        // a@1.2.0 is already yanked in the index -- lock mode accepts it anyway (as cargo does)
        let mut a12 = iv("a", "1.2.0");
        a12.yanked = true;
        a12.features.insert("default".into(), vec!["std".into()]);
        a12.features.insert("std".into(), vec![]);
        src.add("a", vec![iv("a", "1.0.0"), a12, iv("a", "1.5.0")]);
        src.add(
            "b",
            vec![iv("b", "0.3.0"), iv("b", "0.3.1"), iv("b", "0.4.0")],
        );

        let plan = resolve(&root, &mut src).unwrap();
        assert_eq!(
            plan.version_map["a"],
            vec![Version::parse("1.2.0").unwrap()]
        );
        assert_eq!(
            plan.version_map["b"],
            vec![Version::parse("0.3.1").unwrap()]
        );
        // Feature unification: a's default -> std gets enabled
        let a_unit = plan.units.iter().find(|u| u.package == "a").unwrap();
        assert!(a_unit.features.contains("default"));
        assert!(a_unit.features.contains("std"));
        assert!(!a_unit.from_registry || a_unit.source_dir.ends_with("a-1.2.0"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn lock_mode_keeps_same_named_dependency_versions_distinct() {
        let d = tmpdir("lock-duplicate-dependency-name");
        let root = root_project(
            &d,
            "[package]\nname=\"demo\"\nversion=\"0.1.0\"\n[dependencies]\nparent=\"1\"\n",
        );
        std::fs::write(
            d.join("Cargo.lock"),
            "version=4\n\
             [[package]]\nname=\"demo\"\nversion=\"0.1.0\"\ndependencies=[\"parent\"]\n\
             [[package]]\nname=\"parent\"\nversion=\"1.0.0\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies=[\"h 0.2.1\",\"h 0.3.1\"]\n\
             [[package]]\nname=\"h\"\nversion=\"0.2.1\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\n\
             [[package]]\nname=\"h\"\nversion=\"0.3.1\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();

        let mut parent = iv("parent", "1.0.0");
        parent.deps.push(idep("h", "^0.2"));
        let mut renamed = idep("h_03", "^0.3");
        renamed.package = Some("h".into());
        parent.deps.push(renamed);
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add("parent", vec![parent]);
        src.add("h", vec![iv("h", "0.2.1"), iv("h", "0.3.1")]);

        let plan = resolve(&root, &mut src).unwrap();
        assert_eq!(
            plan.version_map["h"],
            vec![Version::new(0, 2, 1), Version::new(0, 3, 1)]
        );
        assert_eq!(
            plan.units.iter().filter(|unit| unit.package == "h").count(),
            2
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn cli_dependency_feature_reaches_dep_without_becoming_root_cfg() {
        let d = tmpdir("cli-dependency-feature");
        let mut root = root_project(
            &d,
            "[package]\nname=\"demo\"\nversion=\"0.1.0\"\n[dependencies]\na=\"1\"\n",
        );
        root.dependency_features
            .entry("a".into())
            .or_default()
            .insert("extra".into());
        std::fs::write(
            d.join("Cargo.lock"),
            "version=4\n[[package]]\nname=\"demo\"\nversion=\"0.1.0\"\ndependencies=[\"a\"]\n\
             [[package]]\nname=\"a\"\nversion=\"1.0.0\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
        let mut a = iv("a", "1.0.0");
        a.features.insert("extra".into(), vec![]);
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add("a", vec![a]);
        let plan = resolve(&root, &mut src).unwrap();
        assert!(
            plan.units
                .iter()
                .find(|unit| unit.package == "a")
                .unwrap()
                .features
                .contains("extra")
        );
        assert!(!plan.root_features.contains("extra"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn run_locks_but_does_not_build_root_dev_dependencies() {
        let d = tmpdir("root-dev-purpose");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nnormal = \"1\"\n\
             [dev-dependencies]\ndevonly = \"1\"\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add("normal", vec![iv("normal", "1.0.0")]);
        src.add("devonly", vec![iv("devonly", "1.0.0")]);

        let run = resolve_for(&root, &mut src, ResolvePurpose::Run).unwrap();
        assert!(run.version_map.contains_key("devonly"));
        assert!(!run.units.iter().any(|unit| unit.package == "devonly"));
        assert!(!run.root_deps.iter().any(|dep| dep.kind == DepKind::Dev));

        let test = resolve_for(&root, &mut src, ResolvePurpose::Test).unwrap();
        let dev_ix = test
            .units
            .iter()
            .position(|unit| unit.package == "devonly")
            .unwrap();
        assert!(
            test.root_deps
                .iter()
                .any(|dep| dep.unit == dev_ix && dep.kind == DepKind::Dev)
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn resolver_three_prefers_compatible_versions_but_falls_back() {
        let directory = tmpdir("resolver-three-rust-version");
        let mut root = root_project(
            &directory,
            "[package]\nname='demo'\nversion='0.1.0'\nresolver='3'\nrust-version='1.70'\n\
             [dependencies]\na='1'\n",
        );
        let mut compatible = iv("a", "1.0.0");
        compatible.rust_version = Some(Version::new(1, 60, 0));
        let mut too_new = iv("a", "1.1.0");
        too_new.rust_version = Some(Version::new(1, 80, 0));
        let mut source = FakeSource::new(directory.join("srcstore"));
        source.add("a", vec![compatible, too_new]);

        let fallback = resolve(&root, &mut source).unwrap();
        assert_eq!(fallback.version_map["a"], vec![Version::new(1, 0, 0)]);
        assert_eq!(
            fallback.lock.format_version, 3,
            "Cargo keeps lock v3 for projects with rust-version 1.82 or earlier"
        );

        root.ignore_rust_version = true;
        let allow = resolve(&root, &mut source).unwrap();
        assert_eq!(allow.version_map["a"], vec![Version::new(1, 1, 0)]);

        root.ignore_rust_version = false;
        root.rust_version = Some(Version::new(1, 83, 0));
        root.resolver_rust_version = root.rust_version.clone();
        let modern_lock = resolve(&root, &mut source).unwrap();
        assert_eq!(modern_lock.lock.format_version, 4);

        root.rust_version = Some(Version::new(1, 50, 0));
        root.resolver_rust_version = Some(Version::new(1, 50, 0));
        let no_compatible = resolve(&root, &mut source).unwrap();
        assert_eq!(
            no_compatible.version_map["a"],
            vec![Version::new(1, 1, 0)],
            "with no compatible candidate, Cargo fallback still picks the usual highest version"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn compiler_rust_version_rejection_can_be_explicitly_ignored() {
        let directory = tmpdir("rust-version-diagnostic");
        let mut root = root_project(
            &directory,
            "[package]\nname='demo'\nversion='0.1.0'\nresolver='3'\n\
             [dependencies]\na='1'\n",
        );
        let mut future = iv("a", "1.0.0");
        future.rust_version = Some(Version::new(999, 0, 0));
        let mut source = FakeSource::new(directory.join("srcstore"));
        source.add("a", vec![future]);
        let error = resolve(&root, &mut source).unwrap_err();
        assert!(error.contains("a@1.0.0"), "{error}");
        assert!(error.contains("--ignore-rust-version"), "{error}");

        root.ignore_rust_version = true;
        assert!(resolve(&root, &mut source).is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn extern_key_prefers_rename_then_lib_name() {
        // cargo semantics (tendril 0.5.1 -> new_debug_unreachable 1.0.6):
        // the --extern name is the rename key when renamed, otherwise the dep package's
        // [lib] name.
        let d = tmpdir("externkey");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nndu = \"1.0\"\n\
             renamed = { version = \"1.0\", package = \"real-pkg\" }\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        src.lib_names
            .insert("ndu".into(), "debug_unreachable".into());
        src.add("ndu", vec![iv("ndu", "1.0.6")]);
        src.add("real-pkg", vec![iv("real-pkg", "1.0.0")]);

        let plan = resolve(&root, &mut src).unwrap();
        let ndu_ix = plan.units.iter().position(|u| u.package == "ndu").unwrap();
        let rp_ix = plan
            .units
            .iter()
            .position(|u| u.package == "real-pkg")
            .unwrap();
        let key_of = |ix: usize| {
            plan.root_deps
                .iter()
                .find(|e| e.unit == ix)
                .map(|e| e.key.clone())
                .unwrap()
        };
        // without a rename the extern name follows the lib name, not the package name
        assert_eq!(key_of(ndu_ix), "debug_unreachable");
        // with a rename the rename key wins
        assert_eq!(key_of(rp_ix), "renamed");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn lock_mode_rejects_stale_lock_loudly() {
        let d = tmpdir("stale");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\na = \"^2.0\"\n",
        );
        std::fs::write(
            d.join("Cargo.lock"),
            "version = 4\n\n\
             [[package]]\nname = \"demo\"\nversion = \"0.1.0\"\ndependencies = [\"a\"]\n\n\
             [[package]]\nname = \"a\"\nversion = \"1.0.0\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add("a", vec![iv("a", "1.0.0"), iv("a", "2.0.0")]);
        let err = resolve(&root, &mut src).unwrap_err();
        assert!(err.contains("stale Cargo.lock"), "{err}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn fresh_solve_diamond_skips_yanked_and_emits_lock() {
        let d = tmpdir("fresh");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nx = \"^1\"\ny = \"^1\"\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        let mut x = iv("x", "1.0.0");
        x.deps.push(idep("z", "^1.2"));
        let mut y = iv("y", "1.0.0");
        y.deps.push(idep("z", ">=1.1, <1.4"));
        src.add("x", vec![x]);
        src.add("y", vec![y]);
        let mut z14 = iv("z", "1.4.0");
        z14.yanked = true;
        src.add(
            "z",
            vec![
                iv("z", "1.0.0"),
                iv("z", "1.2.0"),
                iv("z", "1.3.5"),
                z14,
                iv("z", "1.5.0"),
            ],
        );
        let plan = resolve(&root, &mut src).unwrap();
        // max of the [1.2,1.4) range is 1.3.5 (1.4.0 is yanked and skipped; 1.5.0 does not satisfy <1.4)
        assert_eq!(
            plan.version_map["z"],
            vec![Version::parse("1.3.5").unwrap()]
        );
        let z_lock = plan
            .lock
            .get("z", &Version::parse("1.3.5").unwrap())
            .unwrap();
        assert!(z_lock.source.as_ref().unwrap().starts_with("registry+"));
        assert_eq!(z_lock.checksum.as_deref(), Some(&"0".repeat(64)[..]));
        // the lock can be read back by our own parser and carries dependency lines
        let text = plan.lock.serialize();
        let back = Lockfile::parse(&text).unwrap();
        assert!(back.get("z", &Version::parse("1.3.5").unwrap()).is_some());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn feature_unification_optional_forms_and_class_separation() {
        let d = tmpdir("features");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nm = \"1\"\ns = \"1\"\nw = \"1\"\n\
             [build-dependencies]\ns = { version = \"1\", features = [\"b\"] }\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        // m: default contains an explicit dep:opt activation; opt's of -> deep
        let mut m = iv("m", "1.0.0");
        m.features.insert("default".into(), vec!["dep:opt".into()]);
        m.features.insert("opt".into(), vec![]);
        let mut opt = idep("opt", "1");
        opt.optional = true;
        opt.features = vec!["of".into()];
        m.deps.push(opt);
        src.add("m", vec![m]);
        let mut opt = iv("opt", "1.0.0");
        opt.features.insert("of".into(), vec!["deep".into()]);
        opt.features.insert("deep".into(), vec![]);
        src.add("opt", vec![opt]);
        // s: the normal edge (default) and the build edge (features=["b"]) are separate
        let mut s = iv("s", "1.0.0");
        s.features.insert("default".into(), vec![]);
        s.features.insert("b".into(), vec![]);
        src.add("s", vec![s]);
        // w: weak activation opt2?/inner -- opt2 is never activated, so no opt2 unit
        let mut w = iv("w", "1.0.0");
        w.features
            .insert("default".into(), vec!["opt2?/inner".into()]);
        let mut opt2 = idep("opt2", "1");
        opt2.optional = true;
        w.deps.push(opt2);
        src.add("w", vec![w]);
        let mut opt2 = iv("opt2", "1.0.0");
        opt2.features.insert("inner".into(), vec![]);
        src.add("opt2", vec![opt2]);

        let plan = resolve(&root, &mut src).unwrap();
        let m_unit = plan.units.iter().find(|u| u.package == "m").unwrap();
        assert!(m_unit.features.contains("default"));
        // dep:opt activated opt, and opt received the edge features of -> of/deep expand
        let opt_unit = plan.units.iter().find(|u| u.package == "opt").unwrap();
        assert!(opt_unit.features.contains("of"));
        assert!(opt_unit.features.contains("deep"));
        // two s columns: Normal (default) and Build (b) with different feature sets = two units
        let s_units: Vec<_> = plan.units.iter().filter(|u| u.package == "s").collect();
        assert_eq!(s_units.len(), 2);
        let s_normal = s_units
            .iter()
            .find(|u| u.class == UnitClass::Normal)
            .unwrap();
        let s_build = s_units
            .iter()
            .find(|u| u.class == UnitClass::Build)
            .unwrap();
        assert!(s_normal.features.contains("default"));
        assert!(!s_normal.features.contains("b"));
        assert!(s_build.features.contains("b"));
        // the weak activation never fires: there is no opt2 unit
        assert!(!plan.units.iter().any(|u| u.package == "opt2"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn implicit_optional_feature_also_sets_cfg_flag() {
        // serde facade shape: feature derive = ["se_derive"] (a bare-name reference to an
        // optional dependency, no dep: form) activates the dependency and **also sets the
        // feature flag of the same name** (as in cargo: serde's
        // `#[cfg(feature = "serde_derive")]`). By contrast the explicit dep: form only
        // activates the dependency and sets no flag of the same name.
        let d = tmpdir("implicitfeat");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nse = { version = \"1\", features = [\"derive\"] }\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        let mut se = iv("se", "1.0.0");
        se.features
            .insert("derive".into(), vec!["se_derive".into()]);
        let mut se_derive = idep("se_derive", "1");
        se_derive.optional = true;
        se.deps.push(se_derive);
        src.add("se", vec![se]);
        src.add("se_derive", vec![iv("se_derive", "1.0.0")]);

        let plan = resolve(&root, &mut src).unwrap();
        let se_unit = plan.units.iter().find(|u| u.package == "se").unwrap();
        assert!(se_unit.features.contains("derive"));
        assert!(
            se_unit.features.contains("se_derive"),
            "the implicit feature flag must enter the cfg set: {:?}",
            se_unit.features
        );
        assert!(plan.units.iter().any(|u| u.package == "se_derive"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn strong_dep_feature_also_sets_implicit_cfg_flag() {
        // k256 shape: feature ecdsa = ["ecdsa-core/signing"] (a strong reference to an
        // optional dependency) activates the dependency and propagates signing, while the
        // implicit feature flag of the same name, ecdsa-core, is set as well (as in cargo:
        // k256's `#[cfg(feature = "ecdsa-core")] pub mod ecdsa`; missing the flag gives
        // E0433 cannot find ecdsa in k256).
        let d = tmpdir("strongimplicit");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nk2 = { version = \"1\", features = [\"ecdsa\"] }\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        let mut k2 = iv("k2", "1.0.0");
        k2.features
            .insert("ecdsa".into(), vec!["ecdsa-core/signing".into()]);
        let mut ec = idep("ecdsa-core", "1");
        ec.optional = true;
        k2.deps.push(ec);
        src.add("k2", vec![k2]);
        let mut ecdsa_core = iv("ecdsa-core", "1.0.0");
        ecdsa_core.features.insert("signing".into(), vec![]);
        src.add("ecdsa-core", vec![ecdsa_core]);

        let plan = resolve(&root, &mut src).unwrap();
        let k2_unit = plan.units.iter().find(|u| u.package == "k2").unwrap();
        assert!(k2_unit.features.contains("ecdsa"));
        assert!(
            k2_unit.features.contains("ecdsa-core"),
            "the implicit feature flag of a strongly activated optional dependency must enter the cfg set: {:?}",
            k2_unit.features
        );
        let ec_unit = plan
            .units
            .iter()
            .find(|u| u.package == "ecdsa-core")
            .unwrap();
        assert!(
            ec_unit.features.contains("signing"),
            "the y of x/y propagates as usual"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn strong_dep_enables_same_named_explicit_feature_even_when_dep_hidden() {
        // zerotrie 0.2.4 shape: the optional dependency litemap is shadowed by dep: (so no
        // implicit feature is generated), but an explicit feature of the same name,
        // litemap = [dep:litemap, alloc], is present; and
        // serde = [dep:serde_core, dep:litemap, alloc, litemap/serde]. With
        // features=["serde"] the cfg set must contain litemap -- the strong litemap/serde
        // also enables the same-named **explicit** feature (dep: shadowing kills only the
        // implicit feature); missing the flag gives E0599 because
        // try_from_serde_litemap lives in a `#[cfg(feature = "litemap")]` impl block.
        let d = tmpdir("strongexplicit");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nzt = { version = \"1\", features = [\"serde\"] }\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        let mut zt = iv("zt", "1.0.0");
        zt.features.insert("alloc".into(), vec![]);
        zt.features
            .insert("litemap".into(), vec!["dep:litemap".into(), "alloc".into()]);
        zt.features.insert(
            "serde".into(),
            vec![
                "dep:serde_core".into(),
                "dep:litemap".into(),
                "alloc".into(),
                "litemap/serde".into(),
            ],
        );
        let mut lm = idep("litemap", "1");
        lm.optional = true;
        zt.deps.push(lm);
        let mut sc = idep("serde_core", "1");
        sc.optional = true;
        zt.deps.push(sc);
        src.add("zt", vec![zt]);
        let mut litemap = iv("litemap", "1.0.0");
        litemap.features.insert("serde".into(), vec![]);
        src.add("litemap", vec![litemap]);
        src.add("serde_core", vec![iv("serde_core", "1.0.0")]);

        let plan = resolve(&root, &mut src).unwrap();
        let zt_unit = plan.units.iter().find(|u| u.package == "zt").unwrap();
        assert!(zt_unit.features.contains("serde"));
        assert!(
            zt_unit.features.contains("litemap"),
            "a strong x/y must enable the same-named explicit feature (dep: shadowing kills only the implicit one): {:?}",
            zt_unit.features
        );
        assert!(
            zt_unit.features.contains("alloc"),
            "expanding the explicit litemap feature carries alloc: {:?}",
            zt_unit.features
        );
        assert!(
            plan.units.iter().any(|u| u.package == "litemap"),
            "the litemap dependency itself is activated into units"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn path_dep_joins_solve_and_units() {
        let d = tmpdir("pathdep");
        let sibling = d.join("sibling");
        std::fs::create_dir_all(sibling.join("src")).unwrap();
        std::fs::write(
            sibling.join("Cargo.toml"),
            "[package]\nname = \"sib\"\nversion = \"0.2.0\"\n\
             [features]\napp-side=[]\n[dependencies]\nr = \"1\"\n",
        )
        .unwrap();
        std::fs::write(sibling.join("src/lib.rs"), "").unwrap();
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nsib = { path = \"sibling\" }\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add("r", vec![iv("r", "1.0.0")]);
        let plan = resolve(&root, &mut src).unwrap();
        let sib = plan.units.iter().find(|u| u.package == "sib").unwrap();
        assert!(!sib.from_registry);
        assert_eq!(sib.version.to_string(), "0.2.0");
        assert_eq!(
            plan.version_map["r"],
            vec![Version::parse("1.0.0").unwrap()]
        );
        // generated lock: sib has no source line
        let sib_lock = plan
            .lock
            .get("sib", &Version::parse("0.2.0").unwrap())
            .unwrap();
        assert!(sib_lock.source.is_none());
        let mut overrides = FeatureOverrides::new();
        overrides.insert(
            ("sib".to_string(), Version::new(0, 2, 0), UnitClass::Normal),
            BTreeSet::from(["app-side".to_string()]),
        );
        let with_workspace_features =
            resolve_for_known_with_features(&root, &mut src, ResolvePurpose::Run, &[], &overrides)
                .unwrap();
        assert!(
            with_workspace_features
                .units
                .iter()
                .find(|unit| unit.package == "sib")
                .unwrap()
                .features
                .contains("app-side"),
            "a workspace public package-name feature must map to the local node carrying that source"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn git_repo_path_packages_share_precise_source_and_lock() {
        let d = tmpdir("git-path");
        let repo = d.join("repo");
        let core = repo.join("core");
        let helper = repo.join("helper");
        for package in [&core, &helper] {
            std::fs::create_dir_all(package.join("src")).unwrap();
            std::fs::write(package.join("src/lib.rs"), "").unwrap();
        }
        std::fs::write(
            core.join("Cargo.toml"),
            "[package]\nname='git-core'\nversion='1.2.3'\nedition='2021'\n\
             [dependencies]\ngit-helper={path='../helper'}\n",
        )
        .unwrap();
        std::fs::write(
            helper.join("Cargo.toml"),
            "[package]\nname='git-helper'\nversion='0.4.0'\nedition='2021'\n",
        )
        .unwrap();
        let mut git_core = PackageManifest::parse(
            &std::fs::read_to_string(core.join("Cargo.toml")).unwrap(),
            &core,
        )
        .unwrap();
        git_core.git_checkout_root = Some(repo.clone());
        let root = root_project(
            &d,
            "[package]\nname='demo'\nversion='0.1.0'\n\
             [dependencies]\nchosen={package='git-core',git='https://example.invalid/repo',branch='main',version='^1'}\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add_git("git+https://example.invalid/repo?branch=main", git_core);
        let plan = resolve(&root, &mut src).unwrap();
        let precise = format!(
            "git+https://example.invalid/repo?branch=main#{}",
            "1".repeat(40)
        );
        for package in ["git-core", "git-helper"] {
            let unit = plan
                .units
                .iter()
                .find(|unit| unit.package == package)
                .unwrap();
            assert!(
                unit.from_registry,
                "a Git package is treated as an immutable dependency"
            );
            assert_eq!(unit.immutable_source_id.as_deref(), Some(precise.as_str()));
            assert_eq!(
                plan.lock
                    .packages
                    .iter()
                    .find(|locked| locked.name == package)
                    .and_then(|locked| locked.source.as_deref()),
                Some(precise.as_str())
            );
        }
        std::fs::write(d.join("Cargo.lock"), plan.lock.serialize()).unwrap();
        let locked_plan = resolve(&root, &mut src).unwrap();
        assert_eq!(locked_plan.lock, plan.lock);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn git_rust_version_comes_from_checkout_manifest() {
        let d = tmpdir("git-rust-version");
        let repo = d.join("repo");
        let core = repo.join("core");
        std::fs::create_dir_all(core.join("src")).unwrap();
        std::fs::write(core.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
        std::fs::write(
            core.join("Cargo.toml"),
            "[package]\nname='git-core'\nversion='1.2.3'\nedition='2021'\nrust-version='999.0'\n",
        )
        .unwrap();
        let mut git_core = PackageManifest::parse(
            &std::fs::read_to_string(core.join("Cargo.toml")).unwrap(),
            &core,
        )
        .unwrap();
        git_core.git_checkout_root = Some(repo);
        let root = root_project(
            &d,
            "[package]\nname='demo'\nversion='0.1.0'\n\
             [dependencies]\nchosen={package='git-core',git='https://example.invalid/repo',version='^1'}\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add_git("git+https://example.invalid/repo", git_core);
        let error = resolve(&root, &mut src).unwrap_err();
        assert!(
            error.contains("git-core@1.2.3 requires rustc 999.0"),
            "{error}"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn same_git_package_from_two_revisions_stays_distinct() {
        let d = tmpdir("git-two-revisions");
        let mut src = FakeSource::new(d.join("srcstore"));
        for revision in ["old", "new"] {
            let repo = d.join(format!("repo-{revision}"));
            let core = repo.join("core");
            let helper = repo.join("helper");
            for package in [&core, &helper] {
                std::fs::create_dir_all(package.join("src")).unwrap();
                std::fs::write(package.join("src/lib.rs"), "").unwrap();
            }
            std::fs::write(
                core.join("Cargo.toml"),
                "[package]\nname='git-core'\nversion='1.2.3'\nedition='2021'\n\
                 [dependencies]\ngit-helper={path='../helper'}\n",
            )
            .unwrap();
            std::fs::write(
                helper.join("Cargo.toml"),
                "[package]\nname='git-helper'\nversion='0.4.0'\nedition='2021'\n",
            )
            .unwrap();
            let mut manifest = PackageManifest::parse(
                &std::fs::read_to_string(core.join("Cargo.toml")).unwrap(),
                &core,
            )
            .unwrap();
            manifest.git_checkout_root = Some(repo);
            src.add_git(
                &format!("git+https://example.invalid/repo?rev={revision}"),
                manifest,
            );
        }
        let root = root_project(
            &d,
            "[package]\nname='demo'\nversion='0.1.0'\n\
             [dependencies]\n\
             old={package='git-core',git='https://example.invalid/repo',rev='old',version='^1'}\n\
             new={package='git-core',git='https://example.invalid/repo',rev='new',version='^1'}\n",
        );
        let plan = resolve(&root, &mut src).unwrap();
        assert_eq!(
            plan.units
                .iter()
                .filter(|unit| unit.package == "git-core")
                .count(),
            2
        );
        assert_eq!(
            plan.units
                .iter()
                .filter(|unit| unit.package == "git-helper")
                .count(),
            2
        );
        let root_sources: BTreeSet<String> = plan
            .root_deps
            .iter()
            .map(|dependency| {
                plan.units[dependency.unit]
                    .immutable_source_id
                    .clone()
                    .unwrap()
            })
            .collect();
        assert_eq!(root_sources.len(), 2);
        let root_lock = plan
            .lock
            .packages
            .iter()
            .find(|package| package.name == "demo")
            .unwrap();
        assert_eq!(root_lock.dependencies.len(), 2);
        assert!(
            root_lock
                .dependencies
                .iter()
                .all(
                    |dependency| dependency.version == Some(Version::new(1, 2, 3))
                        && dependency.source.is_some()
                )
        );
        let serialized = plan.lock.serialize();
        let parsed = Lockfile::parse(&serialized).unwrap();
        assert_eq!(parsed, plan.lock);
        std::fs::write(d.join("Cargo.lock"), serialized).unwrap();
        let locked = resolve(&root, &mut src).unwrap();
        assert_eq!(locked.root_deps.len(), 2);
        assert_ne!(locked.root_deps[0].unit, locked.root_deps[1].unit);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn fresh_solve_downgrades_parent_to_avoid_compatible_duplicate() {
        // Cargo prefers unifying one name: wide@1.1 is newer but pins core@1.0.1, while the
        // graph already pins core@1.0.0 and wide@1.0 also satisfies ^1, so it must backtrack
        // rather than keep two copies of core just to take the newest patch version.
        let d = tmpdir("compatible-duplicate");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\ncontainer = \"1\"\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        let mut container = iv("container", "1.0.0");
        container.deps.push(idep("pinned", "1"));
        container.deps.push(idep("wide", "^1.0"));
        src.add("container", vec![container]);

        let mut pinned = iv("pinned", "1.0.0");
        pinned.deps.push(idep("core", "=1.0.0"));
        src.add("pinned", vec![pinned]);

        let mut wide_old = iv("wide", "1.0.0");
        wide_old.deps.push(idep("core", "=1.0.0"));
        let mut wide_new = iv("wide", "1.1.0");
        wide_new.deps.push(idep("core", "=1.0.1"));
        src.add("wide", vec![wide_old, wide_new]);
        src.add("core", vec![iv("core", "1.0.0"), iv("core", "1.0.1")]);

        let plan = resolve(&root, &mut src).unwrap();
        assert_eq!(plan.version_map["wide"], vec![Version::new(1, 0, 0)]);
        assert_eq!(plan.version_map["core"], vec![Version::new(1, 0, 0)]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn multi_version_fork_coexists_with_per_bucket_features() {
        // cargo multi-version semantics: a->h ^0.14 and b->h ^0.15 share no candidate, so
        // both versions coexist (hashbrown 0.14/0.15); each feature table expands
        // independently per bucket
        let d = tmpdir("fork");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\na = \"1\"\nb = \"1\"\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        let mut a = iv("a", "1.0.0");
        a.deps.push(idep("h", "^0.14"));
        let mut b = iv("b", "1.0.0");
        b.deps.push(idep("h", "^0.15"));
        src.add("a", vec![a]);
        src.add("b", vec![b]);
        let mut h14 = iv("h", "0.14.5");
        h14.features.insert("default".into(), vec!["x".into()]);
        h14.features.insert("x".into(), vec![]);
        let mut h15 = iv("h", "0.15.2");
        h15.features.insert("default".into(), vec!["y".into()]);
        h15.features.insert("y".into(), vec![]);
        src.add("h", vec![h14, h15]);

        let plan = resolve(&root, &mut src).unwrap();
        assert_eq!(
            plan.version_map["h"],
            vec![
                Version::parse("0.14.5").unwrap(),
                Version::parse("0.15.2").unwrap()
            ]
        );
        let h_units: Vec<_> = plan.units.iter().filter(|u| u.package == "h").collect();
        assert_eq!(h_units.len(), 2);
        let u14 = h_units
            .iter()
            .find(|u| u.version.to_string() == "0.14.5")
            .unwrap();
        let u15 = h_units
            .iter()
            .find(|u| u.version.to_string() == "0.15.2")
            .unwrap();
        assert!(u14.features.contains("x") && !u14.features.contains("y"));
        assert!(u15.features.contains("y") && !u15.features.contains("x"));
        // the lock holds both versions; the a/b dependency lines carry a disambiguation hint
        assert!(
            plan.lock
                .get("h", &Version::parse("0.14.5").unwrap())
                .is_some()
        );
        assert!(
            plan.lock
                .get("h", &Version::parse("0.15.2").unwrap())
                .is_some()
        );
        let a_line = &plan
            .lock
            .get("a", &Version::parse("1.0.0").unwrap())
            .unwrap()
            .dependencies;
        let b_line = &plan
            .lock
            .get("b", &Version::parse("1.0.0").unwrap())
            .unwrap()
            .dependencies;
        assert!(
            a_line.contains(&LockedDep {
                name: "h".to_string(),
                version: Some(Version::parse("0.14.5").unwrap()),
                source: None,
            }),
            "a line: {a_line:?}"
        );
        assert!(
            b_line.contains(&LockedDep {
                name: "h".to_string(),
                version: Some(Version::parse("0.15.2").unwrap()),
                source: None,
            }),
            "b line: {b_line:?}"
        );
        // the lock can be read back by our own parser
        let back = Lockfile::parse(&plan.lock.serialize()).unwrap();
        assert_eq!(back.packages.len(), plan.lock.packages.len());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn prerelease_req_allows_pre_candidates() {
        // A req carrying a pre comparator admits that package's prereleases (cargo's
        // approximate rule; e.g. argon2 = "0.6.0-rc.8", where the rc family used to be
        // rejected wholesale by the pre filter)
        let d = tmpdir("pre");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\nargon2 = \"0.6.0-rc.8\"\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        src.add(
            "argon2",
            vec![
                iv("argon2", "0.5.3"),
                iv("argon2", "0.6.0-rc.7"),
                iv("argon2", "0.6.0-rc.8"),
            ],
        );
        let plan = resolve(&root, &mut src).unwrap();
        assert_eq!(plan.version_map["argon2"][0].to_string(), "0.6.0-rc.8");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn syn_features2_shape_activates_optional_quote_into_lock_lines() {
        // Reproduces the real index shape of syn 3.0.3 (dep:quote inside features2) plus
        // the serde_derive dep shape: quote must appear on syn's lock dependency line
        let d = tmpdir("synshape");
        let root = root_project(
            &d,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\nsd = \"1\"\n",
        );
        let mut src = FakeSource::new(d.join("srcstore"));
        let mut sd = iv("sd", "1.0.0");
        let mut sd_syn = idep("syn", "^3");
        sd_syn.default_features = false;
        sd_syn.features = vec![
            "clone-impls".into(),
            "derive".into(),
            "parsing".into(),
            "printing".into(),
            "proc-macro".into(),
        ];
        sd.deps.push(sd_syn);
        sd.deps.push(idep("quote", "^1"));
        src.add("sd", vec![sd]);
        let mut syn = iv("syn", "3.0.3");
        syn.features.insert("derive".into(), vec![]);
        syn.features.insert("parsing".into(), vec![]);
        syn.features.insert("clone-impls".into(), vec![]);
        syn.features.insert(
            "default".into(),
            vec![
                "derive".into(),
                "parsing".into(),
                "printing".into(),
                "clone-impls".into(),
                "proc-macro".into(),
            ],
        );
        syn.features
            .insert("printing".into(), vec!["dep:quote".into()]);
        syn.features.insert(
            "proc-macro".into(),
            vec!["proc-macro2/proc-macro".into(), "quote?/proc-macro".into()],
        );
        let mut syn_quote = idep("quote", "^1.0.35");
        syn_quote.optional = true;
        syn_quote.default_features = false;
        syn.deps.push(syn_quote);
        syn.deps.push(idep("proc-macro2", "^1"));
        src.add("syn", vec![syn]);
        let mut quote = iv("quote", "1.0.47");
        quote.features.insert("proc-macro".into(), vec![]);
        src.add("quote", vec![quote]);
        let mut proc_macro2 = iv("proc-macro2", "1.0.107");
        proc_macro2.features.insert("proc-macro".into(), vec![]);
        src.add("proc-macro2", vec![proc_macro2]);

        let plan = resolve(&root, &mut src).unwrap();
        let syn_lock = plan
            .lock
            .get("syn", &Version::parse("3.0.3").unwrap())
            .unwrap();
        let line: Vec<String> = syn_lock
            .dependencies
            .iter()
            .map(|dependency| dependency.name.clone())
            .collect();
        assert!(
            line.contains(&"quote".to_string()),
            "syn dependency line is missing quote: {line:?}"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn req_to_ranges_less_bound_is_exact() {
        // ">=2.0.4, <3" = [2.0.4, 3.0.0) -- the Less arm used to be wrong and give <4.0.0
        let req = VersionReq::parse(">=2.0.4, <3").unwrap();
        let r = req_to_ranges(&req);
        assert!(r.contains(&Version::parse("2.0.4").unwrap()));
        assert!(r.contains(&Version::parse("2.9.9").unwrap()));
        assert!(!r.contains(&Version::parse("3.0.0").unwrap()));
        let req2 = VersionReq::parse("<3.2").unwrap();
        let r2 = req_to_ranges(&req2);
        assert!(r2.contains(&Version::parse("3.1.9").unwrap()));
        assert!(!r2.contains(&Version::parse("3.2.0").unwrap()));
    }

    #[test]
    fn exact_pin_matches_build_metadata_variants() {
        // "=0.18.5" must match 0.18.5+1.9.4 (any build metadata; the semver crate's Ord
        // compares build metadata, so a singleton set would wrongly reject it)
        let req = VersionReq::parse("=0.18.5").unwrap();
        let r = req_to_ranges(&req);
        assert!(r.contains(&Version::parse("0.18.5").unwrap()));
        assert!(r.contains(&Version::parse("0.18.5+1.9.4").unwrap()));
        assert!(!r.contains(&Version::parse("0.18.6").unwrap()));
    }

    #[test]
    fn registry_minimal_accepts_both_proc_macro_spellings() {
        // Both spellings coexist in crates.io normalized output: the hyphen (serde_derive
        // 1.0.228, older normalization) and the underscore (derive_arbitrary 1.3.2, newer
        // normalization). Cargo accepts both; missing one compiles a proc-macro crate as a
        // target dependency.
        let d = tmpdir("proc-macro-spelling");
        for (key, want) in [("proc-macro", true), ("proc_macro", true)] {
            let dir = d.join(key);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"pm\"\nversion = \"1.0.0\"\n[lib]\n{key} = true\n"),
            )
            .unwrap();
            let rm = read_registry_minimal(&dir, "pm", &Version::parse("1.0.0").unwrap()).unwrap();
            assert_eq!(rm.proc_macro, want, "spelling {key} must be recognized");
        }
        // absent = false (a plain lib)
        let dir = d.join("absent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"pm\"\nversion = \"1.0.0\"\n[lib]\n",
        )
        .unwrap();
        let rm = read_registry_minimal(&dir, "pm", &Version::parse("1.0.0").unwrap()).unwrap();
        assert!(!rm.proc_macro);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
