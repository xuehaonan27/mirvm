//! `cargoless/resolve.rs` —— 版本求解 + feature 统一 → 编译单元图（D15 P1，设计档 §3.5）。
//!
//! 双模式（设计档 §5 P1 闭合契约）：
//! - **lock 模式**（项目带 Cargo.lock）：版本全按 lock（yanked 照吃，cargo 同）；
//!   附带完整性校验——manifest req 不被 locked 版本满足即响亮报错（lock 过期）。
//! - **fresh 模式**（frontmatter 脚本/无 lock）：pubgrub 对 sparse index 求解
//!   （yanked 跳过；prerelease 按 cargo 近似规则——任一依赖方 req 带 pre
//!   comparator 才放行，比 cargo 的 per-major.minor.patch 规则宽一档，记账），
//!   解出生成 canonical Cargo.lock（供复现与 cargo --locked 反证）。
//!
//! feature 统一 = resolver v2 语义子集：**normal 边与 build 边分列**（同一 crate
//! 两类 feature 集不同 = 两个编译单元）、dev 边整体不求、optional 三形态
//! （隐式 feature / dep: 显式 / ?/ 弱 + / 强强弱规则）、default_features 边规则。
//! registry crate 的 feature/dep 元数据取自 sparse index（cargo 同），
//! lib 名/proc-macro/links/build.rs 实存从解包源的**最小 manifest 读取**拿
//! （不跑全量子集解析——registry crate manifest 形态不设防）。

// P1 逐切接入中：schedule/audit 后续切片接入后摘除本 allow（设计档 §5）。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use semver::{Version, VersionReq};

use super::lockfile::{LockedPkg, Lockfile};
use super::manifest::{DepKind, DepSource, FeatureValue, PackageManifest, parse_feature_value};
use super::registry::{IndexVersion, Registry};
use pubgrub::Reporter as _;

// ---------- 供给面抽象（生产 = Registry；测试 = 内存 fake） ----------

pub trait PkgSource {
    fn index_entry(&mut self, name: &str) -> Result<Vec<IndexVersion>, String>;
    fn ensure_source(
        &mut self,
        name: &str,
        version: &Version,
        cksum: Option<&str>,
    ) -> Result<PathBuf, String>;
}

impl PkgSource for Registry {
    fn index_entry(&mut self, name: &str) -> Result<Vec<IndexVersion>, String> {
        Registry::index_entry(self, name)
    }
    fn ensure_source(
        &mut self,
        name: &str,
        version: &Version,
        cksum: Option<&str>,
    ) -> Result<PathBuf, String> {
        Registry::ensure_source(self, name, version, cksum)
    }
}

// ---------- 输出模型 ----------

/// 编译单元类别（resolver v2 的 normal/build 分列）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnitClass {
    Normal,
    Build,
}

/// 一条已解依赖边（指向 units 下标）。
#[derive(Clone, Debug)]
pub struct UnitDep {
    /// --extern 命名键（rename key）。
    pub key: String,
    pub unit: usize,
    pub class: UnitClass,
}

/// 一个编译单元（包 × 类别 × feature 集）。
#[derive(Clone, Debug)]
pub struct Unit {
    pub package: String,
    /// lib 目标名（--extern 文件名端；无 lib 时同 package）。
    pub lib_name: String,
    pub version: Version,
    pub source_dir: PathBuf,
    pub from_registry: bool,
    pub class: UnitClass,
    pub features: BTreeSet<String>,
    pub proc_macro: bool,
    pub has_build_script: bool,
    pub links: Option<String>,
    pub deps: Vec<UnitDep>,
}

/// 解析结果。
#[derive(Clone, Debug)]
pub struct ResolvePlan {
    pub root_name: String,
    pub root_version: Version,
    pub root_dir: PathBuf,
    pub root_features: BTreeSet<String>,
    pub units: Vec<Unit>,
    /// name → 已解版本集（审计面：与 Cargo.lock 对账用）。
    pub version_map: BTreeMap<String, Vec<Version>>,
    /// fresh 模式 = 生成的 canonical lock；lock 模式 = 输入 lock 回显。
    pub lock: Lockfile,
}

// ---------- 主入口 ----------

pub fn resolve(root: &PackageManifest, src: &mut impl PkgSource) -> Result<ResolvePlan, String> {
    let lock_path = root.root.join("Cargo.lock");
    let input_lock = if lock_path.is_file() {
        Some(Lockfile::read(&lock_path)?)
    } else {
        None
    };

    // path 依赖 BFS（path 包及其传递 path 依赖的 manifest 全集）
    let mut path_manifests: BTreeMap<String, PackageManifest> = BTreeMap::new();
    let mut queue: VecDeque<PackageManifest> = VecDeque::new();
    queue.push_back(clone_root_shallow(root));
    while let Some(m) = queue.pop_front() {
        for d in &m.deps {
            if let DepSource::Path(p) = &d.source
                && !path_manifests.contains_key(&d.package)
            {
                let pm = PackageManifest::read_dir(p)
                    .map_err(|e| format!("path 依赖 {}（{}）: {e}", d.package, p.display()))?;
                path_manifests.insert(d.package.clone(), pm);
                queue.push_back(clone_root_shallow(path_manifests.get(&d.package).unwrap()));
            }
        }
    }

    // 版本求解
    let (version_map, out_lock) = match &input_lock {
        Some(lf) => {
            let vm = versions_from_lock(root, &path_manifests, lf)?;
            (vm, lf.clone())
        }
        None => {
            let (vm, lf) = solve_fresh(root, &path_manifests, src)?;
            (vm, lf)
        }
    };

    // feature 统一
    let nodes = unify_features(root, &path_manifests, &version_map, src)?;

    // 编译单元装配
    let units = assemble_units(root, &path_manifests, &version_map, &nodes, src)?;

    Ok(ResolvePlan {
        root_name: root.name.clone(),
        root_version: root.version.clone(),
        root_dir: root.root.clone(),
        root_features: nodes
            .get(&(root.name.clone(), UnitClass::Normal))
            .map(|n| n.features.clone())
            .unwrap_or_default(),
        units,
        version_map,
        lock: out_lock,
    })
}

fn clone_root_shallow(m: &PackageManifest) -> PackageManifest {
    m.clone()
}

// ---------- lock 模式 ----------

fn versions_from_lock(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    lf: &Lockfile,
) -> Result<BTreeMap<String, Vec<Version>>, String> {
    let mut map: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    // 从 root 的 lock 行出发走图（root 包本身必在 lock 中）
    let root_locked = lf
        .packages
        .iter()
        .find(|p| p.name == root.name && p.version == root.version)
        .ok_or_else(|| {
            format!(
                "Cargo.lock 无根包 {} {}——lock 与 manifest 脱节（重新解析或删 lock）",
                root.name, root.version
            )
        })?;
    let mut visited: BTreeMap<String, Version> = BTreeMap::new();
    let mut stack: Vec<&LockedPkg> = vec![root_locked];
    while let Some(pkg) = stack.pop() {
        for (depname, depver) in &pkg.dependencies {
            if visited.contains_key(depname) {
                continue;
            }
            let candidates = lf.find(depname);
            let child = match (depver, candidates.len()) {
                (Some(v), _) => candidates.iter().find(|p| p.version == *v).copied(),
                (None, 1) => candidates.first().copied(),
                (None, _) => {
                    return Err(format!(
                        "lock 图歧义：{depname} 有 {} 个版本且父行未指版",
                        candidates.len()
                    ));
                }
            };
            let Some(child) = child else {
                return Err(format!("lock 图断链：{depname} {depver:?} 找不到包行"));
            };
            visited.insert(depname.clone(), child.version.clone());
            stack.push(child);
        }
    }
    // path 包（可能不在 lock 的可达集里——lock 含它们但没 source；BFS 已覆盖）
    for (name, m) in path_manifests {
        visited
            .entry(name.clone())
            .or_insert_with(|| m.version.clone());
    }
    for (name, v) in visited {
        map.entry(name).or_default().push(v);
    }
    // 完整性校验：manifest 的每个 registry req 必须被 locked 版本满足（lock 过期防线）
    let check = |m: &PackageManifest| -> Result<(), String> {
        for d in &m.deps {
            if let DepSource::Registry(req) = &d.source {
                let satisfied = map
                    .get(&d.package)
                    .is_some_and(|vs| vs.iter().any(|v| req.matches(v)));
                if !satisfied {
                    return Err(format!(
                        "Cargo.lock 过期：{} 的 locked 版本不满足 req {req}（manifest 变了，重解或删 lock）",
                        d.package
                    ));
                }
            }
        }
        Ok(())
    };
    check(root)?;
    for m in path_manifests.values() {
        check(m)?;
    }
    Ok(map)
}

// ---------- fresh 模式（pubgrub） ----------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Pkg {
    Root,
    Registry(String),
    Local(String),
}

impl std::fmt::Display for Pkg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pkg::Root => write!(f, "<root>"),
            Pkg::Registry(n) => write!(f, "{n}"),
            Pkg::Local(n) => write!(f, "<path:{n}>"),
        }
    }
}

// pubgrub::Package 由 blanket impl（Clone+Eq+Hash+Debug+Display）自动满足。

struct CratesIo<'a, S: PkgSource> {
    src: std::cell::RefCell<&'a mut S>,
    manifests: &'a BTreeMap<String, PackageManifest>,
    root: &'a PackageManifest,
    allow_pre: std::cell::RefCell<BTreeSet<String>>,
}

fn req_has_pre(req: &VersionReq) -> bool {
    req.comparators.iter().any(|c| !c.pre.is_empty())
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
            Pkg::Registry(name) => self
                .src
                .borrow_mut()
                .index_entry(name)
                .map(|vs| {
                    vs.iter()
                        .filter(|v| !v.yanked && range.contains(&v.version))
                        .count()
                })
                .unwrap_or(0),
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
            Pkg::Registry(name) => {
                let vs = self.src.borrow_mut().index_entry(name).map_err(io_err)?;
                let allow_pre = self.allow_pre.borrow();
                Ok(vs
                    .iter()
                    .filter(|v| {
                        !v.yanked
                            && range.contains(&v.version)
                            && (v.version.pre.is_empty() || allow_pre.contains(name))
                    })
                    .map(|v| v.version.clone())
                    .max())
            }
        }
    }

    fn get_dependencies(
        &self,
        package: &Self::P,
        version: &Self::V,
    ) -> Result<pubgrub::Dependencies<Self::P, Self::VS, Self::M>, Self::Err> {
        let mut out: Vec<(Pkg, pubgrub::Ranges<Version>)> = Vec::new();
        match package {
            Pkg::Root => {
                dep_decls_to_constraints(&self.root.deps, &mut out, self)?;
            }
            Pkg::Local(name) => {
                let m = self
                    .manifests
                    .get(name)
                    .ok_or_else(|| io_err(format!("path 包 {name} 的 manifest 未收编")))?;
                dep_decls_to_constraints(&m.deps, &mut out, self)?;
            }
            Pkg::Registry(name) => {
                let vs = self.src.borrow_mut().index_entry(name).map_err(io_err)?;
                let Some(iv) = vs.iter().find(|v| v.version == *version) else {
                    return Ok(pubgrub::Dependencies::Unavailable(format!(
                        "{name} {version} 不在 index"
                    )));
                };
                for d in &iv.deps {
                    if d.kind.as_deref() == Some("dev") {
                        continue; // dev 边不求（事先明说）
                    }
                    if let Some(target) = &d.target {
                        let hit = super::manifest::eval_cfg(target).map_err(io_err)?;
                        if !hit {
                            continue;
                        }
                    }
                    if req_has_pre(&d.req) {
                        self.allow_pre
                            .borrow_mut()
                            .insert(d.package.clone().unwrap_or_else(|| d.name.clone()));
                    }
                    let pkg_name = d.package.clone().unwrap_or_else(|| d.name.clone());
                    out.push((
                        Pkg::Registry(pkg_name),
                        pubgrub::Ranges::from_req(d.req.clone()),
                    ));
                }
            }
        }
        Ok(pubgrub::Dependencies::Available(out.into_iter().collect()))
    }
}

fn dep_decls_to_constraints<'a, S: PkgSource>(
    decls: &[super::manifest::DepDecl],
    out: &mut Vec<(Pkg, pubgrub::Ranges<Version>)>,
    provider: &CratesIo<'a, S>,
) -> Result<(), std::io::Error> {
    for d in decls {
        match &d.source {
            DepSource::Registry(req) => {
                if req_has_pre(req) {
                    provider.allow_pre.borrow_mut().insert(d.package.clone());
                }
                out.push((
                    Pkg::Registry(d.package.clone()),
                    pubgrub::Ranges::from_req(req.clone()),
                ));
            }
            DepSource::Path(_) => {
                out.push((Pkg::Local(d.package.clone()), pubgrub::Ranges::full()));
            }
        }
    }
    Ok(())
}

fn io_err(e: impl Into<String>) -> std::io::Error {
    std::io::Error::other(e.into())
}

fn solve_fresh(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    src: &mut impl PkgSource,
) -> Result<(BTreeMap<String, Vec<Version>>, Lockfile), String> {
    let provider = CratesIo {
        src: std::cell::RefCell::new(src),
        manifests: path_manifests,
        root,
        allow_pre: std::cell::RefCell::new(BTreeSet::new()),
    };
    let selected = pubgrub::resolve(&provider, Pkg::Root, root.version.clone()).map_err(|e| {
        format!(
            "依赖求解失败（pubgrub）: {}",
            match e {
                pubgrub::PubGrubError::NoSolution(derivation) => {
                    pubgrub::DefaultStringReporter::report(&derivation)
                }
                other => format!("{other}"),
            }
        )
    })?;
    let mut version_map: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    let mut lock = Lockfile {
        format_version: 4,
        packages: vec![LockedPkg {
            name: root.name.clone(),
            version: root.version.clone(),
            source: None,
            checksum: None,
            dependencies: vec![],
        }],
    };
    for (pkg, version) in selected {
        match pkg {
            Pkg::Root => {}
            Pkg::Local(name) => {
                version_map
                    .entry(name.clone())
                    .or_default()
                    .push(version.clone());
                lock.packages.push(LockedPkg {
                    name: name.clone(),
                    version,
                    source: None,
                    checksum: None,
                    dependencies: vec![],
                });
            }
            Pkg::Registry(name) => {
                version_map
                    .entry(name.clone())
                    .or_default()
                    .push(version.clone());
                // cksum 从 index 取（lock 校验链）
                let cksum = provider
                    .src
                    .borrow_mut()
                    .index_entry(&name)
                    .ok()
                    .and_then(|vs| {
                        vs.iter()
                            .find(|v| v.version == version)
                            .map(|v| v.cksum.clone())
                    });
                lock.packages.push(LockedPkg {
                    name,
                    version,
                    source: Some(
                        "registry+https://github.com/rust-lang/crates.io-index".to_string(),
                    ),
                    checksum: cksum,
                    dependencies: vec![],
                });
            }
        }
    }
    // lock 依赖行补齐（canonical 形态要求；审计反证时 cargo 需要）
    let mut guard = provider.src.borrow_mut();
    fill_lock_dependency_lines(&mut lock, root, path_manifests, &version_map, &mut **guard)?;
    Ok((version_map, lock))
}

/// 给生成的 lock 补 dependencies 行（name + 重名消歧 version）。
fn fill_lock_dependency_lines(
    lock: &mut Lockfile,
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    version_map: &BTreeMap<String, Vec<Version>>,
    provider: &mut impl PkgSource,
) -> Result<(), String> {
    // (pkg name, its dep keys → package names)
    let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    edges.insert(
        root.name.clone(),
        root.deps.iter().map(|d| d.package.clone()).collect(),
    );
    for (name, m) in path_manifests {
        edges.insert(
            name.clone(),
            m.deps.iter().map(|d| d.package.clone()).collect(),
        );
    }
    let registry_names: Vec<String> = version_map
        .keys()
        .filter(|n| !path_manifests.contains_key(*n) && *n != &root.name)
        .cloned()
        .collect();
    for name in registry_names {
        let version = version_map[&name][0].clone();
        let vs = provider.index_entry(&name)?;
        let iv = vs
            .iter()
            .find(|v| v.version == version)
            .ok_or_else(|| format!("{name} {version} 不在 index"))?;
        edges.insert(
            name.clone(),
            iv.deps
                .iter()
                .filter(|d| d.kind.as_deref() != Some("dev"))
                .map(|d| d.package.clone().unwrap_or_else(|| d.name.clone()))
                .collect(),
        );
    }
    for pkg in lock.packages.iter_mut() {
        let mut lines: Vec<(String, Option<Version>)> = edges
            .get(&pkg.name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|dep| {
                let vs = version_map.get(&dep).cloned().unwrap_or_default();
                let hint = if vs.len() > 1 {
                    vs.first().cloned()
                } else {
                    None
                };
                (dep, hint)
            })
            .collect();
        lines.sort();
        lines.dedup();
        pkg.dependencies = lines;
    }
    Ok(())
}

// ---------- feature 统一 ----------

#[derive(Clone, Debug, Default)]
struct FeatNode {
    features: BTreeSet<String>,
    /// 本包内被激活的 optional 依赖键。
    activated: BTreeSet<String>,
}

/// 一条参与 feature 传播的依赖边。
#[derive(Clone, Debug)]
struct FeatDep {
    key: String,
    package: String,
    class: UnitClass,
    optional: bool,
    default_features: bool,
    features: Vec<String>,
    registry: bool,
}

type FeatTable = BTreeMap<String, Vec<FeatureValue>>;
type NodeTables = BTreeMap<(String, UnitClass), (FeatTable, Vec<FeatDep>)>;

fn register_node(
    tables: &mut NodeTables,
    nodes: &mut BTreeMap<(String, UnitClass), FeatNode>,
    name: &str,
    class: UnitClass,
    table: FeatTable,
    deps: Vec<FeatDep>,
) {
    tables.insert((name.to_string(), class), (table, deps));
    nodes.entry((name.to_string(), class)).or_default();
}

fn ensure_registry_node(
    tables: &mut NodeTables,
    nodes: &mut BTreeMap<(String, UnitClass), FeatNode>,
    src: &mut impl PkgSource,
    name: &str,
    class: UnitClass,
    version: &Version,
) -> Result<(), String> {
    if tables.contains_key(&(name.to_string(), class)) {
        return Ok(());
    }
    let vs = src.index_entry(name)?;
    let iv = vs
        .iter()
        .find(|v| v.version == *version)
        .ok_or_else(|| format!("{name} {version} 不在 index"))?;
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
        if let Some(target) = &d.target
            && !super::manifest::eval_cfg(target)?
        {
            continue;
        }
        deps.push(FeatDep {
            key: d.name.clone(),
            package: d.package.clone().unwrap_or_else(|| d.name.clone()),
            class: if d.kind.as_deref() == Some("build") {
                UnitClass::Build
            } else {
                UnitClass::Normal
            },
            optional: d.optional,
            default_features: d.default_features,
            features: d.features.clone(),
            registry: true,
        });
    }
    register_node(tables, nodes, name, class, table, deps);
    Ok(())
}

fn unify_features(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    version_map: &BTreeMap<String, Vec<Version>>,
    src: &mut impl PkgSource,
) -> Result<BTreeMap<(String, UnitClass), FeatNode>, String> {
    let mut tables: NodeTables = BTreeMap::new();
    let mut nodes: BTreeMap<(String, UnitClass), FeatNode> = BTreeMap::new();

    register_node(
        &mut tables,
        &mut nodes,
        &root.name,
        UnitClass::Normal,
        root.features.clone(),
        decls_to_featdeps(&root.deps),
    );
    // 根 default feature 启用（cargo run 语义）
    if root.features.contains_key("default") {
        nodes
            .get_mut(&(root.name.clone(), UnitClass::Normal))
            .unwrap()
            .features
            .insert("default".to_string());
    }
    for (name, m) in path_manifests {
        let deps = decls_to_featdeps(&m.deps);
        for class in [UnitClass::Normal, UnitClass::Build] {
            register_node(
                &mut tables,
                &mut nodes,
                name,
                class,
                m.features.clone(),
                deps.clone(),
            );
        }
    }

    // 全局不动点迭代（节点/边规模有界，单调收敛）
    for _pass in 0..64 {
        let mut changed = false;
        let keys: Vec<(String, UnitClass)> = nodes.keys().cloned().collect();
        for key in keys {
            let Some((table, deps)) = tables.get(&key).cloned() else {
                continue;
            };
            let node = nodes.get(&key).cloned().unwrap_or_default();
            let (features, activated, edge_adds) = expand_node(&table, &deps, &node)?;
            if features != node.features || activated != node.activated {
                nodes.insert(
                    key.clone(),
                    FeatNode {
                        features: features.clone(),
                        activated: activated.clone(),
                    },
                );
                changed = true;
            }
            // 边传播
            for dep in &deps {
                if dep.optional && !activated.contains(&dep.key) {
                    continue;
                }
                let child_key = (dep.package.clone(), dep.class);
                // registry 子节点按需注册
                if dep.registry && !tables.contains_key(&child_key) {
                    let version = version_map
                        .get(&dep.package)
                        .and_then(|vs| vs.first())
                        .ok_or_else(|| format!("{} 引用的 {} 无已解版本", key.0, dep.package))?
                        .clone();
                    ensure_registry_node(
                        &mut tables,
                        &mut nodes,
                        src,
                        &dep.package,
                        dep.class,
                        &version,
                    )?;
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
        if !changed {
            return Ok(nodes);
        }
    }
    Err("feature 统一 64 轮未收敛（图异常）".to_string())
}

fn decls_to_featdeps(decls: &[super::manifest::DepDecl]) -> Vec<FeatDep> {
    decls
        .iter()
        .map(|d| FeatDep {
            key: d.key.clone(),
            package: d.package.clone(),
            class: match d.kind {
                DepKind::Normal => UnitClass::Normal,
                DepKind::Build => UnitClass::Build,
            },
            optional: d.optional,
            default_features: d.default_features,
            features: d.features.clone(),
            registry: matches!(d.source, DepSource::Registry(_)),
        })
        .collect()
}

/// 包内 feature 展开（含隐式/显式激活与强弱边规则）。
/// 返回 (features, activated, edge_adds[key → features])。
type Expanded = (
    BTreeSet<String>,
    BTreeSet<String>,
    BTreeMap<String, BTreeSet<String>>,
);

fn expand_node(
    table: &BTreeMap<String, Vec<FeatureValue>>,
    deps: &[FeatDep],
    node: &FeatNode,
) -> Result<Expanded, String> {
    // 隐式 feature 规则：optional 依赖键在任何 dep: 中出现则不再生成隐式 feature
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

    let mut features = node.features.clone();
    let mut activated = node.activated.clone();
    let mut edge_adds: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
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
                        } else {
                            // cargo 同语义：feature 引用必须指向另一 feature 或可选依赖
                            return Err(format!(
                                "feature 引用 {g} 既非 feature 亦非可选依赖（manifest/index 数据非法）"
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
                        edge_adds
                            .entry(dep.clone())
                            .or_default()
                            .insert(feature.clone());
                    }
                    FeatureValue::WeakDep { dep, feature } => {
                        if activated.contains(dep) {
                            edge_adds
                                .entry(dep.clone())
                                .or_default()
                                .insert(feature.clone());
                        }
                    }
                }
            }
        }
        if !changed {
            return Ok((features, activated, edge_adds));
        }
    }
    Err("feature 展开 64 轮未收敛（表异常）".to_string())
}

// ---------- 单元装配 ----------

/// registry 包的最小 manifest 读取（lib 名/proc-macro/links/build.rs；
/// 不跑全量子集解析——registry crate 的 manifest 形态不设防）。
fn read_registry_minimal(
    dir: &Path,
    package: &str,
) -> Result<(String, bool, Option<String>, bool), String> {
    let file = dir.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    let v: toml::Value =
        toml::from_str(&text).map_err(|e| format!("{} 解析失败: {e}", file.display()))?;
    let lib = v.get("lib");
    let name = lib
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| package.replace('-', "_"));
    let proc_macro = lib
        .and_then(|l| l.get("proc-macro"))
        .and_then(|p| p.as_bool())
        .unwrap_or(false);
    let links = v
        .get("package")
        .and_then(|p| p.get("links"))
        .and_then(|l| l.as_str())
        .map(str::to_string);
    let has_build = dir.join("build.rs").is_file()
        || v.get("package").and_then(|p| p.get("build")).is_some()
        || links.is_some();
    Ok((name, proc_macro, links, has_build))
}

fn assemble_units(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    version_map: &BTreeMap<String, Vec<Version>>,
    nodes: &BTreeMap<(String, UnitClass), FeatNode>,
    src: &mut impl PkgSource,
) -> Result<Vec<Unit>, String> {
    let mut units: Vec<Unit> = Vec::new();
    let mut index: BTreeMap<(String, UnitClass), usize> = BTreeMap::new();
    for ((name, class), node) in nodes {
        if name == &root.name {
            continue; // 根本身不是 dep 单元
        }
        if let Some(m) = path_manifests.get(name) {
            let lib_name = m
                .targets
                .iter()
                .find_map(|t| match t {
                    super::manifest::Target::Lib { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| name.replace('-', "_"));
            let proc_macro = m.targets.iter().any(
                |t| matches!(t, super::manifest::Target::Lib { proc_macro, .. } if *proc_macro),
            );
            index.insert((name.clone(), *class), units.len());
            units.push(Unit {
                package: name.clone(),
                lib_name,
                version: m.version.clone(),
                source_dir: m.root.clone(),
                from_registry: false,
                class: *class,
                features: node.features.clone(),
                proc_macro,
                has_build_script: m.has_build_script,
                links: m.links.clone(),
                deps: vec![],
            });
            continue;
        }
        let version = version_map
            .get(name)
            .and_then(|vs| vs.first())
            .ok_or_else(|| format!("{name} 无已解版本（节点存在但版本图缺席）"))?
            .clone();
        let dir = src.ensure_source(name, &version, None)?;
        let (lib_name, proc_macro, links, has_build) = read_registry_minimal(&dir, name)?;
        index.insert((name.clone(), *class), units.len());
        units.push(Unit {
            package: name.clone(),
            lib_name,
            version,
            source_dir: dir,
            from_registry: true,
            class: *class,
            features: node.features.clone(),
            proc_macro,
            has_build_script: has_build,
            links,
            deps: vec![],
        });
    }
    // 依赖边填充（与节点表同规则再算一遍 decls）
    // 收集每个节点的激活态
    let mut edge_rows: Vec<(usize, UnitDep)> = Vec::new();
    for ((name, class), node) in nodes {
        if name == &root.name {
            continue;
        }
        let Some(&from_idx) = index.get(&(name.clone(), *class)) else {
            continue;
        };
        // 该节点的依赖声明（path 用 manifest，registry 用 index 重拉）
        let deps: Vec<FeatDep> = if let Some(m) = path_manifests.get(name) {
            decls_to_featdeps(&m.deps)
        } else {
            let version = &units[from_idx].version;
            let vs = src.index_entry(name)?;
            let iv = vs
                .iter()
                .find(|v| v.version == *version)
                .ok_or_else(|| format!("{name} {version} 不在 index"))?;
            iv.deps
                .iter()
                .filter(|d| d.kind.as_deref() != Some("dev"))
                .filter(|d| {
                    d.target
                        .as_ref()
                        .is_none_or(|t| super::manifest::eval_cfg(t).unwrap_or(false))
                })
                .map(|d| FeatDep {
                    key: d.name.clone(),
                    package: d.package.clone().unwrap_or_else(|| d.name.clone()),
                    class: if d.kind.as_deref() == Some("build") {
                        UnitClass::Build
                    } else {
                        UnitClass::Normal
                    },
                    optional: d.optional,
                    default_features: d.default_features,
                    features: d.features.clone(),
                    registry: true,
                })
                .collect()
        };
        for d in deps {
            if d.optional && !node.activated.contains(&d.key) {
                continue;
            }
            if d.class != *class {
                // resolver v2：节点的依赖按自己类别走（normal 节点的 normal 边、
                // build 节点的 build 边？——否：边类别由边自身 kind 决定）
            }
            let Some(&to_idx) = index.get(&(d.package.clone(), d.class)) else {
                // 子节点缺席（optional 未激活或平台过滤）→ 边不存在
                continue;
            };
            edge_rows.push((
                from_idx,
                UnitDep {
                    key: d.key.clone(),
                    unit: to_idx,
                    class: d.class,
                },
            ));
        }
    }
    for (from, edge) in edge_rows {
        units[from].deps.push(edge);
    }
    Ok(units)
}

// ---------- 测试（离线；FakeSource 罐头 index + tempdir 源） ----------

#[cfg(test)]
mod tests {
    use super::super::manifest::PackageManifest;
    use super::super::registry::IndexDep;
    use super::*;

    struct FakeSource {
        root: PathBuf,
        index: BTreeMap<String, Vec<IndexVersion>>,
    }

    impl FakeSource {
        fn new(root: PathBuf) -> Self {
            std::fs::create_dir_all(&root).unwrap();
            Self {
                root,
                index: BTreeMap::new(),
            }
        }
        fn add(&mut self, name: &str, versions: Vec<IndexVersion>) {
            self.index.insert(name.to_string(), versions);
        }
    }

    impl PkgSource for FakeSource {
        fn index_entry(&mut self, name: &str) -> Result<Vec<IndexVersion>, String> {
            Ok(self.index.get(name).cloned().unwrap_or_default())
        }
        fn ensure_source(
            &mut self,
            name: &str,
            version: &Version,
            _cksum: Option<&str>,
        ) -> Result<PathBuf, String> {
            let dir = self.root.join(format!("{name}-{version}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
            )
            .unwrap();
            Ok(dir)
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
        PackageManifest::parse(manifest, d).unwrap()
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
        // a@1.2.0 在 index 已 yanked——lock 模式照吃（cargo 同）
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
        // feature 统一：a 的 default → std 被启用
        let a_unit = plan.units.iter().find(|u| u.package == "a").unwrap();
        assert!(a_unit.features.contains("default"));
        assert!(a_unit.features.contains("std"));
        assert!(!a_unit.from_registry || a_unit.source_dir.ends_with("a-1.2.0"));
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
        assert!(err.contains("lock 过期") || err.contains("过期"), "{err}");
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
        // [1.2,1.4) 区间 max = 1.3.5（1.4.0 yanked 跳过；1.5.0 不满足 <1.4）
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
        // lock 可被自家 parser 回读且依赖行存在
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
        // m：default 含 dep:opt 显式激活；opt 的 of → deep
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
        // s：normal 边（default）与 build 边（features=["b"]）分列
        let mut s = iv("s", "1.0.0");
        s.features.insert("default".into(), vec![]);
        s.features.insert("b".into(), vec![]);
        src.add("s", vec![s]);
        // w：弱激活 opt2?/inner——opt2 未被激活 → opt2 单元缺席
        let mut w = iv("w", "1.0.0");
        w.features
            .insert("default".into(), vec!["opt2?/inner".into()]);
        let mut opt2 = idep("opt2", "1");
        opt2.optional = true;
        w.deps.push(opt2);
        src.add("w", vec![w]);
        src.add("opt2", vec![iv("opt2", "1.0.0")]);

        let plan = resolve(&root, &mut src).unwrap();
        let m_unit = plan.units.iter().find(|u| u.package == "m").unwrap();
        assert!(m_unit.features.contains("default"));
        // dep:opt 激活了 opt，且 opt 拿到边 features of → of/deep 展开
        let opt_unit = plan.units.iter().find(|u| u.package == "opt").unwrap();
        assert!(opt_unit.features.contains("of"));
        assert!(opt_unit.features.contains("deep"));
        // s 两列：Normal（default）与 Build（b）不同 feature 集 = 两个单元
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
        // 弱激活未触发：opt2 无单元
        assert!(!plan.units.iter().any(|u| u.package == "opt2"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn path_dep_joins_solve_and_units() {
        let d = tmpdir("pathdep");
        let sibling = d.join("sibling");
        std::fs::create_dir_all(sibling.join("src")).unwrap();
        std::fs::write(
            sibling.join("Cargo.toml"),
            "[package]\nname = \"sib\"\nversion = \"0.2.0\"\n[dependencies]\nr = \"1\"\n",
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
        // lock 生成：sib 无 source 行
        let sib_lock = plan
            .lock
            .get("sib", &Version::parse("0.2.0").unwrap())
            .unwrap();
        assert!(sib_lock.source.is_none());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
