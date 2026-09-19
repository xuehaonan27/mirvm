//! `cargoless/buildrs.rs` -- build script full lifecycle: instruction parsing (`cargo::`/`cargo:`
//! two forms), CARGO_CFG_* mapping, execution env construction, DEP_* propagation key
//! normalization, links mutual-exclusion validation, -L propagation collection, and rerun-if
//! fine-grained incrementality (record write/read + rerun decision, cargo-equivalent semantics).
//! Compilation argument shapes are in schedule.rs (`build_script_rustc_args`), scheduling in
//! driver.rs.
//!
//! Propagation rules (verified line by line against cargo 1.98):
//! - `-l` (rustc-link-lib) enters only **this package**'s own compile line; `-L` (rustc-link-search)
//!   enters this package + all transitive dependents; rustc-cfg/check-cfg/rustc-env/link-arg enter
//!   only this package.
//! - metadata (cargo::metadata=K=V) via `DEP_<LINKS>_<K>` env goes only to **direct dependents**
//!   build scripts (transitive dependents cannot see it); cargo does **not** auto-inject
//!   DEP_<LINKS>_ROOT (that is the -sys crate convention of self-emitting metadata=root, not cargo
//!   behavior).
//! - cargo does **not** auto-add --check-cfg for rustc-cfg: crates that need it emit
//!   rustc-check-cfg themselves.
//! - build script warnings are shown only for path packages; registry packages swallow them unless
//!   -vv.
//!
//! Rerun decision (cargo-equivalent semantics; the full rule list is on should_rerun): when the
//! rerun triggers are not met the script is skipped and output.txt is re-parsed into BuildOutput.
//! The instruction stream needs no serialization format of its own, so DEP_*/OUT_DIR/warning
//! playback is byte-for-byte identical to a real rerun. A fingerprint change (flags/dependencies/
//! toolchain/source) lands in a fresh fp directory that naturally has no record and therefore
//! reruns; what the record actually decides is the **fp-unchanged** case -- consecutive runs, env
//! changes, out-of-package rerun-if-changed paths. Registry packages emit no rerun-if-changed and
//! their sources are immutable by checksum, so they never rerun, which is the biggest win.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::manifest::ProfileFlags;
use super::resolve::{ResolvePlan, UnitClass, UnitDep};

/// Parsed instruction set produced by a single build-script execution.
#[derive(Clone, Debug, Default)]
pub struct BuildOutput {
    /// rustc-cfg raw string (`foo` / `foo="bar"`) → --cfg enters only this package's compilation.
    pub cfgs: Vec<String>,
    /// rustc-check-cfg raw string (`cfg(foo, values("bar"))`) → --check-cfg enters only this package.
    pub check_cfgs: Vec<String>,
    /// rustc-env (VAR=VALUE) → this package's compile-time env (readable via env!; injected through cmd.env).
    pub envs: Vec<(String, String)>,
    /// rustc-link-lib raw LIB segment ([KIND[:MOD]=]NAME) → -l enters only this package.
    pub link_libs: Vec<String>,
    /// rustc-link-search raw [KIND=]PATH → -L enters this package + transitive dependents.
    pub link_searches: Vec<String>,
    /// rustc-link-arg / rustc-link-arg-bins → -C link-arg= enters only this package
    /// (cargo hard errors when emitting link-arg-bins for a package with no bin target; v1 does not validate this,
    /// only collects — mirvm's final bin is session-interpreted and produces no link, so the flag is already lazy).
    pub link_args: Vec<String>,
    /// cargo::metadata=K=V → DEP_<LINKS>_<K> for direct dependents' build scripts.
    pub metadata: BTreeMap<String, String>,
    /// cargo::warning=MSG (driver gates display by from_registry, same standard as cargo).
    pub warnings: Vec<String>,
    /// cargo::rerun-if-changed=PATH (consumed by the record and rerun decision; ≥1 instances replace
    /// the default face -- cargo-equivalent: once emitted, only watch these paths, no full-tree scan).
    pub rerun_if_changed: Vec<String>,
    /// cargo::rerun-if-env-changed=VAR (added independently to the file face, applies to both faces).
    pub rerun_if_env_changed: Vec<String>,
}

/// Instruction parsing: line prefixes `cargo::` (new form, 1.77+) and `cargo:` (legacy single colon) are both accepted.
/// Unknown keys in new form are ignored (cargo forward-compatibility same standard); unknown legacy keys are taken as metadata
/// (cargo-equivalent — old build.rs `cargo:KEY=VALUE` is the legacy form of links metadata).
/// cargo::error → Err (driver appends crate name); rerun-if-* both forms are collected into
/// BuildOutput, which the rerun decision consumes (see should_rerun).
pub fn parse_instructions(stdout: &str) -> Result<BuildOutput, String> {
    let mut out = BuildOutput::default();
    for line in stdout.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("cargo::") {
            apply(&mut out, rest, false)?;
        } else if let Some(rest) = line.strip_prefix("cargo:") {
            apply(&mut out, rest, true)?;
        }
        // Remaining lines are the build script's own println! — cargo also ignores them (visible only with -vv)
    }
    Ok(out)
}

fn apply(out: &mut BuildOutput, instr: &str, legacy: bool) -> Result<(), String> {
    let (key, value) = match instr.split_once('=') {
        Some((k, v)) => (k, v),
        None => (instr, ""),
    };
    match key {
        "rustc-link-lib" => out.link_libs.push(value.to_string()),
        "rustc-link-search" => out.link_searches.push(value.to_string()),
        "rustc-flags" => {
            // Only -l/-L allowed (cargo-equivalent), split into the two categories
            for tok in value.split_whitespace() {
                if let Some(v) = tok.strip_prefix("-l") {
                    out.link_libs.push(v.to_string());
                } else if let Some(v) = tok.strip_prefix("-L") {
                    out.link_searches.push(v.to_string());
                } else {
                    return Err(format!("rustc-flags only allows -l/-L flags (got `{tok}`)"));
                }
            }
        }
        "rustc-cfg" => out.cfgs.push(value.to_string()),
        "rustc-check-cfg" => out.check_cfgs.push(value.to_string()),
        "rustc-env" => {
            let Some((k, v)) = value.split_once('=') else {
                return Err(format!("rustc-env missing `=`: `{value}`"));
            };
            out.envs.push((k.to_string(), v.to_string()));
        }
        "rustc-link-arg" | "rustc-link-arg-bins" => out.link_args.push(value.to_string()),
        "metadata" => {
            let Some((k, v)) = value.split_once('=') else {
                return Err(format!("metadata missing `=`: `{value}`"));
            };
            out.metadata.insert(k.to_string(), v.to_string());
        }
        "warning" => out.warnings.push(value.to_string()),
        "error" => return Err(format!("build script emitted cargo::error: {value}")),
        // Both instruction forms are accepted; cargo honors legacy rerun-if too.
        "rerun-if-changed" => out.rerun_if_changed.push(value.to_string()),
        "rerun-if-env-changed" => out.rerun_if_env_changed.push(value.to_string()),
        _ => {
            if legacy {
                out.metadata.insert(key.to_string(), value.to_string());
            }
        }
    }
    Ok(())
}

/// DEP_* key normalization (same as cargo envify): ASCII alphanumerics uppercased, everything else
/// `_` (my-links.x -> MY_LINKS_X).
pub fn envify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Full CARGO_CFG_* env set: the generic mapping of `rustc --print cfg` -- k="v" atoms grouped by
/// key, multiple values joined with commas, bare flags mapped to empty string values; the profile
/// then forces DEBUG_ASSERTIONS/PANIC; FEATURE = this package's enabled features comma-joined.
/// The atom set comes from manifest.rs's host_cfg_atoms (same source as cargo platform matching).
pub fn cargo_cfg_env(
    enabled_features: &BTreeSet<String>,
    profile: &ProfileFlags,
) -> BTreeMap<String, String> {
    let atoms = super::manifest::host_cfg_atoms();
    // key → collected non-bare values (host_cfg_atoms is a BTreeSet, lexicographic order is cargo's concatenation order)
    let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for atom in atoms {
        if let Some((k, v)) = atom.split_once('=') {
            grouped.entry(k).or_default().push(v.trim_matches('"'));
        } else {
            grouped.entry(atom.as_str()).or_default();
        }
    }
    let mut out = BTreeMap::new();
    for (k, vs) in &grouped {
        out.insert(format!("CARGO_CFG_{}", k.to_uppercase()), vs.join(","));
    }
    // profile forces two entries (cargo pins by profile, regardless of --print cfg atom presence)
    if profile.debug_assertions {
        out.insert("CARGO_CFG_DEBUG_ASSERTIONS".into(), String::new());
    } else {
        out.remove("CARGO_CFG_DEBUG_ASSERTIONS");
    }
    out.insert("CARGO_CFG_PANIC".into(), "unwind".into());
    out.insert(
        "CARGO_CFG_FEATURE".into(),
        enabled_features
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
    );
    out
}

/// Direct-dependency links metadata -> DEP_<LINKS>_<KEY> env: **only direct dependents** see it,
/// transitive dependents cannot; both links and metadata keys pass through envify; there is no
/// automatic ROOT -- cargo does not inject DEP_<LINKS>_ROOT.
pub fn dep_metadata_env(
    plan: &ResolvePlan,
    dep_edges: &[UnitDep],
    outputs: &BTreeMap<usize, BuildOutput>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for d in dep_edges {
        let du = &plan.units[d.unit];
        let Some(links) = &du.links else { continue };
        let Some(bo) = outputs.get(&d.unit) else {
            continue;
        };
        for (k, v) in &bo.metadata {
            env.insert(format!("DEP_{}_{}", envify(links), envify(k)), v.clone());
        }
    }
    env
}

/// All inputs for build script execution env (too many flat parameters, gathered into a context struct).
pub struct ExecCtx<'a> {
    /// Full CARGO_PKG_* set (unit.pkg_env / manifest.pkg_env).
    pub pkg_env: &'a BTreeMap<String, String>,
    /// Package root (also used as cwd).
    pub source_dir: &'a Path,
    /// This package's enabled features (CARGO_CFG_FEATURE).
    pub features: &'a BTreeSet<String>,
    pub profile: &'a ProfileFlags,
    /// OUT_DIR = build_root/<pkg>-<fp>/out.
    pub out_dir: &'a Path,
    /// DEP_* env from direct dependencies (produced by dep_metadata_env).
    pub dep_env: BTreeMap<String, String>,
    /// This package manifest `links` value (CARGO_MANIFEST_LINKS; not set if no links key —
    /// ring 0.17.14 build.rs `env::var("CARGO_MANIFEST_LINKS").unwrap()` empirically proves it,
    /// cargo docs: the manifest links value).
    pub links: Option<&'a str>,
    /// LD_LIBRARY_PATH component directories (host_deps + deps; proc-macro build-dep
    /// .so must be findable by dlopen at runtime).
    pub ld_dirs: &'a [PathBuf],
}

/// Full build script execution env (verified line by line against cargo 1.98). No CARGO_MAKEFLAGS:
/// the jobserver is absent, same handling as the cli.rs runner (serial scheduling has no token
/// protocol to hand out).
pub fn build_script_env(ctx: &ExecCtx) -> BTreeMap<String, String> {
    let mut env = ctx.pkg_env.clone();
    env.extend(cargo_cfg_env(ctx.features, ctx.profile));
    env.extend(ctx.dep_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    // CARGO_FEATURE_<NAME>=1 for each enabled feature (cargo-equivalent; build.rs detects features
    // through the canonical channel — cranelift-codegen decides generating
    // pulley_inst_gen.rs empirically; without it OUT_DIR output lacks files and include! blows up)
    for f in ctx.features {
        env.insert(format!("CARGO_FEATURE_{}", envify(f)), "1".to_string());
    }
    let sysroot = PathBuf::from(crate::options::build::DEFAULT_SYSROOT);
    let mut put = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };
    put("OUT_DIR", ctx.out_dir.display().to_string());
    put("CARGO_MANIFEST_DIR", ctx.source_dir.display().to_string());
    put(
        "CARGO_MANIFEST_PATH",
        ctx.source_dir.join("Cargo.toml").display().to_string(),
    );
    if let Some(links) = ctx.links {
        put("CARGO_MANIFEST_LINKS", links.to_string());
    }
    put("HOST", crate::options::build::HOST.to_string());
    put("TARGET", crate::options::build::HOST.to_string());
    // Cargo's built-in dev and test profiles both expose PROFILE as "debug"; the actual
    // differences are carried by OPT_LEVEL/DEBUG and friends.
    put("PROFILE", "debug".into());
    // Cargo's DEBUG says whether the profile emits debuginfo, not the optimization level. Every
    // profile this driver supports pins debuginfo=2, so it is true for O1/O2 as well.
    put("DEBUG", "true".into());
    put("OPT_LEVEL", ctx.profile.opt_level.to_string());
    put(
        "NUM_JOBS",
        std::thread::available_parallelism()
            .map(|n| n.get().to_string())
            .unwrap_or_else(|_| "1".into()),
    );
    put("RUSTC", sysroot.join("bin/rustc").display().to_string());
    put("RUSTDOC", sysroot.join("bin/rustdoc").display().to_string());
    put("RUST_RECURSION_COUNT", "1".into());
    // Difference from cargo: cargo fills the real cargo path, we fill mirvm's current exe —
    // this difference is observable in build.rs via env!("CARGO")/var("CARGO"), test fixtures do not rely on it
    let cargo = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "mirvm".into());
    put("CARGO", cargo);
    let cargo_home = std::env::var("CARGO_HOME").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{h}/.cargo"))
            .unwrap_or_default()
    });
    put("CARGO_HOME", cargo_home);
    // cargo form: <build dir family>:<deps>:<rustlib lib>:<toolchain lib>;
    // ours = host_deps + deps + two toolchain dirs
    let mut ld: Vec<String> = ctx
        .ld_dirs
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    ld.push(
        sysroot
            .join(format!("lib/rustlib/{}/lib", crate::options::build::HOST))
            .display()
            .to_string(),
    );
    ld.push(sysroot.join("lib").display().to_string());
    put("LD_LIBRARY_PATH", ld.join(":"));
    env
}

/// Synchronously execute build script: cwd = package root; stdin null; stdout captured (= instruction stream);
/// stderr captured, returned with error only on failure (cargo-equivalent). Non-zero exit is a loud error.
pub fn run_build_script(
    exe: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<String, String> {
    let out = std::process::Command::new(exe)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .envs(env)
        .output()
        .map_err(|e| format!("build script launch failed {}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!(
            "build script exited non-zero ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("build script stdout not UTF-8: {e}"))
}

/// links mutual exclusion (cargo-equivalent: same links value at most one package — prevents duplicate symbols). Root package and
/// all units checked together; Normal/Build dual units of the same package sharing links are not conflicts
/// (deduplicated by (package, version)).
pub fn check_links_unique(
    root: Option<(&str, Option<&str>)>,
    plan: &ResolvePlan,
) -> Result<(), String> {
    let mut seen: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut note = |links: &str, pkg: &str, ver: &str| -> Result<(), String> {
        match seen.get(links) {
            Some((p, v)) if p == pkg && v == ver => Ok(()),
            Some((p, v)) => Err(format!(
                "links key conflict: `{links}` declared by both {p} {v} and {pkg} {ver} (cargo-equivalent rejection: same links at most one package)"
            )),
            None => {
                seen.insert(links.to_string(), (pkg.to_string(), ver.to_string()));
                Ok(())
            }
        }
    };
    if let Some((name, Some(links))) = root {
        note(links, name, "(root package)")?;
    }
    for u in &plan.units {
        if let Some(links) = &u.links {
            note(links, &u.package, &u.version.to_string())?;
        }
    }
    Ok(())
}

/// -L propagation collection: rustc-link-search enters this package + all transitive dependents.
/// Starting from Normal-class edges in `edges`, BFS along Normal edges, collecting all executed
/// BuildOutput link_searches. A proc-macro unit collects its own but does not go deeper: its
/// dependencies are host-world, unrelated to target linking (same boundary as target_units).
pub fn aggregate_link_searches(
    plan: &ResolvePlan,
    edges: &[UnitDep],
    outputs: &BTreeMap<usize, BuildOutput>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut stack: Vec<usize> = edges
        .iter()
        .filter(|d| d.class == UnitClass::Normal)
        .map(|d| d.unit)
        .collect();
    while let Some(i) = stack.pop() {
        if !seen.insert(i) {
            continue;
        }
        if let Some(bo) = outputs.get(&i) {
            out.extend(bo.link_searches.iter().cloned());
        }
        if plan.units[i].proc_macro {
            continue;
        }
        stack.extend(
            plan.units[i]
                .deps
                .iter()
                .filter(|d| d.class == UnitClass::Normal)
                .map(|d| d.unit),
        );
    }
    out
}

// ---------- rerun-if fine-grained incrementality (record <-> rerun decision, cargo-equivalent semantics) ----------
//
// Two records land in `build/<pkg>-<fp>/` (driver's record_dir):
// - `output.txt`: **raw stdout** from the last execution. When not rerunning, re-parse it with
//   parse_instructions into BuildOutput -- the instruction stream needs no serialization format of
//   its own (DEP_*/OUT_DIR/warning playback fully equivalent).
// - `rerun.txt`: rerun-condition record, hand-written line format (no serialization dependency):
//     first line `mirvm-bldrs-rerun-v1 changed` | `mirvm-bldrs-rerun-v1 default`
//       -- changed = emitted ≥1 rerun-if-changed (replaces the default face); default = none emitted.
//     changed face, one line per path: `P\t<len>\t<mtime_ns>\t<esc(original path)>`
//       (a stat failure at record time is logged as `P\t-\t-\t...`; current absence or record
//       absence at decision time both count as change -- conservative, cargo-equivalent).
//     default face path/root-package tree snapshot as one line: `T\t<folded string>`, folded like
//       source_stamp_dir ((path:len:mtime_ns) sorted and joined with \u{1e}, taking the rest of the
//       line as a single column); registry package sources are immutable by checksum, so they get
//       no T line and the decision skips directly.
//     rerun-if-env-changed, one variable per line, applies to both faces:
//       `E0\t<esc(var)>` (absent then) / `E1\t<esc(var)>\t<esc(value)>`.
//   esc: `\`->`\\`, tab->`\t`, newline->`\n` (covers the theoretical case of a path or env value
//   containing a column separator; non-UTF-8 paths go through display lossily -- record and
//   decision both use the display string, and a stat failure counts as change, conservative and
//   unable to miss a rerun).
// Records are written **after successful execution** (on failure the driver already exits loudly,
// so no half record remains); a missing file or a parse failure makes the decision run, self-healing.

/// Inline escaping (see format description above).
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            _ => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> Result<String, String> {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('\\') => o.push('\\'),
                Some('t') => o.push('\t'),
                Some('n') => o.push('\n'),
                other => return Err(format!("rerun.txt bad escape \\{}", other.unwrap_or('?'))),
            }
        } else {
            o.push(c);
        }
    }
    Ok(o)
}

/// A rerun-if-changed path record snapshot (len, mtime_ns); None = absent at record time
/// (current absence/record absence at decision time both count as change — conservative, cargo-equivalent).
type FileStamp = Option<(u64, u128)>;

/// Parsed rerun.txt, the input to should_rerun's decision.
struct RerunRecord {
    /// Some = changed face: (display-original path, record snapshot).
    changed_paths: Option<Vec<(String, FileStamp)>>,
    /// default face path/root-package tree snapshot (none for registry).
    tree: Option<String>,
    /// (var, value at that time (None = absent then)).
    envs: Vec<(String, Option<String>)>,
}

fn parse_record(text: &str) -> Result<RerunRecord, String> {
    let mut lines = text.lines();
    let face = lines.next().ok_or("rerun.txt empty")?;
    let mut rec = RerunRecord {
        changed_paths: match face {
            "mirvm-bldrs-rerun-v1 changed" => Some(Vec::new()),
            "mirvm-bldrs-rerun-v1 default" => None,
            _ => return Err(format!("rerun.txt first line unrecognized: {face}")),
        },
        tree: None,
        envs: Vec::new(),
    };
    for line in lines {
        if let Some(rest) = line.strip_prefix("P\t") {
            let mut f = rest.splitn(3, '\t');
            let (len, mtime, path) = (
                f.next().unwrap_or(""),
                f.next().unwrap_or(""),
                f.next().ok_or("P line missing path column")?,
            );
            let stamp = if len == "-" && mtime == "-" {
                None
            } else {
                Some((
                    len.parse::<u64>()
                        .map_err(|_| format!("bad P line len: {len}"))?,
                    mtime
                        .parse::<u128>()
                        .map_err(|_| format!("bad P line mtime: {mtime}"))?,
                ))
            };
            rec.changed_paths
                .as_mut()
                .ok_or("default face mixed with P line")?
                .push((unesc(path)?, stamp));
        } else if let Some(stamp) = line.strip_prefix("T\t") {
            rec.tree = Some(stamp.to_string());
        } else if let Some(var) = line.strip_prefix("E0\t") {
            rec.envs.push((unesc(var)?, None));
        } else if let Some(rest) = line.strip_prefix("E1\t") {
            let (var, val) = rest
                .split_once('\t')
                .ok_or("E1 line missing value column")?;
            rec.envs.push((unesc(var)?, Some(unesc(val)?)));
        } else {
            return Err(format!("rerun.txt bad line: {line}"));
        }
    }
    Ok(rec)
}

/// (len, mtime_ns) snapshot (same in-line retrieval as source_stamp_dir: mtime failure recorded as 0).
fn len_mtime(p: &Path) -> Option<(u64, u128)> {
    let md = std::fs::metadata(p).ok()?;
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some((md.len(), mtime_ns))
}

/// rerun-if-changed PATH parsing: absolute path as-is, relative path joined to package root (cargo-equivalent —
/// relative paths are relative to CARGO_MANIFEST_DIR).
fn absolutize(pkg_root: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        pkg_root.join(path)
    }
}

/// Write records after successful execution (output.txt = raw stdout; rerun.txt = condition record).
/// `env_get` injected for unit testing; failures ignored by caller as "next no-record rerun self-heals".
pub fn write_record(
    record_dir: &Path,
    stdout: &str,
    bo: &BuildOutput,
    from_registry: bool,
    pkg: &str,
    pkg_root: &Path,
    env_get: &dyn Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let mut t = String::new();
    if bo.rerun_if_changed.is_empty() {
        t.push_str("mirvm-bldrs-rerun-v1 default\n");
        if !from_registry {
            // default face snapshot folded the same way as fingerprint stamp (source_stamp_dir excludes target/.git)
            let stamp = super::schedule::source_stamp_dir(false, pkg_root, pkg)?;
            t.push_str("T\t");
            t.push_str(&stamp);
            t.push('\n');
        }
    } else {
        t.push_str("mirvm-bldrs-rerun-v1 changed\n");
        for p in &bo.rerun_if_changed {
            let stamp = match len_mtime(&absolutize(pkg_root, p)) {
                Some((l, m)) => format!("{l}\t{m}"),
                None => "-\t-".to_string(),
            };
            t.push_str(&format!("P\t{stamp}\t{}\n", esc(p)));
        }
    }
    for v in &bo.rerun_if_env_changed {
        match env_get(v) {
            Some(val) => t.push_str(&format!("E1\t{}\t{}\n", esc(v), esc(&val))),
            None => t.push_str(&format!("E0\t{}\n", esc(v))),
        }
    }
    let w = |name: &str, data: &str| {
        std::fs::write(record_dir.join(name), data)
            .map_err(|e| format!("writing {name} failed ({}): {e}", record_dir.display()))
    };
    w("rerun.txt", &t)?;
    w("output.txt", stdout)
}

/// Rerun decision (cargo-equivalent semantics; returns (should_rerun, reason_phrase) — reason for
/// MIRVM_DEBUG_BLDRS=1 `bldrs run|skip <pkg> <reason>` observation line).
/// Rules:
/// 1. Missing either record file ⇒ run (`no-record`; fp change ⇒ new fp directory naturally takes this path,
///    "source/flag/dependency/toolchain change ⇒ rerun" is already covered for free by fingerprint).
/// 2. rerun.txt corrupted ⇒ run (`bad-record`, self-heals).
/// 3. A direct dependency with links reran in this session ⇒ run (`links-dep:<dep>`;
///    DEP_* input may change, cargo-equivalent. Only direct dependencies — DEP_* is only given to direct
///    dependents; farther transit is covered by each layer's own decision).
/// 4. rerun-if-env-changed=VAR: current process value ≠ recorded value ⇒ run (`env:<var>`).
/// 5. File face:
///    - changed face: any PATH's (len, mtime_ns) ≠ recorded ⇒ run
///      (`changed-path:<p>`; current absence or record absence both count as change — conservative, cargo-equivalent).
///    - default face: registry ⇒ never rerun (`registry-default-skip`, source immutable by cksum
///      immutable); path/root-package ⇒ recomputed tree snapshot ≠ record ⇒ run (`default-tree`).
///
///    All consistent ⇒ skip (`changed-intact` / `default-tree-intact`).
pub fn should_rerun(
    record_dir: &Path,
    from_registry: bool,
    pkg: &str,
    pkg_root: &Path,
    dep_links_reran: &[String],
    env_get: &dyn Fn(&str) -> Option<String>,
) -> (bool, String) {
    let rerun_txt = record_dir.join("rerun.txt");
    let text = match std::fs::read_to_string(&rerun_txt) {
        Ok(t) if record_dir.join("output.txt").is_file() => t,
        _ => return (true, "no-record".to_string()),
    };
    let rec = match parse_record(&text) {
        Ok(r) => r,
        Err(_) => return (true, "bad-record".to_string()),
    };
    if let Some(dep) = dep_links_reran.first() {
        return (true, format!("links-dep:{dep}"));
    }
    for (var, old) in &rec.envs {
        if env_get(var) != *old {
            return (true, format!("env:{var}"));
        }
    }
    match &rec.changed_paths {
        Some(paths) => {
            for (disp, old) in paths {
                if len_mtime(&absolutize(pkg_root, disp)) != *old {
                    return (true, format!("changed-path:{disp}"));
                }
            }
            (false, "changed-intact".to_string())
        }
        None => {
            if from_registry {
                return (false, "registry-default-skip".to_string());
            }
            let Some(old) = &rec.tree else {
                // default-face path package record must have a T line — missing it self-heals as corrupted
                return (true, "bad-record".to_string());
            };
            match super::schedule::source_stamp_dir(false, pkg_root, pkg) {
                Ok(now) if &now == old => (false, "default-tree-intact".to_string()),
                // tree unread also counts as change (conservative; fp stage already read the same tree, rarely reaches here)
                _ => (true, "default-tree".to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_form_all_keys() {
        let out = parse_instructions(
            "cargo::rustc-link-lib=static=probehelper\n\
             cargo::rustc-link-search=native=/opt/probe/lib\n\
             cargo::rustc-flags=-lfoo -L/bar\n\
             cargo::rustc-cfg=probe_feat\n\
             cargo::rustc-cfg=has_val=\"x\"\n\
             cargo::rustc-check-cfg=cfg(probe_feat2)\n\
             cargo::rustc-env=ROOT_SEEN=bar\n\
             cargo::rustc-link-arg=-Wl,--x\n\
             cargo::rustc-link-arg-bins=-Wl,--y\n\
             cargo::metadata=foo=bar\n\
             cargo::warning=be careful\n\
             cargo::rerun-if-changed=build.rs\n\
             cargo::rerun-if-env-changed=CC\n\
             cargo::future-new-key=ignored\n\
             ordinary output line ignored\n",
        )
        .unwrap();
        assert_eq!(out.link_libs, ["static=probehelper", "foo"]);
        assert_eq!(out.link_searches, ["native=/opt/probe/lib", "/bar"]);
        assert_eq!(out.cfgs, ["probe_feat", "has_val=\"x\""]);
        assert_eq!(out.check_cfgs, ["cfg(probe_feat2)"]);
        assert_eq!(out.envs, [("ROOT_SEEN".to_string(), "bar".to_string())]);
        assert_eq!(out.link_args, ["-Wl,--x", "-Wl,--y"]);
        assert_eq!(out.metadata.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(out.warnings, ["be careful"]);
        // rerun-if keys collected into BuildOutput (consumed by the rerun decision)
        assert_eq!(out.rerun_if_changed, ["build.rs"]);
        assert_eq!(out.rerun_if_env_changed, ["CC"]);
    }

    #[test]
    fn parse_legacy_single_colon_and_unknown_as_metadata() {
        // legacy single colon: known keys as usual, unknown keys taken as metadata (cargo-equivalent)
        let out = parse_instructions(
            "cargo:rustc-link-lib=z\n\
             cargo:rustc-cfg=old\n\
             cargo:root=/opt/sys\n\
             cargo:rustc-link-search=/p\r\n",
        )
        .unwrap();
        assert_eq!(out.link_libs, ["z"]);
        assert_eq!(out.cfgs, ["old"]);
        assert_eq!(out.link_searches, ["/p"], "CRLF tail must be stripped");
        assert_eq!(
            out.metadata.get("root").map(String::as_str),
            Some("/opt/sys")
        );
    }

    #[test]
    fn parse_error_and_bad_forms_are_loud() {
        let e = parse_instructions("cargo::error=missing libfoo").unwrap_err();
        assert!(e.contains("missing libfoo"), "{e}");
        let e = parse_instructions("cargo::rustc-env=NOEQ").unwrap_err();
        assert!(e.contains("rustc-env"), "{e}");
        let e = parse_instructions("cargo::rustc-flags=-O2").unwrap_err();
        assert!(e.contains("-l/-L"), "{e}");
        // new-form unknown keys silently ignored (forward compatibility), no error
        parse_instructions("cargo::brand-new=1").unwrap();
    }

    #[test]
    fn envify_normalizes_dep_keys() {
        assert_eq!(envify("my-links.x"), "MY_LINKS_X");
        assert_eq!(envify("sysd"), "SYSD");
        assert_eq!(envify("foo_bar"), "FOO_BAR");
    }

    #[test]
    fn build_script_env_marks_each_enabled_feature() {
        // cranelift-codegen keys the generation of pulley_inst_gen.rs off CARGO_FEATURE_PULLEY,
        // so CARGO_FEATURE_<NAME>=1 must be present for every enabled feature: without it the
        // OUT_DIR output lacks files and include! blows up.
        let pkg_env = BTreeMap::new();
        let features: BTreeSet<String> = ["pulley", "std"].iter().map(|s| s.to_string()).collect();
        let env = build_script_env(&ExecCtx {
            pkg_env: &pkg_env,
            source_dir: Path::new("/tmp/x"),
            features: &features,
            profile: &ProfileFlags::default(),
            out_dir: Path::new("/tmp/x/out"),
            dep_env: BTreeMap::new(),
            links: None,
            ld_dirs: &[],
        });
        assert_eq!(
            env.get("CARGO_FEATURE_PULLEY").map(String::as_str),
            Some("1")
        );
        assert_eq!(env.get("CARGO_FEATURE_STD").map(String::as_str), Some("1"));
        assert!(!env.contains_key("CARGO_FEATURE_NOPE"));
    }

    #[test]
    fn build_script_env_sets_manifest_links_only_with_links() {
        // ring 0.17.14 build.rs `env::var("CARGO_MANIFEST_LINKS").unwrap()`: set when the manifest
        // has a links key, absent without one (cargo-documented).
        let pkg_env = BTreeMap::new();
        let features = BTreeSet::new();
        let mk = |links: Option<&str>| {
            build_script_env(&ExecCtx {
                pkg_env: &pkg_env,
                source_dir: Path::new("/tmp/x"),
                features: &features,
                profile: &ProfileFlags::default(),
                out_dir: Path::new("/tmp/x/out"),
                dep_env: BTreeMap::new(),
                links,
                ld_dirs: &[],
            })
        };
        assert_eq!(
            mk(Some("ring_core_0_17_14"))
                .get("CARGO_MANIFEST_LINKS")
                .map(String::as_str),
            Some("ring_core_0_17_14")
        );
        assert!(!mk(None).contains_key("CARGO_MANIFEST_LINKS"));
    }

    #[test]
    fn cargo_cfg_env_maps_atoms() {
        let feats: BTreeSet<String> = ["derive", "std"].iter().map(|s| s.to_string()).collect();
        let env = cargo_cfg_env(&feats, &ProfileFlags::default());
        // Real x86_64-linux nightly atoms: multiple values comma-joined, bare flags empty strings.
        assert_eq!(
            env.get("CARGO_CFG_TARGET_ARCH").map(String::as_str),
            Some("x86_64")
        );
        assert_eq!(
            env.get("CARGO_CFG_UNIX").map(String::as_str),
            Some(""),
            "bare unix flag → empty string value"
        );
        let atomic = env.get("CARGO_CFG_TARGET_HAS_ATOMIC").unwrap();
        assert!(
            atomic.split(',').count() >= 4 && atomic.contains("ptr"),
            "multiple values comma-joined: {atomic}"
        );
        assert_eq!(
            env.get("CARGO_CFG_FEATURE").map(String::as_str),
            Some("derive,std"),
            "features comma-joined (BTreeSet lexicographic order)"
        );
        assert_eq!(
            env.get("CARGO_CFG_DEBUG_ASSERTIONS").map(String::as_str),
            Some(""),
            "dev profile → present empty string"
        );
        assert_eq!(
            env.get("CARGO_CFG_PANIC").map(String::as_str),
            Some("unwind")
        );
        // debug_assertions follows the profile
        let rel = ProfileFlags {
            debug_assertions: false,
            overflow_checks: false,
            opt_level: crate::cargoless::manifest::OptLevel::O2,
        };
        let env2 = cargo_cfg_env(&BTreeSet::new(), &rel);
        assert!(!env2.contains_key("CARGO_CFG_DEBUG_ASSERTIONS"));
        assert_eq!(env2.get("CARGO_CFG_FEATURE").map(String::as_str), Some(""));
    }

    // ---- rerun decision matrix + record round-trip ----

    /// One independent temp directory per test (parallel-safe); returns (record_dir, pkg_root).
    fn rerun_tmp(tag: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("mirvm-bldrs-rerun-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let record = base.join("record");
        let root = base.join("pkg");
        std::fs::create_dir_all(&record).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        (record, root)
    }

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    /// changed-face fixture: in-package build.rs + one env variable.
    fn changed_bo() -> BuildOutput {
        BuildOutput {
            rerun_if_changed: vec!["build.rs".into()],
            rerun_if_env_changed: vec!["X".into()],
            ..Default::default()
        }
    }

    #[test]
    fn rerun_no_record_runs() {
        let (record, root) = rerun_tmp("no-record");
        let env = env_of(&[]);
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "no-record", "{why}");
        // rerun.txt alone without output.txt also yields no-record (both files required)
        std::fs::write(record.join("rerun.txt"), "mirvm-bldrs-rerun-v1 default\n").unwrap();
        let (run, why) = should_rerun(&record, true, "demo", &root, &[], &env);
        assert!(run && why == "no-record", "{why}");
    }

    #[test]
    fn changed_face_roundtrip_skips() {
        let (record, root) = rerun_tmp("roundtrip");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "stdout", &changed_bo(), false, "demo", &root, &env).unwrap();
        // record round-trip: same env same file ⇒ skip (changed-intact)
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(!run && why == "changed-intact", "{why}");
    }

    #[test]
    fn changed_path_mtime_or_absence_runs() {
        let (record, root) = rerun_tmp("mtime");
        let f = root.join("build.rs");
        std::fs::write(&f, "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "s", &changed_bo(), false, "demo", &root, &env).unwrap();
        // only mtime changed (len unchanged) ⇒ run ((len, mtime_ns) tuple comparison)
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1000))
            .unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "changed-path:build.rs", "{why}");
        // file absence counts as change (conservative, cargo-equivalent)
        std::fs::remove_file(&f).unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "changed-path:build.rs", "{why}");
    }

    #[test]
    fn env_value_change_runs() {
        let (record, root) = rerun_tmp("env");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        write_record(
            &record,
            "s",
            &changed_bo(),
            false,
            "demo",
            &root,
            &env_of(&[("X", "1")]),
        )
        .unwrap();
        // value change ⇒ run; value removed (absent) ⇒ run
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env_of(&[("X", "2")]));
        assert!(run && why == "env:X", "{why}");
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env_of(&[]));
        assert!(run && why == "env:X", "{why}");
    }

    #[test]
    fn registry_default_face_never_reruns() {
        let (record, root) = rerun_tmp("registry-default");
        // registry package default face: pkg_root need not even be created (tree not read) ⇒ skip
        let env = env_of(&[]);
        write_record(
            &record,
            "s",
            &BuildOutput::default(),
            true,
            "libc",
            &root,
            &env,
        )
        .unwrap();
        let (run, why) = should_rerun(&record, true, "libc", &root, &[], &env);
        assert!(!run && why == "registry-default-skip", "{why}");
    }

    #[test]
    fn path_default_face_tree_change_runs() {
        let (record, root) = rerun_tmp("default-tree");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let env = env_of(&[]);
        write_record(
            &record,
            "s",
            &BuildOutput::default(),
            false,
            "demo",
            &root,
            &env,
        )
        .unwrap();
        // round-trip skip (default-face tree snapshot consistent)
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(!run && why == "default-tree-intact", "{why}");
        // any in-package file change (added) ⇒ run
        std::fs::write(root.join("src/new.rs"), "").unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "default-tree", "{why}");
    }

    #[test]
    fn links_dep_rerun_propagates() {
        let (record, root) = rerun_tmp("links-dep");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "s", &changed_bo(), false, "demo", &root, &env).unwrap();
        // direct-dependency links package reran this session ⇒ this package also run (DEP_* input may change)
        let deps = vec!["bdep".to_string()];
        let (run, why) = should_rerun(&record, false, "demo", &root, &deps, &env);
        assert!(run && why == "links-dep:bdep", "{why}");
    }

    #[test]
    fn corrupt_record_runs() {
        let (record, root) = rerun_tmp("corrupt");
        std::fs::write(root.join("build.rs"), "fn main(){}").unwrap();
        let env = env_of(&[("X", "1")]);
        write_record(&record, "s", &changed_bo(), false, "demo", &root, &env).unwrap();
        std::fs::write(record.join("rerun.txt"), "this is not a record").unwrap();
        let (run, why) = should_rerun(&record, false, "demo", &root, &[], &env);
        assert!(run && why == "bad-record", "{why}");
    }

    #[test]
    fn esc_roundtrip_and_bad_escape() {
        assert_eq!(esc("a\\b\tc\nd"), "a\\\\b\\tc\\nd");
        assert_eq!(unesc(&esc("a\\b\tc\nd")).unwrap(), "a\\b\tc\nd");
        assert!(unesc("lone\\").is_err());
    }
}
