use std::collections::{BTreeMap, BTreeSet, VecDeque};

use semver::{Version, VersionReq};

use pubgrub::Reporter as _;

use crate::cargoless::lockfile::{LockedPkg, Lockfile};
use crate::cargoless::manifest::{DepKind, DepSource, IncompatibleRustVersions, PackageManifest};
use crate::cargoless::registry::{IndexEntry, IndexVersion};

use super::{
    CRATES_IO_LOCK_SOURCE, LOCAL_ID_SEPARATOR, PkgSource, SourceOverrides, UnitClass,
    identity_package_name, identity_source, local_dep_identity, local_package_name, registry_entry,
    registry_identity,
};

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

pub(super) fn dep_unit_class(kind: DepKind) -> UnitClass {
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
    d: &crate::cargoless::manifest::DepDecl,
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
pub(super) fn req_to_ranges(req: &VersionReq) -> pubgrub::Ranges<Version> {
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
pub(super) type Solved = (
    BTreeMap<String, Vec<Version>>, // name -> version set (several versions may coexist)
    Lockfile,                       // skeleton (dependency lines are filled after convergence)
    EdgeVersions,                   // the version each dep edge points at
);

pub(super) struct FreshSolveContext<'a> {
    pub(super) rust_version_policy: IncompatibleRustVersions,
    pub(super) resolver_rust_version: &'a Version,
    pub(super) overrides: &'a SourceOverrides,
}

pub(super) fn solve_fresh(
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
