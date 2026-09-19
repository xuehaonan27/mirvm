use std::collections::BTreeSet;
use std::path::Path;

use super::{
    MErr, OptLevel, ProfileFlags, RawBin, RawLib, RawProfile, RawProfiles, RawTarget, Target,
    TargetKind,
};

// ---------- target discovery ----------

#[allow(clippy::too_many_arguments)]
pub(super) fn discover_targets(
    lib: Option<&RawLib>,
    bin: Option<&Vec<RawBin>>,
    tests: Option<&Vec<RawTarget>>,
    examples: Option<&Vec<RawTarget>>,
    benches: Option<&Vec<RawTarget>>,
    autobins: bool,
    autotests: bool,
    autoexamples: bool,
    autobenches: bool,
    pkg_name: &str,
    root: &Path,
) -> Result<Vec<Target>, MErr> {
    let mut out = Vec::new();
    // lib: an explicit [lib] or the automatic src/lib.rs
    let lib_path = lib
        .and_then(|l| l.path.clone())
        .map(|p| root.join(&p))
        .filter(|p| p.is_file())
        .or_else(|| {
            let p = root.join("src/lib.rs");
            p.is_file().then_some(p)
        });
    if let Some(path) = lib_path {
        let name = lib
            .and_then(|l| l.name.clone())
            .unwrap_or_else(|| pkg_name.replace('-', "_"));
        let proc_macro = lib.and_then(|l| l.proc_macro).unwrap_or(false);
        out.push(Target {
            kind: TargetKind::Lib,
            name,
            path,
            proc_macro,
            test: lib.and_then(|l| l.test).unwrap_or(true),
            harness: lib.and_then(|l| l.harness).unwrap_or(true),
            doctest: lib.and_then(|l| l.doctest).unwrap_or(true),
            required_features: lib
                .and_then(|l| l.required_features.clone())
                .unwrap_or_default(),
        });
    }
    // bin: an explicit entry overrides the automatic target of the same name; with
    // autobins=true other targets are still discovered automatically.
    let explicit: Vec<Target> = bin
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let name = b.name.clone()?;
            let path = b
                .path
                .clone()
                .map(|p| root.join(&p))
                .or_else(|| {
                    let cand = root.join(format!("src/bin/{name}.rs"));
                    cand.is_file().then_some(cand)
                })
                .or_else(|| {
                    let cand = root.join("src/main.rs");
                    (name == pkg_name && cand.is_file()).then_some(cand)
                })?;
            Some(Target {
                kind: TargetKind::Bin,
                name,
                path,
                proc_macro: false,
                test: b.test.unwrap_or(true),
                harness: b.harness.unwrap_or(true),
                doctest: false,
                required_features: b.required_features.clone().unwrap_or_default(),
            })
        })
        .collect();
    let explicit_bin_names: BTreeSet<String> = explicit.iter().map(|t| t.name.clone()).collect();
    out.extend(explicit);
    if autobins {
        let main = root.join("src/main.rs");
        if main.is_file() && !explicit_bin_names.contains(pkg_name) {
            out.push(Target {
                kind: TargetKind::Bin,
                name: pkg_name.to_string(),
                path: main,
                proc_macro: false,
                test: true,
                harness: true,
                doctest: false,
                required_features: Vec::new(),
            });
        }
        let bin_dir = root.join("src/bin");
        if let Ok(rd) = std::fs::read_dir(&bin_dir) {
            let mut extra: Vec<Target> = rd
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    let (name, path) = if p.extension().is_some_and(|x| x == "rs") {
                        (p.file_stem()?.to_string_lossy().into_owned(), p.clone())
                    } else if p.is_dir() && p.join("main.rs").is_file() {
                        (
                            p.file_name()?.to_string_lossy().into_owned(),
                            p.join("main.rs"),
                        )
                    } else {
                        return None;
                    };
                    Some(Target {
                        kind: TargetKind::Bin,
                        name,
                        path,
                        proc_macro: false,
                        test: true,
                        harness: true,
                        doctest: false,
                        required_features: Vec::new(),
                    })
                })
                .collect();
            extra.retain(|target| !explicit_bin_names.contains(&target.name));
            extra.sort_by(|a, b| a.name.cmp(&b.name));
            out.extend(extra);
        }
    }

    out.extend(discover_file_targets(
        tests,
        autotests,
        TargetKind::Test,
        &root.join("tests"),
    ));
    out.extend(discover_file_targets(
        examples,
        autoexamples,
        TargetKind::Example,
        &root.join("examples"),
    ));
    out.extend(discover_file_targets(
        benches,
        autobenches,
        TargetKind::Bench,
        &root.join("benches"),
    ));
    Ok(out)
}

/// Cargo's tests/examples auto-discovery: `dir/name.rs` and `dir/name/main.rs` are each
/// one target; other `.rs` files in a subdirectory are modules and must not be mistaken
/// for separate targets. An explicit entry only overrides the automatic target of the
/// same name.
fn discover_file_targets(
    explicit: Option<&Vec<RawTarget>>,
    auto: bool,
    kind: TargetKind,
    dir: &Path,
) -> Vec<Target> {
    let mut out: Vec<Target> = explicit
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let path = row
                .path
                .as_ref()
                .and_then(|p| dir.parent().map(|root| root.join(p)))
                .or_else(|| {
                    let name = row.name.as_ref()?;
                    let flat = dir.join(format!("{name}.rs"));
                    flat.is_file().then_some(flat).or_else(|| {
                        let main = dir.join(name).join("main.rs");
                        main.is_file().then_some(main)
                    })
                })?;
            let name = row.name.clone().or_else(|| {
                path.parent()
                    .filter(|parent| parent.parent() == Some(dir))
                    .and_then(Path::file_name)
                    .or_else(|| path.file_stem())
                    .map(|n| n.to_string_lossy().into_owned())
            })?;
            Some(Target {
                kind,
                name,
                path,
                proc_macro: false,
                test: row.test.unwrap_or(kind != TargetKind::Example),
                harness: row.harness.unwrap_or(true),
                doctest: false,
                required_features: row.required_features.clone().unwrap_or_default(),
            })
        })
        .collect();
    if !auto {
        return out;
    }
    let explicit_names: BTreeSet<String> = out.iter().map(|target| target.name.clone()).collect();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut automatic: Vec<_> = rd
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let (name, path) = if path.extension().is_some_and(|x| x == "rs") {
                (path.file_stem()?.to_string_lossy().into_owned(), path)
            } else if path.is_dir() && path.join("main.rs").is_file() {
                (
                    path.file_name()?.to_string_lossy().into_owned(),
                    path.join("main.rs"),
                )
            } else {
                return None;
            };
            Some(Target {
                kind,
                name,
                path,
                proc_macro: false,
                test: kind != TargetKind::Example,
                harness: true,
                doctest: false,
                required_features: Vec::new(),
            })
        })
        .collect();
    automatic.retain(|target| !explicit_names.contains(&target.name));
    automatic.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(automatic);
    out
}

// ---------- profile ----------

pub(super) fn profiles_from(
    raw: Option<RawProfiles>,
) -> Result<(ProfileFlags, ProfileFlags), MErr> {
    let Some(raw) = raw else {
        let dev = ProfileFlags::default();
        return Ok((dev, dev));
    };
    let dev = apply_profile(ProfileFlags::default(), raw.dev, "dev")?;
    let test = apply_profile(dev, raw.test, "test")?;
    Ok((dev, test))
}

fn apply_profile(
    mut profile: ProfileFlags,
    raw: Option<RawProfile>,
    name: &str,
) -> Result<ProfileFlags, MErr> {
    let Some(raw) = raw else {
        return Ok(profile);
    };
    if let Some(v) = raw.debug_assertions {
        profile.debug_assertions = v;
    }
    if let Some(v) = raw.overflow_checks {
        profile.overflow_checks = v;
    }
    if let Some(value) = raw.opt_level {
        profile.opt_level = match value {
            toml::Value::Integer(0) => OptLevel::O0,
            toml::Value::Integer(1) => OptLevel::O1,
            toml::Value::Integer(2) => OptLevel::O2,
            toml::Value::Integer(3) => OptLevel::O3,
            toml::Value::String(v) if v == "s" => OptLevel::Os,
            toml::Value::String(v) if v == "z" => OptLevel::Oz,
            other => {
                return Err(format!(
                    "profile.{name}.opt-level is not a Cargo-supported value (only 0/1/2/3/\"s\"/\"z\"): {other}"
                ));
            }
        };
    }
    Ok(profile)
}
