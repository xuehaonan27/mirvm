//! Cargo-script frontmatter: manifest parsing, the effective manifest, and materialization.
//!
//! Both tracks that turn a script into a package go through here: the cargo track writes the text
//! [`effective_manifest`] returns, and the cargoless track parses the same text in memory. They used
//! to synthesize that manifest separately, which let them drift.

use std::path::{Path, PathBuf};

// ===== frontmatter (cargo script syntax: RFC 3502's `---` frontmatter form) =====

/// Parse the manifest embedded in a `---` fence. Returns (manifest, body with the manifest lines
/// blanked so line numbers are preserved). Script entry for cargoless::audit; the core stays
/// private.
pub(crate) fn parse_frontmatter_pub(src: &str) -> Option<(String, String)> {
    parse_frontmatter(src)
}

pub(super) fn parse_frontmatter(src: &str) -> Option<(String, String)> {
    let mut lines = src.lines().enumerate().peekable();
    // Skip shebang
    if lines.peek().is_some_and(|(_, l)| l.starts_with("#!")) {
        lines.next();
    }
    // Skip blank lines
    while lines.peek().is_some_and(|(_, l)| l.trim().is_empty()) {
        lines.next();
    }
    let (_open_idx, open) = lines.next()?;
    let fence = open.trim_end();
    if !fence.starts_with("---") {
        return None;
    }
    // An infostring (such as `---cargo`) is allowed; its content is ignored.
    let mut manifest = String::new();
    let mut close_idx = None;
    for (i, l) in lines {
        if l.trim_end() == "---" {
            close_idx = Some(i);
            break;
        }
        manifest.push_str(l);
        manifest.push('\n');
    }
    let close_idx = close_idx?; // no closing fence => not frontmatter
    // Body = the original file with lines [0, close_idx] replaced by blanks (keeps diagnostic
    // line numbers).
    let body: String = src
        .lines()
        .enumerate()
        .map(|(i, l)| if i <= close_idx { "" } else { l })
        .collect::<Vec<_>>()
        .join("\n");
    Some((manifest, body))
}

/// Where a script's materialized package lives: `$MIRVM_HOME/build/scripts/<hash(absolute path)>`.
///
/// The path hash (not the content) is the key, so editing a script keeps its directory, its
/// `Cargo.lock` and its warm fingerprints.
pub(crate) fn script_cache_dir(script: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};

    let abs = std::path::absolute(script).unwrap_or_else(|_| script.to_path_buf());
    let mut hasher = std::hash::DefaultHasher::new();
    abs.hash(&mut hasher);
    crate::store::SCRIPTS
        .dir()
        .join(format!("{:016x}", hasher.finish()))
}

/// The fields the materialization site decides, because the tracks differ here: the cargo track
/// suffixes the bin with a path hash so scratch variants do not share a final binary, and points it
/// at `src/main.rs`, while the cargoless track names it after the stem and points it at the body it
/// already wrote.
pub(crate) struct ScriptPackage<'a> {
    pub name: &'a str,
    pub bin_name: &'a str,
    pub bin_path: &'a Path,
}

/// The effective `Cargo.toml` of a single-file package: the embedded manifest with RFC 3502's rules
/// applied.
///
/// RFC 3502 makes a single-file package bin-only and forbids the fields that could change that, and
/// it infers the fields cargo would otherwise require from the file. Everything else in the embedded
/// manifest (`[dependencies]`, `[features]`, `[patch]`, ..) passes through untouched, and a
/// `[package]` table is merged rather than rejected, so a script that sets its own edition or name
/// works.
///
/// Deviation: RFC 3502 warns when `package.edition` is unspecified. mirvm is an edition-2024
/// runtime, so it defaults to 2024 silently; a warning line would break the differential suites,
/// which compare stderr byte-for-byte against native cargo.
pub(crate) fn effective_manifest(
    package: ScriptPackage<'_>,
    manifest: &str,
) -> Result<String, String> {
    const DEFAULT_EDITION: &str = "2024";
    const DISALLOWED_TABLES: [&str; 6] = ["workspace", "lib", "bin", "example", "test", "bench"];
    const DISALLOWED_PACKAGE: [&str; 8] = [
        "workspace",
        "build",
        "links",
        "publish",
        "autobins",
        "autoexamples",
        "autotests",
        "autobenches",
    ];

    let mut root: toml::Table =
        toml::from_str(manifest).map_err(|e| format!("invalid embedded manifest: {e}"))?;
    for key in DISALLOWED_TABLES {
        if root.contains_key(key) {
            return Err(format!(
                "`[{key}]` is not allowed in a single-file package (RFC 3502): a script is bin-only, \
                 so move this into a Cargo.toml package"
            ));
        }
    }
    let mut embedded = match root.remove("package") {
        None => toml::Table::new(),
        Some(toml::Value::Table(table)) => table,
        Some(_) => return Err("`package` must be a table".to_string()),
    };
    for key in DISALLOWED_PACKAGE {
        if embedded.contains_key(key) {
            return Err(format!(
                "`package.{key}` is not allowed in a single-file package (RFC 3502)"
            ));
        }
    }

    // Inferred fields are defaults, so a value the script set wins.
    let mut package_table = toml::Table::new();
    package_table.insert(
        "name".to_string(),
        embedded
            .remove("name")
            .unwrap_or_else(|| toml::Value::String(package.name.to_string())),
    );
    package_table.insert(
        "version".to_string(),
        embedded
            .remove("version")
            .unwrap_or_else(|| toml::Value::String("0.0.0".to_string())),
    );
    package_table.insert(
        "edition".to_string(),
        embedded
            .remove("edition")
            .unwrap_or_else(|| toml::Value::String(DEFAULT_EDITION.to_string())),
    );
    for (key, value) in embedded {
        package_table.insert(key, value);
    }

    let mut bin = toml::Table::new();
    bin.insert(
        "name".to_string(),
        toml::Value::String(package.bin_name.to_string()),
    );
    bin.insert(
        "path".to_string(),
        toml::Value::String(package.bin_path.display().to_string()),
    );

    // Rebuild in the order cargo's own materialization reads best: package, the bin, then whatever
    // the script declared.
    let mut out = toml::Table::new();
    out.insert("package".to_string(), toml::Value::Table(package_table));
    out.insert(
        "bin".to_string(),
        toml::Value::Array(vec![toml::Value::Table(bin)]),
    );
    for (key, value) in root {
        out.insert(key, value);
    }
    toml::to_string(&out).map_err(|e| format!("cannot render the effective manifest: {e}"))
}

/// Materialize a script as a cargo project in the cache and return the project directory.
pub(crate) fn materialize_script(
    script: &Path,
    manifest: &str,
    body: &str,
) -> Result<PathBuf, String> {
    let dir = script_cache_dir(script);
    std::fs::create_dir_all(dir.join("src")).expect("failed to create the script cache directory");
    std::fs::create_dir_all(dir.join(".cargo"))
        .expect("failed to create the script .cargo directory");

    let stem = script
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || name.chars().next().unwrap().is_ascii_digit() {
        name = format!("s{name}");
    }

    // The bin name carries a short path-hash suffix: in the shared target dir the final binary
    // lands in the fingerprint-free debug/<binname>, so scripts sharing a stem but not a path
    // (scratch variants under /tmp) do not overwrite each other. The package name keeps the stem
    // so recipes can find the script dir by name.
    let hash = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let bin_name = format!("{name}-{}", &hash[..hash.len().min(8)]);
    let cargo_toml = effective_manifest(
        ScriptPackage {
            name: &name,
            bin_name: &bin_name,
            bin_path: Path::new("src/main.rs"),
        },
        manifest,
    )?;

    // Idempotent materialization: unchanged content is not rewritten. Stable mtime is a
    // precondition for both the L2 IR cache manifest and cargo fingerprints.
    write_if_changed(&dir.join("Cargo.toml"), &cargo_toml);
    write_if_changed(&dir.join("src/main.rs"), body);
    // The native differential build also uses the unified store: the shim build overrides this key
    // with an explicit --target-dir, while native cargo run uses the file config. The two families
    // get separate directories (their sysroots and rustflags differ, so fingerprints would not
    // collide anyway; separate directories only make purge semantics clearer).
    let native_target = crate::store::TARGET.dir().join("native");
    write_if_changed(
        &dir.join(".cargo/config.toml"),
        &format!(
            "[build]
target-dir = \"{}\"
",
            native_target.display()
        ),
    );
    Ok(dir)
}

fn write_if_changed(path: &Path, contents: &str) {
    if std::fs::read(path).is_ok_and(|old| old == contents.as_bytes()) {
        return;
    }
    std::fs::write(path, contents)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(embedded: &str) -> Result<toml::Table, String> {
        let text = effective_manifest(
            ScriptPackage {
                name: "demo",
                bin_name: "demo-01234567",
                bin_path: Path::new("src/main.rs"),
            },
            embedded,
        )?;
        toml::from_str(&text).map_err(|e| e.to_string())
    }

    #[test]
    fn rfc_3502_disallowed_tables_are_rejected() {
        for embedded in [
            "[lib]\npath = \"lib.rs\"\n",
            "[[bin]]\nname = \"other\"\npath = \"other.rs\"\n",
            "[workspace]\nmembers = []\n",
            "[[example]]\nname = \"e\"\npath = \"e.rs\"\n",
            "[[test]]\nname = \"t\"\npath = \"t.rs\"\n",
            "[[bench]]\nname = \"b\"\npath = \"b.rs\"\n",
        ] {
            let error = manifest(embedded).unwrap_err();
            assert!(error.contains("is not allowed"), "{embedded:?} -> {error}");
        }
    }

    #[test]
    fn rfc_3502_disallowed_package_fields_are_rejected() {
        for field in [
            "workspace = true",
            "build = \"build.rs\"",
            "links = \"z\"",
            "publish = true",
            "autobins = false",
        ] {
            let embedded = format!("[package]\n{field}\n");
            let error = manifest(&embedded).unwrap_err();
            assert!(error.contains("package."), "{field:?} -> {error}");
            assert!(error.contains("is not allowed"), "{field:?} -> {error}");
        }
    }

    #[test]
    fn an_embedded_package_table_is_merged_rather_than_duplicated() {
        let table = manifest("[package]\nedition = \"2021\"\ndescription = \"d\"\n").unwrap();
        let package = table.get("package").unwrap().as_table().unwrap();
        assert_eq!(package.get("edition").unwrap().as_str(), Some("2021"));
        assert_eq!(package.get("description").unwrap().as_str(), Some("d"));
        assert_eq!(package.get("name").unwrap().as_str(), Some("demo"));
        assert_eq!(package.get("version").unwrap().as_str(), Some("0.0.0"));
    }

    #[test]
    fn inferred_fields_fill_only_what_the_script_left_out() {
        let table = manifest("[package]\nname = \"mine\"\n").unwrap();
        let package = table.get("package").unwrap().as_table().unwrap();
        assert_eq!(package.get("name").unwrap().as_str(), Some("mine"));
        assert_eq!(package.get("edition").unwrap().as_str(), Some("2024"));
    }

    #[test]
    fn other_tables_pass_through_and_the_bin_is_injected_once() {
        let table = manifest("[dependencies]\nserde_json = \"1\"\n").unwrap();
        assert!(table.get("dependencies").unwrap().is_table());
        let bins = table.get("bin").unwrap().as_array().unwrap();
        assert_eq!(bins.len(), 1);
        let bin = bins[0].as_table().unwrap();
        assert_eq!(bin.get("name").unwrap().as_str(), Some("demo-01234567"));
        assert_eq!(bin.get("path").unwrap().as_str(), Some("src/main.rs"));
    }

    #[test]
    fn unparsable_frontmatter_is_reported() {
        assert!(manifest("[package\n").is_err());
    }
}
