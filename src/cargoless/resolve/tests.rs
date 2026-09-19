// ---------- tests (offline; FakeSource canned index + tempdir sources) ----------

use super::super::lockfile::LockedDep;
use super::super::manifest::PackageManifest;
use super::super::registry::{IndexDep, IndexVersion};
use super::fresh::req_to_ranges;
use super::units::read_registry_minimal;
use super::*;
use semver::VersionReq;

struct FakeSource {
    root: PathBuf,
    index: BTreeMap<String, Vec<IndexVersion>>,
    git: BTreeMap<(String, String), PackageManifest>,
    /// package name -> explicit `[lib] name` (canned for the case where the extern name
    /// follows a lib name different from the package name, as with
    /// new_debug_unreachable).
    lib_names: BTreeMap<String, String>,
}

impl FakeSource {
    fn new(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        Self {
            root,
            index: BTreeMap::new(),
            git: BTreeMap::new(),
            lib_names: BTreeMap::new(),
        }
    }
    fn add(&mut self, name: &str, versions: Vec<IndexVersion>) {
        self.index.insert(name.to_string(), versions);
    }
    fn add_git(&mut self, source_id: &str, manifest: PackageManifest) {
        self.git
            .insert((source_id.to_string(), manifest.name.clone()), manifest);
    }
}

impl PkgSource for FakeSource {
    fn registry_source(&mut self, reference: &RegistryReference) -> Result<String, String> {
        Ok(match reference {
            RegistryReference::CratesIo => CRATES_IO_LOCK_SOURCE.to_string(),
            RegistryReference::Named(name) => format!("registry+test://{name}"),
            RegistryReference::Index(index) => {
                if index.starts_with("sparse+") {
                    index.clone()
                } else {
                    format!("registry+{index}")
                }
            }
        })
    }

    fn index_entry(&mut self, _source: &str, name: &str) -> Result<IndexEntry, String> {
        Ok(self.index.get(name).cloned().unwrap_or_default().into())
    }
    fn ensure_source(
        &mut self,
        _source: &str,
        name: &str,
        version: &Version,
        _cksum: Option<&str>,
    ) -> Result<PathBuf, String> {
        let dir = self.root.join(format!("{name}-{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        let lib_section = match self.lib_names.get(name) {
            Some(ln) => format!("\n[lib]\nname = \"{ln}\"\n"),
            None => String::new(),
        };
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n{lib_section}"),
        )
        .unwrap();
        Ok(dir)
    }

    fn ensure_git_package(
        &mut self,
        spec: &GitSpec,
        package: &str,
        locked_source: Option<&str>,
    ) -> Result<PackageManifest, String> {
        let mut manifest = self
            .git
            .get(&(spec.source_id(), package.to_string()))
            .cloned()
            .ok_or_else(|| format!("test Git package does not exist: {package}"))?;
        manifest.lock_source = Some(
            locked_source
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}#{}", spec.source_id(), "1".repeat(40))),
        );
        Ok(manifest)
    }
}

fn iv(name: &str, version: &str) -> IndexVersion {
    IndexVersion {
        name: name.to_string(),
        version: Version::parse(version).unwrap(),
        cksum: "0".repeat(64),
        yanked: false,
        deps: vec![],
        features: BTreeMap::new(),
        links: None,
        rust_version: None,
    }
}

fn idep(name: &str, req: &str) -> IndexDep {
    IndexDep {
        name: name.to_string(),
        req: VersionReq::parse(req).unwrap(),
        features: vec![],
        optional: false,
        default_features: true,
        target: None,
        kind: None,
        package: None,
        registry: None,
    }
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mirvm-cargoless-resolve-test-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn root_project(d: &Path, manifest: &str) -> PackageManifest {
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
    let manifest = manifest.replacen("[package]", "[package]\nedition=\"2021\"", 1);
    PackageManifest::parse(&manifest, d).unwrap()
}

#[test]
fn lock_mode_pins_versions_and_allows_yanked() {
    let d = tmpdir("lockmode");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\na = \"^1.0\"\nb = \"0.3\"\n",
    );
    std::fs::write(
            d.join("Cargo.lock"),
            "# x\nversion = 4\n\n\
             [[package]]\nname = \"demo\"\nversion = \"0.1.0\"\ndependencies = [\n \"a\",\n \"b\",\n]\n\n\
             [[package]]\nname = \"a\"\nversion = \"1.2.0\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
             [[package]]\nname = \"b\"\nversion = \"0.3.1\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
    let mut src = FakeSource::new(d.join("srcstore"));
    // a@1.2.0 is already yanked in the index -- lock mode accepts it anyway (as cargo does)
    let mut a12 = iv("a", "1.2.0");
    a12.yanked = true;
    a12.features.insert("default".into(), vec!["std".into()]);
    a12.features.insert("std".into(), vec![]);
    src.add("a", vec![iv("a", "1.0.0"), a12, iv("a", "1.5.0")]);
    src.add(
        "b",
        vec![iv("b", "0.3.0"), iv("b", "0.3.1"), iv("b", "0.4.0")],
    );

    let plan = resolve(&root, &mut src).unwrap();
    assert_eq!(
        plan.version_map["a"],
        vec![Version::parse("1.2.0").unwrap()]
    );
    assert_eq!(
        plan.version_map["b"],
        vec![Version::parse("0.3.1").unwrap()]
    );
    // Feature unification: a's default -> std gets enabled
    let a_unit = plan.units.iter().find(|u| u.package == "a").unwrap();
    assert!(a_unit.features.contains("default"));
    assert!(a_unit.features.contains("std"));
    assert!(!a_unit.from_registry || a_unit.source_dir.ends_with("a-1.2.0"));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn lock_mode_keeps_same_named_dependency_versions_distinct() {
    let d = tmpdir("lock-duplicate-dependency-name");
    let root = root_project(
        &d,
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\n[dependencies]\nparent=\"1\"\n",
    );
    std::fs::write(
            d.join("Cargo.lock"),
            "version=4\n\
             [[package]]\nname=\"demo\"\nversion=\"0.1.0\"\ndependencies=[\"parent\"]\n\
             [[package]]\nname=\"parent\"\nversion=\"1.0.0\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies=[\"h 0.2.1\",\"h 0.3.1\"]\n\
             [[package]]\nname=\"h\"\nversion=\"0.2.1\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\n\
             [[package]]\nname=\"h\"\nversion=\"0.3.1\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();

    let mut parent = iv("parent", "1.0.0");
    parent.deps.push(idep("h", "^0.2"));
    let mut renamed = idep("h_03", "^0.3");
    renamed.package = Some("h".into());
    parent.deps.push(renamed);
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add("parent", vec![parent]);
    src.add("h", vec![iv("h", "0.2.1"), iv("h", "0.3.1")]);

    let plan = resolve(&root, &mut src).unwrap();
    assert_eq!(
        plan.version_map["h"],
        vec![Version::new(0, 2, 1), Version::new(0, 3, 1)]
    );
    assert_eq!(
        plan.units.iter().filter(|unit| unit.package == "h").count(),
        2
    );
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn cli_dependency_feature_reaches_dep_without_becoming_root_cfg() {
    let d = tmpdir("cli-dependency-feature");
    let mut root = root_project(
        &d,
        "[package]\nname=\"demo\"\nversion=\"0.1.0\"\n[dependencies]\na=\"1\"\n",
    );
    root.dependency_features
        .entry("a".into())
        .or_default()
        .insert("extra".into());
    std::fs::write(
            d.join("Cargo.lock"),
            "version=4\n[[package]]\nname=\"demo\"\nversion=\"0.1.0\"\ndependencies=[\"a\"]\n\
             [[package]]\nname=\"a\"\nversion=\"1.0.0\"\nsource=\"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
    let mut a = iv("a", "1.0.0");
    a.features.insert("extra".into(), vec![]);
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add("a", vec![a]);
    let plan = resolve(&root, &mut src).unwrap();
    assert!(
        plan.units
            .iter()
            .find(|unit| unit.package == "a")
            .unwrap()
            .features
            .contains("extra")
    );
    assert!(!plan.root_features.contains("extra"));
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn run_locks_but_does_not_build_root_dev_dependencies() {
    let d = tmpdir("root-dev-purpose");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nnormal = \"1\"\n\
             [dev-dependencies]\ndevonly = \"1\"\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add("normal", vec![iv("normal", "1.0.0")]);
    src.add("devonly", vec![iv("devonly", "1.0.0")]);

    let run = resolve_for(&root, &mut src, ResolvePurpose::Run).unwrap();
    assert!(run.version_map.contains_key("devonly"));
    assert!(!run.units.iter().any(|unit| unit.package == "devonly"));
    assert!(!run.root_deps.iter().any(|dep| dep.kind == DepKind::Dev));

    let test = resolve_for(&root, &mut src, ResolvePurpose::Test).unwrap();
    let dev_ix = test
        .units
        .iter()
        .position(|unit| unit.package == "devonly")
        .unwrap();
    assert!(
        test.root_deps
            .iter()
            .any(|dep| dep.unit == dev_ix && dep.kind == DepKind::Dev)
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn resolver_three_prefers_compatible_versions_but_falls_back() {
    let directory = tmpdir("resolver-three-rust-version");
    let mut root = root_project(
        &directory,
        "[package]\nname='demo'\nversion='0.1.0'\nresolver='3'\nrust-version='1.70'\n\
             [dependencies]\na='1'\n",
    );
    let mut compatible = iv("a", "1.0.0");
    compatible.rust_version = Some(Version::new(1, 60, 0));
    let mut too_new = iv("a", "1.1.0");
    too_new.rust_version = Some(Version::new(1, 80, 0));
    let mut source = FakeSource::new(directory.join("srcstore"));
    source.add("a", vec![compatible, too_new]);

    let fallback = resolve(&root, &mut source).unwrap();
    assert_eq!(fallback.version_map["a"], vec![Version::new(1, 0, 0)]);
    assert_eq!(
        fallback.lock.format_version, 3,
        "Cargo keeps lock v3 for projects with rust-version 1.82 or earlier"
    );

    root.ignore_rust_version = true;
    let allow = resolve(&root, &mut source).unwrap();
    assert_eq!(allow.version_map["a"], vec![Version::new(1, 1, 0)]);

    root.ignore_rust_version = false;
    root.rust_version = Some(Version::new(1, 83, 0));
    root.resolver_rust_version = root.rust_version.clone();
    let modern_lock = resolve(&root, &mut source).unwrap();
    assert_eq!(modern_lock.lock.format_version, 4);

    root.rust_version = Some(Version::new(1, 50, 0));
    root.resolver_rust_version = Some(Version::new(1, 50, 0));
    let no_compatible = resolve(&root, &mut source).unwrap();
    assert_eq!(
        no_compatible.version_map["a"],
        vec![Version::new(1, 1, 0)],
        "with no compatible candidate, Cargo fallback still picks the usual highest version"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn compiler_rust_version_rejection_can_be_explicitly_ignored() {
    let directory = tmpdir("rust-version-diagnostic");
    let mut root = root_project(
        &directory,
        "[package]\nname='demo'\nversion='0.1.0'\nresolver='3'\n\
             [dependencies]\na='1'\n",
    );
    let mut future = iv("a", "1.0.0");
    future.rust_version = Some(Version::new(999, 0, 0));
    let mut source = FakeSource::new(directory.join("srcstore"));
    source.add("a", vec![future]);
    let error = resolve(&root, &mut source).unwrap_err();
    assert!(error.contains("a@1.0.0"), "{error}");
    assert!(error.contains("--ignore-rust-version"), "{error}");

    root.ignore_rust_version = true;
    assert!(resolve(&root, &mut source).is_ok());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn extern_key_prefers_rename_then_lib_name() {
    // cargo semantics (tendril 0.5.1 -> new_debug_unreachable 1.0.6):
    // the --extern name is the rename key when renamed, otherwise the dep package's
    // [lib] name.
    let d = tmpdir("externkey");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nndu = \"1.0\"\n\
             renamed = { version = \"1.0\", package = \"real-pkg\" }\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    src.lib_names
        .insert("ndu".into(), "debug_unreachable".into());
    src.add("ndu", vec![iv("ndu", "1.0.6")]);
    src.add("real-pkg", vec![iv("real-pkg", "1.0.0")]);

    let plan = resolve(&root, &mut src).unwrap();
    let ndu_ix = plan.units.iter().position(|u| u.package == "ndu").unwrap();
    let rp_ix = plan
        .units
        .iter()
        .position(|u| u.package == "real-pkg")
        .unwrap();
    let key_of = |ix: usize| {
        plan.root_deps
            .iter()
            .find(|e| e.unit == ix)
            .map(|e| e.key.clone())
            .unwrap()
    };
    // without a rename the extern name follows the lib name, not the package name
    assert_eq!(key_of(ndu_ix), "debug_unreachable");
    // with a rename the rename key wins
    assert_eq!(key_of(rp_ix), "renamed");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn lock_mode_rejects_stale_lock_loudly() {
    let d = tmpdir("stale");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\na = \"^2.0\"\n",
    );
    std::fs::write(
        d.join("Cargo.lock"),
        "version = 4\n\n\
             [[package]]\nname = \"demo\"\nversion = \"0.1.0\"\ndependencies = [\"a\"]\n\n\
             [[package]]\nname = \"a\"\nversion = \"1.0.0\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
    )
    .unwrap();
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add("a", vec![iv("a", "1.0.0"), iv("a", "2.0.0")]);
    let err = resolve(&root, &mut src).unwrap_err();
    assert!(err.contains("stale Cargo.lock"), "{err}");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn fresh_solve_diamond_skips_yanked_and_emits_lock() {
    let d = tmpdir("fresh");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nx = \"^1\"\ny = \"^1\"\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    let mut x = iv("x", "1.0.0");
    x.deps.push(idep("z", "^1.2"));
    let mut y = iv("y", "1.0.0");
    y.deps.push(idep("z", ">=1.1, <1.4"));
    src.add("x", vec![x]);
    src.add("y", vec![y]);
    let mut z14 = iv("z", "1.4.0");
    z14.yanked = true;
    src.add(
        "z",
        vec![
            iv("z", "1.0.0"),
            iv("z", "1.2.0"),
            iv("z", "1.3.5"),
            z14,
            iv("z", "1.5.0"),
        ],
    );
    let plan = resolve(&root, &mut src).unwrap();
    // max of the [1.2,1.4) range is 1.3.5 (1.4.0 is yanked and skipped; 1.5.0 does not satisfy <1.4)
    assert_eq!(
        plan.version_map["z"],
        vec![Version::parse("1.3.5").unwrap()]
    );
    let z_lock = plan
        .lock
        .get("z", &Version::parse("1.3.5").unwrap())
        .unwrap();
    assert!(z_lock.source.as_ref().unwrap().starts_with("registry+"));
    assert_eq!(z_lock.checksum.as_deref(), Some(&"0".repeat(64)[..]));
    // the lock can be read back by our own parser and carries dependency lines
    let text = plan.lock.serialize();
    let back = Lockfile::parse(&text).unwrap();
    assert!(back.get("z", &Version::parse("1.3.5").unwrap()).is_some());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn feature_unification_optional_forms_and_class_separation() {
    let d = tmpdir("features");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nm = \"1\"\ns = \"1\"\nw = \"1\"\n\
             [build-dependencies]\ns = { version = \"1\", features = [\"b\"] }\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    // m: default contains an explicit dep:opt activation; opt's of -> deep
    let mut m = iv("m", "1.0.0");
    m.features.insert("default".into(), vec!["dep:opt".into()]);
    m.features.insert("opt".into(), vec![]);
    let mut opt = idep("opt", "1");
    opt.optional = true;
    opt.features = vec!["of".into()];
    m.deps.push(opt);
    src.add("m", vec![m]);
    let mut opt = iv("opt", "1.0.0");
    opt.features.insert("of".into(), vec!["deep".into()]);
    opt.features.insert("deep".into(), vec![]);
    src.add("opt", vec![opt]);
    // s: the normal edge (default) and the build edge (features=["b"]) are separate
    let mut s = iv("s", "1.0.0");
    s.features.insert("default".into(), vec![]);
    s.features.insert("b".into(), vec![]);
    src.add("s", vec![s]);
    // w: weak activation opt2?/inner -- opt2 is never activated, so no opt2 unit
    let mut w = iv("w", "1.0.0");
    w.features
        .insert("default".into(), vec!["opt2?/inner".into()]);
    let mut opt2 = idep("opt2", "1");
    opt2.optional = true;
    w.deps.push(opt2);
    src.add("w", vec![w]);
    let mut opt2 = iv("opt2", "1.0.0");
    opt2.features.insert("inner".into(), vec![]);
    src.add("opt2", vec![opt2]);

    let plan = resolve(&root, &mut src).unwrap();
    let m_unit = plan.units.iter().find(|u| u.package == "m").unwrap();
    assert!(m_unit.features.contains("default"));
    // dep:opt activated opt, and opt received the edge features of -> of/deep expand
    let opt_unit = plan.units.iter().find(|u| u.package == "opt").unwrap();
    assert!(opt_unit.features.contains("of"));
    assert!(opt_unit.features.contains("deep"));
    // two s columns: Normal (default) and Build (b) with different feature sets = two units
    let s_units: Vec<_> = plan.units.iter().filter(|u| u.package == "s").collect();
    assert_eq!(s_units.len(), 2);
    let s_normal = s_units
        .iter()
        .find(|u| u.class == UnitClass::Normal)
        .unwrap();
    let s_build = s_units
        .iter()
        .find(|u| u.class == UnitClass::Build)
        .unwrap();
    assert!(s_normal.features.contains("default"));
    assert!(!s_normal.features.contains("b"));
    assert!(s_build.features.contains("b"));
    // the weak activation never fires: there is no opt2 unit
    assert!(!plan.units.iter().any(|u| u.package == "opt2"));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn implicit_optional_feature_also_sets_cfg_flag() {
    // serde facade shape: feature derive = ["se_derive"] (a bare-name reference to an
    // optional dependency, no dep: form) activates the dependency and **also sets the
    // feature flag of the same name** (as in cargo: serde's
    // `#[cfg(feature = "serde_derive")]`). By contrast the explicit dep: form only
    // activates the dependency and sets no flag of the same name.
    let d = tmpdir("implicitfeat");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nse = { version = \"1\", features = [\"derive\"] }\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    let mut se = iv("se", "1.0.0");
    se.features
        .insert("derive".into(), vec!["se_derive".into()]);
    let mut se_derive = idep("se_derive", "1");
    se_derive.optional = true;
    se.deps.push(se_derive);
    src.add("se", vec![se]);
    src.add("se_derive", vec![iv("se_derive", "1.0.0")]);

    let plan = resolve(&root, &mut src).unwrap();
    let se_unit = plan.units.iter().find(|u| u.package == "se").unwrap();
    assert!(se_unit.features.contains("derive"));
    assert!(
        se_unit.features.contains("se_derive"),
        "the implicit feature flag must enter the cfg set: {:?}",
        se_unit.features
    );
    assert!(plan.units.iter().any(|u| u.package == "se_derive"));
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn strong_dep_feature_also_sets_implicit_cfg_flag() {
    // k256 shape: feature ecdsa = ["ecdsa-core/signing"] (a strong reference to an
    // optional dependency) activates the dependency and propagates signing, while the
    // implicit feature flag of the same name, ecdsa-core, is set as well (as in cargo:
    // k256's `#[cfg(feature = "ecdsa-core")] pub mod ecdsa`; missing the flag gives
    // E0433 cannot find ecdsa in k256).
    let d = tmpdir("strongimplicit");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nk2 = { version = \"1\", features = [\"ecdsa\"] }\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    let mut k2 = iv("k2", "1.0.0");
    k2.features
        .insert("ecdsa".into(), vec!["ecdsa-core/signing".into()]);
    let mut ec = idep("ecdsa-core", "1");
    ec.optional = true;
    k2.deps.push(ec);
    src.add("k2", vec![k2]);
    let mut ecdsa_core = iv("ecdsa-core", "1.0.0");
    ecdsa_core.features.insert("signing".into(), vec![]);
    src.add("ecdsa-core", vec![ecdsa_core]);

    let plan = resolve(&root, &mut src).unwrap();
    let k2_unit = plan.units.iter().find(|u| u.package == "k2").unwrap();
    assert!(k2_unit.features.contains("ecdsa"));
    assert!(
        k2_unit.features.contains("ecdsa-core"),
        "the implicit feature flag of a strongly activated optional dependency must enter the cfg set: {:?}",
        k2_unit.features
    );
    let ec_unit = plan
        .units
        .iter()
        .find(|u| u.package == "ecdsa-core")
        .unwrap();
    assert!(
        ec_unit.features.contains("signing"),
        "the y of x/y propagates as usual"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn strong_dep_enables_same_named_explicit_feature_even_when_dep_hidden() {
    // zerotrie 0.2.4 shape: the optional dependency litemap is shadowed by dep: (so no
    // implicit feature is generated), but an explicit feature of the same name,
    // litemap = [dep:litemap, alloc], is present; and
    // serde = [dep:serde_core, dep:litemap, alloc, litemap/serde]. With
    // features=["serde"] the cfg set must contain litemap -- the strong litemap/serde
    // also enables the same-named **explicit** feature (dep: shadowing kills only the
    // implicit feature); missing the flag gives E0599 because
    // try_from_serde_litemap lives in a `#[cfg(feature = "litemap")]` impl block.
    let d = tmpdir("strongexplicit");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nzt = { version = \"1\", features = [\"serde\"] }\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    let mut zt = iv("zt", "1.0.0");
    zt.features.insert("alloc".into(), vec![]);
    zt.features
        .insert("litemap".into(), vec!["dep:litemap".into(), "alloc".into()]);
    zt.features.insert(
        "serde".into(),
        vec![
            "dep:serde_core".into(),
            "dep:litemap".into(),
            "alloc".into(),
            "litemap/serde".into(),
        ],
    );
    let mut lm = idep("litemap", "1");
    lm.optional = true;
    zt.deps.push(lm);
    let mut sc = idep("serde_core", "1");
    sc.optional = true;
    zt.deps.push(sc);
    src.add("zt", vec![zt]);
    let mut litemap = iv("litemap", "1.0.0");
    litemap.features.insert("serde".into(), vec![]);
    src.add("litemap", vec![litemap]);
    src.add("serde_core", vec![iv("serde_core", "1.0.0")]);

    let plan = resolve(&root, &mut src).unwrap();
    let zt_unit = plan.units.iter().find(|u| u.package == "zt").unwrap();
    assert!(zt_unit.features.contains("serde"));
    assert!(
        zt_unit.features.contains("litemap"),
        "a strong x/y must enable the same-named explicit feature (dep: shadowing kills only the implicit one): {:?}",
        zt_unit.features
    );
    assert!(
        zt_unit.features.contains("alloc"),
        "expanding the explicit litemap feature carries alloc: {:?}",
        zt_unit.features
    );
    assert!(
        plan.units.iter().any(|u| u.package == "litemap"),
        "the litemap dependency itself is activated into units"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn path_dep_joins_solve_and_units() {
    let d = tmpdir("pathdep");
    let sibling = d.join("sibling");
    std::fs::create_dir_all(sibling.join("src")).unwrap();
    std::fs::write(
        sibling.join("Cargo.toml"),
        "[package]\nname = \"sib\"\nversion = \"0.2.0\"\n\
             [features]\napp-side=[]\n[dependencies]\nr = \"1\"\n",
    )
    .unwrap();
    std::fs::write(sibling.join("src/lib.rs"), "").unwrap();
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\nsib = { path = \"sibling\" }\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add("r", vec![iv("r", "1.0.0")]);
    let plan = resolve(&root, &mut src).unwrap();
    let sib = plan.units.iter().find(|u| u.package == "sib").unwrap();
    assert!(!sib.from_registry);
    assert_eq!(sib.version.to_string(), "0.2.0");
    assert_eq!(
        plan.version_map["r"],
        vec![Version::parse("1.0.0").unwrap()]
    );
    // generated lock: sib has no source line
    let sib_lock = plan
        .lock
        .get("sib", &Version::parse("0.2.0").unwrap())
        .unwrap();
    assert!(sib_lock.source.is_none());
    let mut overrides = FeatureOverrides::new();
    overrides.insert(
        ("sib".to_string(), Version::new(0, 2, 0), UnitClass::Normal),
        BTreeSet::from(["app-side".to_string()]),
    );
    let with_workspace_features =
        resolve_for_known_with_features(&root, &mut src, ResolvePurpose::Run, &[], &overrides)
            .unwrap();
    assert!(
        with_workspace_features
            .units
            .iter()
            .find(|unit| unit.package == "sib")
            .unwrap()
            .features
            .contains("app-side"),
        "a workspace public package-name feature must map to the local node carrying that source"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn git_repo_path_packages_share_precise_source_and_lock() {
    let d = tmpdir("git-path");
    let repo = d.join("repo");
    let core = repo.join("core");
    let helper = repo.join("helper");
    for package in [&core, &helper] {
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::write(package.join("src/lib.rs"), "").unwrap();
    }
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname='git-core'\nversion='1.2.3'\nedition='2021'\n\
             [dependencies]\ngit-helper={path='../helper'}\n",
    )
    .unwrap();
    std::fs::write(
        helper.join("Cargo.toml"),
        "[package]\nname='git-helper'\nversion='0.4.0'\nedition='2021'\n",
    )
    .unwrap();
    let mut git_core = PackageManifest::parse(
        &std::fs::read_to_string(core.join("Cargo.toml")).unwrap(),
        &core,
    )
    .unwrap();
    git_core.git_checkout_root = Some(repo.clone());
    let root = root_project(
        &d,
        "[package]\nname='demo'\nversion='0.1.0'\n\
             [dependencies]\nchosen={package='git-core',git='https://example.invalid/repo',branch='main',version='^1'}\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add_git("git+https://example.invalid/repo?branch=main", git_core);
    let plan = resolve(&root, &mut src).unwrap();
    let precise = format!(
        "git+https://example.invalid/repo?branch=main#{}",
        "1".repeat(40)
    );
    for package in ["git-core", "git-helper"] {
        let unit = plan
            .units
            .iter()
            .find(|unit| unit.package == package)
            .unwrap();
        assert!(
            unit.from_registry,
            "a Git package is treated as an immutable dependency"
        );
        assert_eq!(unit.immutable_source_id.as_deref(), Some(precise.as_str()));
        assert_eq!(
            plan.lock
                .packages
                .iter()
                .find(|locked| locked.name == package)
                .and_then(|locked| locked.source.as_deref()),
            Some(precise.as_str())
        );
    }
    std::fs::write(d.join("Cargo.lock"), plan.lock.serialize()).unwrap();
    let locked_plan = resolve(&root, &mut src).unwrap();
    assert_eq!(locked_plan.lock, plan.lock);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn git_rust_version_comes_from_checkout_manifest() {
    let d = tmpdir("git-rust-version");
    let repo = d.join("repo");
    let core = repo.join("core");
    std::fs::create_dir_all(core.join("src")).unwrap();
    std::fs::write(core.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname='git-core'\nversion='1.2.3'\nedition='2021'\nrust-version='999.0'\n",
    )
    .unwrap();
    let mut git_core = PackageManifest::parse(
        &std::fs::read_to_string(core.join("Cargo.toml")).unwrap(),
        &core,
    )
    .unwrap();
    git_core.git_checkout_root = Some(repo);
    let root = root_project(
        &d,
        "[package]\nname='demo'\nversion='0.1.0'\n\
             [dependencies]\nchosen={package='git-core',git='https://example.invalid/repo',version='^1'}\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add_git("git+https://example.invalid/repo", git_core);
    let error = resolve(&root, &mut src).unwrap_err();
    assert!(
        error.contains("git-core@1.2.3 requires rustc 999.0"),
        "{error}"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn same_git_package_from_two_revisions_stays_distinct() {
    let d = tmpdir("git-two-revisions");
    let mut src = FakeSource::new(d.join("srcstore"));
    for revision in ["old", "new"] {
        let repo = d.join(format!("repo-{revision}"));
        let core = repo.join("core");
        let helper = repo.join("helper");
        for package in [&core, &helper] {
            std::fs::create_dir_all(package.join("src")).unwrap();
            std::fs::write(package.join("src/lib.rs"), "").unwrap();
        }
        std::fs::write(
            core.join("Cargo.toml"),
            "[package]\nname='git-core'\nversion='1.2.3'\nedition='2021'\n\
                 [dependencies]\ngit-helper={path='../helper'}\n",
        )
        .unwrap();
        std::fs::write(
            helper.join("Cargo.toml"),
            "[package]\nname='git-helper'\nversion='0.4.0'\nedition='2021'\n",
        )
        .unwrap();
        let mut manifest = PackageManifest::parse(
            &std::fs::read_to_string(core.join("Cargo.toml")).unwrap(),
            &core,
        )
        .unwrap();
        manifest.git_checkout_root = Some(repo);
        src.add_git(
            &format!("git+https://example.invalid/repo?rev={revision}"),
            manifest,
        );
    }
    let root = root_project(
        &d,
        "[package]\nname='demo'\nversion='0.1.0'\n\
             [dependencies]\n\
             old={package='git-core',git='https://example.invalid/repo',rev='old',version='^1'}\n\
             new={package='git-core',git='https://example.invalid/repo',rev='new',version='^1'}\n",
    );
    let plan = resolve(&root, &mut src).unwrap();
    assert_eq!(
        plan.units
            .iter()
            .filter(|unit| unit.package == "git-core")
            .count(),
        2
    );
    assert_eq!(
        plan.units
            .iter()
            .filter(|unit| unit.package == "git-helper")
            .count(),
        2
    );
    let root_sources: BTreeSet<String> = plan
        .root_deps
        .iter()
        .map(|dependency| {
            plan.units[dependency.unit]
                .immutable_source_id
                .clone()
                .unwrap()
        })
        .collect();
    assert_eq!(root_sources.len(), 2);
    let root_lock = plan
        .lock
        .packages
        .iter()
        .find(|package| package.name == "demo")
        .unwrap();
    assert_eq!(root_lock.dependencies.len(), 2);
    assert!(
        root_lock
            .dependencies
            .iter()
            .all(
                |dependency| dependency.version == Some(Version::new(1, 2, 3))
                    && dependency.source.is_some()
            )
    );
    let serialized = plan.lock.serialize();
    let parsed = Lockfile::parse(&serialized).unwrap();
    assert_eq!(parsed, plan.lock);
    std::fs::write(d.join("Cargo.lock"), serialized).unwrap();
    let locked = resolve(&root, &mut src).unwrap();
    assert_eq!(locked.root_deps.len(), 2);
    assert_ne!(locked.root_deps[0].unit, locked.root_deps[1].unit);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn fresh_solve_downgrades_parent_to_avoid_compatible_duplicate() {
    // Cargo prefers unifying one name: wide@1.1 is newer but pins core@1.0.1, while the
    // graph already pins core@1.0.0 and wide@1.0 also satisfies ^1, so it must backtrack
    // rather than keep two copies of core just to take the newest patch version.
    let d = tmpdir("compatible-duplicate");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\
             [dependencies]\ncontainer = \"1\"\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    let mut container = iv("container", "1.0.0");
    container.deps.push(idep("pinned", "1"));
    container.deps.push(idep("wide", "^1.0"));
    src.add("container", vec![container]);

    let mut pinned = iv("pinned", "1.0.0");
    pinned.deps.push(idep("core", "=1.0.0"));
    src.add("pinned", vec![pinned]);

    let mut wide_old = iv("wide", "1.0.0");
    wide_old.deps.push(idep("core", "=1.0.0"));
    let mut wide_new = iv("wide", "1.1.0");
    wide_new.deps.push(idep("core", "=1.0.1"));
    src.add("wide", vec![wide_old, wide_new]);
    src.add("core", vec![iv("core", "1.0.0"), iv("core", "1.0.1")]);

    let plan = resolve(&root, &mut src).unwrap();
    assert_eq!(plan.version_map["wide"], vec![Version::new(1, 0, 0)]);
    assert_eq!(plan.version_map["core"], vec![Version::new(1, 0, 0)]);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn multi_version_fork_coexists_with_per_bucket_features() {
    // cargo multi-version semantics: a->h ^0.14 and b->h ^0.15 share no candidate, so
    // both versions coexist (hashbrown 0.14/0.15); each feature table expands
    // independently per bucket
    let d = tmpdir("fork");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\na = \"1\"\nb = \"1\"\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    let mut a = iv("a", "1.0.0");
    a.deps.push(idep("h", "^0.14"));
    let mut b = iv("b", "1.0.0");
    b.deps.push(idep("h", "^0.15"));
    src.add("a", vec![a]);
    src.add("b", vec![b]);
    let mut h14 = iv("h", "0.14.5");
    h14.features.insert("default".into(), vec!["x".into()]);
    h14.features.insert("x".into(), vec![]);
    let mut h15 = iv("h", "0.15.2");
    h15.features.insert("default".into(), vec!["y".into()]);
    h15.features.insert("y".into(), vec![]);
    src.add("h", vec![h14, h15]);

    let plan = resolve(&root, &mut src).unwrap();
    assert_eq!(
        plan.version_map["h"],
        vec![
            Version::parse("0.14.5").unwrap(),
            Version::parse("0.15.2").unwrap()
        ]
    );
    let h_units: Vec<_> = plan.units.iter().filter(|u| u.package == "h").collect();
    assert_eq!(h_units.len(), 2);
    let u14 = h_units
        .iter()
        .find(|u| u.version.to_string() == "0.14.5")
        .unwrap();
    let u15 = h_units
        .iter()
        .find(|u| u.version.to_string() == "0.15.2")
        .unwrap();
    assert!(u14.features.contains("x") && !u14.features.contains("y"));
    assert!(u15.features.contains("y") && !u15.features.contains("x"));
    // the lock holds both versions; the a/b dependency lines carry a disambiguation hint
    assert!(
        plan.lock
            .get("h", &Version::parse("0.14.5").unwrap())
            .is_some()
    );
    assert!(
        plan.lock
            .get("h", &Version::parse("0.15.2").unwrap())
            .is_some()
    );
    let a_line = &plan
        .lock
        .get("a", &Version::parse("1.0.0").unwrap())
        .unwrap()
        .dependencies;
    let b_line = &plan
        .lock
        .get("b", &Version::parse("1.0.0").unwrap())
        .unwrap()
        .dependencies;
    assert!(
        a_line.contains(&LockedDep {
            name: "h".to_string(),
            version: Some(Version::parse("0.14.5").unwrap()),
            source: None,
        }),
        "a line: {a_line:?}"
    );
    assert!(
        b_line.contains(&LockedDep {
            name: "h".to_string(),
            version: Some(Version::parse("0.15.2").unwrap()),
            source: None,
        }),
        "b line: {b_line:?}"
    );
    // the lock can be read back by our own parser
    let back = Lockfile::parse(&plan.lock.serialize()).unwrap();
    assert_eq!(back.packages.len(), plan.lock.packages.len());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn prerelease_req_allows_pre_candidates() {
    // A req carrying a pre comparator admits that package's prereleases (cargo's
    // approximate rule; e.g. argon2 = "0.6.0-rc.8", where the rc family used to be
    // rejected wholesale by the pre filter)
    let d = tmpdir("pre");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\nargon2 = \"0.6.0-rc.8\"\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    src.add(
        "argon2",
        vec![
            iv("argon2", "0.5.3"),
            iv("argon2", "0.6.0-rc.7"),
            iv("argon2", "0.6.0-rc.8"),
        ],
    );
    let plan = resolve(&root, &mut src).unwrap();
    assert_eq!(plan.version_map["argon2"][0].to_string(), "0.6.0-rc.8");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn syn_features2_shape_activates_optional_quote_into_lock_lines() {
    // Reproduces the real index shape of syn 3.0.3 (dep:quote inside features2) plus
    // the serde_derive dep shape: quote must appear on syn's lock dependency line
    let d = tmpdir("synshape");
    let root = root_project(
        &d,
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n[dependencies]\nsd = \"1\"\n",
    );
    let mut src = FakeSource::new(d.join("srcstore"));
    let mut sd = iv("sd", "1.0.0");
    let mut sd_syn = idep("syn", "^3");
    sd_syn.default_features = false;
    sd_syn.features = vec![
        "clone-impls".into(),
        "derive".into(),
        "parsing".into(),
        "printing".into(),
        "proc-macro".into(),
    ];
    sd.deps.push(sd_syn);
    sd.deps.push(idep("quote", "^1"));
    src.add("sd", vec![sd]);
    let mut syn = iv("syn", "3.0.3");
    syn.features.insert("derive".into(), vec![]);
    syn.features.insert("parsing".into(), vec![]);
    syn.features.insert("clone-impls".into(), vec![]);
    syn.features.insert(
        "default".into(),
        vec![
            "derive".into(),
            "parsing".into(),
            "printing".into(),
            "clone-impls".into(),
            "proc-macro".into(),
        ],
    );
    syn.features
        .insert("printing".into(), vec!["dep:quote".into()]);
    syn.features.insert(
        "proc-macro".into(),
        vec!["proc-macro2/proc-macro".into(), "quote?/proc-macro".into()],
    );
    let mut syn_quote = idep("quote", "^1.0.35");
    syn_quote.optional = true;
    syn_quote.default_features = false;
    syn.deps.push(syn_quote);
    syn.deps.push(idep("proc-macro2", "^1"));
    src.add("syn", vec![syn]);
    let mut quote = iv("quote", "1.0.47");
    quote.features.insert("proc-macro".into(), vec![]);
    src.add("quote", vec![quote]);
    let mut proc_macro2 = iv("proc-macro2", "1.0.107");
    proc_macro2.features.insert("proc-macro".into(), vec![]);
    src.add("proc-macro2", vec![proc_macro2]);

    let plan = resolve(&root, &mut src).unwrap();
    let syn_lock = plan
        .lock
        .get("syn", &Version::parse("3.0.3").unwrap())
        .unwrap();
    let line: Vec<String> = syn_lock
        .dependencies
        .iter()
        .map(|dependency| dependency.name.clone())
        .collect();
    assert!(
        line.contains(&"quote".to_string()),
        "syn dependency line is missing quote: {line:?}"
    );
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn req_to_ranges_less_bound_is_exact() {
    // ">=2.0.4, <3" = [2.0.4, 3.0.0) -- the Less arm used to be wrong and give <4.0.0
    let req = VersionReq::parse(">=2.0.4, <3").unwrap();
    let r = req_to_ranges(&req);
    assert!(r.contains(&Version::parse("2.0.4").unwrap()));
    assert!(r.contains(&Version::parse("2.9.9").unwrap()));
    assert!(!r.contains(&Version::parse("3.0.0").unwrap()));
    let req2 = VersionReq::parse("<3.2").unwrap();
    let r2 = req_to_ranges(&req2);
    assert!(r2.contains(&Version::parse("3.1.9").unwrap()));
    assert!(!r2.contains(&Version::parse("3.2.0").unwrap()));
}

#[test]
fn exact_pin_matches_build_metadata_variants() {
    // "=0.18.5" must match 0.18.5+1.9.4 (any build metadata; the semver crate's Ord
    // compares build metadata, so a singleton set would wrongly reject it)
    let req = VersionReq::parse("=0.18.5").unwrap();
    let r = req_to_ranges(&req);
    assert!(r.contains(&Version::parse("0.18.5").unwrap()));
    assert!(r.contains(&Version::parse("0.18.5+1.9.4").unwrap()));
    assert!(!r.contains(&Version::parse("0.18.6").unwrap()));
}

#[test]
fn registry_minimal_accepts_both_proc_macro_spellings() {
    // Both spellings coexist in crates.io normalized output: the hyphen (serde_derive
    // 1.0.228, older normalization) and the underscore (derive_arbitrary 1.3.2, newer
    // normalization). Cargo accepts both; missing one compiles a proc-macro crate as a
    // target dependency.
    let d = tmpdir("proc-macro-spelling");
    for (key, want) in [("proc-macro", true), ("proc_macro", true)] {
        let dir = d.join(key);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"pm\"\nversion = \"1.0.0\"\n[lib]\n{key} = true\n"),
        )
        .unwrap();
        let rm = read_registry_minimal(&dir, "pm", &Version::parse("1.0.0").unwrap()).unwrap();
        assert_eq!(rm.proc_macro, want, "spelling {key} must be recognized");
    }
    // absent = false (a plain lib)
    let dir = d.join("absent");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"pm\"\nversion = \"1.0.0\"\n[lib]\n",
    )
    .unwrap();
    let rm = read_registry_minimal(&dir, "pm", &Version::parse("1.0.0").unwrap()).unwrap();
    assert!(!rm.proc_macro);
    std::fs::remove_dir_all(&d).unwrap();
}
