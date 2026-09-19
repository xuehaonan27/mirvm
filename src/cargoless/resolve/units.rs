use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use semver::Version;

use crate::cargoless::manifest::{DepKind, PackageManifest};

use super::features::{FeatDep, FeatNode, NodeKey, decls_to_featdeps, edge_version, root_featdeps};
use super::{
    CRATES_IO_LOCK_SOURCE, EdgeVersions, PkgSource, Unit, UnitClass, UnitDep,
    identity_package_name, identity_source, registry_entry,
};

// ---------- unit assembly ----------

/// Minimal manifest read of a registry package (lib name/path/proc-macro/links/build.rs
/// presence/edition/CARGO_PKG_* inputs; no full subset parse, because registry crate
/// manifests make no promises. The Cargo.toml inside a .crate is cargo's normalized
/// output, so inherited keys such as edition already hold concrete values).
pub(super) struct RegistryMinimal {
    lib_name: String,
    pub(super) proc_macro: bool,
    links: Option<String>,
    has_build: bool,
    build_script_path: Option<PathBuf>,
    edition: String,
    lib_path: PathBuf,
    pkg_env: BTreeMap<String, String>,
    declared_features: BTreeSet<String>,
    rustc_lint_flags: Vec<String>,
}

pub(super) fn read_registry_minimal(
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
    let pkg_env = crate::cargoless::manifest::pkg_env_map(
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
    let rustc_lint_flags =
        crate::cargoless::manifest::parse_lints(v.get("lints")).map_err(|error| {
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
        Some(expr) => crate::cargoless::manifest::eval_cfg(expr),
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

pub(super) fn assemble_units(
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
pub(super) fn validate_compiler_rust_version(
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
