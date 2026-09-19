use super::*;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "mirvm-cargoless-manifest-test-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn parses_string_and_table_deps_with_rename_and_optional() {
    let m = PackageManifest::parse(
        r#"
[package]
name = "demo"
version = "1.2.3"
edition = "2021"

[dependencies]
itertools = "0.14"
renamed = { package = "real-crate", version = "^2.0", optional = true }
no_default = { version = "1", default-features = false, features = ["a", "b"] }
local = { path = "../sibling" }

[build-dependencies]
cc = "1"
"#,
        Path::new("/tmp/x"),
    )
    .unwrap();
    assert_eq!(m.name, "demo");
    assert_eq!(m.version.to_string(), "1.2.3");
    assert_eq!(m.edition, "2021");
    let get = |k: &str| m.deps.iter().find(|d| d.key == k).unwrap();
    assert_eq!(get("itertools").package, "itertools");
    assert_eq!(
        get("itertools").source,
        DepSource::Registry(
            semver::VersionReq::parse("0.14").unwrap(),
            RegistryReference::CratesIo,
        )
    );
    assert_eq!(get("renamed").package, "real-crate");
    assert!(get("renamed").optional);
    assert!(!get("no_default").default_features);
    assert_eq!(get("no_default").features, ["a", "b"]);
    assert_eq!(
        get("local").source,
        DepSource::Path(Path::new("/tmp/x").join("../sibling"))
    );
    assert_eq!(get("cc").kind, DepKind::Build);
}

#[test]
fn parses_git_dependencies_and_cargo_source_ids() {
    let manifest = PackageManifest::parse(
            "[package]\nname='d'\nversion='0.1.0'\n[dependencies]\n\
             default = { package='a', git='https://example.test/repo', version='^1' }\n\
             branch = { package='b', git='ssh://example.test/repo', branch='topic/one', features=['x'] }\n\
             tag = { package='c', git='file:///tmp/repo', tag='release 1' }\n\
             revision = { package='e', git='https://example.test/e', rev='abc123' }\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
    let source = |key: &str| {
        let DepSource::Git(spec) = &manifest
            .deps
            .iter()
            .find(|dep| dep.key == key)
            .unwrap()
            .source
        else {
            panic!("{key} should be a Git dependency")
        };
        spec.clone()
    };
    let default = source("default");
    assert_eq!(default.source_id(), "git+https://example.test/repo");
    assert_eq!(default.version.to_string(), "^1");
    assert_eq!(
        source("branch").source_id(),
        "git+ssh://example.test/repo?branch=topic%2Fone"
    );
    assert_eq!(
        source("tag").source_id(),
        "git+file:///tmp/repo?tag=release%201"
    );
    assert_eq!(
        source("revision").source_id(),
        "git+https://example.test/e?rev=abc123"
    );
}

#[test]
fn rejects_invalid_git_and_workspace_inherited_deps_loudly() {
    for (dependency, needle) in [
        (
            "foo = { git='https://x', branch='main', tag='v1' }",
            "only one of branch/tag/rev",
        ),
        ("foo = { branch='main' }", "no git URL"),
        (
            "foo = { git='https://x', path='../x' }",
            "both git and path",
        ),
        ("foo = { git='https://x', rev='' }", "must not be empty"),
        ("foo = { git='--upload-pack=bad' }", "invalid git URL"),
        ("foo = { git='https://x?a=b' }", "query/fragment"),
    ] {
        let err = PackageManifest::parse(
            &format!("[package]\nname='d'\nversion='0.1.0'\n[dependencies]\n{dependency}\n"),
            Path::new("/tmp/x"),
        )
        .unwrap_err();
        assert!(err.contains(needle), "{dependency}: {err}");
    }
    let err = PackageManifest::parse(
        "[package]\nname=\"d\"\nversion=\"0.1.0\"\n[dependencies]\nfoo.workspace = true",
        Path::new("/tmp/x"),
    )
    .unwrap_err();
    assert!(err.contains("workspace inheritance"), "{err}");
}

#[test]
fn parses_patch_and_replace_sources() {
    let manifest = PackageManifest::parse(
        "[package]\nname='d'\nversion='0.1.0'\n\
             [patch.crates-io]\nfoo = { path = '../foo' }\n\
             [replace]\n'bar:1.2.3' = { git = 'https://example.test/bar', rev = 'abc' }\n",
        Path::new("/tmp/x"),
    )
    .unwrap();
    assert_eq!(manifest.patches.len(), 1);
    assert_eq!(manifest.patches[0].registry, RegistryReference::CratesIo);
    assert_eq!(manifest.patches[0].dependency.package, "foo");
    assert_eq!(
        manifest.patches[0].dependency.source,
        DepSource::Path(PathBuf::from("/tmp/x/../foo"))
    );
    assert_eq!(manifest.replacements.len(), 1);
    assert_eq!(manifest.replacements[0].package, "bar");
    assert_eq!(
        manifest.replacements[0].version,
        semver::Version::new(1, 2, 3)
    );
    assert!(matches!(
        manifest.replacements[0].dependency.source,
        DepSource::Git(_)
    ));
}

#[test]
fn parses_lints_in_cargo_priority_order() {
    let manifest = PackageManifest::parse(
        "[package]\nname='d'\nversion='0.1.0'\n\
             [lints.rust]\nwarnings={level='allow',priority=-1}\n\
             unused={level='allow',priority=0}\n\
             dead_code={level='deny',priority=0}\n\
             unexpected_cfgs={level='warn',priority=1,check-cfg=['cfg(bootstrap)']}\n\
             unsafe_code={level='forbid',priority=2}\n\
             [lints.clippy]\npedantic={level='warn',priority=-2}\n",
        Path::new("/tmp/x"),
    )
    .unwrap();
    assert_eq!(
        manifest.rustc_lint_flags,
        [
            "--warn=clippy::pedantic",
            "--allow=warnings",
            "--allow=unused",
            "--deny=dead_code",
            "--warn=unexpected_cfgs",
            "--forbid=unsafe_code",
            "--check-cfg",
            "cfg(bootstrap)",
        ]
    );
}

#[test]
fn rejects_invalid_lint_shapes_loudly() {
    for (body, want) in [
        ("unsafe_code='force-warn'", "allow/warn/deny/forbid"),
        (
            "unsafe_code={level='warn',check-cfg=['cfg(x)']}",
            "unexpected_cfgs",
        ),
        (
            "unsafe_code={level='warn',priority='high'}",
            "must be an integer",
        ),
    ] {
        let error = PackageManifest::parse(
            &format!("[package]\nname='d'\nversion='0.1.0'\n[lints.rust]\n{body}\n"),
            Path::new("/tmp/x"),
        )
        .unwrap_err();
        assert!(error.contains(want), "{error}");
    }
}

#[test]
fn parses_three_feature_value_forms() {
    let m = PackageManifest::parse(
            "[package]\nname=\"d\"\nversion=\"0.1.0\"\n\
             [features]\ndefault = [\"std\", \"dep:serde\", \"itertools?/use_alloc\", \"old/strong\"]\n",
            Path::new("/tmp/x"),
        )
        .unwrap();
    let vals = &m.features["default"];
    assert_eq!(vals[0], FeatureValue::Simple("std".into()));
    assert_eq!(vals[1], FeatureValue::DepActivation("serde".into()));
    assert_eq!(
        vals[2],
        FeatureValue::WeakDep {
            dep: "itertools".into(),
            feature: "use_alloc".into()
        }
    );
    assert_eq!(
        vals[3],
        FeatureValue::StrongDep {
            dep: "old".into(),
            feature: "strong".into()
        }
    );
}

#[test]
fn cfg_platform_atoms_evaluate_for_host() {
    assert!(eval_cfg("cfg(target_os=\"linux\")").unwrap());
    assert!(!eval_cfg("cfg(target_os=\"windows\")").unwrap());
    assert!(eval_cfg("cfg(unix)").unwrap());
    assert!(eval_cfg("cfg(any(target_os=\"macos\", target_os=\"linux\"))").unwrap());
    assert!(!eval_cfg("cfg(not(unix))").unwrap());
    assert!(eval_cfg("cfg(all(unix, target_arch=\"x86_64\"))").unwrap());
    assert!(eval_cfg("cfg(target_pointer_width=\"64\")").unwrap());
    assert!(eval_cfg("cfg(target_endian=\"little\")").unwrap());
    // Custom keys (rustix-style) and unknown keys are naturally false (same source
    // as rustc --print cfg)
    assert!(!eval_cfg("cfg(rustix_use_libc)").unwrap());
    assert!(!eval_cfg("cfg(some_custom_key)").unwrap());
    // target_feature is covered exactly by rustc's list (x86_64 baseline = sse/sse2
    // true, avx2 false)
    assert!(eval_cfg("cfg(target_feature=\"sse2\")").unwrap());
    assert!(!eval_cfg("cfg(target_feature=\"avx512f\")").unwrap());
    // Complex nesting (a miniature rustix form)
    assert!(eval_cfg(
            "cfg(all(not(rustix_use_libc), target_os=\"linux\", any(target_arch=\"x86_64\", target_arch=\"aarch64\")))"
        )
        .unwrap());
}

#[test]
fn target_specific_dep_tables_carry_cfg_expr_unfiltered() {
    // Union over all platforms: no filtering at parse time, the cfg expression
    // travels with the row (same as cargo lock)
    let m = PackageManifest::parse(
        "[package]\nname=\"d\"\nversion=\"0.1.0\"\n\
             [target.'cfg(unix)'.dependencies]\nnix = \"0.29\"\n\
             [target.'cfg(windows)'.dependencies]\nwinapi = \"0.3\"\n",
        Path::new("/tmp/x"),
    )
    .unwrap();
    let nix = m.deps.iter().find(|d| d.key == "nix").unwrap();
    let winapi = m.deps.iter().find(|d| d.key == "winapi").unwrap();
    assert_eq!(nix.platform_cfg.as_deref(), Some("cfg(unix)"));
    assert_eq!(winapi.platform_cfg.as_deref(), Some("cfg(windows)"));
    // A plain table carries no marker
    assert!(nix.platform_cfg.is_some());
    // Host evaluation happens at use time: unix true, windows false
    assert!(eval_cfg(nix.platform_cfg.as_ref().unwrap()).unwrap());
    assert!(!eval_cfg(winapi.platform_cfg.as_ref().unwrap()).unwrap());
}

#[test]
fn autodiscovers_lib_main_and_bin_dir() {
    let d = tmpdir("autodisc");
    std::fs::create_dir_all(d.join("src/bin")).unwrap();
    std::fs::write(d.join("src/lib.rs"), "").unwrap();
    std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
    std::fs::write(d.join("src/bin/extra.rs"), "fn main(){}").unwrap();
    let m = PackageManifest::parse("[package]\nname=\"d\"\nversion=\"0.1.0\"\n", &d).unwrap();
    let bins: Vec<_> = m
        .targets
        .iter()
        .filter(|t| t.is_bin())
        .map(|t| t.name.as_str())
        .collect();
    assert!(m.targets.iter().any(Target::is_lib));
    assert_eq!(bins, ["d", "extra"]);
    // With several bins runnable_bin rejects loudly (resolve with an explicit --bin
    // or default-run); default-run pins one and resolves it
    let err = m.runnable_bin().unwrap_err();
    assert!(err.contains("--bin"), "{err}");
    let m2 = PackageManifest::parse(
        "[package]\nname=\"d\"\nversion=\"0.1.0\"\ndefault-run=\"extra\"\n",
        &d,
    )
    .unwrap();
    assert_eq!(m2.runnable_bin().unwrap().0, "extra");
    // --bin selects by name; a name not on the list is a loud error listing the
    // available ones
    assert_eq!(m.runnable_bin_opt(Some("extra")).unwrap().0, "extra");
    let err = m.runnable_bin_opt(Some("nope")).unwrap_err();
    assert!(err.contains("nope") && err.contains("extra"), "{err}");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn lib_proc_macro_accepts_both_spellings() {
    // Both spellings are accepted (the same discipline as the minimal registry read
    // in resolve.rs: hyphen = the cargo documentation form, underscore = what newer
    // cargo normalizes to)
    let d = tmpdir("libpm");
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/lib.rs"), "").unwrap();
    for key in ["proc-macro", "proc_macro"] {
        let m = PackageManifest::parse(
            &format!("[package]\nname=\"d\"\nversion=\"0.1.0\"\n[lib]\n{key} = true\n"),
            &d,
        )
        .unwrap();
        let pm = m.targets.iter().any(|t| t.is_lib() && t.proc_macro);
        assert!(pm, "spelling {key} must be recognized");
    }
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn parses_dev_dependencies_test_profile_and_test_targets() {
    let d = tmpdir("test-targets");
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::create_dir_all(d.join("tests/nested")).unwrap();
    std::fs::create_dir_all(d.join("examples")).unwrap();
    std::fs::write(d.join("src/lib.rs"), "").unwrap();
    std::fs::write(d.join("tests/api.rs"), "").unwrap();
    std::fs::write(d.join("tests/required.rs"), "").unwrap();
    std::fs::write(d.join("tests/nested/main.rs"), "").unwrap();
    std::fs::write(d.join("tests/nested/helper.rs"), "").unwrap();
    std::fs::write(d.join("examples/demo.rs"), "fn main(){}").unwrap();
    let m = PackageManifest::parse(
        "[package]\nname=\"d\"\nversion=\"0.1.0\"\n\
             [dev-dependencies]\nhelper=\"1\"\n\
             [profile.dev]\nopt-level=1\n\
             [profile.test]\nopt-level=\"z\"\ndebug-assertions=false\n\
             [[test]]\nname=\"required\"\npath=\"tests/required.rs\"\n",
        &d,
    )
    .unwrap();
    assert_eq!(
        m.deps.iter().find(|d| d.key == "helper").unwrap().kind,
        DepKind::Dev
    );
    assert_eq!(m.profile.opt_level, OptLevel::O1);
    assert_eq!(m.test_profile.opt_level, OptLevel::Oz);
    assert!(!m.test_profile.debug_assertions);
    let targets: Vec<_> = m
        .targets
        .iter()
        .map(|t| (t.kind, t.name.as_str()))
        .collect();
    assert!(targets.contains(&(TargetKind::Test, "api")));
    assert!(targets.contains(&(TargetKind::Test, "nested")));
    assert!(targets.contains(&(TargetKind::Test, "required")));
    assert!(targets.contains(&(TargetKind::Example, "demo")));
    assert!(
        !m.targets
            .iter()
            .find(|target| target.kind == TargetKind::Example && target.name == "demo")
            .unwrap()
            .test
    );
    assert!(!targets.contains(&(TargetKind::Test, "helper")));
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn build_script_detection_and_workspace_package_inheritance() {
    let d = tmpdir("buildrs");
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
    std::fs::write(d.join("build.rs"), "fn main(){}").unwrap();
    let m = PackageManifest::parse(
        "[package]\nname=\"d\"\nedition.workspace = true\nversion.workspace = true\n\
             [workspace]\n[workspace.package]\nversion = \"9.9.9\"\nedition = \"2021\"\n",
        &d,
    )
    .unwrap();
    assert_eq!(m.version.to_string(), "9.9.9");
    assert_eq!(m.edition, "2021");
    assert!(m.has_build_script);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn build_eq_false_disables_build_script_detection() {
    // cargo semantics: `build = false` disables it explicitly (confirmed by cfg-if;
    // the key being present is not the same as having build.rs); even with a build.rs
    // sitting in the root it does not count (as in cargo)
    let d = tmpdir("buildfalse");
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
    std::fs::write(d.join("build.rs"), "fn main(){}").unwrap();
    let m = PackageManifest::parse(
        "[package]\nname = \"d\"\nversion = \"0.1.0\"\nbuild = false\n",
        &d,
    )
    .unwrap();
    assert!(!m.has_build_script);
    let m = PackageManifest::parse(
        "[package]\nname = \"d\"\nversion = \"0.1.0\"\nbuild = \"custom.rs\"\n",
        &d,
    )
    .unwrap();
    assert!(m.has_build_script);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn frontmatter_pseudo_package_parses() {
    let d = tmpdir("frontmatter");
    let body = d.join("main.rs");
    std::fs::write(&body, "fn main(){}").unwrap();
    let m =
        PackageManifest::from_frontmatter("c_demo", "[dependencies]\nserde_json = \"1\"\n", &body)
            .unwrap();
    assert_eq!(m.name, "c_demo");
    assert_eq!(m.edition, "2024");
    assert_eq!(m.deps.len(), 1);
    assert_eq!(m.runnable_bin().unwrap().0, "c_demo");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn frontmatter_at_decouples_root_and_body_path() {
    // Same layout as the materialized cargo-side project: root=<cache> (the
    // CARGO_MANIFEST_DIR convention) while the body lives in <cache>/src/main.rs
    // (so file!() = "src/main.rs" after remapping).
    let d = tmpdir("frontmatterat");
    let body = d.join("src/main.rs");
    std::fs::create_dir_all(body.parent().unwrap()).unwrap();
    std::fs::write(&body, "fn main(){}").unwrap();
    let m = PackageManifest::from_frontmatter_at(
        "c_demo",
        "[dependencies]\nserde_json = \"1\"\n",
        &d,
        &body,
    )
    .unwrap();
    assert_eq!(m.root, d);
    assert_eq!(m.runnable_bin().unwrap().1, body.as_path());
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn pkg_env_splits_version_and_blanks_missing_keys() {
    let m = PackageManifest::parse(
        r#"
[package]
name = "demo"
version = "1.2.3-rc.1"
edition = "2021"
authors = ["Alice <a@x>", "Bob"]
description = "演示包"
readme = true
license = "MIT"
"#,
        Path::new("/tmp/x"),
    )
    .unwrap();
    let e = &m.pkg_env;
    assert_eq!(e["CARGO_PKG_NAME"], "demo");
    assert_eq!(e["CARGO_PKG_VERSION"], "1.2.3-rc.1");
    assert_eq!(e["CARGO_PKG_VERSION_MAJOR"], "1");
    assert_eq!(e["CARGO_PKG_VERSION_MINOR"], "2");
    assert_eq!(e["CARGO_PKG_VERSION_PATCH"], "3");
    assert_eq!(e["CARGO_PKG_VERSION_PRE"], "rc.1");
    assert_eq!(e["CARGO_PKG_AUTHORS"], "Alice <a@x>:Bob");
    assert_eq!(e["CARGO_PKG_DESCRIPTION"], "演示包");
    assert_eq!(e["CARGO_PKG_README"], "README.md");
    assert_eq!(e["CARGO_PKG_LICENSE"], "MIT");
    // A missing key is the empty string (the same cargo contract), but the key must
    // be present
    for k in [
        "CARGO_PKG_HOMEPAGE",
        "CARGO_PKG_REPOSITORY",
        "CARGO_PKG_LICENSE_FILE",
        "CARGO_PKG_RUST_VERSION",
    ] {
        assert_eq!(
            e.get(k).map(String::as_str),
            Some(""),
            "{k} should be empty"
        );
    }
    // A version without a prerelease segment has an empty PRE
    let e2 = pkg_env_map(
        "d",
        &semver::Version::new(0, 1, 0),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    assert_eq!(e2["CARGO_PKG_VERSION_PRE"], "");
    assert_eq!(e2["CARGO_PKG_AUTHORS"], "");
}

#[test]
fn check_cfg_values_include_implicit_optional_and_exclude_dep_shadowed() {
    let m = PackageManifest::parse(
        "[package]\nname = \"d\"\nversion = \"0.1.0\"\n\
             [dependencies]\nserde = { version = \"1\", optional = true }\n\
             itertools = { version = \"0.14\", optional = true }\n\
             plain = \"1\"\n\
             [features]\ndefault = [\"dep:serde\"]\nextra = []\n",
        Path::new("/tmp/x"),
    )
    .unwrap();
    let vals = m.check_cfg_feature_values();
    // [features] table keys are present
    assert!(vals.contains("default"));
    assert!(vals.contains("extra"));
    // An optional dependency not named by dep: -> an implicit feature of the same name
    assert!(vals.contains("itertools"));
    // Shadowed by dep:serde -> no implicit feature of the same name
    assert!(!vals.contains("serde"));
    // A non-optional dependency never enters the value table
    assert!(!vals.contains("plain"));
}

#[test]
fn resolver_and_rust_version_follow_cargo_manifest_rules() {
    let directory = tmpdir("resolver-rust-version");
    std::fs::create_dir_all(directory.join("src")).unwrap();
    std::fs::write(directory.join("src/main.rs"), "fn main() {}\n").unwrap();

    let implicit = PackageManifest::parse(
        "[package]\nname='implicit'\nversion='0.1.0'\nedition='2024'\nrust-version='1.85'\n",
        &directory,
    )
    .unwrap();
    assert_eq!(implicit.resolver, ResolverVersion::V3);
    assert_eq!(implicit.rust_version, Some(semver::Version::new(1, 85, 0)));

    let explicit = PackageManifest::parse(
            "[package]\nname='explicit'\nversion='0.1.0'\nedition='2021'\nresolver='3'\nrust-version='1.62'\n",
            &directory,
        )
        .unwrap();
    assert_eq!(explicit.resolver, ResolverVersion::V3);
    assert_eq!(explicit.rust_version, Some(semver::Version::new(1, 62, 0)));

    let error = PackageManifest::parse(
        "[package]\nname='too-old'\nversion='0.1.0'\nedition='2024'\nrust-version='1.62'\n",
        &directory,
    )
    .unwrap_err();
    assert!(error.contains("1.85.0"), "{error}");
    assert!(
        PackageManifest::parse(
            "[package]\nname='bad'\nversion='0.1.0'\nedition='2021'\nrust-version='>=1.70'\n",
            &directory,
        )
        .is_err()
    );
    std::fs::remove_dir_all(directory).unwrap();
}
