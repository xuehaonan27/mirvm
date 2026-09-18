//! `cargoless/vendor.rs` — vendored directory supply surface (D15 P4 cut⑥a, the second
//! production implementation of `PkgSource`, alongside `Registry`; reused by P5 source
//! replacement).
//!
//! Two supply shapes, both zero-network and zero-write-outside (ensure_source returns
//! the directory itself):
//! - `dirs`: `cargo vendor` output directories — `<name>-<version>/` laid flat, manifest
//!   is cargo-normalized form (`[lib]` explicit, `build = false`, etc.; our parser
//!   already supports). rust-src's `library/vendor/` is this shape, versions aligned
//!   one-to-one with `library/Cargo.lock`.
//! - `overrides`: package name -> directory precise mapping, serving packages that are
//!   "registry by name, local by body" (rust-src's `[patch.crates-io]`:
//!   rustc-std-workspace trio and windows-sys point to same-name subdirectories under
//!   `library/`). Override-directory manifests are in **raw form** (path deps not
//!   normalized) — path edges become IndexDep req `*`: the version is pinned by the
//!   lock, req only participates in edge_version range matching, `*` always matches
//!   (cargo patch semantics: patched packages resolve internally by their original
//!   graph, lock wins).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::Version;

use super::manifest::{DepKind, DepSource, FeatureValue, PackageManifest, RegistryReference};
use super::registry::{IndexDep, IndexEntry, IndexVersion};
use super::resolve::PkgSource;

pub struct VendorDir {
    dirs: Vec<PathBuf>,
    overrides: BTreeMap<String, PathBuf>,
    /// index_entry synthesis cache (resolve re-queries the same package many times —
    /// unify registration plus node_featdeps re-fetch; manifest parsing is not cheap,
    /// so remember each result).
    cache: BTreeMap<String, IndexEntry>,
}

impl VendorDir {
    pub fn new(dirs: Vec<PathBuf>, overrides: BTreeMap<String, PathBuf>) -> Self {
        Self {
            dirs,
            overrides,
            cache: BTreeMap::new(),
        }
    }

    /// One directory's manifest -> single-version IndexVersion (version read from manifest self-report).
    pub(crate) fn entry_from_dir(dir: &Path, what: &str) -> Result<IndexVersion, String> {
        let m = PackageManifest::read_dir(dir).map_err(|e| {
            format!(
                "vendor source {what} ({}) manifest parse failed: {e}",
                dir.display()
            )
        })?;
        Self::entry_from_manifest(&m)
    }

    pub(crate) fn entry_from_manifest(m: &PackageManifest) -> Result<IndexVersion, String> {
        let mut deps = Vec::new();
        for d in &m.deps {
            let req = match &d.source {
                DepSource::Registry(req, _) => req.clone(),
                DepSource::Git(spec) => spec.version.clone(),
                // override directory's path dep: see file-header note (lock pins version, req always matches)
                DepSource::Path(_) => semver::VersionReq::STAR,
            };
            deps.push(IndexDep {
                name: d.key.clone(),
                req,
                features: d.features.clone(),
                optional: d.optional,
                default_features: d.default_features,
                target: d.platform_cfg.clone(),
                kind: match d.kind {
                    DepKind::Normal => None,
                    DepKind::Build => Some("build".to_string()),
                    DepKind::Dev => Some("dev".to_string()),
                },
                package: (d.package != d.key).then(|| d.package.clone()),
                registry: match &d.source {
                    DepSource::Registry(_, RegistryReference::Index(index)) => Some(index.clone()),
                    _ => None,
                },
            });
        }
        let features = m
            .features
            .iter()
            .map(|(k, vs)| {
                (
                    k.clone(),
                    vs.iter().map(feature_value_text).collect::<Vec<_>>(),
                )
            })
            .collect();
        Ok(IndexVersion {
            name: m.name.clone(),
            version: m.version.clone(),
            // vendor source has no cksum concept (ensure_source does not verify — content is the local tree)
            cksum: String::new(),
            yanked: false,
            deps,
            features,
            links: m.links.clone(),
            rust_version: m.rust_version.clone(),
        })
    }

    /// Scan dirs for `<name>-<version>/` directories (strip the `{name}-` prefix and parse
    /// the rest as semver; prefix collisions like `r-efi` vs `r-efi-alloc-2.1.0` naturally
    /// fall through as parse failures). Versions are sorted by semver (deterministic).
    fn scan_versions(&self, name: &str) -> Result<Vec<(Version, PathBuf)>, String> {
        let prefix = format!("{name}-");
        let mut out = Vec::new();
        for dir in &self.dirs {
            let rd = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(e) => return Err(format!("vendor directory read failed {}: {e}", dir.display())),
            };
            for ent in rd {
                let ent = ent.map_err(|e| format!("vendor directory entry read failed: {e}"))?;
                let file_name = ent.file_name();
                let Some(dir_name) = file_name.to_str() else {
                    continue;
                };
                let Some(ver_text) = dir_name.strip_prefix(&prefix) else {
                    continue;
                };
                let Ok(version) = Version::parse(ver_text) else {
                    continue; // prefix collision (name is someone else's prefix), not a version of this package
                };
                out.push((version, ent.path()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

impl PkgSource for VendorDir {
    fn registry_source(&mut self, _reference: &RegistryReference) -> Result<String, String> {
        Ok("registry+vendor".to_string())
    }

    fn index_entry(&mut self, _source: &str, name: &str) -> Result<IndexEntry, String> {
        if let Some(hit) = self.cache.get(name) {
            return Ok(hit.clone());
        }
        let entries = if let Some(dir) = self.overrides.get(name) {
            // override directory = the single version of this package (patch semantics: exact replacement)
            vec![Self::entry_from_dir(dir, &format!("override package {name}"))?]
        } else {
            let mut entries = Vec::new();
            for (version, dir) in self.scan_versions(name)? {
                let iv = Self::entry_from_dir(&dir, &format!("package {name}"))?;
                // Directory name is the authoritative version key (lock pins by it); manifest self-report
                // should match, and if it does not we fail loudly — wrong version is much harder to debug
                // than an error.
                if iv.version != version {
                    return Err(format!(
                        "vendor directory {} manifest self-report version {} does not match directory name",
                        dir.display(),
                        iv.version
                    ));
                }
                entries.push(iv);
            }
            if entries.is_empty() {
                return Err(format!(
                    "vendor source has no {name} (neither {:?} nor overrides)",
                    self.dirs
                ));
            }
            entries
        };
        let entries: IndexEntry = entries.into();
        self.cache.insert(name.to_string(), entries.clone());
        Ok(entries)
    }

    fn ensure_source(
        &mut self,
        _source: &str,
        name: &str,
        version: &Version,
        _cksum: Option<&str>,
    ) -> Result<PathBuf, String> {
        if let Some(dir) = self.overrides.get(name) {
            return Ok(dir.clone());
        }
        let want = format!("{name}-{version}");
        for dir in &self.dirs {
            let hit = dir.join(&want);
            if hit.is_dir() {
                return Ok(hit);
            }
        }
        Err(format!(
            "vendor source has no {want} directory ({:?}) — lock-pinned version is out of sync with vendor tree",
            self.dirs
        ))
    }
}

/// FeatureValue -> index text form (inverse of parse_feature_value, lossless).
fn feature_value_text(v: &FeatureValue) -> String {
    match v {
        FeatureValue::Simple(s) => s.clone(),
        FeatureValue::DepActivation(d) => format!("dep:{d}"),
        FeatureValue::StrongDep { dep, feature } => format!("{dep}/{feature}"),
        FeatureValue::WeakDep { dep, feature } => format!("{dep}?/{feature}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("mirvm-vendor-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Materialize one vendor package directory under dir (normalized-form manifest).
    fn write_pkg(dir: &Path, dir_name: &str, manifest: &str) -> PathBuf {
        let pkg = dir.join(dir_name);
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(pkg.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(pkg.join("src/lib.rs"), "").unwrap();
        pkg
    }

    #[test]
    fn index_entry_reads_versions_deps_features_targets() {
        let tmp = tmpdir("entry");
        let dir_a = tmp.join("a");
        let dir_b = tmp.join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        write_pkg(
            &dir_a,
            "foo-1.2.3",
            "[package]\nname = \"foo\"\nversion = \"1.2.3\"\nedition = \"2021\"\n\
             rust-version = \"1.70\"\nlinks = \"foo-native\"\n\
             [lib]\nname = \"foo\"\npath = \"src/lib.rs\"\n\
             [dependencies]\n\
             bar = { version = \"0.4\", features = [\"x\"], optional = true, default-features = false }\n\
             renamed = { version = \"2.0\", package = \"real-pkg\" }\n\
             [build-dependencies]\n\
             btool = \"1.0\"\n\
             [target.'cfg(windows)'.dependencies]\n\
             winonly = \"3.0\"\n\
             [features]\n\
             default = [\"bar\"]\n\
             full = [\"dep:bar\", \"renamed/y\", \"btool?/z\"]\n",
        );
        write_pkg(
            &dir_b,
            "foo-1.0.0",
            "[package]\nname = \"foo\"\nversion = \"1.0.0\"\n\
             [lib]\npath = \"src/lib.rs\"\n",
        );
        // Prefix-collision directory: foo-bar should not pollute foo's version set.
        write_pkg(
            &dir_b,
            "foo-bar-9.9.9",
            "[package]\nname = \"foo-bar\"\nversion = \"9.9.9\"\n\
             [lib]\npath = \"src/lib.rs\"\n",
        );

        let mut src = VendorDir::new(vec![dir_a.clone(), dir_b.clone()], BTreeMap::new());
        let vs = src.index_entry("registry+vendor", "foo").unwrap();
        assert_eq!(
            vs.iter().map(|v| v.version.to_string()).collect::<Vec<_>>(),
            vec!["1.0.0", "1.2.3"]
        );
        let v = &vs[1];
        assert_eq!(v.name, "foo");
        assert_eq!(v.links.as_deref(), Some("foo-native"));
        assert_eq!(v.rust_version, Some(Version::new(1, 70, 0)));
        assert!(!v.yanked);
        // deps: normal/build/target all present; rename/default-features/optional preserved
        let bar = v.deps.iter().find(|d| d.name == "bar").unwrap();
        assert_eq!(bar.req.to_string(), "^0.4");
        assert_eq!(bar.features, vec!["x"]);
        assert!(bar.optional);
        assert!(!bar.default_features);
        assert_eq!(bar.target, None);
        assert_eq!(bar.kind, None);
        assert_eq!(bar.package, None);
        let ren = v.deps.iter().find(|d| d.name == "renamed").unwrap();
        assert_eq!(ren.package.as_deref(), Some("real-pkg"));
        let btool = v.deps.iter().find(|d| d.name == "btool").unwrap();
        assert_eq!(btool.kind.as_deref(), Some("build"));
        let win = v.deps.iter().find(|d| d.name == "winonly").unwrap();
        assert_eq!(win.target.as_deref(), Some("cfg(windows)"));
        // features: three forms round-trip back to text
        assert_eq!(v.features["default"], vec!["bar"]);
        assert_eq!(v.features["full"], vec!["dep:bar", "renamed/y", "btool?/z"]);

        // ensure_source: returns directory directly; missing version fails loudly
        let got = src
            .ensure_source(
                "registry+vendor",
                "foo",
                &Version::parse("1.2.3").unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(got, dir_a.join("foo-1.2.3"));
        let miss = src.ensure_source(
            "registry+vendor",
            "foo",
            &Version::parse("9.9.9").unwrap(),
            None,
        );
        assert!(miss.is_err(), "missing version must fail loudly: {miss:?}");
        let nope = src.index_entry("registry+vendor", "nonexistent");
        assert!(nope.is_err(), "missing package must fail loudly: {nope:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn override_maps_name_to_dir_with_star_req_for_path_deps() {
        let tmp = tmpdir("override");
        let vend = tmp.join("vendor");
        std::fs::create_dir_all(&vend).unwrap();
        let ovl = write_pkg(
            &tmp,
            "wscore",
            "[package]\nname = \"rustc-std-workspace-core\"\nversion = \"1.99.0\"\n\
             edition = \"2024\"\n\
             [lib]\npath = \"src/lib.rs\"\n\
             [dependencies]\n\
             core = { path = \"../core\" }\n\
             compiler_builtins = { path = \"../cb\", features = [\"compiler-builtins\"] }\n",
        );
        let mut overrides = BTreeMap::new();
        overrides.insert("rustc-std-workspace-core".to_string(), ovl.clone());
        let mut src = VendorDir::new(vec![vend], overrides);
        let vs = src
            .index_entry("registry+vendor", "rustc-std-workspace-core")
            .unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].version.to_string(), "1.99.0");
        // path dep -> req * (lock pins version, range match always succeeds)
        let core = vs[0].deps.iter().find(|d| d.name == "core").unwrap();
        assert_eq!(core.req, semver::VersionReq::STAR);
        let cb = vs[0]
            .deps
            .iter()
            .find(|d| d.name == "compiler_builtins")
            .unwrap();
        assert_eq!(cb.features, vec!["compiler-builtins"]);
        // ensure_source returns override directory directly (regardless of version — patch semantics, single body)
        let got = src
            .ensure_source(
                "registry+vendor",
                "rustc-std-workspace-core",
                &Version::parse("1.99.0").unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(got, ovl);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
