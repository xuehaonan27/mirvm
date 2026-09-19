//! Per-crate rustc argument tables: dependency, root, host and build-script compile recipes.

use std::path::{Path, PathBuf};

use crate::cargoless::buildrs::BuildOutput;
use crate::cargoless::manifest::{DepKind, PackageManifest, ProfileFlags, Target};
use crate::cargoless::resolve::{ResolvePlan, Unit, UnitClass};

use super::Layout;

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
