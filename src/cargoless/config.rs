//! Cargo 配置发现与当前依赖来源切片需要的类型化读取。
//!
//! Cargo 从 `$CARGO_HOME` 开始，再按调用目录祖先由浅到深叠加 `.cargo/config`
//! 或 `config.toml`；`include` 先于包含它的文件。这里保留每一层及其来源路径，
//! 让 directory/local-registry 的相对路径能按声明文件解释。只读取 resolver、
//! registries、registry credential 和 source replacement；其他 Cargo 配置键不建模。

use std::path::{Path, PathBuf};

use super::manifest::{IncompatibleRustVersions, ResolverVersion};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryConfig {
    pub index: String,
    pub token: Option<String>,
    pub credential_provider: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceConfig {
    pub replace_with: Option<String>,
    pub registry: Option<String>,
    pub local_registry: Option<PathBuf>,
    pub directory: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct Layer {
    path: PathBuf,
    value: toml::Value,
}

#[derive(Clone, Debug, Default)]
pub struct CargoConfig {
    layers: Vec<Layer>,
    cargo_home: Option<PathBuf>,
}

impl CargoConfig {
    pub fn load_at(current: &Path, cargo_home: Option<&Path>) -> Result<Self, String> {
        let mut layers = Vec::new();
        if let Some(home) = cargo_home
            && let Some(file) = config_file(home)
        {
            load_file(&file, &mut Vec::new(), &mut layers)?;
        }
        let mut ancestors: Vec<&Path> = current.ancestors().collect();
        ancestors.reverse();
        for directory in ancestors {
            if let Some(file) = config_file(&directory.join(".cargo")) {
                load_file(&file, &mut Vec::new(), &mut layers)?;
            }
        }
        Ok(Self {
            layers,
            cargo_home: cargo_home.map(Path::to_path_buf),
        })
    }

    pub fn incompatible_rust_versions(
        &self,
        resolver: ResolverVersion,
    ) -> Result<IncompatibleRustVersions, String> {
        let mut selected = None;
        for layer in &self.layers {
            if let Some(value) = layer
                .value
                .get("resolver")
                .and_then(|table| table.get("incompatible-rust-versions"))
            {
                let value = value.as_str().ok_or_else(|| {
                    format!(
                        "Cargo config {} 的 resolver.incompatible-rust-versions 必须是字符串",
                        layer.path.display()
                    )
                })?;
                selected = Some(IncompatibleRustVersions::parse(value)?);
            }
        }
        if let Ok(value) = std::env::var("CARGO_RESOLVER_INCOMPATIBLE_RUST_VERSIONS") {
            selected = Some(IncompatibleRustVersions::parse(&value)?);
        }
        Ok(selected.unwrap_or(match resolver {
            ResolverVersion::V3 => IncompatibleRustVersions::Fallback,
            ResolverVersion::V1 | ResolverVersion::V2 => IncompatibleRustVersions::Allow,
        }))
    }

    pub fn registry(&self, name: &str) -> Result<RegistryConfig, String> {
        self.optional_registry(name)?.ok_or_else(|| {
            let env_name = registry_env_name(name);
            format!(
                "Cargo registry `{name}` 未配置 index（需要 [registries.{name}] 或 CARGO_REGISTRIES_{env_name}_INDEX）"
            )
        })
    }

    pub fn optional_registry(&self, name: &str) -> Result<Option<RegistryConfig>, String> {
        let mut index = None;
        let mut token = None;
        let mut credential_provider = None;
        for layer in &self.layers {
            let Some(table) = layer
                .value
                .get("registries")
                .and_then(|value| value.get(name))
                .and_then(toml::Value::as_table)
            else {
                continue;
            };
            if let Some(value) = table.get("index") {
                index = Some(config_string(value, &layer.path, "registries.*.index")?);
            }
            if let Some(value) = table.get("token") {
                token = Some(config_string(value, &layer.path, "registries.*.token")?);
            }
            if let Some(value) = table.get("credential-provider") {
                credential_provider = Some(config_command(
                    value,
                    &layer.path,
                    "registries.*.credential-provider",
                )?);
            }
        }
        let env_name = registry_env_name(name);
        if let Ok(value) = std::env::var(format!("CARGO_REGISTRIES_{env_name}_INDEX")) {
            index = Some(value);
        }
        if let Ok(value) = std::env::var(format!("CARGO_REGISTRIES_{env_name}_TOKEN")) {
            token = Some(value);
        }
        if let Ok(value) = std::env::var(format!("CARGO_REGISTRIES_{env_name}_CREDENTIAL_PROVIDER"))
        {
            credential_provider = Some(value);
        }
        let Some(index) = index else {
            return Ok(None);
        };
        if token.is_none() {
            token = self.credential_token(name)?;
        }
        Ok(Some(RegistryConfig {
            index,
            token,
            credential_provider,
        }))
    }

    pub fn crates_io(&self) -> Result<RegistryConfig, String> {
        let mut token = std::env::var("CARGO_REGISTRY_TOKEN").ok();
        let mut credential_provider = std::env::var("CARGO_REGISTRY_CREDENTIAL_PROVIDER").ok();
        for layer in &self.layers {
            let Some(table) = layer.value.get("registry").and_then(toml::Value::as_table) else {
                continue;
            };
            if token.is_none()
                && let Some(value) = table.get("token")
            {
                token = Some(config_string(value, &layer.path, "registry.token")?);
            }
            if credential_provider.is_none()
                && let Some(value) = table.get("credential-provider")
            {
                credential_provider = Some(config_command(
                    value,
                    &layer.path,
                    "registry.credential-provider",
                )?);
            }
        }
        if token.is_none() {
            token = self.credential_token("crates-io")?;
        }
        Ok(RegistryConfig {
            index: "sparse+https://index.crates.io/".to_string(),
            token,
            credential_provider,
        })
    }

    pub fn registry_name_for_index(&self, index: &str) -> Result<Option<String>, String> {
        let mut names = std::collections::BTreeSet::new();
        for layer in &self.layers {
            let Some(registries) = layer
                .value
                .get("registries")
                .and_then(toml::Value::as_table)
            else {
                continue;
            };
            names.extend(registries.keys().cloned());
        }
        for (key, _) in std::env::vars()
            .filter(|(key, _)| key.starts_with("CARGO_REGISTRIES_") && key.ends_with("_INDEX"))
        {
            let encoded = key
                .trim_start_matches("CARGO_REGISTRIES_")
                .trim_end_matches("_INDEX");
            names.insert(encoded.to_ascii_lowercase().replace('_', "-"));
        }
        for name in names {
            if self.registry(&name)?.index == index {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    pub fn source(&self, name: &str) -> Result<Option<SourceConfig>, String> {
        let mut selected = SourceConfig::default();
        let mut found = false;
        for layer in &self.layers {
            let Some(table) = layer
                .value
                .get("source")
                .and_then(|value| value.get(name))
                .and_then(toml::Value::as_table)
            else {
                continue;
            };
            found = true;
            if let Some(value) = table.get("replace-with") {
                selected.replace_with =
                    Some(config_string(value, &layer.path, "source.*.replace-with")?);
            }
            if let Some(value) = table.get("registry") {
                selected.registry = Some(config_string(value, &layer.path, "source.*.registry")?);
            }
            if let Some(value) = table.get("local-registry") {
                let path = config_string(value, &layer.path, "source.*.local-registry")?;
                selected.local_registry = Some(resolve_config_path(&layer.path, &path));
            }
            if let Some(value) = table.get("directory") {
                let path = config_string(value, &layer.path, "source.*.directory")?;
                selected.directory = Some(resolve_config_path(&layer.path, &path));
            }
        }
        let kinds = [
            selected.registry.is_some(),
            selected.local_registry.is_some(),
            selected.directory.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if kinds > 1 {
            return Err(format!(
                "Cargo source `{name}` 同时声明 registry/local-registry/directory"
            ));
        }
        Ok(found.then_some(selected))
    }

    pub fn global_credential_providers(&self) -> Result<Vec<String>, String> {
        let mut providers = Vec::new();
        for layer in &self.layers {
            let Some(value) = layer
                .value
                .get("registry")
                .and_then(|registry| registry.get("global-credential-providers"))
            else {
                continue;
            };
            let values = value.as_array().ok_or_else(|| {
                format!(
                    "Cargo config {} 的 registry.global-credential-providers 必须是数组",
                    layer.path.display()
                )
            })?;
            providers = values
                .iter()
                .map(|value| {
                    config_command(value, &layer.path, "registry.global-credential-providers")
                })
                .collect::<Result<_, _>>()?;
        }
        if let Ok(value) = std::env::var("CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS") {
            providers = value.split_whitespace().map(str::to_string).collect();
        }
        Ok(providers)
    }

    pub fn credential_provider(&self, name: &str) -> Result<String, String> {
        let env_name = registry_env_name(name);
        if let Ok(value) = std::env::var(format!("CARGO_CREDENTIAL_ALIAS_{env_name}")) {
            return Ok(value);
        }
        let mut selected = None;
        for layer in &self.layers {
            if let Some(value) = layer
                .value
                .get("credential-alias")
                .and_then(|aliases| aliases.get(name))
            {
                selected = Some(config_command(value, &layer.path, "credential-alias.*")?);
            }
        }
        Ok(selected.unwrap_or_else(|| name.to_string()))
    }

    fn credential_token(&self, name: &str) -> Result<Option<String>, String> {
        let Some(home) = &self.cargo_home else {
            return Ok(None);
        };
        for file in [home.join("credentials"), home.join("credentials.toml")] {
            if !file.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&file).map_err(|error| {
                format!("读取 Cargo credentials {} 失败: {error}", file.display())
            })?;
            let value: toml::Value = toml::from_str(&text).map_err(|error| {
                format!("解析 Cargo credentials {} 失败: {error}", file.display())
            })?;
            let token = if name == "crates-io" {
                value
                    .get("registry")
                    .and_then(|registry| registry.get("token"))
            } else {
                value
                    .get("registries")
                    .and_then(|registries| registries.get(name))
                    .and_then(|registry| registry.get("token"))
            };
            if let Some(token) = token {
                return Ok(Some(config_string(token, &file, "credentials token")?));
            }
        }
        Ok(None)
    }
}

pub(crate) fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
}

fn registry_env_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn config_file(directory: &Path) -> Option<PathBuf> {
    let legacy = directory.join("config");
    if legacy.is_file() {
        return Some(legacy);
    }
    let toml = directory.join("config.toml");
    toml.is_file().then_some(toml)
}

fn load_file(path: &Path, stack: &mut Vec<PathBuf>, layers: &mut Vec<Layer>) -> Result<(), String> {
    let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if stack.contains(&identity) {
        return Err(format!("Cargo config include 形成环: {}", path.display()));
    }
    stack.push(identity);
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读取 Cargo config {} 失败: {error}", path.display()))?;
    let value: toml::Value = toml::from_str(&text)
        .map_err(|error| format!("解析 Cargo config {} 失败: {error}", path.display()))?;
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
            load_file(&include_path, stack, layers)?;
        }
    }
    layers.push(Layer {
        path: path.to_path_buf(),
        value,
    });
    stack.pop();
    Ok(())
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
        .ok_or_else(|| format!("Cargo config {} include 表缺字符串 path", source.display()))?;
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

fn config_string(value: &toml::Value, source: &Path, field: &str) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("Cargo config {} 的 {field} 必须是字符串", source.display()))
}

fn config_command(value: &toml::Value, source: &Path, field: &str) -> Result<String, String> {
    if let Some(command) = value.as_str() {
        return Ok(command.to_string());
    }
    let array = value.as_array().ok_or_else(|| {
        format!(
            "Cargo config {} 的 {field} 必须是字符串或数组",
            source.display()
        )
    })?;
    array
        .iter()
        .map(|part| {
            part.as_str().map(str::to_string).ok_or_else(|| {
                format!(
                    "Cargo config {} 的 {field} 数组成员必须是字符串",
                    source.display()
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("\u{1f}"))
}

fn resolve_config_path(config: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    let cargo_dir = config.parent().unwrap_or(Path::new("."));
    cargo_dir.parent().unwrap_or(cargo_dir).join(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("mirvm-cargo-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn layers_include_registries_sources_and_paths() {
        let directory = tmpdir("layers");
        let home = directory.join("home");
        let project = directory.join("project/member");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(directory.join("project/.cargo")).unwrap();
        std::fs::create_dir_all(project.join(".cargo")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "[registries.alt]\nindex='sparse+https://low.invalid/'\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("project/.cargo/base.toml"),
            "[source.vendor]\ndirectory='vendor'\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("project/.cargo/config.toml"),
            "include=['base.toml']\n[registries.alt]\nindex='sparse+https://high.invalid/'\n[source.crates-io]\nreplace-with='vendor'\n",
        )
        .unwrap();
        let config = CargoConfig::load_at(&project, Some(&home)).unwrap();
        assert_eq!(
            config.registry("alt").unwrap().index,
            "sparse+https://high.invalid/"
        );
        assert_eq!(
            config
                .source("crates-io")
                .unwrap()
                .unwrap()
                .replace_with
                .as_deref(),
            Some("vendor")
        );
        assert_eq!(
            config.source("vendor").unwrap().unwrap().directory,
            Some(directory.join("project/vendor"))
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn include_cycles_and_conflicting_source_kinds_are_loud() {
        let directory = tmpdir("errors");
        std::fs::create_dir_all(directory.join(".cargo")).unwrap();
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
        assert!(CargoConfig::load_at(&directory, None).is_err());
        std::fs::write(
            directory.join(".cargo/config.toml"),
            "[source.bad]\nregistry='https://x.invalid/index'\ndirectory='vendor'\n",
        )
        .unwrap();
        let config = CargoConfig::load_at(&directory, None).unwrap();
        assert!(config.source("bad").is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
