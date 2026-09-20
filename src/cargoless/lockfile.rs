//! `cargoless/lockfile.rs` -- Cargo.lock read/write.
//!
//! Read: the v3/v4 format (`version = N` header plus `[[package]]` tables);
//! a path package has no source/checksum. Dependency lines come in the forms
//! `"name"` / `"name version"` / `"name version (source)"` and are parsed
//! loosely as strings (only the name and the optional version are kept).
//! Write: keep the v3/v4 header the model specifies, with package rows in the
//! canonical form common to both (resolve writes the lock to support
//! reproducibility and `cargo --locked` refutation).
//!
//! Supported sources are crates.io registry and an exact Git commit; a Git
//! package has no checksum.

#![allow(dead_code)]

use std::path::Path;

/// One locked package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockedPkg {
    pub name: String,
    pub version: semver::Version,
    /// `"registry+https://github.com/rust-lang/crates.io-index"`; `None` for a path package.
    pub source: Option<String>,
    pub checksum: Option<String>,
    /// Cargo's legacy `[replace]`: the original package row points at a replacement
    /// package row with the same name and version.
    pub replace: Option<String>,
    /// This row's dependency references; packages sharing a name are distinguished by
    /// version, and still-ambiguous ones by source.
    pub dependencies: Vec<LockedDep>,
}

/// One dependency reference on a locked package row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockedDep {
    pub name: String,
    pub version: Option<semver::Version>,
    pub source: Option<String>,
}

/// A `[patch]` candidate Cargo keeps in the lock without using it in the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnusedPatch {
    pub name: String,
    pub version: semver::Version,
    pub source: Option<String>,
    pub checksum: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lockfile {
    pub format_version: u32,
    pub packages: Vec<LockedPkg>,
    pub unused_patches: Vec<UnusedPatch>,
}

type LErr = String;

impl Lockfile {
    pub fn read(path: &Path) -> Result<Self, LErr> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, LErr> {
        #[derive(serde::Deserialize)]
        struct RawLock {
            version: Option<u32>,
            package: Option<Vec<RawPkg>>,
            patch: Option<RawPatch>,
        }
        #[derive(serde::Deserialize)]
        struct RawPatch {
            unused: Option<Vec<RawUnusedPatch>>,
        }
        #[derive(serde::Deserialize)]
        struct RawUnusedPatch {
            name: String,
            version: String,
            source: Option<String>,
            checksum: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct RawPkg {
            name: String,
            version: String,
            source: Option<String>,
            checksum: Option<String>,
            replace: Option<String>,
            dependencies: Option<Vec<String>>,
        }
        let raw: RawLock =
            toml::from_str(text).map_err(|e| format!("failed to parse Cargo.lock: {e}"))?;
        let format_version = raw.version.unwrap_or(1);
        if !(1..=4).contains(&format_version) {
            return Err(format!(
                "Cargo.lock format version {format_version} is outside the supported subset"
            ));
        }
        let mut packages = Vec::new();
        for p in raw.package.unwrap_or_default() {
            if let Some(src) = &p.source
                && !src.starts_with("registry+")
                && !src.starts_with("sparse+")
                && !src.starts_with("git+")
            {
                return Err(format!(
                    "source of lock package {} is outside the supported subset: {src}",
                    p.name
                ));
            }
            if p.source
                .as_deref()
                .is_some_and(|source| source.starts_with("git+"))
            {
                let source = p.source.as_deref().unwrap();
                let precise = source.rsplit_once('#').map(|(_, precise)| precise);
                if !precise.is_some_and(|precise| {
                    matches!(precise.len(), 40 | 64)
                        && precise.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) {
                    return Err(format!(
                        "source of lock Git package {} lacks a 40/64-digit precise commit: {source}",
                        p.name
                    ));
                }
                if p.checksum.is_some() {
                    return Err(format!(
                        "lock Git package {} must not carry a checksum",
                        p.name
                    ));
                }
            }
            let version = semver::Version::parse(&p.version).map_err(|e| {
                format!(
                    "invalid version {} of lock package {}: {e}",
                    p.version, p.name
                )
            })?;
            let dependencies = p
                .dependencies
                .unwrap_or_default()
                .into_iter()
                .map(|line| parse_dep_line(&line))
                .collect::<Result<Vec<_>, _>>()?;
            packages.push(LockedPkg {
                name: p.name,
                version,
                source: p.source,
                checksum: p.checksum,
                replace: p.replace,
                dependencies,
            });
        }
        let unused_patches = raw
            .patch
            .and_then(|patch| patch.unused)
            .unwrap_or_default()
            .into_iter()
            .map(|patch| {
                let version = semver::Version::parse(&patch.version).map_err(|error| {
                    format!(
                        "invalid version {} of unused lock patch {}: {error}",
                        patch.version, patch.name
                    )
                })?;
                Ok(UnusedPatch {
                    name: patch.name,
                    version,
                    source: patch.source,
                    checksum: patch.checksum,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            format_version,
            packages,
            unused_patches,
        })
    }

    /// All locked versions of a name (several versions of one name may coexist in a lock).
    pub fn find(&self, name: &str) -> Vec<&LockedPkg> {
        self.packages.iter().filter(|p| p.name == name).collect()
    }

    /// Exact lookup by name and version.
    pub fn get(&self, name: &str, version: &semver::Version) -> Option<&LockedPkg> {
        self.packages
            .iter()
            .find(|p| p.name == name && p.version == *version)
    }

    /// Canonical v3/v4 serialization, used to write resolve's own lock; packages are
    /// sorted by (name, version) for determinism.
    pub fn serialize(&self) -> String {
        debug_assert!(matches!(self.format_version, 3 | 4));
        let mut out = format!(
            "# This file is automatically @generated by Cargo.\n\
             # It is not intended for manual editing.\nversion = {}\n",
            self.format_version
        );
        let mut pkgs: Vec<&LockedPkg> = self.packages.iter().collect();
        pkgs.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then(a.version.cmp(&b.version))
                .then(a.source.cmp(&b.source))
        });
        for p in pkgs {
            out.push_str("\n[[package]]\n");
            out.push_str(&format!("name = \"{}\"\n", p.name));
            out.push_str(&format!("version = \"{}\"\n", p.version));
            if let Some(src) = &p.source {
                out.push_str(&format!("source = \"{src}\"\n"));
            }
            if let Some(sum) = &p.checksum {
                out.push_str(&format!("checksum = \"{sum}\"\n"));
            }
            if let Some(replace) = &p.replace {
                out.push_str(&format!("replace = \"{replace}\"\n"));
            }
            if !p.dependencies.is_empty() {
                // cargo canonical form: every line carries a trailing comma, the last one
                // included. `--locked` rejects a non-canonical lock as "needs rewrite"
                // (confirmed against c_serde_json).
                let mut lines: Vec<String> = p
                    .dependencies
                    .iter()
                    .map(
                        |dependency| match (&dependency.version, &dependency.source) {
                            (Some(version), Some(source)) => {
                                format!(" \"{} {} ({})\",", dependency.name, version, source)
                            }
                            (Some(version), None) => {
                                format!(" \"{} {}\",", dependency.name, version)
                            }
                            (None, None) => format!(" \"{}\",", dependency.name),
                            (None, Some(_)) => {
                                unreachable!("source disambiguation always carries a version")
                            }
                        },
                    )
                    .collect();
                lines.sort();
                out.push_str(&format!("dependencies = [\n{}\n]\n", lines.join("\n")));
            }
        }
        let mut unused = self.unused_patches.clone();
        unused.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then(left.version.cmp(&right.version))
                .then(left.source.cmp(&right.source))
        });
        for patch in unused {
            out.push_str("\n[[patch.unused]]\n");
            out.push_str(&format!("name = \"{}\"\n", patch.name));
            out.push_str(&format!("version = \"{}\"\n", patch.version));
            if let Some(source) = patch.source {
                out.push_str(&format!("source = \"{source}\"\n"));
            }
            if let Some(checksum) = patch.checksum {
                out.push_str(&format!("checksum = \"{checksum}\"\n"));
            }
        }
        out
    }
}

/// A lock dependency line: `"name"` / `"name version"` / `"name version (source)"`.
fn parse_dep_line(line: &str) -> Result<LockedDep, LErr> {
    let line = line.trim();
    let (without_src, source) = line
        .strip_suffix(')')
        .and_then(|l| l.split_once(" ("))
        .map(|(line, source)| (line, Some(source.to_string())))
        .unwrap_or((line, None));
    let mut it = without_src.split_whitespace();
    let name = it
        .next()
        .ok_or_else(|| format!("invalid lock dependency line: {line}"))?;
    let version = it
        .next()
        .map(|v| {
            semver::Version::parse(v)
                .map_err(|e| format!("invalid version {v} on lock dependency line {line}: {e}"))
        })
        .transpose()?;
    if source.is_some() && version.is_none() {
        return Err(format!(
            "a lock dependency line with a source must carry a version: {line}"
        ));
    }
    if let Some(source) = &source {
        if !source.starts_with("registry+")
            && !source.starts_with("sparse+")
            && !source.starts_with("git+")
        {
            return Err(format!(
                "source of lock dependency line is outside the supported subset: {source}"
            ));
        }
        if source.starts_with("git+") && source.contains('#') {
            return Err(format!(
                "source of a lock Git dependency line must not carry a precise commit fragment: {source}"
            ));
        }
    }
    Ok(LockedDep {
        name: name.to_string(),
        version,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v4_lock_with_registry_and_path_packages() {
        let lf = Lockfile::parse(
            r#"
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = "demo"
version = "0.1.0"
dependencies = [
 "anyhow",
 "local 0.2.0",
]

[[package]]
name = "anyhow"
version = "1.0.103"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "1b1c0055b1c8f8e3f4a8b1d2c3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2"

[[package]]
name = "local"
version = "0.2.0"
"#,
        )
        .unwrap();
        assert_eq!(lf.format_version, 4);
        assert_eq!(lf.packages.len(), 3);
        let demo = lf.get("demo", &semver::Version::new(0, 1, 0)).unwrap();
        assert_eq!(demo.source, None);
        assert_eq!(
            demo.dependencies,
            vec![
                LockedDep {
                    name: "anyhow".to_string(),
                    version: None,
                    source: None,
                },
                LockedDep {
                    name: "local".to_string(),
                    version: Some(semver::Version::new(0, 2, 0)),
                    source: None,
                }
            ]
        );
        let anyhow = &lf.find("anyhow")[0];
        assert!(anyhow.source.as_ref().unwrap().starts_with("registry+"));
        assert!(anyhow.checksum.is_some());
        assert_eq!(lf.find("local")[0].source, None);
    }

    #[test]
    fn parses_git_source_with_precise_commit() {
        let lock = Lockfile::parse(
            "version = 4\n\n[[package]]\nname = \"g\"\nversion = \"0.1.0\"\n\
             source = \"git+https://github.com/x/y#0123456789abcdef0123456789abcdef01234567\"\n",
        )
        .unwrap();
        assert_eq!(
            lock.packages[0].source.as_deref(),
            Some("git+https://github.com/x/y#0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn parses_git_dependency_source_without_precise_fragment() {
        let dependency = parse_dep_line(
            "g 0.1.0 (git+https://github.com/x/y?rev=0123456789abcdef0123456789abcdef01234567)",
        )
        .unwrap();
        assert_eq!(dependency.name, "g");
        assert_eq!(dependency.version, Some(semver::Version::new(0, 1, 0)));
        assert_eq!(
            dependency.source.as_deref(),
            Some("git+https://github.com/x/y?rev=0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn rejects_git_source_without_precise_commit() {
        let err = Lockfile::parse(
            "version = 4\n\n[[package]]\nname='g'\nversion='0.1.0'\n\
             source='git+https://github.com/x/y?branch=main'\n",
        )
        .unwrap_err();
        assert!(err.contains("precise commit"), "{err}");
    }

    #[test]
    fn serialize_roundtrips_deterministically() {
        let lf = Lockfile {
            format_version: 4,
            packages: vec![
                LockedPkg {
                    name: "b".into(),
                    version: semver::Version::new(1, 0, 0),
                    source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
                    checksum: Some("deadbeef".into()),
                    replace: None,
                    dependencies: vec![LockedDep {
                        name: "a".into(),
                        version: None,
                        source: None,
                    }],
                },
                LockedPkg {
                    name: "a".into(),
                    version: semver::Version::new(0, 2, 0),
                    source: None,
                    checksum: None,
                    replace: None,
                    dependencies: vec![],
                },
            ],
            unused_patches: vec![],
        };
        let text = lf.serialize();
        let back = Lockfile::parse(&text).unwrap();
        assert_eq!(back.packages.len(), 2);
        assert_eq!(back.packages[0].name, "a"); // sort determinism
        let mut sorted = lf.clone();
        sorted.packages.sort_by(|x, y| {
            x.name
                .cmp(&y.name)
                .then(x.version.cmp(&y.version))
                .then(x.source.cmp(&y.source))
        });
        assert_eq!(back, sorted);
        assert_eq!(lf.serialize(), text); // idempotent

        let mut v3 = lf.clone();
        v3.format_version = 3;
        let text = v3.serialize();
        assert!(text.contains("\nversion = 3\n"));
        assert_eq!(Lockfile::parse(&text).unwrap().format_version, 3);
    }

    #[test]
    fn parses_repo_own_lockfile() {
        // The repo's own Cargo.lock (v4, several hundred packages) must parse in full
        let lf = Lockfile::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("Cargo.lock")
                .as_path(),
        )
        .unwrap();
        assert!(lf.packages.len() > 100);
        assert!(lf.get("mirvm", &semver::Version::new(0, 0, 1)).is_some());
        assert!(
            lf.packages
                .iter()
                .all(|p| { p.source.as_ref().is_none_or(|s| s.starts_with("registry+")) })
        );
    }

    /// A `Cargo.lock` cargo wrote must survive mirvm's parse and serialize unchanged.
    ///
    /// This is the precondition for mirvm writing one into a project at all: the file it leaves
    /// behind has to be the file cargo would have left, byte for byte, or the two tools fight over
    /// the same path. The samples are real cargo output: a registry graph, workspaces, path
    /// packages, a git-source row, and `[[patch.unused]]` with a path, a git and a sparse-registry
    /// source.
    ///
    /// No sample carries a checksum inside `[[patch.unused]]`, and none can: cargo records a
    /// checksum only for packages that are in the resolve, while an unused patch is by definition
    /// absent from it. Probed both ways — patching crates-io with a package from a fixture registry,
    /// and then with that same package also present as a direct dependency, in which case cargo
    /// omits the unused entry altogether. The serializer keeps the field because cargo's schema has
    /// it.
    /// One line per sample so the list stays reviewable; rustfmt would expand every `include_str!`
    /// to four lines otherwise.
    #[rustfmt::skip]
    const CARGO_LOCK_SAMPLES: &[(&str, &str)] = &[
        ("c-unwind-contract", include_str!("../../tests/data/fixtures/c-unwind-contract/Cargo.lock")),
        ("cargoless/doctest-contract", include_str!("../../tests/data/fixtures/cargoless/doctest-contract/Cargo.lock")),
        ("cargoless/path-build-script", include_str!("../../tests/data/fixtures/cargoless/path-build-script/Cargo.lock")),
        ("cargoless/path-proc-macro", include_str!("../../tests/data/fixtures/cargoless/path-proc-macro/Cargo.lock")),
        ("cargoless/proc-macro-test-contract", include_str!("../../tests/data/fixtures/cargoless/proc-macro-test-contract/Cargo.lock")),
        ("cargoless/project", include_str!("../../tests/data/fixtures/cargoless/project/Cargo.lock")),
        ("cargoless/test-contract", include_str!("../../tests/data/fixtures/cargoless/test-contract/Cargo.lock")),
        ("cargoless/two-bin-workspace", include_str!("../../tests/data/fixtures/cargoless/two-bin-workspace/Cargo.lock")),
        ("cargoless/workspace-contract", include_str!("../../tests/data/fixtures/cargoless/workspace-contract/Cargo.lock")),
        ("cargoless/workspace-legacy-contract", include_str!("../../tests/data/fixtures/cargoless/workspace-legacy-contract/Cargo.lock")),
        ("lock-shapes/git-source", include_str!("../../tests/data/fixtures/lock-shapes/git-source/Cargo.lock")),
        ("lock-shapes/patch-unused-git", include_str!("../../tests/data/fixtures/lock-shapes/patch-unused-git/Cargo.lock")),
        ("lock-shapes/patch-unused-path", include_str!("../../tests/data/fixtures/lock-shapes/patch-unused-path/Cargo.lock")),
        ("lock-shapes/patch-unused-sparse-registry", include_str!("../../tests/data/fixtures/lock-shapes/patch-unused-sparse-registry/Cargo.lock")),
        ("tsan", include_str!("../../tests/data/fixtures/tsan/Cargo.lock")),
    ];

    #[test]
    fn cargo_written_locks_round_trip_byte_for_byte() {
        for &(name, text) in CARGO_LOCK_SAMPLES {
            let parsed = Lockfile::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(parsed.serialize(), text, "{name} does not round-trip");
        }
    }
}
