use std::collections::{BTreeMap, BTreeSet};

use semver::{Version, VersionReq};

use crate::cargoless::manifest::{
    DepKind, DepSource, FeatureValue, PackageManifest, ResolverVersion, parse_feature_value,
};

use super::fresh::{dep_unit_class, req_to_ranges};
use super::{EdgeVersions, FeatureOverrides, PkgSource, UnitClass, registry_entry};

// ---------- feature unification ----------

#[derive(Clone, Debug, Default)]
pub(super) struct FeatNode {
    pub(super) features: BTreeSet<String>,
    /// Optional dependency keys activated inside this package.
    pub(super) activated: BTreeSet<String>,
    /// Dependency keys weakly referenced (?/) by an enabled feature (not activated, but
    /// they enter resolution and the lock lines).
    pub(super) weak_refs: BTreeSet<String>,
}

/// One dependency edge that takes part in feature propagation.
/// platform_cfg is the platform cfg expression: **it is not evaluated while parsing or
/// unifying (union over all platforms); it is evaluated against the host only when the
/// build graph is assembled (`assemble_units`)** -- cargo's split between lock semantics
/// and the build graph.
#[derive(Clone, Debug)]
pub(super) struct FeatDep {
    pub(super) key: String,
    pub(super) package: String,
    pub(super) class: UnitClass,
    pub(super) kind: DepKind,
    pub(super) optional: bool,
    pub(super) default_features: bool,
    pub(super) features: Vec<String>,
    pub(super) registry: bool,
    pub(super) platform_cfg: Option<String>,
    /// req string (the edge disambiguator for entries with several reqs for one name;
    /// `None` for a path dependency)
    pub(super) req: Option<String>,
    /// Git manifest source id (without the precise commit), so lock mode can tell apart
    /// same-name same-version sources.
    pub(super) source_id: Option<String>,
}

type FeatTable = BTreeMap<String, Vec<FeatureValue>>;
/// Unification node key: (package, version, class) -- with several versions in play the
/// feature table and edges are distinguished by exact version.
pub(super) type NodeKey = (String, Version, UnitClass);
type NodeTables = BTreeMap<NodeKey, (FeatTable, Vec<FeatDep>)>;

/// Edge version lookup: an exact class hit first, falling back to the other class on a
/// miss (lock mode registers under both keys, fresh mode registers exactly by kind; the
/// kind from the root and path manifests is always exact).
pub(super) fn edge_version<'a>(
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
    if crate::options::get().debug_unify {
        eprintln!("DBG-UNIFY register {name} {class:?} v{version}");
    }
    Ok(())
}

/// The product of feature unification: (node table, the set of optional dependencies
/// activated by (parent package name, dependency key)).
pub(super) type Unified = (
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

pub(super) fn unify_features(
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
            if crate::options::get().debug_unify {
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

pub(super) fn decls_to_featdeps(
    decls: &[crate::cargoless::manifest::DepDecl],
) -> Result<Vec<FeatDep>, String> {
    featdeps(decls, false)
}

pub(super) fn root_featdeps(
    decls: &[crate::cargoless::manifest::DepDecl],
    include_dev: bool,
) -> Result<Vec<FeatDep>, String> {
    featdeps(decls, include_dev)
}

fn featdeps(
    decls: &[crate::cargoless::manifest::DepDecl],
    include_dev: bool,
) -> Result<Vec<FeatDep>, String> {
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
