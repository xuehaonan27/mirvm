use super::*;
use crate::cargoless::buildrs::BuildOutput;
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
    // fp must change (the dependency-image pre-key invariant "a transitive closure change changes
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
        w[0] == "--extern" && w[1] == format!("normal=/tmp/cless/deps/libnormal-{}.rlib", fps[0])
    }));
    assert!(args.windows(2).any(|w| {
        w[0] == "--extern" && w[1] == format!("devonly=/tmp/cless/deps/libdevonly-{}.rlib", fps[1])
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
