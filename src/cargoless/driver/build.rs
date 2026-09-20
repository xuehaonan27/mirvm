//! Unit compile segment: fingerprints, host/target/build set membership and the unit-level
//! Kahn ready-queue parallel scheduler that runs every unit pipeline.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::cargoless::buildrs::{self, BuildOutput};
use crate::cargoless::manifest::{PackageManifest, ProfileFlags};
use crate::cargoless::resolve::{ResolvePlan, Unit};
use crate::cargoless::schedule::{self, Layout};

/// compile_plan return value: completion table + unit fingerprint table (drive's root package phase also uses
/// fps to compute root fingerprint — dep fp component of root_fingerprint; sysroot build does not consume).
pub struct CompiledPlan {
    pub tables: UnitTables,
    pub fps: Vec<String>,
}

/// unit compile segment: fingerprints +
/// host/target/build sets + unit-level Kahn ready-queue parallel scheduling (run_scheduler)
/// runs all unit pipelines. Shared by drive and sysroot self-build:
///
/// - drive passes MIR sysroot and its stamp (the consumption base for this run's compilation);
/// - sysroot build passes **toolchain sysroot** and its stamp — outputs cannot be their own
///   compile input (the --sysroot for compiling std can only be the distro toolchain, chicken-and-egg).
///
/// Failure = first compile error original text (caller prepends `mirvm: ` prefix and exits loudly).
#[allow(clippy::too_many_arguments)]
pub fn compile_plan(
    plan: &ResolvePlan,
    layout: &Layout,
    profile: &ProfileFlags,
    rustflags: &[String],
    sysroot: &Path,
    stamp: &str,
    root_has_build_script: bool,
    root_proc_macro: bool,
    quiet_build_warnings: bool,
) -> Result<CompiledPlan, String> {
    // unit-level Kahn ready-queue parallel scheduling: a unit is ready when all its deps are 'done'
    // (build.rs lifecycle + host/target compilation all finished according to set membership);
    // N workers each run the full pipeline of assigned units (build.rs decision/
    // execution → compilation), completion table is only gathered on the main thread.
    for d in [&layout.deps, &layout.host_deps, &layout.build_root] {
        std::fs::create_dir_all(d).map_err(|e| format!("create {} failed: {e}", d.display()))?;
    }
    let fps = schedule::fingerprints(plan, profile, stamp, rustflags)
        .map_err(|e| format!("dependency fingerprint computation failed: {e}"))?;
    let host_set = schedule::host_closure_for_root(plan, root_proc_macro);
    let target_set = schedule::target_units(plan);
    let build_set = schedule::build_closure(plan, root_has_build_script);
    let self_exe = std::env::current_exe().expect("current_exe failed");
    let jobs = cless_jobs();
    let (dependents, mut indeg) = schedule::dep_graph(plan);
    // Shared read-only worker context (borrowed via thread::scope, immutable for the whole
    // scheduling phase; the completion table stays on the main thread, so no lock is needed).
    // Thread-safety argument:
    // - no rustc session inside the driver process — compilation is entirely in __cless-dep/real-rustc
    //   child processes, no compiler global state between workers;
    // - all env writes are on Command instances (per-child, thread-safe); std::env::set_var is forbidden
    //   inside workers (whole-crate check: set_var only in bin phase = main thread after convergence;
    //   build script env all goes through Command.envs). std::env::var
    //   reads (rerun gate env_get, build_script_env's CARGO_HOME, etc.) do not race
    //   with set_var, safe;
    // - directory creation create_dir_all is idempotent; artifact content-addressed (fp stamped), different units
    //   use different stems so no name collision; same-fp duplicate units (Normal/Build dual units with same
    //   package/version/feature — fp omits class so names collide) are serialized by FpLocks over the whole
    //   pipeline: when the late worker starts, artifacts are already complete and rerun gate reads archive to skip,
    //   byte-identical to serial 'first runner runs, late runners all skip';
    // - MIRVM_DEBUG_BLDRS observation lines and child diagnostics may interleave when jobs>1 (debug
    //   knob; differential anchor = jobs=1 matches serial order + default N corpus judge —
    //   program output is in the converged bin session, naturally serial).
    let ctx = SharedCtx {
        plan,
        profile,
        fps: &fps,
        layout,
        sysroot,
        rustflags,
        self_exe: &self_exe,
        host_set: &host_set,
        target_set: &target_set,
        build_set: &build_set,
        quiet_build_warnings,
        fp_locks: FpLocks::default(),
    };
    if plan.units.is_empty() {
        return Ok(CompiledPlan {
            tables: UnitTables::default(),
            fps,
        });
    }
    let tables = schedule::run_scheduler(
        UnitTables::default(),
        &dependents,
        &mut indeg,
        jobs.min(plan.units.len()),
        |t: &UnitTables, ix| build_work_msg(&ctx, t, ix),
        |msg| run_unit_pipeline(msg, &ctx),
        |t, ix, done| {
            if let Some(bo) = done.bo {
                t.outputs.insert(ix, bo);
            }
            if done.ran {
                t.re_ran.insert(ix);
            }
        },
    )?;
    Ok(CompiledPlan { tables, fps })
}

/// Concurrency: MIRVM_CLESS_JOBS override, default available_parallelism
/// (fallback to 1 if unavailable). **=1 dispatch order matches the serial topo order bit-for-bit — differential debugging anchor,
/// pinned**. Illegal values (non-positive integers) are loudly rejected and exit.
fn cless_jobs() -> usize {
    match crate::options::get().cless_jobs() {
        Ok(jobs) => jobs,
        // This phase cannot propagate a failure to the command boundary yet; the value is rejected
        // loudly either way, and the error carries its own class and message.
        Err(error) => crate::error::Error::from(error).report_and_exit(),
    }
}

/// Worker shared read-only context (borrowed via thread::scope; immutable for the whole scheduling
/// phase — the completion table stays on the main thread, so no lock is needed here).
struct SharedCtx<'a> {
    plan: &'a ResolvePlan,
    profile: &'a ProfileFlags,
    fps: &'a [String],
    layout: &'a Layout,
    sysroot: &'a Path,
    rustflags: &'a [String],
    self_exe: &'a Path,
    host_set: &'a BTreeSet<usize>,
    target_set: &'a BTreeSet<usize>,
    build_set: &'a BTreeSet<usize>,
    quiet_build_warnings: bool,
    /// Pipeline mutex lock table for same-fp duplicate units.
    fp_locks: FpLocks,
}

/// Lazy fp → mutex table: Normal/Build dual units with same package/version/feature have the same fp (fp omits class)
/// and collide on artifact names/build dirs — the whole pipeline is mutexed by fp; when the late worker starts,
/// artifacts are already complete and the rerun gate reads the archive to skip, same effect as serial
/// 'first runner runs, late runners all skip'. The lock table itself is only held during lock acquisition;
/// unique-fp locks have zero contention.
#[derive(Default)]
struct FpLocks(std::sync::Mutex<BTreeMap<String, std::sync::Arc<std::sync::Mutex<()>>>>);

impl FpLocks {
    fn lock_for(&self, fp: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
        self.0
            .lock()
            .expect("fp lock table poisoned (internal error)")
            .entry(fp.to_string())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
            .clone()
    }
}

/// Completion table (owned only by main thread: dependency-side inputs needed by workers — DEP_* env,
/// transitive -L aggregation, links rerun list — are computed from this table by the main thread at **dispatch** time
/// and carried along with WorkMsg; at that moment all deps must be done, values are bit-identical
/// to the serial version computed at unit start). Returned by compile_plan: the drive's root
/// package phase consumes it, the sysroot build takes Ok without reading fields.
#[derive(Default)]
pub struct UnitTables {
    /// unit index → executed BuildOutput (consumed in three places: this unit's compilation corrections, dependents' -L
    /// aggregation, direct dependents' build script DEP_*).
    pub outputs: BTreeMap<usize, BuildOutput>,
    /// Units whose build.rs was actually rerun in this session (links propagation:
    /// a package with links in direct dependencies being in re_ran ⇒ dependent also reruns, DEP_* input may change).
    pub re_ran: BTreeSet<usize>,
}

/// A unit's work order (main thread computes all dependency-side inputs at dispatch time, see UnitTables note;
/// also computed for fp-hit units — pure computation with no output, in exchange workers never access the completion table).
struct WorkMsg {
    ix: usize,
    /// Run build.rs lifecycle (has_build_script ∧ in any compile set; orphan
    /// build-deps do not run — cargo does not compile the kind whose parent has no build.rs,
    /// running its build.rs is overreach)
    run_build: bool,
    /// DEP_* env from direct dependencies (same measure as dep_metadata_env; empty when run_build=false)
    dep_env: BTreeMap<String, String>,
    /// Names of packages with links in direct dependencies that reran in this session (rerun gate condition 4)
    dep_links_reran: Vec<String>,
    /// Transitive -L aggregation (same measure as aggregate_link_searches; empty when not in any compile set)
    searches: Vec<String>,
}

/// A unit's completion receipt (worker → main thread).
struct PerUnitDone {
    /// build.rs output (None for units that did not run build.rs)
    bo: Option<BuildOutput>,
    /// whether build.rs was actually rerun this time
    ran: bool,
}

/// Work order construction (main thread): set membership decision + dependency-side input computation.
fn build_work_msg(ctx: &SharedCtx, t: &UnitTables, ix: usize) -> WorkMsg {
    let u = &ctx.plan.units[ix];
    let in_host = ctx.host_set.contains(&ix) || ctx.build_set.contains(&ix);
    let in_target = ctx.target_set.contains(&ix);
    let run_build = u.has_build_script && (in_host || in_target);
    let (dep_env, dep_links_reran) = if run_build {
        (
            buildrs::dep_metadata_env(ctx.plan, &u.deps, &t.outputs),
            // Condition 4 links propagation: packages with links in direct dependencies that reran this time
            // (DEP_* only goes to direct dependents — further propagation is decided/covered by each layer itself)
            u.deps
                .iter()
                .filter(|d| t.re_ran.contains(&d.unit) && ctx.plan.units[d.unit].links.is_some())
                .map(|d| ctx.plan.units[d.unit].package.clone())
                .collect(),
        )
    } else {
        (BTreeMap::new(), Vec::new())
    };
    let searches = if in_host || in_target {
        buildrs::aggregate_link_searches(ctx.plan, &u.deps, &t.outputs)
    } else {
        Vec::new()
    };
    WorkMsg {
        ix,
        run_build,
        dep_env,
        dep_links_reran,
        searches,
    }
}

/// A unit's full pipeline (worker thread): fp-lock mutex (same-fp duplicate units) →
/// build.rs lifecycle → host-side compile → target-side compile; hit stages are skipped as usual
/// (**disk checked inside the lock** — artifacts from same-fp predecessor must be visible to count as hit). Failure returns
/// original error text (`mirvm: ` prefix added by main thread after convergence, byte-identical to serial text).
fn run_unit_pipeline(msg: WorkMsg, ctx: &SharedCtx) -> Result<PerUnitDone, String> {
    let ix = msg.ix;
    let u = &ctx.plan.units[ix];
    let fp = &ctx.fps[ix];
    // Same-fp duplicate unit mutex (lock poisoning can only come from a predecessor worker panic — internal error already
    // finalized at that point, take inner and continue, do not stack failures)
    let fp_mutex = ctx.fp_locks.lock_for(fp);
    let _fp_guard = fp_mutex.lock().unwrap_or_else(|e| e.into_inner());
    let stem = format!("lib{}-{}", u.lib_name, fp);
    let mut done = PerUnitDone {
        bo: None,
        ran: false,
    };
    // build.rs lifecycle (ready decision guarantees its build-deps and their build.rs are all done)
    if msg.run_build {
        let (bo, ran) = run_build_lifecycle(u, ix, ctx, msg.dep_env, msg.dep_links_reran)?;
        done.ran = ran;
        done.bo = Some(bo);
    }
    let bo = done.bo.as_ref();
    // host side: proc-macro proper produces dylib; closure normal units (including build-deps closure) produce host rlib
    if ctx.host_set.contains(&ix) || ctx.build_set.contains(&ix) {
        let hit = if u.proc_macro {
            ctx.layout
                .host_deps
                .join(format!("{stem}{}", std::env::consts::DLL_SUFFIX))
                .is_file()
        } else {
            ctx.layout.host_deps.join(format!("{stem}.rmeta")).is_file()
                && ctx.layout.host_deps.join(format!("{stem}.rlib")).is_file()
        };
        if !hit {
            let (args, what) = if u.proc_macro {
                (
                    schedule::proc_macro_rustc_args(
                        ctx.plan,
                        ix,
                        ctx.profile,
                        ctx.fps,
                        ctx.layout,
                        bo,
                        &msg.searches,
                    ),
                    "proc-macro",
                )
            } else {
                (
                    schedule::host_rustc_args(
                        ctx.plan,
                        ix,
                        ctx.profile,
                        ctx.fps,
                        ctx.layout,
                        bo,
                        &msg.searches,
                    ),
                    "host dep",
                )
            };
            let mut cmd = std::process::Command::new(&args[0]);
            cmd.args(&args[1..]);
            apply_unit_env(&mut cmd, u);
            apply_build_env(&mut cmd, ctx.layout, u, fp, bo);
            run_compile(&mut cmd, u, what)?;
        }
    }
    // target side: the __cless-dep child (-Zno-codegen rlib)
    if ctx.target_set.contains(&ix) {
        // fingerprint hit: content-addressed, same-name artifact means same content, skip
        let hit = ctx.layout.deps.join(format!("{stem}.rmeta")).is_file()
            && ctx.layout.deps.join(format!("{stem}.rlib")).is_file();
        if !hit {
            let args = schedule::dep_rustc_args(
                ctx.plan,
                ix,
                ctx.profile,
                ctx.fps,
                ctx.sysroot,
                ctx.layout,
                bo,
                &msg.searches,
                ctx.rustflags,
            );
            let mut cmd = std::process::Command::new(ctx.self_exe);
            cmd.arg("__cless-dep").args(&args[1..]);
            apply_unit_env(&mut cmd, u);
            apply_build_env(&mut cmd, ctx.layout, u, fp, bo);
            run_compile(&mut cmd, u, "dep")?;
        }
    }
    Ok(done)
}

/// A unit's build.rs full lifecycle: build script compilation (skip on fp hit) →
/// rerun decision (buildrs::should_rerun, same semantics as cargo) — if skipped, read
/// output.txt and reparse to replay BuildOutput; if run, execute with cargo-compatible env and write archive
/// (only after successful execution — failure already returned by this function, no partial archive) → (BuildOutput, whether it actually ran).
/// `dep_env`/`dep_links_reran` are computed and carried by the main thread at dispatch time (see WorkMsg note).
/// Any step failure returns original error text (main thread loudly names after convergence).
fn run_build_lifecycle(
    u: &Unit,
    ix: usize,
    ctx: &SharedCtx,
    dep_env: BTreeMap<String, String>,
    dep_links_reran: Vec<String>,
) -> Result<(BuildOutput, bool), String> {
    let fp = &ctx.fps[ix];
    let bdir = ctx.layout.build_dir(&u.package, fp);
    if let Err(e) = std::fs::create_dir_all(bdir.join("out")) {
        return Err(format!(
            "failed to create build directory {} ({} {}): {e}",
            bdir.display(),
            u.package,
            u.version
        ));
    }
    let bexe = bdir.join(format!("build_script_build-{fp}"));
    if !bexe.is_file() {
        let args =
            schedule::build_script_rustc_args(ctx.plan, ix, ctx.profile, ctx.fps, ctx.layout);
        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        apply_unit_env(&mut cmd, u);
        // The compiled crate is the build script itself (same as cargo: CARGO_CRATE_NAME
        // follows the compiled crate, not the owning package's lib name)
        cmd.env("CARGO_CRATE_NAME", "build_script_build");
        run_compile(&mut cmd, u, "build script")?;
    }
    let env = buildrs::build_script_env(&buildrs::ExecCtx {
        pkg_env: &u.pkg_env,
        source_dir: &u.source_dir,
        features: &u.features,
        profile: ctx.profile,
        out_dir: &bdir.join("out"),
        dep_env,
        links: u.links.as_deref(),
        ld_dirs: &[ctx.layout.host_deps.clone(), ctx.layout.deps.clone()],
    });
    rerun_gate(
        &u.package,
        &u.version.to_string(),
        u.from_registry,
        &u.source_dir,
        &bdir,
        &bexe,
        &env,
        &dep_links_reran,
        ctx.quiet_build_warnings,
    )
}

/// Root package build.rs lifecycle (root is not a unit: pkg_env/features/profile supplied
/// directly by manifest/plan; root is a local path package, warnings shown normally).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_build_lifecycle_root(
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &Layout,
    root_fp: &str,
    outputs: &BTreeMap<usize, BuildOutput>,
    re_ran: &BTreeSet<usize>,
    quiet_build_warnings: bool,
) -> (BuildOutput, bool) {
    let bdir = layout.build_dir(&manifest.name, root_fp);
    if let Err(e) = std::fs::create_dir_all(bdir.join("out")) {
        eprintln!(
            "mirvm: failed to create build directory {} (root package {}): {e}",
            bdir.display(),
            manifest.name
        );
        std::process::exit(1);
    }
    let bexe = bdir.join(format!("build_script_build-{root_fp}"));
    if !bexe.is_file() {
        let args = schedule::root_build_script_rustc_args(manifest, plan, fps, layout, root_fp);
        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        // root package compile-time env (full CARGO_PKG_* set + two manifest entries, same as cargo)
        cmd.envs(manifest.pkg_env.iter());
        cmd.env("CARGO_CRATE_NAME", "build_script_build");
        cmd.env("CARGO_MANIFEST_DIR", &manifest.root);
        cmd.env(
            "CARGO_MANIFEST_PATH",
            manifest.root.join("Cargo.toml").display().to_string(),
        );
        let status = match cmd.status() {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "mirvm: build script compilation child process failed to start (root package {} {}): {e}",
                    manifest.name, manifest.version
                );
                std::process::exit(1);
            }
        };
        if !status.success() {
            eprintln!(
                "mirvm: build script compilation failed: root package {} {}",
                manifest.name, manifest.version
            );
            std::process::exit(1);
        }
    }
    let env = buildrs::build_script_env(&buildrs::ExecCtx {
        pkg_env: &manifest.pkg_env,
        source_dir: &manifest.root,
        features: &plan.root_features,
        profile: &manifest.profile,
        out_dir: &bdir.join("out"),
        dep_env: buildrs::dep_metadata_env(plan, &plan.root_deps, outputs),
        links: manifest.links.as_deref(),
        ld_dirs: &[layout.host_deps.clone(), layout.deps.clone()],
    });
    let dep_links_reran: Vec<String> = plan
        .root_deps
        .iter()
        .filter(|d| re_ran.contains(&d.unit) && plan.units[d.unit].links.is_some())
        .map(|d| plan.units[d.unit].package.clone())
        .collect();
    // root phase runs on main thread after convergence — errors still exit loudly (text same shape as worker returns)
    match rerun_gate(
        &manifest.name,
        &manifest.version.to_string(),
        false,
        &manifest.root,
        &bdir,
        &bexe,
        &env,
        &dep_links_reran,
        quiet_build_warnings,
    ) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("mirvm: {e}");
            std::process::exit(1);
        }
    }
}

/// rerun gate: decision (buildrs::should_rerun) → if skip, read archive
/// output.txt and reparse to replay (instruction stream zero serialization distortion, warnings replayed from cache
/// under same gate — same as cargo); if run, execute + write both output.txt and rerun.txt archives.
/// When MIRVM_DEBUG_BLDRS=1, print `bldrs run|skip <pkg> <reason>` observation line to stderr
/// (observation lines from workers may interleave when jobs>1 — debug knob, not differential surface).
/// Returns (BuildOutput, whether this run actually ran); failure returns original error text (caller prepends `mirvm: `
/// prefix and exits loudly — root path on main thread prepends in place, worker path prepends after convergence).
// flat parameter precedent same as run_build_lifecycle
#[allow(clippy::too_many_arguments)]
fn rerun_gate(
    pkg: &str,
    ver: &str,
    from_registry: bool,
    pkg_root: &Path,
    bdir: &Path,
    bexe: &Path,
    env: &BTreeMap<String, String>,
    dep_links_reran: &[String],
    quiet_build_warnings: bool,
) -> Result<(BuildOutput, bool), String> {
    let env_get = |k: &str| std::env::var(k).ok();
    let (rerun, why) = buildrs::should_rerun(
        bdir,
        from_registry,
        pkg,
        pkg_root,
        dep_links_reran,
        &env_get,
    );
    if crate::options::get().debug_bldrs {
        eprintln!("bldrs {} {pkg} {why}", if rerun { "run" } else { "skip" });
    }
    if !rerun {
        // skip execution: output.txt reparse is BuildOutput (replay failure treated as corrupted archive, self-healing by falling through to run)
        if let Ok(stdout) = std::fs::read_to_string(bdir.join("output.txt"))
            && let Ok(bo) = buildrs::parse_instructions(&stdout)
        {
            show_warnings(pkg, ver, from_registry, &bo, quiet_build_warnings);
            return Ok((bo, false));
        }
    }
    let (bo, stdout) = exec_and_parse(
        pkg,
        ver,
        from_registry,
        bexe,
        pkg_root,
        env,
        quiet_build_warnings,
    )?;
    // archive write failure is non-fatal — next no-record rerun self-heals (disk-layer failure would have already blown
    // earlier compilation writes); silent, does not disturb differential stderr
    let _ = buildrs::write_record(bdir, &stdout, &bo, from_registry, pkg, pkg_root, &env_get);
    Ok((bo, true))
}

/// Build script warning replay gate (same format and criterion as cargo: `warning: <pkg>@<ver>:
/// <msg>`; registry packages swallow by default, path packages show; identical on the execution and
/// archive-replay paths).
fn show_warnings(pkg: &str, ver: &str, from_registry: bool, bo: &BuildOutput, quiet: bool) {
    if !from_registry && !quiet {
        for w in &bo.warnings {
            eprintln!("warning: {pkg}@{ver}: {w}");
        }
    }
}

/// Execute + instruction parse + warning replay, returns (BuildOutput, raw stdout)
/// (raw stdout for caller to write output.txt archive — replay relies on reparse, zero serialization distortion).
/// Failure returns original error text (caller prepends `mirvm: ` prefix and exits loudly).
fn exec_and_parse(
    pkg: &str,
    ver: &str,
    from_registry: bool,
    bexe: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    quiet_build_warnings: bool,
) -> Result<(BuildOutput, String), String> {
    let stdout = buildrs::run_build_script(bexe, cwd, env)
        .map_err(|e| format!("build script execution failed ({pkg} {ver}): {e}"))?;
    let bo = buildrs::parse_instructions(&stdout)
        .map_err(|e| format!("build script instruction parse failed ({pkg} {ver}): {e}"))?;
    show_warnings(pkg, ver, from_registry, &bo, quiet_build_warnings);
    Ok((bo, stdout))
}

/// cargo compile-time env contract (readable by source env!): full CARGO_PKG_* set + three crate/manifest
/// entries (cargo sets these on every rustc call; same on both host real rustc and __cless-dep sides).
fn apply_unit_env(cmd: &mut std::process::Command, u: &Unit) {
    cmd.envs(u.pkg_env.iter());
    cmd.env("CARGO_CRATE_NAME", &u.lib_name);
    cmd.env("CARGO_MANIFEST_DIR", &u.source_dir);
    cmd.env(
        "CARGO_MANIFEST_PATH",
        u.source_dir.join("Cargo.toml").display().to_string(),
    );
}

/// This unit's build script compile-time env injection: OUT_DIR + rustc-env (set by cargo when compiling
/// packages with a build script; readable by env!).
fn apply_build_env(
    cmd: &mut std::process::Command,
    layout: &Layout,
    u: &Unit,
    fp: &str,
    bo: Option<&BuildOutput>,
) {
    if let Some(bo) = bo {
        cmd.env("OUT_DIR", layout.build_dir(&u.package, fp).join("out"));
        for (k, v) in &bo.envs {
            cmd.env(k, v);
        }
    }
}

/// Compile child process runs synchronously to completion; startup/compile failure returns original error text (what = artifact category,
/// names the crate — main thread prepends `mirvm: ` prefix and exits loudly after convergence, same shape as serial text).
fn run_compile(cmd: &mut std::process::Command, u: &Unit, what: &str) -> Result<(), String> {
    let status = cmd.status().map_err(|e| {
        format!(
            "{what} compilation child process failed to start ({} {}): {e}",
            u.package, u.version
        )
    })?;
    if !status.success() {
        return Err(format!(
            "{what} compilation failed: {} {}",
            u.package, u.version
        ));
    }
    Ok(())
}
