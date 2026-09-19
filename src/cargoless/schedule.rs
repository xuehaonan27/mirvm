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

use super::buildrs::BuildOutput;
use super::manifest::{DepKind, PackageManifest, ProfileFlags, Target};
use super::resolve::{ResolvePlan, Unit, UnitClass};

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
        let base = crate::sysroot::cache_dir()
            .join("target/cargoless")
            .join(env!("MIRVM_HOST"))
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
        let mut key = String::from(env!("MIRVM_BUILD_ID"));
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
        fps[ix] = Some(format!("{:016x}", crate::lower::asm::fnv1a(key.as_bytes())));
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
    let mut key = String::from(env!("MIRVM_BUILD_ID"));
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
    Ok(format!("{:016x}", crate::lower::asm::fnv1a(key.as_bytes())))
}

/// The three profile flags. debug-assertions and overflow-checks enter MIR semantics, so a
/// mismatch with cargo's dev profile would show up as a differential drift.
fn push_profile_flags(a: &mut Vec<String>, p: &ProfileFlags) {
    let yn = |b: bool| if b { "yes" } else { "no" };
    a.push("-C".into());
    a.push(format!("debug-assertions={}", yn(p.debug_assertions)));
    a.push("-C".into());
    a.push(format!("overflow-checks={}", yn(p.overflow_checks)));
    if !p.opt_level.is_zero() {
        a.push("-C".into());
        a.push(format!("opt-level={}", p.opt_level));
    }
}

/// Absolute path of the real rustc, taken from the default sysroot baked in at compile time
/// (same approach as manifest.rs host_cfg_atoms). Host-side compilation trusts only this: a
/// rustc on PATH may belong to another toolchain, and the proc-macro dylib's compiler version
/// must match the interpreter session's exactly (same discipline as cargo_shim's wrapper).
fn real_rustc() -> String {
    PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT"))
        .join("bin/rustc")
        .display()
        .to_string()
}

fn real_rustdoc() -> String {
    PathBuf::from(env!("MIRVM_DEFAULT_SYSROOT"))
        .join("bin/rustdoc")
        .display()
        .to_string()
}

/// Dispatch of one dep edge's --extern target path: a proc-macro dep points at the host-deps
/// dylib (the consumer dlopens it at compile time to expand macros); otherwise it points at
/// this side's directory and the given artifact extension.
fn extern_path(layout: &Layout, dir: &Path, du: &Unit, fp: &str, ext: &str) -> String {
    if du.proc_macro {
        format!(
            "{}/lib{}-{}{}",
            layout.host_deps.display(),
            du.lib_name,
            fp,
            std::env::consts::DLL_SUFFIX
        )
    } else {
        format!("{}/lib{}-{}.{}", dir.display(), du.lib_name, fp, ext)
    }
}

/// Append this package's BuildOutput compile flags (-l/cfg/check-cfg/link-arg enter this
/// package only; -L enters this package plus the transitive `searches` aggregation).
/// rustc-env does not enter argv -- the driver injects it through cmd.env, together with
/// OUT_DIR. Flag order matches cargo's own-package line (-L, -l, link-arg, --cfg,
/// --check-cfg); the differential comparison does not compare argv, but keeping the shape
/// honest costs nothing.
fn append_build_output(a: &mut Vec<String>, bo: Option<&BuildOutput>, searches: &[String]) {
    if let Some(bo) = bo {
        for s in &bo.link_searches {
            a.push("-L".into());
            a.push(s.clone());
        }
        for l in &bo.link_libs {
            a.push("-l".into());
            a.push(l.clone());
        }
        for f in &bo.link_args {
            a.push("-C".into());
            a.push(format!("link-arg={f}"));
        }
        for c in &bo.cfgs {
            a.push("--cfg".into());
            a.push(c.clone());
        }
        for c in &bo.check_cfgs {
            a.push("--check-cfg".into());
            a.push(c.clone());
        }
    }
    for s in searches {
        a.push("-L".into());
        a.push(s.clone());
    }
}

/// rustc arguments for one dep unit (the driver spawns a `__cless-dep` child that feeds
/// cli::run_dep_compiler; the shape matches cargo's call for a target dependency plus the MIR
/// sysroot/-Z injection of cargo_shim's wrapper).
/// argv0 = "mirvm-cless-rustc" (the driver strips it and substitutes the real name).
/// --extern takes **Normal-class edges** only (a Build edge is not a code dependency);
/// `bo` = this unit's build script output, `searches` = the transitive -L aggregation.
/// `rustflags` are appended at the **end** of the argument list (after the -Z flags): later
/// rustc flags override earlier ones, so user flags win -- see the rustflags.rs header (with
/// --target, rustflags land on target units only; the host-side argument functions do not
/// take rustflags at all).
// Flat parameters mirror the compile recipe slot by slot (same precedent as manifest.rs
// pkg_env_map); bundling them into a struct would lose the visual correspondence with argv
#[allow(clippy::too_many_arguments)]
pub fn dep_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
) -> Vec<String> {
    let u = &plan.units[unit_ix];
    let fp = &fps[unit_ix];
    let deps = layout.deps.display();
    let mut a: Vec<String> = vec!["mirvm-cless-rustc".into()];
    a.push("--crate-name".into());
    a.push(u.lib_name.clone());
    a.push(format!("--edition={}", u.edition));
    a.push(u.lib_path.display().to_string());
    a.push("--crate-type=lib".into());
    a.push("--emit=dep-info,metadata,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    a.push("-C".into());
    a.push("debuginfo=2".into());
    for f in &u.features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    if u.from_registry {
        // registry code is not the user's to change, so lints are silenced (same as cargo); path dependencies warn as usual
        a.push("--cap-lints".into());
        a.push("allow".into());
    }
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    // extra-filename must be the next separate argv slot after -C: run_dep_compiler
    // extracts it from that two-slot window to determine the rlib stem
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(deps.to_string());
    a.push("-L".into());
    a.push(format!("dependency={deps}"));
    // host-deps also enters -L: rustc looks up a facade-re-exported proc-macro as a .so by
    // crate hash in the -L directories, and we keep two directories so both must be listed
    // (cargo's single deps directory covers this naturally)
    a.push("-L".into());
    a.push(format!("dependency={}", layout.host_deps.display()));
    for d in &u.deps {
        if d.kind != DepKind::Normal {
            continue; // a Build edge is not a code dependency
        }
        let du = &plan.units[d.unit];
        // proc-macro edges point at the host-deps dylib; normal edges at the target .rmeta
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, du, &fps[d.unit], "rmeta")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    a.push("-Zalways-encode-mir".into());
    a.push("-Zno-codegen".into());
    // rustflags appended at the end so later flags override earlier ones (a --cap-lints
    // allow must be able to override a path dependency's built-in lint behavior)
    a.extend(rustflags.iter().cloned());
    a
}

/// bin (root crate) session arguments -- they go through the existing MirvmCallbacks lowering
/// path (stops at after_analysis, Compilation::Stop, zero artifacts): **no** -Z flags,
/// --out-dir or -C metadata, the same shape as the cargo path's runner bin session.
/// --extern takes Normal-class root edges only; `bo` = the root build script's output
/// (cfg/check-cfg/link flags enter the session), `searches` = the transitive -L aggregation.
/// `rustflags` are appended at the **end** of the argument list (after --sysroot): later
/// flags override earlier ones (the bin is a path package with no built-in --cap-lints, so a
/// RUSTFLAGS --cap-lints allow can suppress lint warnings here).
/// `root_lib` = Some((lib_name, lib_fp)) adds the --extern for the root package's lib target
/// (with [lib]+[[bin]] dual targets the bin implicitly depends on the same-name lib, and in
/// cargo's line the root lib is mixed in with the other --extern entries pointing at the
/// .rlib produced by root_lib_rustc_args).
// flat-parameter precedent as in dep_rustc_args
#[allow(clippy::too_many_arguments)]
pub fn bin_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    bin_name: &str,
    bin_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    root_lib: Option<(&str, &str)>,
) -> Vec<String> {
    let deps = layout.deps.display();
    let mut a: Vec<String> = vec!["mirvm".into()];
    a.push(bin_path.display().to_string());
    a.push("--crate-name".into());
    a.push(bin_name.replace('-', "_"));
    a.push(format!("--edition={}", manifest.edition));
    a.push("--crate-type=bin".into());
    // file!()/panic Location/diagnostic path parity: cargo invokes rustc with cwd = package
    // root and the relative path src/main.rs, so local package paths are relative in every
    // output; we pass absolute paths and use remap to rewrite the cargo compile root (the
    // workspace root, equal to the package root for a single package) prefix to empty. remap
    // affects all output including compiler diagnostics, and real rustc confirms that absolute
    // input plus remap gives byte-identical file!() and panic locations to relative input.
    // registry/path dependency paths stay absolute (same as cargo); only the root package
    // directory is remapped.
    a.push(format!(
        "--remap-path-prefix={}/=",
        manifest.lock_root.display()
    ));
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    a.push("-C".into());
    a.push("debuginfo=2".into());
    for f in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    a.extend(manifest.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let values = manifest.check_cfg_feature_values();
    let vals = values
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, &manifest.profile);
    for d in &plan.root_deps {
        if d.kind != DepKind::Normal {
            continue; // a Build edge is not a code dependency
        }
        let du = &plan.units[d.unit];
        // bin-side --extern uses .rlib (matching cargo's final-crate invocation shape);
        // a proc-macro root edge points at the host-deps dylib
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, du, &fps[d.unit], "rlib")
        ));
    }
    // --extern for the root package's lib target: the host dylib when the same-package root
    // is a proc-macro, the target rlib for an ordinary lib.
    if let Some((lib_name, lib_fp)) = root_lib {
        let root_is_proc_macro = manifest
            .targets
            .iter()
            .any(|target| target.is_lib() && target.proc_macro && target.name == lib_name);
        let path = if root_is_proc_macro {
            format!(
                "{}/lib{}-{lib_fp}{}",
                layout.host_deps.display(),
                lib_name.replace('-', "_"),
                std::env::consts::DLL_SUFFIX
            )
        } else {
            format!("{deps}/lib{}-{lib_fp}.rlib", lib_name.replace('-', "_"))
        };
        a.push("--extern".into());
        a.push(format!("{}={path}", lib_name.replace('-', "_")));
    }
    a.push("-L".into());
    a.push(format!("dependency={deps}"));
    // host-deps also enters -L (.so lookup for a facade-re-exported proc-macro; same note as
    // dep_rustc_args)
    a.push("-L".into());
    a.push(format!("dependency={}", layout.host_deps.display()));
    append_build_output(&mut a, bo, searches);
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    // rustflags appended at the end: later flags override earlier ones (as in dep_rustc_args)
    a.extend(rustflags.iter().cloned());
    a
}

/// Root package test target arguments. They reuse the normal bin's cargo-aligned common
/// section and change only two things:
/// - a libtest harness target uses `--test` instead of an explicit `--crate-type=bin`;
/// - the test context additionally sees the root's Dev edges. The normal root lib is still
///   compiled by root_lib_rustc_args and consumes Normal edges only, which corresponds to
///   Cargo compiling the root lib twice.
#[allow(clippy::too_many_arguments)]
pub fn test_target_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    target_name: &str,
    target_path: &Path,
    harness: bool,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    root_lib: Option<(&str, &str)>,
) -> Vec<String> {
    let mut args = bin_rustc_args(
        manifest,
        plan,
        fps,
        sysroot,
        layout,
        target_name,
        target_path,
        bo,
        searches,
        rustflags,
        root_lib,
    );
    if harness {
        if let Some(i) = args.iter().position(|a| a == "--crate-type=bin") {
            args.remove(i);
        }
        args.push("--test".into());
    } else {
        // Cargo's harness=false tests still set cfg(test) but keep the user's main.
        args.push("--cfg".into());
        args.push("test".into());
    }
    if manifest
        .targets
        .iter()
        .any(|target| target.is_lib() && target.proc_macro && target.path == target_path)
    {
        args.push("-C".into());
        args.push("prefer-dynamic".into());
        args.push("--extern".into());
        args.push("proc_macro".into());
    }
    append_root_dev_externs(&mut args, plan, fps, layout);
    args
}

fn append_root_dev_externs(
    args: &mut Vec<String>,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
) {
    for d in &plan.root_deps {
        if d.kind != DepKind::Dev {
            continue;
        }
        let unit = &plan.units[d.unit];
        args.push("--extern".into());
        args.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, unit, &fps[d.unit], "rlib")
        ));
    }
}

/// `cargo test` compiles examples by default and normal bins when an integration test is
/// present, but does not run them. This reuses the same argument body and finishes parsing,
/// type checking and mono collection with a metadata-only rustc session; `include_dev`
/// corresponds to example=true and normal bin=false.
#[allow(clippy::too_many_arguments)]
pub fn check_root_target_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    target_name: &str,
    target_path: &Path,
    include_dev: bool,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    root_lib: Option<(&str, &str)>,
    fp: &str,
) -> Vec<String> {
    let mut args = bin_rustc_args(
        manifest,
        plan,
        fps,
        sysroot,
        layout,
        target_name,
        target_path,
        bo,
        searches,
        rustflags,
        root_lib,
    );
    if include_dev {
        append_root_dev_externs(&mut args, plan, fps, layout);
    }
    args.push("--emit=dep-info,metadata".into());
    args.push("-C".into());
    args.push(format!("metadata={fp}"));
    args.push("-C".into());
    args.push(format!("extra-filename=-{fp}"));
    args.push("--out-dir".into());
    args.push(layout.deps.display().to_string());
    args.push("-Zalways-encode-mir".into());
    args.push("-Zno-codegen".into());
    args
}

/// rustc arguments for the root package's lib target. With [lib]+[[bin]] dual targets the
/// bin implicitly depends on the same-name lib, and cargo first compiles the root lib into a
/// target rlib and then lets the bin --extern it (cargo's line: root lib = `--crate-type lib
/// --emit=dep-info,metadata,link` plus --extern pointing at the deps' .rmeta, with no
/// --cap-lints since a path package warns as usual). Shape = dep_rustc_args applied to the
/// root lib (__cless-dep channel, -Zno-codegen rlib into layout.deps), with these
/// differences: source/edition/features come from manifest/plan (the root is not a unit); the
/// check-cfg feature value table is the declared set plus implicit optionals (same as bin);
/// --extern takes root_deps' Normal-class edges; rustflags are appended at the end (the root
/// lib is a target unit and consumes RUSTFLAGS -- cargo --target semantics). `fp` =
/// root_fingerprint (artifact name lib<lib_name>-<fp>.{rmeta,rlib}).
// flat-parameter precedent as in dep_rustc_args
#[allow(clippy::too_many_arguments)]
pub fn root_lib_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    lib_name: &str,
    lib_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    fp: &str,
) -> Vec<String> {
    let deps = layout.deps.display();
    let mut a: Vec<String> = vec!["mirvm-cless-rustc".into()];
    a.push("--crate-name".into());
    a.push(lib_name.replace('-', "_"));
    a.push(format!("--edition={}", manifest.edition));
    a.push(lib_path.display().to_string());
    a.push("--crate-type=lib".into());
    a.push("--emit=dep-info,metadata,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    a.push("-C".into());
    a.push("debuginfo=2".into());
    for f in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    // a path package gets no --cap-lints (same as cargo: it warns as usual)
    a.extend(manifest.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let values = manifest.check_cfg_feature_values();
    let vals = values
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, &manifest.profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    // extra-filename window discipline as in dep_rustc_args (extracted by run_dep_compiler)
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(deps.to_string());
    a.push("-L".into());
    a.push(format!("dependency={deps}"));
    // host-deps enters -L (.so lookup for a facade-re-exported proc-macro; see dep_rustc_args)
    a.push("-L".into());
    a.push(format!("dependency={}", layout.host_deps.display()));
    for d in &plan.root_deps {
        if d.kind != DepKind::Normal {
            continue; // the root's normal lib consumes neither Build nor Dev edges
        }
        let du = &plan.units[d.unit];
        // proc-macro edges point at the host-deps dylib; normal edges at the target .rmeta
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.deps, du, &fps[d.unit], "rmeta")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a.push("--sysroot".into());
    a.push(sysroot.display().to_string());
    a.push("-Zalways-encode-mir".into());
    a.push("-Zno-codegen".into());
    // rustflags appended at the end: later flags override earlier ones (as in dep_rustc_args)
    a.extend(rustflags.iter().cloned());
    a
}

/// Host dylib arguments for a root proc-macro. It cannot take the VM's `-Zno-codegen`
/// channel: when integration tests are compiled later, rustc must really dlopen this
/// artifact.
#[allow(clippy::too_many_arguments)]
pub fn root_proc_macro_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    lib_name: &str,
    lib_path: &Path,
    bo: Option<&BuildOutput>,
    searches: &[String],
    rustflags: &[String],
    fp: &str,
) -> Vec<String> {
    let host = layout.host_deps.display();
    let mut a = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push(lib_name.replace('-', "_"));
    a.push(format!("--edition={}", manifest.edition));
    a.push(lib_path.display().to_string());
    a.push("--crate-type=proc-macro".into());
    a.push("--emit=dep-info,link".into());
    a.push("-C".into());
    a.push("prefer-dynamic".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for feature in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{feature}\""));
    }
    a.extend(manifest.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let values = manifest.check_cfg_feature_values();
    let vals = values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, &manifest.profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(host.to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for dep in &plan.root_deps {
        if dep.class != UnitClass::Normal {
            continue;
        }
        let unit = &plan.units[dep.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            dep.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, unit, &fps[dep.unit], "rlib")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a.push("--extern".into());
    a.push("proc_macro".into());
    a.extend(rustflags.iter().cloned());
    a
}

/// rustdoc doctest arguments for the root lib. rustdoc still owns Markdown extraction, line
/// numbers, compile_fail and harness summarization; `--test-builder` only hands the extracted
/// temporary crate to mirvm for compilation and execution.
#[allow(clippy::too_many_arguments)]
pub fn doctest_rustdoc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    sysroot: &Path,
    layout: &Layout,
    target: &Target,
    root_fp: &str,
    bo: Option<&BuildOutput>,
    searches: &[String],
    builder: &Path,
) -> Vec<String> {
    let crate_name = target.name.replace('-', "_");
    let mut args = vec![real_rustdoc()];
    args.push(format!("--edition={}", manifest.edition));
    args.push("--crate-type".into());
    args.push(if target.proc_macro {
        "proc-macro".into()
    } else {
        "lib".into()
    });
    args.push("--color".into());
    args.push("auto".into());
    args.push("--crate-name".into());
    args.push(crate_name.clone());
    args.push("--test".into());
    args.push(
        target
            .path
            .strip_prefix(&manifest.root)
            .unwrap_or(&target.path)
            .display()
            .to_string(),
    );
    args.push("--test-run-directory".into());
    args.push(manifest.root.display().to_string());

    let root_path = if target.proc_macro {
        format!(
            "{}/lib{}-{root_fp}{}",
            layout.host_deps.display(),
            crate_name,
            std::env::consts::DLL_SUFFIX
        )
    } else {
        format!("{}/lib{}-{root_fp}.rlib", layout.deps.display(), crate_name)
    };
    args.push("--extern".into());
    args.push(format!("{crate_name}={root_path}"));

    let mut externs = std::collections::BTreeSet::new();
    for dep in &plan.root_deps {
        if dep.kind == DepKind::Build || !externs.insert(dep.key.clone()) {
            continue;
        }
        let unit = &plan.units[dep.unit];
        let dir = if target.proc_macro && dep.kind == DepKind::Normal {
            &layout.host_deps
        } else {
            &layout.deps
        };
        args.push("--extern".into());
        args.push(format!(
            "{}={}",
            dep.key.replace('-', "_"),
            extern_path(layout, dir, unit, &fps[dep.unit], "rlib")
        ));
    }
    args.push("-L".into());
    args.push(format!("dependency={}", layout.deps.display()));
    args.push("-L".into());
    args.push(format!("dependency={}", layout.host_deps.display()));
    args.push("-C".into());
    args.push("embed-bitcode=no".into());
    for feature in &plan.root_features {
        args.push("--cfg".into());
        args.push(format!("feature=\"{feature}\""));
    }
    args.extend(manifest.rustc_lint_flags.iter().cloned());
    args.push("--check-cfg".into());
    args.push("cfg(docsrs,test)".into());
    let values = manifest.check_cfg_feature_values();
    let values = values
        .iter()
        .map(|value| format!("\"{value}\""))
        .collect::<Vec<_>>()
        .join(",");
    args.push("--check-cfg".into());
    args.push(format!("cfg(feature, values({values}))"));
    append_build_output(&mut args, bo, searches);
    args.push("--sysroot".into());
    args.push(sysroot.display().to_string());
    args.push("-Z".into());
    args.push("unstable-options".into());
    args.push("--test-builder".into());
    args.push(builder.display().to_string());
    args.push("--error-format".into());
    args.push("human".into());
    args
}

/// Real rustc arguments for an ordinary host-closure unit: `--crate-type lib
/// --emit=dep-info,metadata,link -C embed-bitcode=no` (**no debuginfo, no prefer-dynamic**)
/// really codegens a host rlib; dep edges point at the host-deps .rmeta (proc-macro edges at
/// .so) and **only Normal-class edges are consumed** (a Build edge is a build script input,
/// not this crate's code dependency).
/// **No --sysroot** (real rustc uses its own sysroot) and no -Z flags.
/// argv0 = the absolute real rustc path (the driver spawns it directly, not via __cless-dep).
pub fn host_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    layout: &Layout,
    bo: Option<&BuildOutput>,
    searches: &[String],
) -> Vec<String> {
    let u = &plan.units[unit_ix];
    let fp = &fps[unit_ix];
    let host = layout.host_deps.display();
    let mut a: Vec<String> = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push(u.lib_name.clone());
    a.push(format!("--edition={}", u.edition));
    a.push(u.lib_path.display().to_string());
    a.push("--crate-type=lib".into());
    a.push("--emit=dep-info,metadata,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for f in &u.features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    if u.from_registry {
        // registry code is not the user's to change, so lints are silenced (same as cargo); path dependencies warn as usual
        a.push("--cap-lints".into());
        a.push("allow".into());
    }
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(host.to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &u.deps {
        if d.class != UnitClass::Normal {
            continue;
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rmeta")
        ));
    }
    append_build_output(&mut a, bo, searches);
    a
}

/// Real rustc arguments for a proc-macro crate itself: `--crate-type proc-macro
/// --emit=dep-info,link -C prefer-dynamic -C embed-bitcode=no` (**no debuginfo**) plus a bare
/// `--extern proc_macro` at the end (the compiler's built-in bridge crate); dep edges point
/// at host-deps .rlib (really linked into the dylib) and **only Normal-class edges are
/// consumed**.
/// No --sysroot/-Z; argv0 = the absolute real rustc path.
pub fn proc_macro_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    layout: &Layout,
    bo: Option<&BuildOutput>,
    searches: &[String],
) -> Vec<String> {
    let u = &plan.units[unit_ix];
    let fp = &fps[unit_ix];
    let host = layout.host_deps.display();
    let mut a: Vec<String> = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push(u.lib_name.clone());
    a.push(format!("--edition={}", u.edition));
    a.push(u.lib_path.display().to_string());
    a.push("--crate-type=proc-macro".into());
    a.push("--emit=dep-info,link".into());
    a.push("-C".into());
    a.push("prefer-dynamic".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for f in &u.features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    if u.from_registry {
        a.push("--cap-lints".into());
        a.push("allow".into());
    }
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(host.to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &u.deps {
        if d.class != UnitClass::Normal {
            continue;
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rlib")
        ));
    }
    append_build_output(&mut a, bo, searches);
    // bare --extern proc_macro at the end: the compiler's built-in bridge, resolved from real rustc's own sysroot
    a.push("--extern".into());
    a.push("proc_macro".into());
    a
}

/// Real rustc arguments for compiling a build script:
/// `--crate-name build_script_build --edition=<e> <build.rs path>
/// --crate-type bin --emit=dep-info,link -C embed-bitcode=no`, plus feature cfgs,
/// `--check-cfg cfg(docsrs,test)` and `cfg(feature, values(...))`, the profile flags,
/// `-C metadata/extra-filename`, `--out-dir <build/<pkg>-<fp>>`, `-L dependency=<host-deps>`,
/// and **Build-class edges** --extern pointing at host-deps artifacts (a proc-macro build-dep
/// points at .so; extern_path dispatches that).
/// registry adds --cap-lints allow (cap-lints already covers unexpected_cfgs; the feature
/// value table fills only enabled features, whereas cargo uses the declared set plus implicit
/// optionals, so a path build.rs using an unenabled feature's cfg would emit one extra
/// unexpected_cfgs -- fixtures do not trigger it, noted). No --sysroot/-Z (real rustc's own
/// sysroot); no incremental (cargo enables it for path packages only, an internal
/// optimization not copied here). argv0 = the absolute real rustc path.
pub fn build_script_rustc_args(
    plan: &ResolvePlan,
    unit_ix: usize,
    profile: &ProfileFlags,
    fps: &[String],
    layout: &Layout,
) -> Vec<String> {
    let u = &plan.units[unit_ix];
    let fp = &fps[unit_ix];
    let host = layout.host_deps.display();
    let mut a: Vec<String> = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push("build_script_build".into());
    a.push(format!("--edition={}", u.edition));
    a.push(
        u.build_script_path
            .clone()
            .unwrap_or_else(|| u.source_dir.join("build.rs"))
            .display()
            .to_string(),
    );
    a.push("--crate-type=bin".into());
    a.push("--emit=dep-info,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for f in &u.features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    if u.from_registry {
        a.push("--cap-lints".into());
        a.push("allow".into());
    }
    a.extend(u.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let vals = u
        .declared_features
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(layout.build_dir(&u.package, fp).display().to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &u.deps {
        if d.kind != DepKind::Build {
            continue; // a build script consumes Build-class edges only (build-deps)
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rlib")
        ));
    }
    a
}

/// Root package build script compile arguments (the root is not a unit: features and the
/// edge table come from the manifest and plan.root_deps; the feature value table uses
/// manifest.check_cfg_feature_values() -- the declared set plus implicit optionals, exactly
/// matching cargo). `fp` = root_fingerprint.
pub fn root_build_script_rustc_args(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    fp: &str,
) -> Vec<String> {
    let host = layout.host_deps.display();
    let mut a: Vec<String> = vec![real_rustc()];
    a.push("--crate-name".into());
    a.push("build_script_build".into());
    a.push(format!("--edition={}", manifest.edition));
    a.push(
        manifest
            .build_script_path
            .clone()
            .unwrap_or_else(|| manifest.root.join("build.rs"))
            .display()
            .to_string(),
    );
    a.push("--crate-type=bin".into());
    a.push("--emit=dep-info,link".into());
    a.push("-C".into());
    a.push("embed-bitcode=no".into());
    for f in &plan.root_features {
        a.push("--cfg".into());
        a.push(format!("feature=\"{f}\""));
    }
    a.extend(manifest.rustc_lint_flags.iter().cloned());
    a.push("--check-cfg".into());
    a.push("cfg(docsrs,test)".into());
    let values = manifest.check_cfg_feature_values();
    let vals = values
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(",");
    a.push("--check-cfg".into());
    a.push(format!("cfg(feature, values({vals}))"));
    push_profile_flags(&mut a, &manifest.profile);
    a.push("-C".into());
    a.push(format!("metadata={fp}"));
    a.push("-C".into());
    a.push(format!("extra-filename=-{fp}"));
    a.push("--out-dir".into());
    a.push(layout.build_dir(&manifest.name, fp).display().to_string());
    a.push("-L".into());
    a.push(format!("dependency={host}"));
    for d in &plan.root_deps {
        if d.class != UnitClass::Build {
            continue;
        }
        let du = &plan.units[d.unit];
        a.push("--extern".into());
        a.push(format!(
            "{}={}",
            d.key.replace('-', "_"),
            extern_path(layout, &layout.host_deps, du, &fps[d.unit], "rlib")
        ));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargoless::lockfile::Lockfile;
    use crate::cargoless::manifest::DepKind;
    use crate::cargoless::resolve::{UnitClass, UnitDep};
    use semver::Version;
    use std::collections::{BTreeMap, BTreeSet};

    fn unit(
        name: &str,
        version: &str,
        from_registry: bool,
        features: &[&str],
        deps: Vec<UnitDep>,
    ) -> Unit {
        Unit {
            package: name.to_string(),
            lib_name: name.replace('-', "_"),
            version: Version::parse(version).unwrap(),
            source_dir: PathBuf::from(format!("/tmp/{name}")),
            from_registry,
            immutable_source_id: None,
            class: UnitClass::Normal,
            features: features
                .iter()
                .map(|f| f.to_string())
                .collect::<BTreeSet<_>>(),
            declared_features: features
                .iter()
                .map(|f| f.to_string())
                .collect::<BTreeSet<_>>(),
            proc_macro: false,
            has_build_script: false,
            build_script_path: None,
            links: None,
            deps,
            edition: "2021".to_string(),
            lib_path: PathBuf::from(format!("/tmp/{name}/src/lib.rs")),
            pkg_env: BTreeMap::new(),
            rustc_lint_flags: Vec::new(),
        }
    }

    fn plan_with(units: Vec<Unit>, root_deps: Vec<UnitDep>) -> ResolvePlan {
        ResolvePlan {
            root_name: "demo".to_string(),
            root_version: Version::new(0, 1, 0),
            root_dir: PathBuf::from("/tmp/demo"),
            root_features: BTreeSet::new(),
            units,
            root_deps,
            version_map: BTreeMap::new(),
            lock: Lockfile::default(),
        }
    }

    fn diamond_plan() -> ResolvePlan {
        // b and c depend on a; the root depends on b and c
        let a = unit("a", "1.0.0", true, &["std"], vec![]);
        let b = unit(
            "b",
            "1.0.0",
            true,
            &[],
            vec![UnitDep {
                key: "a".into(),
                unit: 0,
                class: UnitClass::Normal,
                kind: DepKind::Normal,
            }],
        );
        let c = unit(
            "c",
            "1.0.0",
            true,
            &[],
            vec![UnitDep {
                key: "a".into(),
                unit: 0,
                class: UnitClass::Normal,
                kind: DepKind::Normal,
            }],
        );
        plan_with(
            vec![a, b, c],
            vec![
                UnitDep {
                    key: "b".into(),
                    unit: 1,
                    class: UnitClass::Normal,
                    kind: DepKind::Normal,
                },
                UnitDep {
                    key: "c".into(),
                    unit: 2,
                    class: UnitClass::Normal,
                    kind: DepKind::Normal,
                },
            ],
        )
    }

    fn layout() -> Layout {
        Layout {
            deps: PathBuf::from("/tmp/cless/deps"),
            host_deps: PathBuf::from("/tmp/cless/host-deps"),
            build_root: PathBuf::from("/tmp/cless/build"),
        }
    }

    #[test]
    fn topo_order_puts_deps_before_dependents() {
        let plan = diamond_plan();
        let order = topo_order(&plan).unwrap();
        let pos = |i| order.iter().position(|&x| x == i).unwrap();
        assert!(pos(0) < pos(1), "a must come before b: {order:?}");
        assert!(pos(0) < pos(2), "a must come before c: {order:?}");
        assert_eq!(order.len(), 3);
    }

    // ---- run_scheduler (Kahn ready-queue parallel scheduling core) ----
    // Tests are decoupled from the plan: they write the "dep index table per unit" directly
    // and expand it into (dependents, indeg) with dep_graph's discipline (ascending index,
    // duplicate edges counted).

    fn graph_from_deps(deps: &[&[usize]]) -> (Vec<Vec<usize>>, Vec<usize>) {
        let n = deps.len();
        let mut indeg = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, ds) in deps.iter().enumerate() {
            for &d in *ds {
                indeg[i] += 1;
                dependents[d].push(i);
            }
        }
        (dependents, indeg)
    }

    /// Diamond + isolated node + duplicate edge, jobs=4: no unit starts before its deps
    /// (checked inside the worker and surfaced as Err), and every unit finishes into state.
    #[test]
    fn scheduler_never_starts_before_deps_and_finishes_all() {
        let deps: &[&[usize]] = &[&[], &[0], &[0], &[1, 2], &[], &[0, 0]];
        let n = deps.len();
        let (dependents, mut indeg) = graph_from_deps(deps);
        let done_flags = std::sync::Mutex::new(vec![false; n]);
        let r = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            4,
            |_s, ix| ix,
            |ix| {
                {
                    let d = done_flags.lock().unwrap();
                    for &dep in deps[ix] {
                        if !d[dep] {
                            return Err(format!("unit {ix} started before dep {dep} finished"));
                        }
                    }
                }
                done_flags.lock().unwrap()[ix] = true;
                Ok(ix)
            },
            |s, ix, _| s.push(ix),
        );
        let mut got = r.unwrap();
        got.sort_unstable();
        assert_eq!(
            got,
            (0..n).collect::<Vec<_>>(),
            "all units finished: {got:?}"
        );
    }

    /// With jobs=1 the dispatch order must match Kahn FIFO (topo_order's discipline) position
    /// by position -- the differential-debugging anchor (pinned in the run_scheduler header).
    #[test]
    fn scheduler_jobs1_matches_kahn_fifo_order() {
        // a<-b, a<-c; {b,c}<-d; e isolated. Hand-computed Kahn FIFO: seed [0,4] -> 0 finishes
        // and releases 1,2 -> 4 -> 1 -> 2 (releases 3) -> 3
        let deps: &[&[usize]] = &[&[], &[0], &[0], &[1, 2], &[]];
        let (dependents, mut indeg) = graph_from_deps(deps);
        let done = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            1,
            |_s, ix| ix,
            Ok,
            |s, ix, _| s.push(ix),
        )
        .unwrap();
        assert_eq!(done, vec![0, 4, 1, 2, 3]);
    }

    /// Failure semantics (jobs=1 chain 0->1->2, both 1 and 2 fail): the first error is kept
    /// and dispatch stops after a failure (2 never starts).
    #[test]
    fn scheduler_first_error_wins_and_dispatch_stops() {
        let deps: &[&[usize]] = &[&[], &[0], &[1]];
        let (dependents, mut indeg) = graph_from_deps(deps);
        let started = std::sync::Mutex::new(Vec::new());
        let r = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            1,
            |_s, ix| ix,
            |ix| {
                started.lock().unwrap().push(ix);
                if ix >= 1 {
                    Err(format!("boom-{ix}"))
                } else {
                    Ok(ix)
                }
            },
            |s, ix, _| s.push(ix),
        );
        assert_eq!(r.unwrap_err(), "boom-1", "first error kept");
        assert_eq!(
            *started.lock().unwrap(),
            vec![0, 1],
            "dispatch stops after a failure"
        );
    }

    /// Cyclic dependency graph: nothing to dispatch yet not everything collected -> loud error (same text as topo_order).
    #[test]
    fn scheduler_reports_cycle_loudly() {
        let deps: &[&[usize]] = &[&[1], &[0]]; // 0<->1 cycle
        let (dependents, mut indeg) = graph_from_deps(deps);
        let r = run_scheduler(
            Vec::<usize>::new(),
            &dependents,
            &mut indeg,
            2,
            |_s, ix| ix,
            Ok,
            |s, ix, _| s.push(ix),
        );
        assert_eq!(
            r.unwrap_err(),
            "internal inconsistency: the compilation-unit dependency graph has a cycle (cargo's resolution graph should be a DAG)"
        );
    }

    #[test]
    fn fingerprint_propagates_transitive_dep_change() {
        let plan = diamond_plan();
        let fps0 = fingerprints(&plan, &ProfileFlags::default(), "stamp0", &[]).unwrap();
        // b's own fields are untouched; only its transitive dep a's feature set changes => b's
        // fp must change (the depsimage pre-key invariant "a transitive closure change changes
        // every direct dependency's artifact stamp")
        let mut plan2 = diamond_plan();
        plan2.units[0].features.insert("alloc".to_string());
        let fps1 = fingerprints(&plan2, &ProfileFlags::default(), "stamp0", &[]).unwrap();
        assert_ne!(fps0[0], fps1[0], "a's own fp must change");
        assert_ne!(
            fps0[1], fps1[1],
            "a changed => b's fp must change (transitive propagation)"
        );
        assert_ne!(
            fps0[2], fps1[2],
            "a changed => c's fp must change (transitive propagation)"
        );
        // sysroot_stamp and profile also enter the fp
        let fps2 = fingerprints(&plan, &ProfileFlags::default(), "stamp1", &[]).unwrap();
        assert_ne!(fps0[0], fps2[0], "sysroot_stamp enters the fp");
        let relaxed = ProfileFlags {
            debug_assertions: false,
            overflow_checks: false,
            opt_level: crate::cargoless::manifest::OptLevel::O2,
        };
        let fps3 = fingerprints(&plan, &relaxed, "stamp0", &[]).unwrap();
        assert_ne!(fps0[0], fps3[0], "the three profile flags enter the fp");
    }

    #[test]
    fn fingerprint_distinguishes_git_commits() {
        let mut first = diamond_plan();
        first.units[0].immutable_source_id = Some(
            "git+https://example.invalid/repo?branch=main#1111111111111111111111111111111111111111"
                .to_string(),
        );
        let mut second = first.clone();
        second.units[0].immutable_source_id = Some(
            "git+https://example.invalid/repo?branch=main#2222222222222222222222222222222222222222"
                .to_string(),
        );
        let first_fps = fingerprints(&first, &ProfileFlags::default(), "stamp", &[]).unwrap();
        let second_fps = fingerprints(&second, &ProfileFlags::default(), "stamp", &[]).unwrap();
        assert_ne!(
            first_fps[0], second_fps[0],
            "the Git commit must enter the unit's own fingerprint"
        );
        assert_ne!(
            first_fps[1], second_fps[1],
            "a Git commit change must propagate along dependency edges"
        );
    }

    #[test]
    fn dep_args_carry_key_flags_and_window_shaped_extra_filename() {
        let plan = diamond_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let a = dep_rustc_args(
            &plan,
            1,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &[],
        );
        assert_eq!(a[0], "mirvm-cless-rustc");
        assert!(a.windows(2).any(|w| w[0] == "--crate-name" && w[1] == "b"));
        assert!(a.iter().any(|x| x == "--edition=2021"));
        assert!(a.iter().any(|x| x == "--crate-type=lib"));
        assert!(a.iter().any(|x| x == "--emit=dep-info,metadata,link"));
        // registry unit cap-lints; feature --cfg as two argv slots
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--cap-lints" && w[1] == "allow")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(docsrs,test)")
        );
        // extra-filename must be the next separate argv slot after -C (extracted by run_dep_compiler)
        let want = format!("extra-filename=-{}", fps[1]);
        assert!(
            a.windows(2).any(|w| w[0] == "-C" && w[1] == want),
            "missing -C/extra-filename window: {a:?}"
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == format!("metadata={}", fps[1]))
        );
        // --extern points at the dep's .rmeta (cargo's two-slot shape)
        let ext = format!("a=/tmp/cless/deps/liba-{}.rmeta", fps[0]);
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "missing --extern: {a:?}"
        );
        assert!(a.windows(2).any(|w| w[0] == "--sysroot" && w[1] == "/sys"));
        assert!(a.iter().any(|x| x == "-Zalways-encode-mir"));
        assert!(a.iter().any(|x| x == "-Zno-codegen"));
        // dev profile default: debug-assertions and overflow-checks on, no opt-level
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "debug-assertions=yes")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "overflow-checks=yes")
        );
        assert!(!a.iter().any(|x| x.starts_with("opt-level")));
    }

    #[test]
    fn package_lints_reach_proc_macro_args_and_fingerprint() {
        let mut plain = diamond_plan();
        let plain_fps = fingerprints(&plain, &ProfileFlags::default(), "s", &[]).unwrap();
        plain.units[1].rustc_lint_flags = vec![
            "--warn=unexpected_cfgs".into(),
            "--check-cfg".into(),
            "cfg(bootstrap)".into(),
        ];
        let lint_fps = fingerprints(&plain, &ProfileFlags::default(), "s", &[]).unwrap();
        assert_ne!(
            plain_fps[1], lint_fps[1],
            "the lint configuration must enter the unit fingerprint"
        );

        let args = proc_macro_rustc_args(
            &plain,
            1,
            &ProfileFlags::default(),
            &lint_fps,
            &layout(),
            None,
            &[],
        );
        assert!(args.iter().any(|arg| arg == "--warn=unexpected_cfgs"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--check-cfg", "cfg(bootstrap)"])
        );
    }

    #[test]
    fn bin_args_use_rlib_and_skip_z_flags() {
        let mut plan = diamond_plan();
        plan.root_features.insert("std".to_string());
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nb = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo-bin",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &[],
            None,
        );
        assert_eq!(a[0], "mirvm");
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--crate-name" && w[1] == "demo_bin")
        );
        assert!(a.iter().any(|x| x == "--crate-type=bin"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--cfg" && w[1] == "feature=\"std\"")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(docsrs,test)")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1].starts_with("cfg(feature, values(")),
            "missing feature value table check-cfg: {a:?}"
        );
        // root-edge --extern uses .rlib; no -Z flags, no --out-dir, no -C metadata
        let ext = format!("b=/tmp/cless/deps/libb-{}.rlib", fps[1]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
        assert!(!a.iter().any(|x| x.starts_with("-Z")));
        assert!(!a.iter().any(|x| x == "--out-dir"));
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "-C" && w[1].starts_with("metadata="))
        );
    }

    #[test]
    fn test_args_use_libtest_and_add_only_dev_externs_to_test_unit() {
        let normal = unit("normal", "1.0.0", true, &[], vec![]);
        let dev = unit("devonly", "1.0.0", true, &[], vec![]);
        let mut plan = plan_with(
            vec![normal, dev],
            vec![
                UnitDep {
                    key: "normal".into(),
                    unit: 0,
                    class: UnitClass::Normal,
                    kind: DepKind::Normal,
                },
                UnitDep {
                    key: "devonly".into(),
                    unit: 1,
                    class: UnitClass::Normal,
                    kind: DepKind::Dev,
                },
            ],
        );
        plan.root_features.insert("root-feature".into());
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [features]\nroot-feature = []\n\
             [dependencies]\nnormal = \"1\"\n\
             [dev-dependencies]\ndevonly = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let lo = layout();
        let args = test_target_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/lib.rs"),
            true,
            None,
            &[],
            &[],
            None,
        );
        assert!(args.iter().any(|arg| arg == "--test"));
        assert!(!args.iter().any(|arg| arg == "--crate-type=bin"));
        assert!(args.windows(2).any(|w| {
            w[0] == "--extern"
                && w[1] == format!("normal=/tmp/cless/deps/libnormal-{}.rlib", fps[0])
        }));
        assert!(args.windows(2).any(|w| {
            w[0] == "--extern"
                && w[1] == format!("devonly=/tmp/cless/deps/libdevonly-{}.rlib", fps[1])
        }));

        let normal_lib = root_lib_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/lib.rs"),
            None,
            &[],
            &[],
            "rootfp",
        );
        assert!(!normal_lib.iter().any(|arg| arg.contains("devonly=")));
    }

    /// proc-macro scenario (serde-family shape): shared is used by both sides (bin and
    /// my_derive), pm_helper is host-only, my_derive = proc-macro, and uses_pm is an ordinary
    /// target dep with a proc-macro edge.
    fn pm_plan() -> ResolvePlan {
        let shared = unit("shared", "1.0.0", true, &[], vec![]);
        let dep = |key: &str, unit: usize| UnitDep {
            key: key.into(),
            unit,
            class: UnitClass::Normal,
            kind: DepKind::Normal,
        };
        let pm_helper = unit("pm-helper", "1.0.0", true, &[], vec![dep("shared", 0)]);
        let mut my_derive = unit(
            "my-derive",
            "1.0.0",
            true,
            &[],
            vec![dep("pm_helper", 1), dep("shared", 0)],
        );
        my_derive.proc_macro = true;
        let uses_pm = unit(
            "uses-pm",
            "1.0.0",
            true,
            &[],
            vec![dep("my_derive", 2), dep("shared", 0)],
        );
        plan_with(
            vec![shared, pm_helper, my_derive, uses_pm],
            vec![dep("uses_pm", 3), dep("shared", 0), dep("my_derive", 2)],
        )
    }

    #[test]
    fn host_target_partition() {
        let plan = pm_plan();
        let host = host_closure_for_root(&plan, false);
        let target = target_units(&plan);
        assert_eq!(
            host,
            BTreeSet::from([0, 1, 2]),
            "the whole proc-macro closure enters host"
        );
        assert_eq!(
            target,
            BTreeSet::from([0, 3]),
            "the proc-macro itself and its host-only dependency (pm_helper) stay out of the target set"
        );
        assert!(
            host.contains(&0) && target.contains(&0),
            "the dual-use unit (shared) is on both sides"
        );
    }

    #[test]
    fn proc_macro_args_five_pins() {
        let plan = pm_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let a = proc_macro_rustc_args(&plan, 2, &ProfileFlags::default(), &fps, &lo, None, &[]);
        assert!(a[0].ends_with("bin/rustc"), "argv0 = real rustc: {}", a[0]);
        assert!(a.iter().any(|x| x == "--crate-type=proc-macro"));
        assert!(a.iter().any(|x| x == "--emit=dep-info,link"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "prefer-dynamic")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "embed-bitcode=no")
        );
        // no debuginfo, no --sysroot, no -Z
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "-C" && w[1].starts_with("debuginfo"))
        );
        assert!(!a.iter().any(|x| x == "--sysroot"));
        assert!(!a.iter().any(|x| x.starts_with("-Z")));
        // bare --extern proc_macro at the end (the last pin)
        assert_eq!(a.last().unwrap(), "proc_macro");
        assert_eq!(a[a.len() - 2], "--extern");
        // dep edges point at host-deps .rlib (really linked)
        let ext = format!(
            "pm_helper=/tmp/cless/host-deps/libpm_helper-{}.rlib",
            fps[1]
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "missing --extern: {a:?}"
        );
        // --out-dir points at host-deps; registry unit cap-lints
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--out-dir" && w[1] == "/tmp/cless/host-deps")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--cap-lints" && w[1] == "allow")
        );
    }

    #[test]
    fn host_rlib_args_shape() {
        let plan = pm_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let a = host_rustc_args(&plan, 1, &ProfileFlags::default(), &fps, &lo, None, &[]);
        assert!(a[0].ends_with("bin/rustc"), "argv0 = real rustc: {}", a[0]);
        assert!(a.iter().any(|x| x == "--crate-type=lib"));
        assert!(a.iter().any(|x| x == "--emit=dep-info,metadata,link"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "embed-bitcode=no")
        );
        // no prefer-dynamic, no debuginfo, no --sysroot, no -Z
        assert!(!a.iter().any(|x| x == "prefer-dynamic"));
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "-C" && w[1].starts_with("debuginfo"))
        );
        assert!(!a.iter().any(|x| x == "--sysroot"));
        assert!(!a.iter().any(|x| x.starts_with("-Z")));
        // dep edges point at host-deps .rmeta
        let ext = format!("shared=/tmp/cless/host-deps/libshared-{}.rmeta", fps[0]);
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "missing --extern: {a:?}"
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--out-dir" && w[1] == "/tmp/cless/host-deps")
        );
    }

    #[test]
    fn target_and_bin_proc_macro_edges_point_to_dylib() {
        let plan = pm_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let so = format!(
            "my_derive=/tmp/cless/host-deps/libmy_derive-{}{}",
            fps[2],
            std::env::consts::DLL_SUFFIX
        );
        // a target dep's proc-macro edge -> the host-deps dylib; normal edges stay .rmeta
        let a = dep_rustc_args(
            &plan,
            3,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &[],
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == so),
            "dep is missing the .so --extern: {a:?}"
        );
        let ext = format!("shared=/tmp/cless/deps/libshared-{}.rmeta", fps[0]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
        // the bin's proc-macro root edge -> dylib; normal root edges stay .rlib
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nuses-pm = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &[],
            None,
        );
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == so),
            "bin is missing the .so --extern: {a:?}"
        );
        let ext = format!("uses_pm=/tmp/cless/deps/libuses_pm-{}.rlib", fps[3]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
    }

    /// build.rs scenario: bdep = a Build-class build-dep (b's build.rs uses it); b has a
    /// build.rs; bdep itself also has a build.rs and a build-dep (cc0).
    fn buildrs_plan() -> ResolvePlan {
        let bdep = |key: &str, unit: usize| UnitDep {
            key: key.into(),
            unit,
            class: UnitClass::Build,
            kind: DepKind::Build,
        };
        let ndep = |key: &str, unit: usize| UnitDep {
            key: key.into(),
            unit,
            class: UnitClass::Normal,
            kind: DepKind::Normal,
        };
        let mut cc0 = unit("cc0", "1.0.0", true, &[], vec![]);
        cc0.class = UnitClass::Build;
        let mut bdep_u = unit("bdep", "1.0.0", true, &[], vec![bdep("cc0", 0)]);
        bdep_u.class = UnitClass::Build;
        bdep_u.has_build_script = true;
        bdep_u.links = Some("mylinks".into());
        let mut b = unit("b", "1.0.0", true, &[], vec![bdep("bdep", 1)]);
        b.has_build_script = true;
        plan_with(
            vec![cc0, bdep_u, b],
            vec![
                ndep("b", 2),
                // the root declares a build-dep too (a seed only when the root has a build.rs)
                bdep("bdep", 1),
            ],
        )
    }

    #[test]
    fn build_closure_follows_build_edges_then_all_edges() {
        let plan = buildrs_plan();
        // root has no build.rs: the only seed is b's Build edge -> bdep; the closure expands along all edges to cc0
        let set = build_closure(&plan, false);
        assert_eq!(set, BTreeSet::from([1, 0]), "{set:?}");
        // root has a build.rs: the root's Build edge is a seed too (same result set, since bdep is shared)
        let set2 = build_closure(&plan, true);
        assert_eq!(set2, BTreeSet::from([1, 0]), "{set2:?}");
        // no build.rs on the root or on anyone else -> empty set (orphan build-deps are not compiled)
        let mut plan2 = buildrs_plan();
        plan2.units[1].has_build_script = false;
        plan2.units[2].has_build_script = false;
        assert!(build_closure(&plan2, false).is_empty());
        // the target set consumes no Build edges: b is in, bdep/cc0 are not
        assert_eq!(target_units(&plan), BTreeSet::from([2]));
    }

    #[test]
    fn build_edges_stay_out_of_code_compiles_but_feed_build_script() {
        let plan = buildrs_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        // b's target compile: --extern consumes no Build edge (bdep does not appear)
        let a = dep_rustc_args(
            &plan,
            2,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &[],
        );
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "--extern" && w[1].starts_with("bdep=")),
            "a Build edge leaked into the lib arguments: {a:?}"
        );
        // b's build script compile: --extern consumes Build edges only (bdep -> host-deps rlib)
        let bs = build_script_rustc_args(&plan, 2, &ProfileFlags::default(), &fps, &lo);
        assert!(
            bs[0].ends_with("bin/rustc"),
            "argv0 = real rustc: {}",
            bs[0]
        );
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--crate-name" && w[1] == "build_script_build")
        );
        assert!(bs.iter().any(|x| x == "--crate-type=bin"));
        assert!(bs.iter().any(|x| x == "--emit=dep-info,link"));
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(docsrs,test)")
        );
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1].starts_with("cfg(feature, values(")),
            "missing feature value table check-cfg: {bs:?}"
        );
        let want_out = format!("/tmp/cless/build/b-{}", fps[2]);
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--out-dir" && w[1] == want_out),
            "build script --out-dir shape: {bs:?}"
        );
        let ext = format!("bdep=/tmp/cless/host-deps/libbdep-{}.rlib", fps[1]);
        assert!(
            bs.windows(2).any(|w| w[0] == "--extern" && w[1] == ext),
            "build script is missing the Build-edge --extern: {bs:?}"
        );
        // registry unit cap-lints; the default build.rs path
        assert!(
            bs.windows(2)
                .any(|w| w[0] == "--cap-lints" && w[1] == "allow")
        );
        assert!(bs.iter().any(|x| x == "/tmp/b/build.rs"));
        // the bin session likewise consumes no Build edge
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nb = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &[],
            None,
        );
        assert!(
            !a.windows(2)
                .any(|w| w[0] == "--extern" && w[1].starts_with("bdep=")),
            "a Build edge leaked into the bin arguments: {a:?}"
        );
    }

    #[test]
    fn build_output_flags_land_on_own_compile_only() {
        let plan = buildrs_plan();
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let lo = layout();
        let bo = BuildOutput {
            cfgs: vec!["bdep_feat".into()],
            check_cfgs: vec!["cfg(bdep_feat)".into()],
            link_libs: vec!["static=probehelper".into()],
            link_searches: vec!["native=/opt/probe/lib".into()],
            link_args: vec!["-Wl,--x".into()],
            ..Default::default()
        };
        let searches = vec!["native=/opt/transitive".to_string()];
        let a = dep_rustc_args(
            &plan,
            2,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            Some(&bo),
            &searches,
            &[],
        );
        // this package's bo: its own -L, plus -l, link-arg, --cfg, --check-cfg and the aggregated -L
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-L" && w[1] == "native=/opt/probe/lib")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-l" && w[1] == "static=probehelper")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "link-arg=-Wl,--x")
        );
        assert!(a.windows(2).any(|w| w[0] == "--cfg" && w[1] == "bdep_feat"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1] == "cfg(bdep_feat)")
        );
        assert!(
            a.windows(2)
                .any(|w| w[0] == "-L" && w[1] == "native=/opt/transitive")
        );
        // without bo none of these flags appear (propagation goes through explicit parameters only)
        let a0 = dep_rustc_args(
            &plan,
            2,
            &ProfileFlags::default(),
            &fps,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &[],
        );
        assert!(!a0.iter().any(|x| x == "static=probehelper"));
        assert!(
            !a0.windows(2)
                .any(|w| w[0] == "--cfg" && w[1] == "bdep_feat")
        );
    }

    /// rustflags: each enters every unit fp in order; they are appended at the end of the
    /// target-side arguments (for dep after the -Z flags, for bin after --sysroot). The three
    /// host-side argument functions do not take rustflags at all, so not consuming them is
    /// guaranteed at compile time and needs no assertion.
    #[test]
    fn rustflags_enter_fingerprint_and_target_args_tail() {
        let plan = diamond_plan();
        let rf = vec!["--cap-lints".to_string(), "allow".to_string()];
        let fps0 = fingerprints(&plan, &ProfileFlags::default(), "s", &[]).unwrap();
        let fps1 = fingerprints(&plan, &ProfileFlags::default(), "s", &rf).unwrap();
        assert_ne!(fps0[0], fps1[0], "rustflags enter the unit fp");
        assert_ne!(
            fps0[1], fps1[1],
            "rustflags enter the unit fp (the transitive side changes too)"
        );
        // order is meaningful: a different flag order gives a different fp (later flags override earlier ones; not a set)
        let rf_rev = vec!["allow".to_string(), "--cap-lints".to_string()];
        let fps2 = fingerprints(&plan, &ProfileFlags::default(), "s", &rf_rev).unwrap();
        assert_ne!(
            fps1[0], fps2[0],
            "rustflags enter the fp in order (not sorted)"
        );
        // dep: rustflags come after the -Z flags (end of the argument list)
        let lo = layout();
        let a = dep_rustc_args(
            &plan,
            1,
            &ProfileFlags::default(),
            &fps1,
            Path::new("/sys"),
            &lo,
            None,
            &[],
            &rf,
        );
        let zpos = a.iter().rposition(|x| x.starts_with("-Z")).unwrap();
        // A registry unit already has a built-in --cap-lints (coexisting with RUSTFLAGS, the
        // two-flag shape seen in cargo's serde line), so the rustflags assertions must take the
        // **last** one
        let rfpos = a.iter().rposition(|x| x == "--cap-lints").unwrap();
        assert!(
            rfpos > zpos,
            "rustflags must come after the -Z flags: {a:?}"
        );
        assert_eq!(
            &a[a.len() - 2..],
            &["--cap-lints", "allow"],
            "appended at the end"
        );
        // bin: rustflags come after --sysroot (end of the argument list)
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [dependencies]\nb = \"1\"\n",
            Path::new("/tmp/demo"),
        )
        .unwrap();
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps1,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &rf,
            None,
        );
        let syspos = a.iter().rposition(|x| x == "--sysroot").unwrap();
        let rfpos = a.iter().rposition(|x| x == "--cap-lints").unwrap();
        assert!(rfpos > syspos, "rustflags must come after --sysroot: {a:?}");
        assert_eq!(
            &a[a.len() - 2..],
            &["--cap-lints", "allow"],
            "appended at the end"
        );
        // empty rustflags: the argument list is character-for-character the previous shape (zero drift on the no-flag path)
        let a_empty = bin_rustc_args(
            &manifest,
            &plan,
            &fps0,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &[],
            None,
        );
        assert!(a_empty.ends_with(&["--sysroot".into(), "/sys".into()]));
    }

    /// Root package lib target: the root_lib argument shape = the dep arguments applied to
    /// the root (__cless-dep/-Z/-C metadata window/.rmeta --extern), with pinned differences:
    /// no --cap-lints (path package), the feature value table check-cfg present, rustflags
    /// appended at the end; root_fingerprint changes with rustflags; the bin session adds the
    /// root lib --extern pointing at .rlib.
    #[test]
    fn root_lib_args_shape_and_bin_extern() {
        let plan = diamond_plan();
        let rf = vec!["--cap-lints".to_string(), "allow".to_string()];
        let fps = fingerprints(&plan, &ProfileFlags::default(), "s", &rf).unwrap();
        let lo = layout();
        // root_fingerprint stamps the root source directory (source_stamp_dir), so it must exist
        let root = std::env::temp_dir().join(format!(
            "mirvm-cargoless-schedule-test-rootlib-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main(){}").unwrap();
        let manifest = PackageManifest::parse(
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
             [lib]\nname = \"demo\"\npath = \"src/lib.rs\"\n\
             [dependencies]\nb = \"1\"\n",
            &root,
        )
        .unwrap();
        let rfp0 =
            root_fingerprint(&manifest, &plan, &fps, &ProfileFlags::default(), "s", &[]).unwrap();
        let rfp1 =
            root_fingerprint(&manifest, &plan, &fps, &ProfileFlags::default(), "s", &rf).unwrap();
        assert_ne!(rfp0, rfp1, "rustflags enter root_fingerprint");
        let a = root_lib_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            &root.join("src/lib.rs"),
            None,
            &[],
            &rf,
            &rfp1,
        );
        assert_eq!(a[0], "mirvm-cless-rustc");
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--crate-name" && w[1] == "demo")
        );
        let want_src = root.join("src/lib.rs").display().to_string();
        assert!(a.iter().any(|x| x == &want_src));
        assert!(a.iter().any(|x| x == "--crate-type=lib"));
        assert!(a.iter().any(|x| x == "-Zno-codegen"));
        // extra-filename window (the shape run_dep_compiler extracts)
        let want = format!("extra-filename=-{rfp1}");
        assert!(a.windows(2).any(|w| w[0] == "-C" && w[1] == want));
        // the feature value table check-cfg is present (same full-set criterion as bin); a
        // path package has no built-in --cap-lints, so the only --cap-lints is the trailing
        // rustflags
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--check-cfg" && w[1].starts_with("cfg(feature, values(")),
            "missing feature value table check-cfg: {a:?}"
        );
        assert_eq!(
            &a[a.len() - 2..],
            &["--cap-lints", "allow"],
            "rustflags at the end"
        );
        // --extern consumes root_deps' Normal edges and points at .rmeta
        let ext = format!("b=/tmp/cless/deps/libb-{}.rmeta", fps[1]);
        assert!(a.windows(2).any(|w| w[0] == "--extern" && w[1] == ext));
        // the bin session adds the root lib --extern (pointing at .rlib, mixed into the same section as the root edges)
        let a = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &rf,
            Some(("demo", &rfp1)),
        );
        let want_ext = format!("demo=/tmp/cless/deps/libdemo-{rfp1}.rlib");
        assert!(
            a.windows(2).any(|w| w[0] == "--extern" && w[1] == want_ext),
            "bin is missing the root lib --extern: {a:?}"
        );
        // the root package directory remap is present (relative file!()/panic Location =
        // cargo's cwd=package-root relative invocation semantics, verified on redb_kv and gix_pure)
        let want_remap = format!("--remap-path-prefix={}/=", root.display());
        assert!(
            a.iter().any(|x| x == &want_remap),
            "bin is missing the root package directory remap: {a:?}"
        );
        // no root lib means no root lib --extern (the argument list is otherwise unchanged)
        let a0 = bin_rustc_args(
            &manifest,
            &plan,
            &fps,
            Path::new("/sys"),
            &lo,
            "demo",
            Path::new("/tmp/demo/src/main.rs"),
            None,
            &[],
            &rf,
            None,
        );
        assert!(
            !a0.windows(2)
                .any(|w| w[0] == "--extern" && w[1].starts_with("demo=")),
            "there must be no root lib --extern without a root lib: {a0:?}"
        );
    }
}
