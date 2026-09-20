//! `cargoless/schedule.rs` — topological ordering, content fingerprints and per-crate
//! rustc argument computation.
//!
//! Artifact layout: `cache_dir()/target/cargoless/<MIRVM_HOST>/debug/{deps,host-deps,build}`
//! — target artifacts (`deps`) and host artifacts (`host-deps`: the proc-macro closure and
//! the build-deps closure, compiled by real rustc with real codegen) live in separate
//! directories so identical fps do not collide; `build/` holds the build script family
//! (`build/<pkg>-<fp>/{build_script_build-<fp>,out}`). This coexists with the cargo path's
//! `target/mirvm` artifacts, and the two paths never overwrite each other.
//! Artifact names are `lib<lib_name>-<fp>.{rmeta,rlib,so}`; the fp scheme is local to this
//! file (cargo's -C metadata algorithm is not stable enough to mimic, and cargo has left the
//! process, so internal consistency is all that matters).

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use super::manifest::{PackageManifest, ProfileFlags};
use super::resolve::{ResolvePlan, Unit, UnitClass};

mod args;

pub use args::*;

/// Artifact layout (see the module header).
pub struct Layout {
    /// Target artifact directory (__cless-dep, -Zno-codegen rlib).
    pub deps: PathBuf,
    /// Host artifact directory (proc-macro closure and build-deps closure, real rustc codegen).
    pub host_deps: PathBuf,
    /// Build script family root (`build/<pkg>-<fp>/{build_script_build-<fp>,out}`).
    pub build_root: PathBuf,
}

impl Layout {
    pub fn new() -> Self {
        let base = crate::store::TARGET
            .dir()
            .join("cargoless")
            .join(crate::options::build::HOST)
            .join("debug");
        Self {
            deps: base.join("deps"),
            host_deps: base.join("host-deps"),
            build_root: base.join("build"),
        }
    }

    /// Explicit layout (used when the sysroot manages its own artifacts): `deps` points at
    /// the flat sysroot lib directory while host artifacts and the build script family go to
    /// a separate staging area (host binaries and host rlibs are not target artifacts).
    pub fn at(deps: PathBuf, host_deps: PathBuf, build_root: PathBuf) -> Self {
        Self {
            deps,
            host_deps,
            build_root,
        }
    }

    /// Working directory of one package's build script (its compiled artifacts and OUT_DIR
    /// live under it).
    pub fn build_dir(&self, pkg: &str, fp: &str) -> PathBuf {
        self.build_root.join(format!("{pkg}-{fp}"))
    }
}

/// Dependency graph: forward indegree + reverse edge table (dep -> dependent; dependents are
/// appended in ascending unit index because construction walks units 0..n and pushes their
/// edges). topo_order and the parallel scheduler (run_scheduler) share this construction
/// discipline: a unit's several edges (different key/class) are counted on both sides, so
/// decrementing once per edge keeps the two sides balanced.
pub fn dep_graph(plan: &ResolvePlan) -> (Vec<Vec<usize>>, Vec<usize>) {
    let n = plan.units.len();
    let mut indeg = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n]; // reverse edges: dep -> dependent
    for (i, u) in plan.units.iter().enumerate() {
        for d in &u.deps {
            indeg[i] += 1;
            dependents[d.unit].push(i);
        }
    }
    (dependents, indeg)
}

/// Kahn topological order: deps before dependents. Returns a sequence of unit indices.
pub fn topo_order(plan: &ResolvePlan) -> Result<Vec<usize>, String> {
    let n = plan.units.len();
    let (dependents, mut indeg) = dep_graph(plan);
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = queue.pop_front() {
        order.push(i);
        for &j in &dependents[i] {
            indeg[j] -= 1;
            if indeg[j] == 0 {
                queue.push_back(j);
            }
        }
    }
    if order.len() != n {
        return Err("internal inconsistency: the compilation-unit dependency graph has a cycle (cargo's resolution graph should be a DAG)".into());
    }
    Ok(order)
}

/// Kahn ready-queue parallel scheduler: a unit is ready once all of its deps are "done".
/// The main thread runs the scheduling loop (owning indegree, the ready queue and the
/// completion state), `jobs` worker threads take `(index, M)` from a channel and run `work`,
/// and send `(index, Result<T, String>)` back through another channel; the main thread
/// consumes completions, absorbs them with `on_done`, decrements the dependents' indegree and
/// enqueues newly ready units. On failure it keeps the **first** error (by completion arrival
/// order, not by topological rank), stops dispatching new work, and returns Err once every
/// in-flight worker has joined.
///
/// - `build_msg(&state, ix) -> M` and `on_done(&mut state, ix, T)` run on the main thread
///   only — the completion table (state) never crosses threads, so no lock is needed; the
///   dependency-side inputs a worker needs are computed by the main thread at **dispatch**
///   time and carried in M (all deps are done by then, and the values equal the serial
///   version's, computed at unit start).
/// - `work` runs on worker threads and must be `Sync` (all workers share one closure and its
///   captured read-only context). A worker panic is converted into an ordinary failure by
///   catch_unwind — otherwise the main thread would wait forever on a unit recorded in
///   in_flight (the panic hook still prints the panic text to stderr; the wording differs from
///   a serial run crashing outright, but that is an internal error path, not a differential
///   surface).
/// - with jobs=1 the dispatch order matches topo_order position by position (same dep_graph
///   construction discipline plus a single unit in flight => completion order = dispatch order
///   = Kahn FIFO) — the differential-debugging anchor, pinned.
///
/// Returns `Ok(state)` on success (the completion table comes home); `Err` is the first error
/// text. `indeg` is consumed by in-place decrementing (dep_graph's output, not reused).
pub fn run_scheduler<S, M, T>(
    state: S,
    dependents: &[Vec<usize>],
    indeg: &mut [usize],
    jobs: usize,
    build_msg: impl Fn(&S, usize) -> M,
    work: impl Fn(M) -> Result<T, String> + Sync,
    mut on_done: impl FnMut(&mut S, usize, T),
) -> Result<S, String>
where
    M: Send,
    T: Send,
{
    let n = dependents.len();
    let mut state = state;
    let jobs = jobs.max(1);
    // Ready-queue seeding follows topo_order's discipline: scan indeg==0 in ascending index order
    let mut ready: VecDeque<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let (work_tx, work_rx) = mpsc::channel::<(usize, M)>();
    let (done_tx, done_rx) = mpsc::channel::<(usize, Result<T, String>)>();
    // std's mpsc is single-consumer: the worker pool shares one lock to poll for work (one
    // recv per lock acquisition; the contention is negligible next to a rustc compile)
    let work_rx = Arc::new(Mutex::new(work_rx));
    let first_error = std::thread::scope(|s| {
        for _ in 0..jobs {
            let rx = Arc::clone(&work_rx);
            let tx = done_tx.clone();
            let work = &work;
            s.spawn(move || {
                loop {
                    let next = {
                        let g = match rx.lock() {
                            Ok(g) => g,
                            Err(_) => break, // lock poisoned (a worker panicked while holding it) = stop working
                        };
                        g.recv()
                    };
                    let (ix, m) = match next {
                        Ok(x) => x,
                        Err(_) => break, // main side hung up = stop working
                    };
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(m)))
                        .unwrap_or_else(|_| {
                            Err(format!(
                                "compile worker for unit {ix} panicked (internal error)"
                            ))
                        });
                    if tx.send((ix, r)).is_err() {
                        break; // main side already left (it leaves only after collecting every result; defensive)
                    }
                }
            });
        }
        let mut first_error: Option<String> = None;
        let mut workers_dead = false; // completion stream closed = all workers dead (lock poisoned)
        let mut in_flight = 0usize;
        let mut finished = 0usize;
        while finished < n {
            // dispatch first: ready non-empty, in-flight below jobs, no failure yet (failure stops dispatch)
            while first_error.is_none() && in_flight < jobs {
                match ready.pop_front() {
                    Some(ix) => {
                        if work_tx.send((ix, build_msg(&state, ix))).is_err() {
                            workers_dead = true;
                            break;
                        }
                        in_flight += 1;
                    }
                    None => break,
                }
            }
            if in_flight == 0 {
                break;
            }
            let (ix, res) = match done_rx.recv() {
                Ok(x) => x,
                Err(_) => {
                    workers_dead = true;
                    break;
                }
            };
            in_flight -= 1;
            finished += 1;
            match res {
                Ok(t) => {
                    on_done(&mut state, ix, t);
                    for &j in &dependents[ix] {
                        indeg[j] -= 1;
                        if indeg[j] == 0 {
                            ready.push_back(j);
                        }
                    }
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }
        drop(work_tx); // hang up: idle workers stop (the scope joins them automatically)
        if first_error.is_none() && finished < n {
            first_error = Some(if workers_dead {
                "compile worker threads terminated abnormally (internal error)".to_string()
            } else {
                // nothing left to dispatch but not everything finished = the dependency
                // graph has a cycle (unreachable on the normal path: topo_order inside
                // fingerprints fails first) -- fail loudly
                "internal inconsistency: the compilation-unit dependency graph has a cycle (cargo's resolution graph should be a DAG)".to_string()
            });
        }
        first_error
    });
    match first_error {
        None => Ok(state),
        Some(e) => Err(e),
    }
}

/// Host closure: the set reachable from each proc-macro unit by following dep edges
/// (proc-macro units included). Edges are followed regardless of class -- a proc-macro's
/// Build edges (its build-deps) are host compilation inputs too. Closure units are compiled
/// into host artifacts with real rustc and real codegen.
/// When the root package is itself a proc-macro, its Normal deps are host compilation inputs
/// as well: beyond the closure around dependency proc-macros, expand from the root's Normal
/// edges along all dependency edges.
pub fn host_closure_for_root(plan: &ResolvePlan, root_proc_macro: bool) -> BTreeSet<usize> {
    let mut set = BTreeSet::new();
    let mut stack: Vec<usize> = plan
        .units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.proc_macro)
        .map(|(i, _)| i)
        .collect();
    if root_proc_macro {
        stack.extend(
            plan.root_deps
                .iter()
                .filter(|dep| dep.class == UnitClass::Normal)
                .map(|dep| dep.unit),
        );
    }
    while let Some(i) = stack.pop() {
        if set.insert(i) {
            stack.extend(plan.units[i].deps.iter().map(|d| d.unit));
        }
    }
    set
}

/// Build-deps closure: the seeds are every has_build_script unit's **Build-class edges**
/// (with root_has_build the root's Build edges count too -- the root is not a unit, so the
/// driver passes manifest.has_build_script); the closure expands along **all** edges (a
/// build-dep's normal dependencies are host compilation inputs as well, and a build-dep may
/// itself have a build.rs whose build-deps join the closure through the all-edge expansion,
/// with topological order guaranteeing they run first). Closure units are compiled into host
/// artifacts (the host-deps directory) with real rustc and real codegen.
pub fn build_closure(plan: &ResolvePlan, root_has_build: bool) -> BTreeSet<usize> {
    let mut stack: Vec<usize> = Vec::new();
    for u in &plan.units {
        if u.has_build_script {
            stack.extend(
                u.deps
                    .iter()
                    .filter(|d| d.class == UnitClass::Build)
                    .map(|d| d.unit),
            );
        }
    }
    if root_has_build {
        stack.extend(
            plan.root_deps
                .iter()
                .filter(|d| d.class == UnitClass::Build)
                .map(|d| d.unit),
        );
    }
    let mut set = BTreeSet::new();
    while let Some(i) = stack.pop() {
        if set.insert(i) {
            stack.extend(plan.units[i].deps.iter().map(|d| d.unit));
        }
    }
    set
}

/// Target compilation set: BFS from the root's Normal-class edges along Normal edges -- a
/// Build edge is not a code dependency; a proc-macro unit is a leaf for the target, neither
/// in the set nor descended into (its dependencies are host dependencies, not the root's
/// target dependencies, and cargo does not produce a target rlib for a proc-macro crate
/// either). It may intersect the host closure: a unit used by both the bin and a proc-macro
/// is compiled on both sides, and the two artifacts sit in separate directories without
/// interfering.
pub fn target_units(plan: &ResolvePlan) -> BTreeSet<usize> {
    let mut set = BTreeSet::new();
    let mut stack: Vec<usize> = plan
        .root_deps
        .iter()
        .filter(|d| d.class == UnitClass::Normal)
        .map(|d| d.unit)
        .collect();
    while let Some(i) = stack.pop() {
        if plan.units[i].proc_macro {
            continue;
        }
        if set.insert(i) {
            stack.extend(
                plan.units[i]
                    .deps
                    .iter()
                    .filter(|d| d.class == UnitClass::Normal)
                    .map(|d| d.unit),
            );
        }
    }
    set
}

/// Per-unit content fingerprint (computed in topological order -- a dep's fp is produced
/// before its dependents'). fp(unit) = fnv1a(BUILD_ID, package, version, edition, sorted
/// features, the three profile flags, sysroot_stamp, each rustflag in order, source stamp,
/// **each dep's fp, sorted**). The last component is required: the depsimage pre-key
/// invariant "a transitive closure change changes every direct dependency's artifact stamp"
/// propagates through it -- a transitive dep's fp changes => the direct dep's fp changes =>
/// its artifact file name changes => the bin's --extern stamp changes (same semantics as the
/// depsimage.rs header note; pinned, do not delete). When no dep source changed but the locked
/// version set did, the version fields already cover it. rustflags enter every unit fp in
/// order: the host side does not consume rustflags, and following a stale value is harmless;
/// order is meaningful (later flags override earlier ones) so they are not sorted.
pub fn fingerprints(
    plan: &ResolvePlan,
    profile: &ProfileFlags,
    sysroot_stamp: &str,
    rustflags: &[String],
) -> Result<Vec<String>, String> {
    let order = topo_order(plan)?;
    let mut fps: Vec<Option<String>> = vec![None; plan.units.len()];
    for &ix in &order {
        let u = &plan.units[ix];
        let mut dep_fps: Vec<String> = u
            .deps
            .iter()
            .map(|d| {
                fps[d.unit]
                    .clone()
                    .expect("topo order guarantees dep fps are computed first")
            })
            .collect();
        dep_fps.sort();
        let src_stamp = source_stamp(u)?;
        let mut key = String::from(crate::options::build::BUILD_ID);
        let mut put = |s: &str| {
            key.push('\u{1f}');
            key.push_str(s);
        };
        put(&u.package);
        put(&u.version.to_string());
        put(&u.edition);
        for f in &u.features {
            put(f); // BTreeSet iteration is already lexicographic order
        }
        put(if profile.debug_assertions {
            "da1"
        } else {
            "da0"
        });
        put(if profile.overflow_checks {
            "oc1"
        } else {
            "oc0"
        });
        put(&profile.opt_level.to_string());
        put(sysroot_stamp);
        for f in rustflags {
            put(f); // in order: rustflag order is meaningful (later flags override earlier ones)
        }
        for flag in &u.rustc_lint_flags {
            put(flag);
        }
        put(&src_stamp);
        for d in &dep_fps {
            put(d);
        }
        fps[ix] = Some(format!(
            "{:016x}",
            crate::utils::content::fnv1a(key.as_bytes())
        ));
    }
    Ok(fps
        .into_iter()
        .map(|f| f.expect("topo order fills every fp"))
        .collect())
}

/// Source stamp: a Git unit = the exact source id from the lock (including the commit); a
/// registry unit = the literal "registry" (registry sources are immutable by checksum and are
/// not stamped -- the mtime of an unpacked .crate tree is the unpack time, so stamping it
/// would only cause pointless rebuilds, and the version already covers the content); a path
/// unit = recursive walk over every file under source_dir (excluding target/ and .git/),
/// folding (relative path, len, mtime_ns) after sorting. The root package follows the same
/// rule (root_fingerprint reuses this).
fn source_stamp(u: &Unit) -> Result<String, String> {
    if let Some(source_id) = &u.immutable_source_id {
        return Ok(source_id.clone());
    }
    source_stamp_dir(u.from_registry, &u.source_dir, &u.package)
}

/// The build.rs rerun decision in buildrs.rs folds the default-face tree snapshot the same
/// way, so the archive snapshot and the fingerprint stamp share one criterion and can only
/// drift together.
pub(super) fn source_stamp_dir(
    from_registry: bool,
    source_dir: &Path,
    package: &str,
) -> Result<String, String> {
    if from_registry {
        return Ok("registry".to_string());
    }
    let mut rows: Vec<String> = Vec::new();
    let mut stack = vec![source_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).map_err(|e| {
            format!(
                "failed to read source directory of path dependency {} at {}: {e}",
                package,
                dir.display()
            )
        })?;
        for ent in rd {
            let ent = ent.map_err(|e| {
                format!(
                    "failed to read an entry of path dependency {} source directory: {e}",
                    package
                )
            })?;
            let p = ent.path();
            if p.is_dir() {
                if ent.file_name() == "target" || ent.file_name() == ".git" {
                    continue;
                }
                stack.push(p);
            } else if p.is_file() {
                let md = std::fs::metadata(&p).map_err(|e| {
                    format!(
                        "failed to stat source file of path dependency {} at {}: {e}",
                        package,
                        p.display()
                    )
                })?;
                let rel = p.strip_prefix(source_dir).unwrap_or(&p);
                let mtime_ns = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                rows.push(format!("{}:{}:{}", rel.display(), md.len(), mtime_ns));
            }
        }
    }
    rows.sort();
    Ok(rows.join("\u{1e}"))
}

/// Root package fingerprint (the root build script's compile cache key and build directory
/// name, and also the artifact-name stamp of the root lib target). The root is not a unit and
/// is absent from fingerprints(), so it is computed separately with the same recipe: BUILD_ID,
/// name/version/edition, sorted root features, the three profile flags, sysroot_stamp, each
/// rustflag in order (the root lib is a target unit and consumes rustflags, so its fp must
/// include them; the root build script compile does not, and following a stale value is
/// harmless), the root source stamp, and every root edge dep's fp, sorted (Build edges
/// included -- the build script's --extern stamp follows them).
pub fn root_fingerprint(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    profile: &ProfileFlags,
    sysroot_stamp: &str,
    rustflags: &[String],
) -> Result<String, String> {
    let src_stamp = source_stamp_dir(false, &manifest.root, &manifest.name)?;
    let mut key = String::from(crate::options::build::BUILD_ID);
    let mut put = |s: &str| {
        key.push('\u{1f}');
        key.push_str(s);
    };
    put(&manifest.name);
    put(&manifest.version.to_string());
    put(&manifest.edition);
    for f in &plan.root_features {
        put(f);
    }
    put(if profile.debug_assertions {
        "da1"
    } else {
        "da0"
    });
    put(if profile.overflow_checks {
        "oc1"
    } else {
        "oc0"
    });
    put(&profile.opt_level.to_string());
    put(sysroot_stamp);
    for f in rustflags {
        put(f); // in order (same discipline as the unit fp)
    }
    for flag in &manifest.rustc_lint_flags {
        put(flag);
    }
    put(&src_stamp);
    let mut dep_fps: Vec<&str> = plan
        .root_deps
        .iter()
        .map(|d| fps[d.unit].as_str())
        .collect();
    dep_fps.sort();
    for d in dep_fps {
        put(d);
    }
    Ok(format!(
        "{:016x}",
        crate::utils::content::fnv1a(key.as_bytes())
    ))
}

#[cfg(test)]
mod tests;
