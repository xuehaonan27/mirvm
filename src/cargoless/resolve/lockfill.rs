use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};

use crate::cargoless::lockfile::{LockedDep, LockedPkg, Lockfile, UnusedPatch};
use crate::cargoless::manifest::{DepKind, DepSource, PackageManifest};

use super::features::{FeatDep, FeatNode, NodeKey, edge_version};
use super::fresh::dep_unit_class;
use super::{
    CRATES_IO_LOCK_SOURCE, EdgeVersions, PkgSource, SourceOverrides, UnitClass,
    identity_package_name, identity_source, registry_entry, registry_identity,
};

pub(super) fn fill_unused_patches(
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
pub(super) fn fill_lock_dependency_lines(
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

pub(super) fn lock_dependency_source(source: &str) -> String {
    if source.starts_with("git+") {
        source.rsplit_once('#').map(|(id, _)| id).unwrap_or(source)
    } else {
        source
    }
    .to_string()
}
