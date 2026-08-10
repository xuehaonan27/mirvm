//! `cargoless/vendor.rs` —— vendored 目录供给面（D15 P4 切⑥a，`PkgSource`
//! 的第二种生产实现，与 `Registry` 并列；P5 的 source replacement 复用本件）。
//!
//! 供给形态两种，都零网络、零写出界（ensure_source 直返目录本体）：
//! - `dirs`：`cargo vendor` 产物目录集——`<name>-<version>/` 平铺，manifest
//!   是 cargo 归一化形态（`[lib]` 显式、`build = false` 等，我们的 parser
//!   已支持）。rust-src 的 `library/vendor/` 就是这个形态，版本与
//!   `library/Cargo.lock` 逐一对齐。
//! - `overrides`：包名 → 目录的精确映射，服务「registry 名、本地身」的包
//!   （rust-src 的 `[patch.crates-io]`：rustc-std-workspace 三件套与
//!   windows-sys 指向 `library/` 下同名片目录）。override 目录的 manifest
//!   是**原始形态**（path 依赖未归一化）——path 边转成 req `*` 的 IndexDep：
//!   版本由 lock 钉死，req 只参与 edge_version 的范围匹配，`*` 恒配
//!   （cargo patch 语义：patched 包内部照原图解析，锁优先）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use semver::Version;

use super::manifest::{DepKind, DepSource, FeatureValue, PackageManifest};
use super::registry::{IndexDep, IndexEntry, IndexVersion};
use super::resolve::PkgSource;

pub struct VendorDir {
    dirs: Vec<PathBuf>,
    overrides: BTreeMap<String, PathBuf>,
    /// index_entry 合成缓存（resolve 对同一包多次重查——unify 注册 +
    /// node_featdeps 再取；manifest 解析不便宜，查一次记一次）。
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

    /// 一个目录的 manifest → 单版本 IndexVersion（版本读 manifest 自报）。
    fn entry_from_dir(dir: &Path, what: &str) -> Result<IndexVersion, String> {
        let m = PackageManifest::read_dir(dir).map_err(|e| {
            format!(
                "vendor 源 {what}（{}）manifest 解析失败: {e}",
                dir.display()
            )
        })?;
        let mut deps = Vec::new();
        for d in &m.deps {
            let req = match &d.source {
                DepSource::Registry(req) => req.clone(),
                DepSource::Git(spec) => spec.version.clone(),
                // override 目录的 path 依赖：见文件头注（lock 钉版，req 恒配）
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
            // vendor 源无 cksum 概念（ensure_source 不校验——内容即本地树）
            cksum: String::new(),
            yanked: false,
            deps,
            features,
            links: m.links.clone(),
            rust_version: m.rust_version.clone(),
        })
    }

    /// 扫 dirs 找 `<name>-<version>/` 目录（目录名剥 `{name}-` 前缀后按
    /// semver 解析；`r-efi` 撞 `r-efi-alloc-2.1.0` 这类前缀误配由解析
    /// 失败自然跳过）。版本集按 semver 排序（确定性）。
    fn scan_versions(&self, name: &str) -> Result<Vec<(Version, PathBuf)>, String> {
        let prefix = format!("{name}-");
        let mut out = Vec::new();
        for dir in &self.dirs {
            let rd = match std::fs::read_dir(dir) {
                Ok(rd) => rd,
                Err(e) => return Err(format!("vendor 目录读取失败 {}: {e}", dir.display())),
            };
            for ent in rd {
                let ent = ent.map_err(|e| format!("vendor 目录条目读取失败: {e}"))?;
                let file_name = ent.file_name();
                let Some(dir_name) = file_name.to_str() else {
                    continue;
                };
                let Some(ver_text) = dir_name.strip_prefix(&prefix) else {
                    continue;
                };
                let Ok(version) = Version::parse(ver_text) else {
                    continue; // 前缀撞名（name 是别人的前缀），不是本包版本
                };
                out.push((version, ent.path()));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

impl PkgSource for VendorDir {
    fn index_entry(&mut self, name: &str) -> Result<IndexEntry, String> {
        if let Some(hit) = self.cache.get(name) {
            return Ok(hit.clone());
        }
        let entries = if let Some(dir) = self.overrides.get(name) {
            // override 目录 = 该包的唯一版本（patch 语义：精确替换）
            vec![Self::entry_from_dir(dir, &format!("override 包 {name}"))?]
        } else {
            let mut entries = Vec::new();
            for (version, dir) in self.scan_versions(name)? {
                let iv = Self::entry_from_dir(&dir, &format!("包 {name}"))?;
                // 目录名是权威版本键（lock 按它钉）；manifest 自报应一致，
                // 不一致响亮——拿错版本比报错难查得多
                if iv.version != version {
                    return Err(format!(
                        "vendor 目录 {} 的 manifest 自报版本 {} 与目录名不符",
                        dir.display(),
                        iv.version
                    ));
                }
                entries.push(iv);
            }
            if entries.is_empty() {
                return Err(format!(
                    "vendor 源无 {name}（{:?} 与 overrides 均无）",
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
            "vendor 源无 {want} 目录（{:?}）——lock 钉的版本与 vendor 树脱节",
            self.dirs
        ))
    }
}

/// FeatureValue → index 文本形态（parse_feature_value 的逆映射，无损）。
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

    /// 在 dir 下物化一个 vendor 包目录（归一化形态 manifest）。
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
        // 前缀撞名目录：foo-bar 的包不应混进 foo 的版本集
        write_pkg(
            &dir_b,
            "foo-bar-9.9.9",
            "[package]\nname = \"foo-bar\"\nversion = \"9.9.9\"\n\
             [lib]\npath = \"src/lib.rs\"\n",
        );

        let mut src = VendorDir::new(vec![dir_a.clone(), dir_b.clone()], BTreeMap::new());
        let vs = src.index_entry("foo").unwrap();
        assert_eq!(
            vs.iter().map(|v| v.version.to_string()).collect::<Vec<_>>(),
            vec!["1.0.0", "1.2.3"]
        );
        let v = &vs[1];
        assert_eq!(v.name, "foo");
        assert_eq!(v.links.as_deref(), Some("foo-native"));
        assert_eq!(v.rust_version, Some(Version::new(1, 70, 0)));
        assert!(!v.yanked);
        // deps：normal/build/target 三类齐全，rename/default-features/optional 保真
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
        // features：三形态值反转回文本
        assert_eq!(v.features["default"], vec!["bar"]);
        assert_eq!(v.features["full"], vec!["dep:bar", "renamed/y", "btool?/z"]);

        // ensure_source：直返目录；缺版响亮
        let got = src
            .ensure_source("foo", &Version::parse("1.2.3").unwrap(), None)
            .unwrap();
        assert_eq!(got, dir_a.join("foo-1.2.3"));
        let miss = src.ensure_source("foo", &Version::parse("9.9.9").unwrap(), None);
        assert!(miss.is_err(), "缺版必须响亮: {miss:?}");
        let nope = src.index_entry("nonexistent");
        assert!(nope.is_err(), "缺包必须响亮: {nope:?}");

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
        let vs = src.index_entry("rustc-std-workspace-core").unwrap();
        assert_eq!(vs.len(), 1);
        assert_eq!(vs[0].version.to_string(), "1.99.0");
        // path 依赖 → req *（lock 钉版，范围匹配恒配）
        let core = vs[0].deps.iter().find(|d| d.name == "core").unwrap();
        assert_eq!(core.req, semver::VersionReq::STAR);
        let cb = vs[0]
            .deps
            .iter()
            .find(|d| d.name == "compiler_builtins")
            .unwrap();
        assert_eq!(cb.features, vec!["compiler-builtins"]);
        // ensure_source 直返 override 目录（不问版本——patch 语义唯一身）
        let got = src
            .ensure_source(
                "rustc-std-workspace-core",
                &Version::parse("1.99.0").unwrap(),
                None,
            )
            .unwrap();
        assert_eq!(got, ovl);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
