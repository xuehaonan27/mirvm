//! `cargoless/rustflags.rs` — rustflags subset parser (D15 P3 slice 5a).
//! Handles `CARGO_ENCODED_RUSTFLAGS` / `RUSTFLAGS` env vars plus `.cargo/config.toml`
//! `build.rustflags` / `target.<triple>.rustflags` / `target.'cfg(all())'.rustflags`.
//!
//! Placement semantics (slice 5a proven by probe_buildrs + `--target x86_64-unknown-linux-gnu`
//! with RUSTFLAGS='--cap-lints allow', checking 13 rustc invocation lines by class): when cargo
//! is given --target, rustflags land only on **target units** — target deps and the final bin
//! (registry target deps show two `--cap-lints allow`: one built-in + one from RUSTFLAGS;
//! path-based bin crates show only the RUSTFLAGS one). **Host units never get them** —
//! build.rs, proc-macro, and host dep compilations have no RUSTFLAGS trace.
//! This module only produces the flag list; placement discipline lives in schedule.rs:
//! dep_rustc_args appends after -Z flags, bin_rustc_args appends at the end (later rustc flags
//! override earlier ones, user flags override built-ins). The three host-side argument signatures
//! simply omit rustflags, so the compiler enforces they are not fed.
//!
//! Boundaries (documented, not claimed closed):
//! - HOST_RUSTFLAGS is not implemented (host-side compilation never consumes user rustflags).
//! - Config discovery = first `.cargo/config.toml` found by walking up from the project root,
//!   then `$HOME/.cargo/config.toml`; multi-file merging is not done — nearest wins; legacy
//!   extension-less `.cargo/config` is not read.
//! - `target.'cfg(...)'` only special-cases the always-true `cfg(all())` (exact string match,
//!   whitespace variants like `cfg( all() )` are rejected); other cfg expressions are not
//!   evaluated and the key is ignored (cargo would evaluate and merge matching cfgs; v1 does not).
//! - Higher priority fully overrides lower priority, first hit wins without concatenation (same
//!   as cargo); env presence wins even if empty (empty string = empty flag list, still overrides
//!   config).

use std::path::{Path, PathBuf};

/// Resolve rustflags: priority encoded > env > config (first hit wins, no concatenation).
/// `env_get` injects environment reads (tests avoid the real environment to dodge env races
/// under concurrent test runs); `root` = project root (config discovery start; scripts = cache
/// materialization directory).
pub fn resolve(
    env_get: impl Fn(&str) -> Option<String>,
    root: &Path,
) -> Result<Vec<String>, String> {
    if let Some(v) = env_get("CARGO_ENCODED_RUSTFLAGS") {
        return Ok(split_encoded(&v));
    }
    if let Some(v) = env_get("RUSTFLAGS") {
        return Ok(split_ws(&v));
    }
    let home = env_get("HOME").map(PathBuf::from);
    let Some(cfg) = find_config(root, home.as_deref()) else {
        return Ok(Vec::new());
    };
    parse_config(&cfg)
}

/// Real-environment entry point used by drive().
pub fn from_env_and_disk(root: &Path) -> Result<Vec<String>, String> {
    resolve(|k| std::env::var(k).ok(), root)
}

/// CARGO_ENCODED_RUSTFLAGS: split on \x1f (empty string = empty list; empty segments are
/// filtered out harmlessly).
fn split_encoded(v: &str) -> Vec<String> {
    v.split('\x1f')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// RUSTFLAGS / config string form: split on whitespace (same as cargo).
fn split_ws(v: &str) -> Vec<String> {
    v.split_whitespace().map(str::to_string).collect()
}

/// Config discovery: first `.cargo/config.toml` found by walking up from `root`; if none,
/// check `$HOME/.cargo/config.toml` (when root is outside $HOME such as /tmp, the ancestor
/// chain does not reach $HOME, so cargo also checks both). Nearest wins; no multi-file merge.
fn find_config(root: &Path, home: Option<&Path>) -> Option<PathBuf> {
    for dir in root.ancestors() {
        let p = dir.join(".cargo/config.toml");
        if p.is_file() {
            return Some(p);
        }
    }
    let p = home?.join(".cargo/config.toml");
    p.is_file().then_some(p)
}

/// Single-file config parsing: highest priority target.<exact MIRVM_HOST triple>, then
/// target.'cfg(all())', finally build.rustflags (first hit wins). Missing all three = empty
/// list (not an error); invalid values (non-string/non-array, bad toml) = loud error — user
/// mistakes should not be swallowed silently.
fn parse_config(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let v: toml::Value =
        toml::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
    let target = v.get("target").and_then(toml::Value::as_table);
    let target_hit = |key: &str| -> Option<&toml::Value> {
        target
            .and_then(|t| t.get(key))
            .and_then(|t| t.get("rustflags"))
    };
    if let Some(f) = target_hit(env!("MIRVM_HOST")) {
        return flags_value(f, path);
    }
    if let Some(f) = target_hit("cfg(all())") {
        return flags_value(f, path);
    }
    if let Some(f) = v.get("build").and_then(|b| b.get("rustflags")) {
        return flags_value(f, path);
    }
    Ok(Vec::new())
}

/// rustflags value: string (split on whitespace) or array of strings; other types error loudly.
fn flags_value(v: &toml::Value, path: &Path) -> Result<Vec<String>, String> {
    match v {
        toml::Value::String(s) => Ok(split_ws(s)),
        toml::Value::Array(xs) => {
            let mut out = Vec::with_capacity(xs.len());
            for x in xs {
                let Some(s) = x.as_str() else {
                    return Err(format!(
                        "rustflags array members in {} must be strings: {x}",
                        path.display()
                    ));
                };
                out.push(s.to_string());
            }
            Ok(out)
        }
        _ => Err(format!(
            "rustflags in {} must be a string or array of strings: {v}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mirvm-cargoless-rustflags-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Empty environment (not even HOME set — avoids accidentally reading the host's real
    /// ~/.cargo/config.toml).
    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn encoded_beats_env_beats_config() {
        // Create a config too: all three sources present to verify first-hit priority.
        let dir = tmpdir("prio");
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--from-config\"]\n",
        )
        .unwrap();
        // encoded > env > config
        let f = resolve(
            |k| match k {
                "CARGO_ENCODED_RUSTFLAGS" => Some("--a\x1f--b".to_string()),
                "RUSTFLAGS" => Some("--from-env".to_string()),
                _ => None,
            },
            &dir,
        )
        .unwrap();
        assert_eq!(f, vec!["--a", "--b"], "encoded has highest priority");
        // env > config (encoded absent)
        let f = resolve(
            |k| match k {
                "RUSTFLAGS" => Some("--from-env".to_string()),
                _ => None,
            },
            &dir,
        )
        .unwrap();
        assert_eq!(f, vec!["--from-env"], "env overrides config");
        // Empty env still overrides config (presence wins, empty = empty list)
        let f = resolve(|k| (k == "RUSTFLAGS").then(String::new), &dir).unwrap();
        assert!(
            f.is_empty(),
            "empty env overrides config and yields empty list"
        );
        // Only when all are absent does config apply
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--from-config"]);
    }

    #[test]
    fn two_split_forms() {
        // encoded splits on \x1f, whitespace inside a segment is preserved
        let f = resolve(
            |k| (k == "CARGO_ENCODED_RUSTFLAGS").then(|| "--cfg\x1ffoo bar\x1f".to_string()),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(
            f,
            vec!["--cfg", "foo bar"],
            "empty segments dropped, inner whitespace kept"
        );
        // env splits on whitespace
        let f = resolve(
            |k| (k == "RUSTFLAGS").then(|| "  --cap-lints  allow\t--cfg x ".to_string()),
            Path::new("/nonexistent"),
        )
        .unwrap();
        assert_eq!(f, vec!["--cap-lints", "allow", "--cfg", "x"]);
    }

    #[test]
    fn config_discovery_walks_up_and_nearest_wins() {
        // Project root at tmp/proj/sub/deeper: create .cargo/config.toml in tmp/proj,
        // walking up from deeper must find it.
        let dir = tmpdir("walk");
        let proj = dir.join("proj");
        let deeper = proj.join("sub/deeper");
        std::fs::create_dir_all(deeper.join(".cargo")).unwrap();
        std::fs::create_dir_all(proj.join(".cargo")).unwrap();
        std::fs::write(
            proj.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--outer\"\n",
        )
        .unwrap();
        // The nearer sub/deeper/.cargo/config.toml wins (nearest wins, no merge)
        std::fs::write(
            deeper.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--inner\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &deeper).unwrap();
        assert_eq!(f, vec!["--inner"], "nearest wins");
        std::fs::remove_file(deeper.join(".cargo/config.toml")).unwrap();
        let f = resolve(no_env, &deeper).unwrap();
        assert_eq!(f, vec!["--outer"], "walked up to ancestor config");
        // $HOME fallback: when root and home are disjoint, check $HOME/.cargo/config.toml
        let home = tmpdir("home");
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(
            home.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-home\"\n",
        )
        .unwrap();
        let root = tmpdir("elsewhere");
        let f = resolve(|k| (k == "HOME").then(|| home.display().to_string()), &root).unwrap();
        assert_eq!(f, vec!["--from-home"]);
    }

    #[test]
    fn config_precedence_triple_then_cfg_all_then_build() {
        let dir = tmpdir("cfgprio");
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        let triple = env!("MIRVM_HOST");
        // All three present: triple wins
        std::fs::write(
            dir.join(".cargo/config.toml"),
            format!(
                "[build]\nrustflags = \"--from-build\"\n\
                 [target.'cfg(all())']\nrustflags = \"--from-cfg-all\"\n\
                 [target.'{triple}']\nrustflags = \"--from-triple\"\n"
            ),
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(
            f,
            vec!["--from-triple"],
            "target.<triple> has highest priority"
        );
        // Triple absent: cfg(all()) wins
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-build\"\n\
             [target.'cfg(all())']\nrustflags = \"--from-cfg-all\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(
            f,
            vec!["--from-cfg-all"],
            "cfg(all()) always-true special case beats build"
        );
        // Other cfg expressions are not evaluated; the key is ignored and falls back to build
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-build\"\n\
             [target.'cfg(windows)']\nrustflags = \"--from-windows\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(
            f,
            vec!["--from-build"],
            "cfg(windows) not evaluated, ignored"
        );
        // Target table present but no rustflags key -> fall through to next level
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--from-build\"\n[target.'cfg(all())']\nlinker = \"cc\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--from-build"]);
    }

    #[test]
    fn config_value_forms_and_errors() {
        let dir = tmpdir("forms");
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        // String form splits on whitespace
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = \"--cap-lints allow\"\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--cap-lints", "allow"]);
        // Array form takes each element
        std::fs::write(
            dir.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--cap-lints\", \"allow\"]\n",
        )
        .unwrap();
        let f = resolve(no_env, &dir).unwrap();
        assert_eq!(f, vec!["--cap-lints", "allow"]);
        // Invalid type errors loudly
        std::fs::write(dir.join(".cargo/config.toml"), "[build]\nrustflags = 42\n").unwrap();
        assert!(
            resolve(no_env, &dir).is_err(),
            "integer rustflags must error"
        );
        // Bad toml errors loudly
        std::fs::write(dir.join(".cargo/config.toml"), "[build\n").unwrap();
        assert!(resolve(no_env, &dir).is_err(), "bad toml must error");
    }
}
