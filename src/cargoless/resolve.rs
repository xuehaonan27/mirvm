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

// P1 逐切接入中：P2 切① 已接上 schedule/driver（设计档 §5）；个别字段的消费
// 归后续切片（就地 #[allow(dead_code)] 点名），不再整文件豁免。

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
    /// --extern 命名键：有 rename（`package = "..."`）用 rename key，否则用
    /// dep 包的 lib target 名（extern_key 的 cargo 语义出处见该函数注释）。
    pub key: String,
    pub unit: usize,
    /// 边类（normal/build）；切③ 起按边类分列消费（extern 过滤、build
    /// 闭包、build script --extern）。
    pub class: UnitClass,
    pub kind: DepKind,
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
    /// 本 unit 的类别（resolver v2 normal/build 分列的节点键成分；边类过滤
    /// 走 UnitDep.class，本字段是审计/模型面留存）。
    #[allow(dead_code)]
    pub class: UnitClass,
    pub features: BTreeSet<String>,
    /// Cargo 传给 rustc `--check-cfg cfg(feature, values(...))` 的声明全集；
    /// 与上面的本次启用集合是两件事。
    pub declared_features: BTreeSet<String>,
    pub proc_macro: bool,
    pub has_build_script: bool,
    /// `[package] build = "custom.rs"` 的自定义 build script 路径；
    /// None = 缺省 <source_dir>/build.rs（切③ build.rs 调度用）。
    pub build_script_path: Option<PathBuf>,
    /// -sys 链接键（切③：DEP_* 传播键与 links 互斥校验消费）。
    pub links: Option<String>,
    pub deps: Vec<UnitDep>,
    /// package.edition（registry 包缺省 "2015"）——dep rustc 参数用。
    pub edition: String,
    /// lib 根文件绝对路径（[lib] path 或缺省 src/lib.rs）——dep rustc 参数用。
    pub lib_path: PathBuf,
    /// CARGO_PKG_* env 全集（manifest::pkg_env_map 计算；dep 编译子进程 env）。
    pub pkg_env: BTreeMap<String, String>,
}

/// 解析结果。
#[derive(Clone, Debug)]
pub struct ResolvePlan {
    pub root_name: String,
    pub root_version: Version,
    /// 审计面字段（P1 对账工具留存；P2 driver 从 manifest 直取根目录）。
    #[allow(dead_code)]
    pub root_dir: PathBuf,
    pub root_features: BTreeSet<String>,
    pub units: Vec<Unit>,
    /// 根（bin）的 --extern 边表：根本身不是 unit，但 bin 会话需要同一套
    /// 依赖边（与 units 填边同一套门，assemble_units 产出）。
    pub root_deps: Vec<UnitDep>,
    /// name → 已解版本集（审计面：与 Cargo.lock 对账用）。
    pub version_map: BTreeMap<String, Vec<Version>>,
    /// fresh 模式 = 生成的 canonical lock；lock 模式 = 输入 lock 回显。
    pub lock: Lockfile,
}

/// 同一次 workspace 命令中，从其他根传播来的 resolver v2 feature 并集。
/// 键包含版本与 normal/build 类别，避免把 Cargo 明确分开的编译单元揉在一起。
pub type FeatureOverrides = BTreeMap<(String, Version, UnitClass), BTreeSet<String>>;

/// 根包当前用途只改变 dev 依赖是否进入构建图；版本求解和 Cargo.lock 始终
/// 看见根 dev 依赖，与 Cargo 在 `cargo build` 时也会锁定 dev 依赖的行为一致。
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

// ---------- 主入口 ----------

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

/// Workspace 版入口：`known_paths` 是已经完成 workspace 继承物化的成员清单。
/// path 边命中成员目录时必须复用它，不能重新读取仍含 `workspace = true` 的原文。
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
    let lock_path = root.lock_root.join("Cargo.lock");
    let input_lock = if lock_path.is_file() {
        Some(Lockfile::read(&lock_path)?)
    } else {
        None
    };

    // path 依赖 BFS（path 包及其传递 path 依赖的 manifest 全集）
    let mut path_manifests: BTreeMap<String, PackageManifest> = BTreeMap::new();
    let mut queue: VecDeque<(PackageManifest, bool)> = VecDeque::new();
    queue.push_back((clone_root_shallow(root), true));
    while let Some((m, is_root)) = queue.pop_front() {
        for d in m.deps.iter().filter(|d| is_root || d.kind != DepKind::Dev) {
            if let DepSource::Path(p) = &d.source
                && !path_manifests.contains_key(&d.package)
            {
                let absolute = std::fs::canonicalize(p)
                    .or_else(|_| std::path::absolute(p))
                    .unwrap_or_else(|_| p.clone());
                let pm = known_paths
                    .iter()
                    .find(|known| known.root == absolute)
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| PackageManifest::read_dir(&absolute))
                    .map_err(|e| {
                        format!("path 依赖 {}（{}）: {e}", d.package, absolute.display())
                    })?;
                path_manifests.insert(d.package.clone(), pm);
                queue.push_back((
                    clone_root_shallow(path_manifests.get(&d.package).unwrap()),
                    false,
                ));
            }
        }
    }

    // 版本求解 + feature 统一（两遍：resolve 图 = 强 ∪ 弱引用（lock/求解门），
    // 构建图 = 仅强边（units 的 feature 与可构建性，与 cargo build 图一致））
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
            // 迭代不动点：optional 依赖只在被（父包, 依赖键）激活或弱引用时
            // 进版本求解（cargo 语义——全局包名门会把 zerovec 的 yoke 误植到
            // litemap）；激活集单调扩张 ⇒ 收敛。
            let mut activated: BTreeSet<(String, Version, String)> = BTreeSet::new();
            loop {
                if std::env::var_os("MIRVM_DEBUG_UNIFY").is_some() {
                    eprintln!("DBG-SOLVE pass activated={activated:?}");
                }
                let (vm, lf, ev) = solve_fresh(root, &path_manifests, src, &activated)?;
                let (nodes, new_activated) = unify_features(
                    root,
                    &path_manifests,
                    &ev,
                    src,
                    true,
                    true,
                    workspace_features,
                )?;
                if std::env::var_os("MIRVM_DEBUG_UNIFY").is_some() {
                    eprintln!("DBG-SOLVE pass end new_activated={new_activated:?}");
                }
                if new_activated == activated {
                    // 收敛后才补 lock 依赖行：optional 门按（父包, 依赖键）
                    // 判定（全局集合会把 cipher 的 zeroize 误植到
                    // generic-array——chacha 实锤）
                    let mut lf = lf;
                    fill_lock_dependency_lines(
                        &mut lf,
                        root,
                        &path_manifests,
                        &vm,
                        &ev,
                        &nodes,
                        src,
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
                    break (vm, lf, nodes, build_nodes, ev);
                }
                activated = new_activated;
            }
        }
    };

    // 编译单元装配（构建图节点：仅强边激活面）+ 根的 --extern 边表
    let (units, root_deps) = assemble_units(
        root,
        &path_manifests,
        &edge_versions,
        &build_nodes,
        src,
        purpose.includes_dev(),
    )?;

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

// ---------- lock 模式 ----------

fn versions_from_lock(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    lf: &Lockfile,
) -> Result<(BTreeMap<String, Vec<Version>>, EdgeVersions), String> {
    let mut map: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    let mut edges: EdgeVersions = BTreeMap::new();
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
    let mut visited: BTreeSet<(String, Version)> = BTreeSet::new();
    let mut stack: Vec<&LockedPkg> = vec![root_locked];
    while let Some(pkg) = stack.pop() {
        for (depname, depver) in &pkg.dependencies {
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
            // 边版本记录：lock 依赖行不带 kind 信息——Normal/Build 双键登记，
            // 消歧器 = 子版本串（同名多 req 条目各 hint 分立，ruint 实锤）
            for class in [UnitClass::Normal, UnitClass::Build] {
                edges.insert(
                    (
                        pkg.name.clone(),
                        pkg.version.clone(),
                        depname.clone(),
                        child.version.to_string(),
                        class,
                    ),
                    (depname.clone(), child.version.clone()),
                );
            }
            // 同名多版本并存合法——visited 键必须含版本，否则后者被静默吃掉
            if visited.insert((depname.clone(), child.version.clone())) {
                stack.push(child);
            }
        }
    }
    // path 包（可能不在 lock 的可达集里——lock 含它们但没 source；BFS 已覆盖）
    for (name, m) in path_manifests {
        visited.insert((name.clone(), m.version.clone()));
    }
    for (name, v) in visited {
        let vs = map.entry(name).or_default();
        if !vs.contains(&v) {
            vs.push(v);
        }
    }
    // 完整性校验：manifest 的每个 registry req 必须被 locked 版本满足（lock 过期防线）
    let check = |m: &PackageManifest, include_dev: bool| -> Result<(), String> {
        for d in m
            .deps
            .iter()
            .filter(|d| include_dev || d.kind != DepKind::Dev)
        {
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
    check(root, true)?;
    for m in path_manifests.values() {
        check(m, false)?;
    }
    Ok((map, edges))
}

// ---------- fresh 模式（pubgrub + lazy-bucket 多版本） ----------
//
// pubgrub 标准模型是"每包一个版本"；cargo 允许同名多版本并存
// （hashbrown 0.14/0.15、syn 1/2/3 同图——boa/arkworks 实锤）。
// 表达法 = lazy-bucket：包 id = (name, bucket)，dep 边到达时若与某既有
// bucket 的累积区间存在共同候选（index 有版本同满足）则并入，否则开新
// bucket——恰为 cargo 的"可统一则统一、不可统一则并存"语义；
// 回退由 pubgrub 按 bucket 独立完成。

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
            Pkg::Local(n) => write!(f, "<path:{n}>"),
        }
    }
}

// pubgrub::Package 由 blanket impl（Clone+Eq+Hash+Debug+Display）自动满足。

/// 求解后每条 dep 边指向的版本：((父 lock 名, 父版本), 依赖键, 消歧器, 类别)
/// → (包, 版本)。消歧器：fresh 模式 = req 串（同名多 req 条目必须分立，
/// ruint 四个 ark-ff 系列实锤）；lock 模式 = 子版本串（dep 行 hint）。
pub type EdgeVersions = BTreeMap<(String, Version, String, String, UnitClass), (String, Version)>;

type RawDep = (String, String, VersionReq, UnitClass, bool); // (依赖键, 包名, req, 类别, 是否 registry)

/// (parent, dep key, class) → (pkg, bucket) 的边分派记录类型。
type EdgeAssign = BTreeMap<(String, Version, String, String, UnitClass), (String, u32)>;

fn dep_unit_class(kind: DepKind) -> UnitClass {
    match kind {
        DepKind::Normal | DepKind::Dev => UnitClass::Normal,
        DepKind::Build => UnitClass::Build,
    }
}
/// get_dependencies 幂等 memo 值类型。
type DepsRc = std::rc::Rc<Vec<(Pkg, pubgrub::Ranges<Version>)>>;

/// pre comparator 记录类型（(major, minor, patch) 三元组列表）。
type PreComparators = Vec<(u64, Option<u64>, Option<u64>)>;

struct CratesIo<'a, S: PkgSource> {
    src: std::cell::RefCell<&'a mut S>,
    manifests: &'a BTreeMap<String, PackageManifest>,
    root: &'a PackageManifest,
    /// pre comparator 记录（cargo 精确规则：pre 版仅当被该包某 req 中
    /// major/minor/patch 全同且带 pre 的 comparator 点名时才可选——
    /// ark-ff-asm 0.5.0-alpha.0 误选实锤）。值 = (major, minor, patch)。
    allow_pre: std::cell::RefCell<BTreeMap<String, PreComparators>>,
    /// 本轮按（父包名, 依赖键）激活的 optional 依赖集（迭代不动点输入；
    /// 全局包名集合会把 zerovec 的 yoke 误植到 litemap——boa 实锤）。
    activated: &'a BTreeSet<(String, Version, String)>,
    /// name → 下一个 bucket 号（0 起）。
    buckets: std::cell::RefCell<BTreeMap<String, u32>>,
    /// (name, bucket) → 截至目前的累积约束（合并判断用）。
    bucket_ranges: std::cell::RefCell<BTreeMap<(String, u32), pubgrub::Ranges<Version>>>,
    /// (parent, dep key, class) → (pkg, bucket)：边分派记录（幂等依赖 memo 的副产）。
    edge_assign: std::cell::RefCell<EdgeAssign>,
    /// get_dependencies 幂等 memo（同一 (P,V) 多次调用必须返回同一份分派）。
    deps_memo: std::cell::RefCell<BTreeMap<(Pkg, Version), DepsRc>>,
}

fn req_has_pre(req: &VersionReq) -> bool {
    req.comparators.iter().any(|c| !c.pre.is_empty())
}

impl<'a, S: PkgSource> CratesIo<'a, S> {
    /// pre 版放行判定（cargo 精确规则）：该包存在带 pre 且 major/minor/
    /// patch 全同的 comparator 时才放行该 pre 版。
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

    /// 候选判定：index 中存在非 yanked 版本满足 range（pre 规则同 choose_version）。
    fn any_candidate(
        &self,
        name: &str,
        range: &pubgrub::Ranges<Version>,
    ) -> Result<bool, std::io::Error> {
        let vs = self.src.borrow_mut().index_entry(name).map_err(io_err)?;
        Ok(vs
            .iter()
            .any(|v| !v.yanked && range.contains(&v.version) && self.pre_allowed(name, &v.version)))
    }

    /// 边分派：req 与既有 bucket 的累积区间有共同候选 → 并入；否则开新 bucket。
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

    fn parent_lock_name(&self, package: &Pkg) -> String {
        match package {
            Pkg::Root => self.root.name.clone(),
            Pkg::Local(n) => n.clone(),
            Pkg::Registry(n, _) => n.clone(),
        }
    }

    /// (P,V) 的原始依赖清单（幂等 memo + 边分派记录）。
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
                let m = self
                    .manifests
                    .get(name)
                    .ok_or_else(|| io_err(format!("path 包 {name} 的 manifest 未收编")))?;
                for d in m.deps.iter().filter(|d| d.kind != DepKind::Dev) {
                    collect_decl(name, &m.version, d, self.activated, self, &mut raw)?;
                }
            }
            Pkg::Registry(name, _) => {
                let vs = self.src.borrow_mut().index_entry(name).map_err(io_err)?;
                let Some(iv) = vs.iter().find(|v| v.version == *version) else {
                    return Err(io_err(format!("{name} {version} 不在 index")));
                };
                for d in &iv.deps {
                    if d.kind.as_deref() == Some("dev") {
                        continue; // dev 边不求（事先明说）
                    }
                    let pkg_name = d.package.clone().unwrap_or_else(|| d.name.clone());
                    // optional 未激活不求（按（父包名, 依赖键）门控；平台 cfg 不求值——并集语义）
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
                // path 依赖：bucket 0 同式登记（bucket_versions 由 solve 输出端补）
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

/// 单条 manifest 依赖进 raw（root 与 path 包共用；registry/path 以末位标记区分）。
fn collect_decl<'a, S: PkgSource>(
    parent_name: &str,
    parent_version: &Version,
    d: &super::manifest::DepDecl,
    activated: &BTreeSet<(String, Version, String)>,
    provider: &CratesIo<'a, S>,
    raw: &mut Vec<RawDep>,
) -> Result<(), std::io::Error> {
    // optional 未激活不求（按（父包名, 依赖键）门控；平台 cfg 同样不求值——并集语义）
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
        DepSource::Registry(req) => {
            if req_has_pre(req) {
                for c in &req.comparators {
                    if !c.pre.is_empty() {
                        provider
                            .allow_pre
                            .borrow_mut()
                            .entry(d.package.clone())
                            .or_default()
                            .push((c.major, c.minor, c.patch));
                    }
                }
            }
            raw.push((
                d.key.clone(),
                d.package.clone(),
                req.clone(),
                dep_unit_class(d.kind),
                true,
            ));
        }
        DepSource::Path(_) => {
            raw.push((
                d.key.clone(),
                d.package.clone(),
                VersionReq::STAR,
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
            Pkg::Registry(name, _) => {
                let vs = self.src.borrow_mut().index_entry(name).map_err(io_err)?;
                Ok(vs
                    .iter()
                    .filter(|v| {
                        !v.yanked
                            && range.contains(&v.version)
                            && self.pre_allowed(name, &v.version)
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
        if let Pkg::Registry(name, _) = package {
            let vs = self.src.borrow_mut().index_entry(name).map_err(io_err)?;
            if !vs.iter().any(|v| v.version == *version) {
                return Ok(pubgrub::Dependencies::Unavailable(format!(
                    "{name} {version} 不在 index"
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

/// req → Ranges 转换（镜像 version_ranges::semver 算法，但**保留下界 pre**：
/// `^0.6.0-rc.8` = [0.6.0-rc.8, 0.7.0)——from_req 丢 pre 得 [0.6.0, 0.7.0)，
/// semver 序 0.6.0-rc.x < 0.6.0 ⇒ rc 族全被误杀（argon2 实锤）；
/// 上界永不带 pre；pre comparator 之外的 comparator 与 from_req 等价）。
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
                // "=M.m.p" = [M.m.p, M.m.(p+1))：build 元数据任意（libgit2-sys
                // 0.18.5+1.9.4 实锤——singleton 按 semver crate Ord 会比 build，
                // 空 build 的钉子把带 build 的候选全顶出去）
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
                    // semver 新 op（本仓钉版未知）——退回 from_req（pre 会丢，响亮记账）
                    return pubgrub::Ranges::from_req(req.clone());
                }
            };
        acc = acc.intersection(&r);
    }
    acc
}

/// fresh 求解产物。
type Solved = (
    BTreeMap<String, Vec<Version>>, // name → 版本集（多版本并存）
    Lockfile,                       // 骨架（依赖行收敛后补）
    EdgeVersions,                   // 每条 dep 边指向的版本
);

fn solve_fresh(
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    src: &mut impl PkgSource,
    activated: &BTreeSet<(String, Version, String)>,
) -> Result<Solved, String> {
    let provider = CratesIo {
        src: std::cell::RefCell::new(src),
        manifests: path_manifests,
        root,
        allow_pre: std::cell::RefCell::new(BTreeMap::new()),
        activated,
        buckets: std::cell::RefCell::new(BTreeMap::new()),
        bucket_ranges: std::cell::RefCell::new(BTreeMap::new()),
        edge_assign: std::cell::RefCell::new(BTreeMap::new()),
        deps_memo: std::cell::RefCell::new(BTreeMap::new()),
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
    // 出解集合（bucket → 版本）与全量边分派
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
    // 可达集过滤：pubgrub 决定历史可能残留"父包已被回退换版"的孤儿 bucket
    // （boa 的 yoke#1 实锤）——只收从根沿"父版本恰为出解版本"的边可达的
    // bucket；孤儿不进 version_map/lock/edge_versions。
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
        format_version: 4,
        packages: vec![LockedPkg {
            name: root.name.clone(),
            version: root.version.clone(),
            source: None,
            checksum: None,
            dependencies: vec![],
        }],
    };
    for ((name, bucket), version) in &bucket_versions {
        if !reachable.contains(&(name.clone(), *bucket)) {
            continue;
        }
        version_map
            .entry(name.clone())
            .or_default()
            .push(version.clone());
        let (source, checksum) = if path_manifests.contains_key(name) {
            (None, None)
        } else {
            let cksum = provider
                .src
                .borrow_mut()
                .index_entry(name)
                .ok()
                .and_then(|vs| {
                    vs.iter()
                        .find(|v| v.version == *version)
                        .map(|v| v.cksum.clone())
                });
            (
                Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
                cksum,
            )
        };
        lock.packages.push(LockedPkg {
            name: name.clone(),
            version: version.clone(),
            source,
            checksum,
            dependencies: vec![],
        });
    }
    for vs in version_map.values_mut() {
        vs.sort();
        vs.dedup();
    }
    // 边分派解析为具体版本（(父名, 父版本, 依赖键, 消歧器, 类别) → (包, 版本)）；
    // 只收可达父 + 可达子的边（其余 = 回退剪枝副产，arkworks/boa 实锤）。
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
        edge_versions.insert(
            (
                pname.clone(),
                pver.clone(),
                key.clone(),
                dis.clone(),
                *class,
            ),
            (pkg.clone(), v.clone()),
        );
    }
    // lock 依赖行在 feature 统一收敛后补齐（见 resolve() 迭代循环——
    // optional 门按（父包, 依赖键）判定，需要统一产物）
    Ok((version_map, lock, edge_versions))
}

/// 给生成的 lock 补 dependencies 行（name + 重名消歧 version）；
/// optional 未激活不入（cargo lock 语义）。
fn fill_lock_dependency_lines(
    lock: &mut Lockfile,
    root: &PackageManifest,
    path_manifests: &BTreeMap<String, PackageManifest>,
    version_map: &BTreeMap<String, Vec<Version>>,
    edge_versions: &EdgeVersions,
    nodes: &BTreeMap<NodeKey, FeatNode>,
    src: &mut impl PkgSource,
) -> Result<(), String> {
    // optional 门按（父包, 父版本, 依赖键）判定：全局集合会把 A 包激活的
    // 同名依赖误植到 B 包（cipher/zeroize vs generic-array 实锤）；
    // 弱形引用（?/）同样放行（cargo 语义：yoke/serde?/alloc 实锤）
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
    // (pkg name, version) → [(dep package, hint)]（同名多 req 条目各带
    // 自身 hint 分立——ruint 四个 ark-ff 系列实锤；hint 经 req 精确查边分派）
    type EdgeLines = BTreeMap<(String, Version), Vec<(String, Option<Version>)>>;
    let mut edges: EdgeLines = BTreeMap::new();
    let hint_of = |parent: &NodeKey,
                   key: &str,
                   pkg_name: &str,
                   req: Option<&VersionReq>,
                   class: UnitClass|
     -> Option<Version> {
        if !version_map
            .get(pkg_name)
            .map(|vs| vs.len() > 1)
            .unwrap_or(false)
        {
            return None;
        }
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
        };
        edge_version(edge_versions, parent, &dep).map(|(_, v)| v.clone())
    };
    edges.insert(
        (root.name.clone(), root.version.clone()),
        root.deps
            .iter()
            .filter(|d| {
                !d.optional || activated_keys(&root.name, &root.version).contains(&d.key.as_str())
            })
            .map(|d| {
                let class = dep_unit_class(d.kind);
                let req = match &d.source {
                    DepSource::Registry(r) => Some(r),
                    DepSource::Path(_) => None,
                };
                let parent = (root.name.clone(), root.version.clone(), class);
                (
                    d.package.clone(),
                    hint_of(&parent, &d.key, &d.package, req, class),
                )
            })
            .collect(),
    );
    for (name, m) in path_manifests {
        edges.insert(
            (name.clone(), m.version.clone()),
            m.deps
                .iter()
                .filter(|d| d.kind != DepKind::Dev)
                .filter(|d| {
                    !d.optional || activated_keys(name, &m.version).contains(&d.key.as_str())
                })
                .map(|d| {
                    let class = dep_unit_class(d.kind);
                    let req = match &d.source {
                        DepSource::Registry(r) => Some(r),
                        DepSource::Path(_) => None,
                    };
                    let parent = (name.clone(), m.version.clone(), class);
                    (
                        d.package.clone(),
                        hint_of(&parent, &d.key, &d.package, req, class),
                    )
                })
                .collect(),
        );
    }
    for (name, versions) in version_map {
        if path_manifests.contains_key(name) || name == &root.name {
            continue;
        }
        for version in versions {
            let vs = src.index_entry(name)?;
            let iv = vs
                .iter()
                .find(|v| v.version == *version)
                .ok_or_else(|| format!("{name} {version} 不在 index"))?;
            edges.insert(
                (name.clone(), version.clone()),
                iv.deps
                    .iter()
                    .filter(|d| d.kind.as_deref() != Some("dev"))
                    .filter(|d| {
                        !d.optional || activated_keys(name, version).contains(&d.name.as_str())
                    })
                    .map(|d| {
                        let pkg_name = d.package.clone().unwrap_or_else(|| d.name.clone());
                        let class = if d.kind.as_deref() == Some("build") {
                            UnitClass::Build
                        } else {
                            UnitClass::Normal
                        };
                        let parent = (name.clone(), version.clone(), class);
                        (
                            pkg_name.clone(),
                            hint_of(&parent, &d.name, &pkg_name, Some(&d.req), class),
                        )
                    })
                    .collect(),
            );
        }
    }
    for pkg in lock.packages.iter_mut() {
        let mut lines: Vec<(String, Option<Version>)> = edges
            .get(&(pkg.name.clone(), pkg.version.clone()))
            .cloned()
            .unwrap_or_default();
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
    /// 被启用 feature 以 ?/ 弱形引用的依赖键（不激活，但进求解与 lock 行）。
    weak_refs: BTreeSet<String>,
}

/// 一条参与 feature 传播的依赖边。
/// platform_cfg：平台 cfg 表达式——**解析/统一期不求值（全平台并集），
/// 只在装配构建图（assemble_units）时按 host 求值**（cargo lock ∪ 构建图 分裂语义）。
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
    /// req 串（同名多 req 条目的边消歧器；path 依赖为 None）
    req: Option<String>,
}

type FeatTable = BTreeMap<String, Vec<FeatureValue>>;
/// 统一节点键：(包, 版本, 类别)——多版本并存时 feature 表与边按精确版本区分。
type NodeKey = (String, Version, UnitClass);
type NodeTables = BTreeMap<NodeKey, (FeatTable, Vec<FeatDep>)>;

/// 边版本查询：先精确类别命中，miss 时回退另一类别（lock 模式双键登记/
/// fresh 模式按 kind 精确登记；root 与 path 的 manifest kind 总是精确）。
fn edge_version<'a>(
    ev: &'a EdgeVersions,
    parent: &NodeKey,
    dep: &FeatDep,
) -> Option<&'a (String, Version)> {
    let other = match dep.class {
        UnitClass::Normal => UnitClass::Build,
        UnitClass::Build => UnitClass::Normal,
    };
    // fresh：req 串精确键（同名多 req 条目按串分立）
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
    // lock：扫描命中——子版本（消歧器串）落在 req 区间内即配；
    // 名字按 依赖键 或 真包名 双路匹配（rename/下划线键实锤：
    // rustix libc_errno → libc-errno、grep-searcher memmap → memmap2）
    let range = dep
        .req
        .as_deref()
        .unwrap_or("*")
        .parse::<VersionReq>()
        .ok()
        .map(|r| req_to_ranges(&r));
    ev.iter()
        .find(|((pn, pv, k, _dis, c), (_, dv))| {
            pn == &parent.0
                && pv == &parent.1
                && (k == &dep.key || k == &dep.package)
                && (*c == dep.class || *c == other)
                && range.as_ref().is_none_or(|r| r.contains(dv))
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
        // 平台 cfg 在此不求值（并集语义；target 表达式随行，装配期求值）
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

/// feature 统一产物：(节点表, 按（父包名, 依赖键）激活的 optional 依赖集)。
type Unified = (
    BTreeMap<NodeKey, FeatNode>,
    BTreeSet<(String, Version, String)>,
);

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
    // 根 feature 来自 CLI 选择；未指定 --no-default-features 时才启用 default。
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
    for (key, features) in workspace_features {
        if let Some(node) = nodes.get_mut(key) {
            node.features.extend(features.iter().cloned());
        }
    }

    // 全局不动点迭代（节点/边规模有界，单调收敛）
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
            // 三路产物任一变化都要落表——weak_refs 漏插会静悄悄地丢
            // ?/ 弱引用（tracing-core 的 valuable?/std 实锤）
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
            // 边传播
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
                // 子节点身份：边分派记录（多版本并存时按 (父,键,类) 精确到版本）
                let Some((child_name, child_version)) = edge_version(edge_versions, &key, dep)
                else {
                    // optional 本轮新激活、下轮求解才进边分派——本轮跳过
                    // （激活已记账进 activated_pkgs，迭代不动点会补）
                    if dep.optional {
                        continue;
                    }
                    return Err(format!(
                        "{}@{} 的依赖 {} 无边分派记录（内部不一致）",
                        key.0, key.1, dep.key
                    ));
                };
                let child_key = (child_name.clone(), child_version.clone(), dep.class);
                // registry 子节点按需注册
                if dep.registry && !tables.contains_key(&child_key) {
                    ensure_registry_node(
                        &mut tables,
                        &mut nodes,
                        src,
                        child_name,
                        child_version,
                        dep.class,
                    )?;
                    // 新注册节点本身也是变化——否则 adds 为空时提前收敛，
                    // 其子图永远不展开（syn/quote 实锤）
                    changed = true;
                }
                if let Some(seed) = workspace_features.get(&child_key) {
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
        if !changed {
            // 收敛后：按（父包名, 依赖键）激活的 optional 依赖集 ∪ 弱形引用集
            // （迭代不动点输入；弱形引用同样进求解与 lock 行——cargo 语义）
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
    Err("feature 统一 64 轮未收敛（图异常）".to_string())
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
            registry: matches!(d.source, DepSource::Registry(_)),
            platform_cfg: d.platform_cfg.clone(),
            req: match &d.source {
                DepSource::Registry(req) => Some(req.to_string()),
                DepSource::Path(_) => None,
            },
        })
        .collect())
}

/// 包内 feature 展开（含隐式/显式激活与强弱边规则）。
/// 返回 (features, activated, edge_adds[key → features], weak_refs[被启用
/// feature 以 ?/ 弱形引用的依赖键])。weak_refs 不激活依赖，但该依赖进
/// 版本求解与 lock 依赖行（cargo 语义：被启用 feature 的 ?/ 弱形引用会
/// 把被引用包收进解析图与 lock 行——boa 的 yoke/serde?/alloc 实锤）。
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

    if let Some(feature) = node
        .features
        .iter()
        .find(|feature| !table.contains_key(*feature) && !optional_keys.contains(*feature))
    {
        return Err(format!("请求了不存在的 feature `{feature}`"));
    }
    if let Some(feature) = node.features.iter().find(|feature| {
        !table.contains_key(*feature)
            && optional_keys.contains(*feature)
            && hidden.contains(*feature)
    }) {
        return Err(format!(
            "feature `{feature}` 已被 dep:{feature} 隐藏，不能作为隐式 feature 启用"
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
                            // 隐式 feature 被引用 = 同名 feature 旗同时启用
                            // （cargo 同——serde facade 的
                            // #[cfg(feature = "serde_derive")] 实锤；dep:
                            // 显式形只激活依赖、不加同名旗）
                            if features.insert(g.clone()) {
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
                        // 强形 x/y 激活可选依赖 = 同名 feature 旗同时启用：
                        // - x 有显式 feature 定义（table 键在场）→ 启用该显式
                        //   feature，哪怕 x 已被 dep: 遮蔽——遮蔽只杀隐式
                        //   feature，杀不了显式定义（zerotrie 0.2.4 实锤：
                        //   serde = [dep:litemap, litemap/serde, …] 且
                        //   litemap = [dep:litemap, alloc] 显式在场 ⇒ cargo
                        //   实证 cfg feature="litemap" 置位，serde.rs 调的
                        //   try_from_serde_litemap 所在
                        //   #[cfg(feature = "litemap")] impl 块随之进场）；
                        // - 无显式定义且未被 dep: 遮蔽 → 视同指定其同名隐式
                        //   feature（cargo 同——k256 的 ecdsa-core/signing ⇒
                        //   #[cfg(feature = "ecdsa-core")] 模块实锤）。
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
                        // 弱形引用（?/）：包含 feature 已启用 ⇒ 被引用依赖进
                        // 解析图（不激活），且其特征照常下发（rust_decimal
                        // std → borsh?/std → bytes?/std 级联实锤——
                        // cargo 的 resolve 图语义，与 build 图的"激活才下发"不同）
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
    // 收尾清扫：feature 旗标从边/表任意来源到达后，若它本身不是表键而是
    // 非隐藏的 optional 依赖键，即激活该依赖（rand_core 的 getrandom 实锤——
    // 旗标经边到达且无表项可展开，缺这条规则时激活丢失）
    for f in features.iter() {
        if !table.contains_key(f) && optional_keys.contains(f) && !hidden.contains(f) {
            activated.insert(f.clone());
        }
    }
    Ok((features, activated, edge_adds, weak_refs))
}

// ---------- 单元装配 ----------

/// registry 包的最小 manifest 读取（lib 名/path/proc-macro/links/build.rs 实存/
/// edition/CARGO_PKG_* 原料；不跑全量子集解析——registry crate 的 manifest
/// 形态不设防；.crate 内的 Cargo.toml 是 cargo 归一化产物，edition 等继承键
/// 已是具体值）。
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
}

fn read_registry_minimal(
    dir: &Path,
    package: &str,
    version: &Version,
) -> Result<RegistryMinimal, String> {
    let file = dir.join("Cargo.toml");
    let text =
        std::fs::read_to_string(&file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    let v: toml::Value =
        toml::from_str(&text).map_err(|e| format!("{} 解析失败: {e}", file.display()))?;
    let pkg_table = v.get("package");
    let lib = v.get("lib");
    let name = lib
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| package.replace('-', "_"));
    let proc_macro = lib
        .and_then(|l| l.get("proc-macro"))
        .or_else(|| lib.and_then(|l| l.get("proc_macro"))) // 新归一化下划线形态（derive_arbitrary 实锤）
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
    // cargo 语义：`build = false` 是显式关闭（cfg-if 实锤——键在场 ≠ 有 build.rs）
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
    // CARGO_PKG_* 原料（与 manifest.rs 根/path 包同一 pkg_env_map 计算）
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
    })
}

/// 节点的依赖边再取（path 用 manifest，registry 用 index 按精确版本重拉）。
fn node_featdeps(
    name: &str,
    version: &Version,
    path_manifests: &BTreeMap<String, PackageManifest>,
    src: &mut impl PkgSource,
) -> Result<Vec<FeatDep>, String> {
    if let Some(m) = path_manifests.get(name) {
        return decls_to_featdeps(&m.deps);
    }
    let vs = src.index_entry(name)?;
    let iv = vs
        .iter()
        .find(|v| v.version == *version)
        .ok_or_else(|| format!("{name} {version} 不在 index"))?;
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
        })
        .collect())
}

/// 该边是否进 host 构建图（platform_cfg 按 host 求值）。
fn host_edge(dep: &FeatDep) -> Result<bool, String> {
    match &dep.platform_cfg {
        None => Ok(true),
        Some(expr) => super::manifest::eval_cfg(expr),
    }
}

/// --extern 命名键（cargo 语义）：有 rename（toml `package = "..."` / index
/// `package` 键 ⇒ key ≠ package）用 rename key；无 rename 用 dep 包的
/// **lib target 名**——lib 名 ≠ 包名时 extern 名跟 lib 名走
/// （tendril → new_debug_unreachable（[lib] name = "debug_unreachable"）实锤：
/// cargo -v 实证行拼 `--extern debug_unreachable=…/libdebug_unreachable-….rmeta`；
/// new_debug_unreachable 整包存在意义即"lib 名叫 debug_unreachable 的再发布"）。
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
    // 可构建集：从根出发沿 host cfg 为真的边可达（cargo 构建图过滤——
    // 版本/lock 是全平台并集，构建图按 host 求值；serde facade 系那种
    // cfg(any()) 永假边引的子图只进 lock 不进构建图）。
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
                    "{}@{} 的依赖 {} 无边分派记录（内部不一致）",
                    key.0, key.1, dep.key
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
            continue; // 根本身不是 dep 单元；不可构建子图只进 lock
        }
        if let Some(m) = path_manifests.get(name) {
            let (lib_name, lib_path) = m
                .targets
                .iter()
                .find(|t| t.is_lib())
                .map(|t| (t.name.clone(), t.path.clone()))
                // 无 lib 目标的 path 依赖是病理包（cargo 同拒）——回退缺省路径，
                // dep 编译期 rustc 报文件不存在（响亮，不静默吞）
                .unwrap_or_else(|| (name.replace('-', "_"), m.root.join("src/lib.rs")));
            let proc_macro = m.targets.iter().any(|t| t.is_lib() && t.proc_macro);
            index.insert((name.clone(), version.clone(), *class), units.len());
            units.push(Unit {
                package: name.clone(),
                lib_name,
                version: m.version.clone(),
                source_dir: m.root.clone(),
                from_registry: false,
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
            });
            continue;
        }
        let dir = src.ensure_source(name, version, None)?;
        let rm = read_registry_minimal(&dir, name, version)?;
        index.insert((name.clone(), version.clone(), *class), units.len());
        units.push(Unit {
            package: name.clone(),
            lib_name: rm.lib_name,
            version: version.clone(),
            source_dir: dir,
            from_registry: true,
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
        });
    }
    // 依赖边填充（可构建节点 × host 为真边 × optional 激活门）
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
                    "{}@{} 的依赖 {} 无边分派记录（内部不一致）",
                    key.0, key.1, d.key
                ));
            };
            let Some(&to_idx) = index.get(&(child_name.clone(), child_version.clone(), d.class))
            else {
                continue; // 子节点不可构建（永假边子图）→ 边不存在
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
    // 根的 --extern 边表：根本身不是 unit（见上「根本身不是 dep 单元」注释），
    // 但 bin 会话的 --extern 闭包要与 units 填边同一套门（optional 激活门 +
    // host 平台求值 + 边分派版本）；只补边表，根不进 units。
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
                    "{}@{} 的依赖 {} 无边分派记录（内部不一致）",
                    root_key.0, root_key.1, d.key
                ));
            };
            let Some(&to_idx) = index.get(&(child_name.clone(), child_version.clone(), d.class))
            else {
                continue; // 子节点不可构建（永假边子图）→ 边不存在
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

// ---------- 测试（离线；FakeSource 罐头 index + tempdir 源） ----------

#[cfg(test)]
mod tests {
    use super::super::manifest::PackageManifest;
    use super::super::registry::IndexDep;
    use super::*;

    struct FakeSource {
        root: PathBuf,
        index: BTreeMap<String, Vec<IndexVersion>>,
        /// 包名 → 显式 `[lib] name`（lib 名 ≠ 包名的 extern 命名场景罐头，
        /// new_debug_unreachable 实锤形态）。
        lib_names: BTreeMap<String, String>,
    }

    impl FakeSource {
        fn new(root: PathBuf) -> Self {
            std::fs::create_dir_all(&root).unwrap();
            Self {
                root,
                index: BTreeMap::new(),
                lib_names: BTreeMap::new(),
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
    fn extern_key_prefers_rename_then_lib_name() {
        // cargo 语义（tendril 0.5.1 → new_debug_unreachable 1.0.6 实锤）：
        // --extern 名 = rename key（有 rename 时），否则 dep 包的 [lib] name。
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
        // 无 rename：extern 名跟 lib 名走，不是包名
        assert_eq!(key_of(ndu_ix), "debug_unreachable");
        // 有 rename：rename key 优先
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
        let mut opt2 = iv("opt2", "1.0.0");
        opt2.features.insert("inner".into(), vec![]);
        src.add("opt2", vec![opt2]);

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
    fn implicit_optional_feature_also_sets_cfg_flag() {
        // serde facade 形状：feature derive = ["se_derive"]（裸名引用可选
        // 依赖，无 dep: 形）——激活依赖的同时**同名 feature 旗启用**
        // （cargo 同：serde 的 #[cfg(feature = "serde_derive")] 实锤）。
        // 对照：dep: 显式形只激活依赖，不加同名旗。
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
            "隐式 feature 旗必须进 cfg 集: {:?}",
            se_unit.features
        );
        assert!(plan.units.iter().any(|u| u.package == "se_derive"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn strong_dep_feature_also_sets_implicit_cfg_flag() {
        // k256 形状：feature ecdsa = ["ecdsa-core/signing"]（强形引用可选
        // 依赖）——激活依赖并下发 signing 的同时，同名隐式 feature 旗
        // ecdsa-core 启用（cargo 同：k256 的
        // #[cfg(feature = "ecdsa-core")] pub mod ecdsa 实锤；缺旗 =
        // E0433 cannot find ecdsa in k256，corpus smoke 分诊实锤）。
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
            "强形激活的可选依赖其隐式 feature 旗必须进 cfg 集: {:?}",
            k2_unit.features
        );
        let ec_unit = plan
            .units
            .iter()
            .find(|u| u.package == "ecdsa-core")
            .unwrap();
        assert!(ec_unit.features.contains("signing"), "x/y 的 y 照常下发");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn strong_dep_enables_same_named_explicit_feature_even_when_dep_hidden() {
        // zerotrie 0.2.4 形状：optional 依赖 litemap 被 dep: 遮蔽（隐式
        // feature 不生成），但同名显式 feature litemap = [dep:litemap, alloc]
        // 在场；serde = [dep:serde_core, dep:litemap, alloc, litemap/serde]。
        // cargo 实证（zerotrie features=["serde"]）：cfg 集含 litemap——强形
        // litemap/serde 把同名**显式** feature 一并启用（dep: 遮蔽只杀隐式
        // feature）；缺旗 = E0599 try_from_serde_litemap 不在
        // #[cfg(feature = "litemap")] impl 块里（typst_pdf 分诊实锤）。
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
            "强形 x/y 必须启用同名显式 feature（dep: 遮蔽只杀隐式）: {:?}",
            zt_unit.features
        );
        assert!(
            zt_unit.features.contains("alloc"),
            "显式 litemap feature 展开带 alloc: {:?}",
            zt_unit.features
        );
        assert!(
            plan.units.iter().any(|u| u.package == "litemap"),
            "litemap 依赖本体被激活进 units"
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

    #[test]
    fn multi_version_fork_coexists_with_per_bucket_features() {
        // cargo 多版本语义：a→h ^0.14、b→h ^0.15 无共同候选 ⇒ 两版并存
        // （hashbrown 0.14/0.15 实锤）；各自 feature 表按 bucket 独立展开
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
        // lock：两版皆在；a/b 依赖行带消歧 hint
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
            a_line.contains(&("h".to_string(), Some(Version::parse("0.14.5").unwrap()))),
            "a 行: {a_line:?}"
        );
        assert!(
            b_line.contains(&("h".to_string(), Some(Version::parse("0.15.2").unwrap()))),
            "b 行: {b_line:?}"
        );
        // lock 可被自家 parser 回读
        let back = Lockfile::parse(&plan.lock.serialize()).unwrap();
        assert_eq!(back.packages.len(), plan.lock.packages.len());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn prerelease_req_allows_pre_candidates() {
        // req 带 pre comparator ⇒ 该包的 pre 版进候选（cargo 近似规则；
        // argon2 = "0.6.0-rc.8" 实锤——rc 族全被 pre 过滤器误杀过）
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
        // 复刻 syn 3.0.3 的真实 index 形状（features2 的 dep:quote）
        // + serde_derive 的 dep 形状：quote 必须进 syn 的 lock 依赖行
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
            .map(|(n, _)| n.clone())
            .collect();
        assert!(
            line.contains(&"quote".to_string()),
            "syn 依赖行缺 quote: {line:?}"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn req_to_ranges_less_bound_is_exact() {
        // ">=2.0.4, <3" = [2.0.4, 3.0.0)——Less 臂曾错给 <4.0.0（brotli 实锤）
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
        // "=0.18.5" 必须匹配 0.18.5+1.9.4（build 任意；semver crate 的 Ord
        // 会比 build，singleton 会误杀——libgit2-sys 实锤）
        let req = VersionReq::parse("=0.18.5").unwrap();
        let r = req_to_ranges(&req);
        assert!(r.contains(&Version::parse("0.18.5").unwrap()));
        assert!(r.contains(&Version::parse("0.18.5+1.9.4").unwrap()));
        assert!(!r.contains(&Version::parse("0.18.6").unwrap()));
    }

    #[test]
    fn registry_minimal_accepts_both_proc_macro_spellings() {
        // crates.io 归一化产物两种拼写并存：连字符（serde_derive 1.0.228，
        // 老归一化）与下划线（derive_arbitrary 1.3.2，新归一化）——cargo
        // 双侧受理，漏一种 = proc-macro 误当 target dep 编（tokei 实锤）
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
            assert_eq!(rm.proc_macro, want, "拼写 {key} 必须识别");
        }
        // 缺席 = false（普通 lib）
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
