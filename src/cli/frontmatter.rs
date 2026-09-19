//! Cargo-script frontmatter: manifest parsing and script materialization.

use std::path::{Path, PathBuf};

// ===== frontmatter (cargo script RFC 3424 syntax) =====

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

/// Materialize a script as a cargo project in the cache and return the project directory.
pub(super) fn materialize_script(script: &Path, manifest: &str, body: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};

    let abs = std::path::absolute(script).unwrap_or_else(|_| script.to_path_buf());
    let mut hasher = std::hash::DefaultHasher::new();
    abs.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let dir = crate::sysroot::cache_dir().join("scripts").join(&hash);
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
    let bin_name = format!("{name}-{}", &hash[..8]);
    let cargo_toml = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [[bin]]\nname = \"{bin_name}\"\npath = \"src/main.rs\"\n\n{manifest}"
    );
    // Idempotent materialization: unchanged content is not rewritten. Stable mtime is a
    // precondition for both the L2 IR cache manifest and cargo fingerprints.
    write_if_changed(&dir.join("Cargo.toml"), &cargo_toml);
    write_if_changed(&dir.join("src/main.rs"), body);
    // The native differential build also uses the unified store: the shim build overrides this key
    // with an explicit --target-dir, while native cargo run uses the file config. The two families
    // get separate directories (their sysroots and rustflags differ, so fingerprints would not
    // collide anyway; separate directories only make purge semantics clearer).
    let native_target = crate::sysroot::cache_dir().join("target/native");
    write_if_changed(
        &dir.join(".cargo/config.toml"),
        &format!("[build]\ntarget-dir = \"{}\"\n", native_target.display()),
    );
    dir
}

fn write_if_changed(path: &Path, contents: &str) {
    if std::fs::read(path).is_ok_and(|old| old == contents.as_bytes()) {
        return;
    }
    std::fs::write(path, contents)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}
