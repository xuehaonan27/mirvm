#![cfg(test)]

//! Cargo 配置中只与当前依赖求解批次有关的一项：
//! `resolver.incompatible-rust-versions`。
//!
//! 发现和合并遵循 Cargo 层级：`$CARGO_HOME` 最低，当前目录的祖先从浅到深
//! 逐层覆盖；同目录同时存在时 `.cargo/config` 优先于 `config.toml`。本模块
//! 也处理 Cargo 的 `include`，避免配置中已有该机制时静默漏掉 resolver 策略。
//! 其余配置键留给各自产品切片消费，不在这里建设通用配置对象。

use std::path::{Path, PathBuf};

use super::manifest::{IncompatibleRustVersions, ResolverVersion};

fn default_policy(resolver: ResolverVersion) -> IncompatibleRustVersions {
    match resolver {
        ResolverVersion::V3 => IncompatibleRustVersions::Fallback,
        ResolverVersion::V1 | ResolverVersion::V2 => IncompatibleRustVersions::Allow,
    }
}

fn resolve(
    env_get: impl Fn(&str) -> Option<String>,
    current: &Path,
    cargo_home: Option<&Path>,
    resolver: ResolverVersion,
) -> Result<IncompatibleRustVersions, String> {
    let mut selected = None;
    if let Some(home) = cargo_home
        && let Some(file) = config_file(home)
    {
        selected = load_file(&file, &mut Vec::new())?.or(selected);
    }

    let mut ancestors: Vec<&Path> = current.ancestors().collect();
    ancestors.reverse();
    for directory in ancestors {
        if let Some(file) = config_file(&directory.join(".cargo")) {
            selected = load_file(&file, &mut Vec::new())?.or(selected);
        }
    }

    if let Some(value) = env_get("CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS") {
        selected = Some(IncompatibleRustVersions::parse(&value)?);
    }
    Ok(selected.unwrap_or_else(|| default_policy(resolver)))
}

/// Cargo 为兼容旧项目，在两个文件同时存在时选择无扩展名的 `config`。
fn config_file(directory: &Path) -> Option<PathBuf> {
    let legacy = directory.join("config");
    if legacy.is_file() {
        return Some(legacy);
    }
    let toml = directory.join("config.toml");
    toml.is_file().then_some(toml)
}

fn load_file(
    path: &Path,
    stack: &mut Vec<PathBuf>,
) -> Result<Option<IncompatibleRustVersions>, String> {
    let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if stack.contains(&identity) {
        return Err(format!("Cargo config include 形成环: {}", path.display()));
    }
    stack.push(identity.clone());
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读取 Cargo config {} 失败: {error}", path.display()))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| format!("解析 Cargo config {} 失败: {error}", path.display()))?;
    let mut selected = None;
    if let Some(includes) = value.get("include") {
        let includes = includes
            .as_array()
            .ok_or_else(|| format!("Cargo config {} 的 include 必须是数组", path.display()))?;
        for include in includes {
            let (relative, optional) = include_value(include, path)?;
            let include_path = path.parent().unwrap_or(Path::new(".")).join(relative);
            if !include_path.is_file() {
                if optional {
                    continue;
                }
                return Err(format!(
                    "Cargo config {} include 的 {} 不存在",
                    path.display(),
                    include_path.display()
                ));
            }
            selected = load_file(&include_path, stack)?.or(selected);
        }
    }
    if let Some(policy) = value
        .get("resolver")
        .and_then(|resolver| resolver.get("incompatible-rust-versions"))
    {
        let policy = policy.as_str().ok_or_else(|| {
            format!(
                "Cargo config {} 的 resolver.incompatible-rust-versions 必须是字符串",
                path.display()
            )
        })?;
        selected = Some(IncompatibleRustVersions::parse(policy)?);
    }
    stack.pop();
    Ok(selected)
}

fn include_value(value: &toml::Value, source: &Path) -> Result<(PathBuf, bool), String> {
    if let Some(path) = value.as_str() {
        return Ok((PathBuf::from(path), false));
    }
    let table = value.as_table().ok_or_else(|| {
        format!(
            "Cargo config {} 的 include 成员必须是路径或表",
            source.display()
        )
    })?;
    let path = table
        .get("path")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            format!(
                "Cargo config {} 的 include 表缺字符串 path",
                source.display()
            )
        })?;
    let optional = table
        .get("optional")
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                format!(
                    "Cargo config {} 的 include.optional 必须是布尔值",
                    source.display()
                )
            })
        })
        .transpose()?
        .unwrap_or(false);
    Ok((PathBuf::from(path), optional))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "mirvm-resolver-config-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn defaults_follow_resolver_and_environment_wins() {
        let directory = tmpdir("defaults");
        assert_eq!(
            resolve(|_| None, &directory, None, ResolverVersion::V2).unwrap(),
            IncompatibleRustVersions::Allow
        );
        assert_eq!(
            resolve(|_| None, &directory, None, ResolverVersion::V3).unwrap(),
            IncompatibleRustVersions::Fallback
        );
        assert_eq!(
            resolve(
                |key| {
                    (key == "CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS")
                        .then(|| "allow".to_string())
                },
                &directory,
                None,
                ResolverVersion::V3,
            )
            .unwrap(),
            IncompatibleRustVersions::Allow
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deeper_config_and_own_value_override_includes() {
        let directory = tmpdir("merge");
        let home = directory.join("home");
        let project = directory.join("project/member");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(directory.join("project/.cargo")).unwrap();
        std::fs::create_dir_all(project.join(".cargo")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[resolver]\nincompatible-rust-versions='allow'\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("project/.cargo/base.toml"),
            "[resolver]\nincompatible-rust-versions='allow'\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("project/.cargo/config.toml"),
            "include=['base.toml']\n[resolver]\nincompatible-rust-versions='fallback'\n",
        )
        .unwrap();
        std::fs::write(
            project.join(".cargo/config"),
            "[resolver]\nincompatible-rust-versions='allow'\n",
        )
        .unwrap();
        assert_eq!(
            resolve(|_| None, &project, Some(&home), ResolverVersion::V3).unwrap(),
            IncompatibleRustVersions::Allow
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_policy_and_include_cycle_are_loud() {
        let directory = tmpdir("errors");
        std::fs::create_dir_all(directory.join(".cargo")).unwrap();
        std::fs::write(
            directory.join(".cargo/config.toml"),
            "[resolver]\nincompatible-rust-versions='guess'\n",
        )
        .unwrap();
        assert!(resolve(|_| None, &directory, None, ResolverVersion::V3).is_err());
        std::fs::write(
            directory.join(".cargo/config.toml"),
            "include=['loop.toml']\n",
        )
        .unwrap();
        std::fs::write(
            directory.join(".cargo/loop.toml"),
            "include=['config.toml']\n",
        )
        .unwrap();
        assert!(resolve(|_| None, &directory, None, ResolverVersion::V3).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
