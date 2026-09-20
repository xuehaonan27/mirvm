//! `cargoless/registry.rs` — Cargo registry and source replacement access layer.
//!
//! Own store layout (root = `$MIRVM_HOME/data/registry`; `MIRVM_HOME` is the only relocation knob):
//! ```text
//! index/<reg-key>/<sparse path>     # sparse index cache (JSON line files)
//! cache/<reg-key>/<name>-<version>.crate
//! src/<reg-key>/<name>-<version>/   # .crate unpack tree (.cargo-ok marks completion)
//! ```
//! `<reg-key>` = stable directory name for registry URL (read-through side discovers via glob `index.crates.io-*`
//! and does not hard-code cargo's hash suffix — the cargo suffix algorithm has drifted across versions).
//!
//! Read-through order (ruling ②, read-only without polluting): own src → own cache →
//! `~/.cargo/registry/src` → `~/.cargo/registry/cache` → HTTP
//! (index.crates.io / static.crates.io, pure-Rust ureq stack).
//! `MIRVM_OFFLINE=1`: disable HTTP, rely entirely on local cache, fail loudly on miss.
//!
//! Supports crates.io, alternative sparse/Git registries, and registry/local-registry/
//! directory source replacement; logical lock source and actual package-fetch backend are stored separately.
//! yanked semantics: allowed in lock (same as cargo); fresh resolution skips yanked (resolve.rs consumes this convention).

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use semver::{Version, VersionReq};

use super::config::{CargoConfig, RegistryConfig, SourceConfig, cargo_home};
use super::git::GitStore;
use super::manifest::{GitSpec, PackageManifest, RegistryReference, parse_rust_version};
use super::vendor::VendorDir;

const CRATES_IO_LOCK_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// Metadata for one version in the sparse index (one JSON line per version).
#[derive(Clone, Debug)]
pub struct IndexVersion {
    pub name: String,
    pub version: Version,
    pub cksum: String,
    pub yanked: bool,
    pub deps: Vec<IndexDep>,
    pub features: std::collections::BTreeMap<String, Vec<String>>,
    #[allow(dead_code)]
    // Cargo index fidelity; build validation reads the verified manifest value.
    pub links: Option<String>,
    pub rust_version: Option<Version>,
}

pub type IndexEntry = Arc<[IndexVersion]>;

/// Dependency metadata in the index (authoritative dependency description for registry crates — resolve does not read
/// Cargo.toml, same as cargo it uses the index as source of truth).
#[derive(Clone, Debug)]
pub struct IndexDep {
    pub name: String,
    pub req: VersionReq,
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    /// `cfg(...)` platform expression (None = all platforms); evaluation goes to manifest::eval_cfg.
    pub target: Option<String>,
    /// "build" / "dev" / None(normal).
    pub kind: Option<String>,
    /// Real name when `package = "real"` renames the dependency.
    pub package: Option<String>,
    /// `null` means same registry as parent; non-null is another registry's index URL.
    pub registry: Option<String>,
}

type RErr = String;

#[derive(Clone, Debug)]
enum Backend {
    Sparse { base: String },
    GitIndex { url: String, checkout: PathBuf },
    LocalRegistry { path: PathBuf },
    Directory { path: PathBuf },
}

#[derive(Clone, Debug)]
struct Endpoint {
    index_url: String,
    name: Option<String>,
    token: Option<String>,
    credential_provider: Option<String>,
    backend: Backend,
    download: Option<DownloadConfig>,
}

#[derive(Clone, Debug)]
struct DownloadConfig {
    template: String,
    auth_required: bool,
}

pub struct Registry {
    root: PathBuf,
    offline: bool,
    agent: ureq::Agent,
    git: GitStore,
    config: CargoConfig,
    endpoints: std::cell::RefCell<BTreeMap<String, Endpoint>>,
    /// Index entries are immutable within a single command; version solving and feature convergence repeatedly query the same
    /// package, so cache parsed results to avoid re-reading and re-parsing the whole JSON line each round.
    index_cache: std::cell::RefCell<BTreeMap<String, IndexEntry>>,
}

impl Registry {
    /// Open own store (directories created on demand; offline taken from MIRVM_OFFLINE).
    #[cfg(test)]
    pub fn open() -> Result<Self, RErr> {
        let current = std::env::current_dir().map_err(|error| error.to_string())?;
        Self::open_for_at(
            crate::store::REGISTRY.dir(),
            crate::options::get().offline(),
            &current,
        )
    }

    pub fn open_for(project: &Path) -> Result<Self, RErr> {
        Self::open_for_at(
            crate::store::REGISTRY.dir(),
            crate::options::get().offline(),
            project,
        )
    }

    #[cfg(test)]
    pub fn open_at(root: PathBuf, offline: bool) -> Result<Self, RErr> {
        let current = std::env::current_dir().map_err(|error| error.to_string())?;
        Self::open_for_at(root, offline, &current)
    }

    pub fn open_for_at(root: PathBuf, offline: bool, project: &Path) -> Result<Self, RErr> {
        for sub in ["index", "cache", "src"] {
            std::fs::create_dir_all(root.join(sub))
                .map_err(|e| format!("registry store creation failed {}: {e}", root.display()))?;
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build();
        Ok(Self {
            git: GitStore::open(root.join("git"), offline)?,
            config: CargoConfig::load_at(project, cargo_home().as_deref())?,
            root,
            offline,
            agent: config.into(),
            endpoints: std::cell::RefCell::new(BTreeMap::new()),
            index_cache: std::cell::RefCell::new(BTreeMap::new()),
        })
    }

    pub fn ensure_git_package(
        &self,
        spec: &GitSpec,
        package: &str,
        locked_source: Option<&str>,
    ) -> Result<PackageManifest, RErr> {
        self.git.ensure_package(spec, package, locked_source)
    }

    // ---------- sparse index ----------

    /// Sparse path convention (same as cargo): lowercased; 1→`1/n`, 2→`2/n`, 3→`3/{c1}/{n}`,
    /// otherwise `{c1c2}/{c3c4}/{n}`.
    pub fn sparse_path(name: &str) -> Result<String, RErr> {
        let n = name.to_ascii_lowercase();
        let bytes = n.as_bytes();
        let path = match bytes.len() {
            0 => return Err("crate name is empty".into()),
            1 => format!("1/{n}"),
            2 => format!("2/{n}"),
            3 => format!("3/{}/{n}", &n[..1]),
            _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
        };
        Ok(path)
    }

    fn index_file(&self, source: &str, name: &str) -> Result<PathBuf, RErr> {
        Ok(self
            .root
            .join("index")
            .join(source_key(source))
            .join(Self::sparse_path(name)?))
    }

    pub fn registry_source(&self, reference: &RegistryReference) -> Result<String, RErr> {
        let (logical, name, registry) = match reference {
            RegistryReference::CratesIo => (
                CRATES_IO_LOCK_SOURCE.to_string(),
                "crates-io".to_string(),
                self.config.crates_io()?,
            ),
            RegistryReference::Named(name) => {
                let registry = self.config.registry(name)?;
                (lock_source(&registry.index), name.clone(), registry)
            }
            RegistryReference::Index(index) => {
                let name = self.config.registry_name_for_index(index)?;
                let registry = match &name {
                    Some(name) => self.config.registry(name)?,
                    None => RegistryConfig {
                        index: index.clone(),
                        token: None,
                        credential_provider: None,
                    },
                };
                (
                    lock_source(index),
                    name.unwrap_or_else(|| source_key(index)),
                    registry,
                )
            }
        };
        if !self.endpoints.borrow().contains_key(&logical) {
            let endpoint = self.resolve_endpoint(&name, registry)?;
            self.endpoints
                .borrow_mut()
                .insert(logical.clone(), endpoint);
        }
        Ok(logical)
    }

    fn endpoint(&self, source: &str) -> Result<Endpoint, RErr> {
        if let Some(endpoint) = self.endpoints.borrow().get(source) {
            return Ok(endpoint.clone());
        }
        let endpoint = if source == CRATES_IO_LOCK_SOURCE {
            self.resolve_endpoint("crates-io", self.config.crates_io()?)?
        } else {
            let index = source
                .strip_prefix("registry+")
                .or_else(|| source.strip_prefix("sparse+"))
                .ok_or_else(|| format!("Cargo.lock registry source invalid: {source}"))?;
            let configured = self
                .config
                .registry_name_for_index(source)?
                .or(self.config.registry_name_for_index(index)?);
            let (name, registry) = match configured {
                Some(name) => {
                    let registry = self.config.registry(&name)?;
                    (name, registry)
                }
                None => (
                    source_key(source),
                    RegistryConfig {
                        index: if source.starts_with("sparse+") {
                            source.to_string()
                        } else {
                            index.to_string()
                        },
                        token: None,
                        credential_provider: None,
                    },
                ),
            };
            self.resolve_endpoint(&name, registry)?
        };
        self.endpoints
            .borrow_mut()
            .insert(source.to_string(), endpoint.clone());
        Ok(endpoint)
    }

    fn resolve_endpoint(
        &self,
        logical_name: &str,
        mut registry: RegistryConfig,
    ) -> Result<Endpoint, RErr> {
        let mut current = logical_name.to_string();
        let mut seen = std::collections::BTreeSet::new();
        let mut source = self.config.source(&current)?;
        while let Some(replacement) = source
            .as_ref()
            .and_then(|source| source.replace_with.clone())
        {
            if !seen.insert(current.clone()) || seen.contains(&replacement) {
                return Err(format!(
                    "Cargo source replacement cycle: {} -> {replacement}",
                    seen.into_iter().collect::<Vec<_>>().join(" -> ")
                ));
            }
            current = replacement;
            source = self.config.source(&current)?;
            if source.is_none() {
                if let Some(named_registry) = self.config.optional_registry(&current)? {
                    registry = named_registry;
                    break;
                } else {
                    return Err(format!(
                        "Cargo source `{logical_name}` replace-with points to undefined source/registry `{current}`"
                    ));
                }
            }
        }
        let mut endpoint_name = Some(current.clone());
        if let Some(index) = source
            .as_ref()
            .and_then(|source| source.registry.as_deref())
        {
            if let Some(name) = self.config.registry_name_for_index(index)? {
                registry = self.config.registry(&name)?;
                endpoint_name = Some(name);
            } else {
                endpoint_name = None;
            }
        }
        endpoint_from_config(&self.root, source.as_ref(), registry, endpoint_name)
    }

    /// Read an index entry (use cache on hit; otherwise read from the backend selected by config and cache it).
    pub fn index_entry(&self, source: &str, name: &str) -> Result<IndexEntry, RErr> {
        let cache_key = format!("{source}\u{1f}{name}");
        if let Some(entry) = self.index_cache.borrow().get(&cache_key) {
            return Ok(entry.clone());
        }
        let mut endpoint = self.endpoint(source)?;
        let text = match endpoint.backend.clone() {
            Backend::Sparse { base } => {
                self.ensure_download_config(&mut endpoint)?;
                let file = self.index_file(source, name)?;
                if file.is_file() {
                    std::fs::read_to_string(&file)
                        .map_err(|e| format!("index cache read failed {}: {e}", file.display()))?
                } else {
                    if self.offline {
                        return Err(crate::options::offline_error(format!(
                            "{source} index has no local cache for {name}"
                        )));
                    }
                    let url = format!(
                        "{}{}",
                        ensure_trailing_slash(&base),
                        Self::sparse_path(name)?
                    );
                    let token = if endpoint
                        .download
                        .as_ref()
                        .is_some_and(|config| config.auth_required)
                    {
                        endpoint_token(&endpoint, &self.config)?
                    } else {
                        endpoint.token.clone()
                    };
                    let text = self.http_text(&url, token)?;
                    write_cache(&file, text.as_bytes(), "index")?;
                    text
                }
            }
            Backend::GitIndex { url, checkout } => {
                ensure_git_index(&checkout, &url, self.offline)?;
                std::fs::read_to_string(checkout.join(Self::sparse_path(name)?)).map_err(|e| {
                    format!(
                        "Git registry index missing {name} ({}): {e}",
                        checkout.display()
                    )
                })?
            }
            Backend::LocalRegistry { path } => std::fs::read_to_string(
                path.join("index").join(Self::sparse_path(name)?),
            )
            .map_err(|e| format!("local registry missing {name} ({}): {e}", path.display()))?,
            Backend::Directory { path } => {
                let entries = directory_index_entry(&path, name)?;
                self.index_cache
                    .borrow_mut()
                    .insert(cache_key, entries.clone());
                return Ok(entries);
            }
        };
        let entry: IndexEntry = parse_index_lines(&text)?.into();
        self.index_cache
            .borrow_mut()
            .insert(cache_key, entry.clone());
        Ok(entry)
    }

    // ---------- .crate download and unpack ----------

    /// Ensure the unpacked source of {name}-{version} is present and return its directory (five-level read-through chain).
    pub fn ensure_source(
        &self,
        source: &str,
        name: &str,
        version: &Version,
        cksum: Option<&str>,
    ) -> Result<PathBuf, RErr> {
        let mut endpoint = self.endpoint(source)?;
        if let Backend::Directory { path } = &endpoint.backend {
            return ensure_directory_source(path, name, version, cksum);
        }
        let dir_name = format!("{name}-{version}");
        // ① own src
        let key = source_key(source);
        let own = self.root.join("src").join(&key).join(&dir_name);
        if own.join(".cargo-ok").is_file() {
            return Ok(own);
        }
        // ③ cargo src (read-only reuse, no copy-back — used directly as parse input)
        if source == CRATES_IO_LOCK_SOURCE
            && let Some(d) = glob_dirs(&cargo_registry_sub("src"), &dir_name)
                .into_iter()
                .next()
        {
            return Ok(d);
        }
        // ② own cache .crate files; ④ cargo cache .crate files
        let own_crate = self
            .root
            .join("cache")
            .join(&key)
            .join(format!("{dir_name}.crate"));
        let crate_file = if own_crate.is_file() {
            own_crate
        } else {
            let mut found = if source == CRATES_IO_LOCK_SOURCE {
                glob_dirs(&cargo_registry_sub("cache"), &format!("{dir_name}.crate"))
            } else {
                Vec::new()
            };
            if found.is_empty() {
                // ⑤ HTTP
                if self.offline {
                    return Err(crate::options::offline_error(format!(
                        "{dir_name} has no local cache (neither own nor read-through)"
                    )));
                }
                let bytes = match &endpoint.backend {
                    Backend::LocalRegistry { path } => {
                        std::fs::read(path.join(format!("{dir_name}.crate")))
                            .map_err(|e| format!("local registry crate missing {dir_name}: {e}"))?
                    }
                    Backend::Sparse { .. } | Backend::GitIndex { .. } => {
                        self.ensure_download_config(&mut endpoint)?;
                        self.download_crate(&endpoint, name, version, cksum)?
                    }
                    Backend::Directory { .. } => unreachable!(),
                };
                if let Some(parent) = own_crate.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("crate cache directory creation failed: {e}"))?;
                }
                std::fs::write(&own_crate, &bytes).map_err(|e| {
                    format!("crate cache write failed {}: {e}", own_crate.display())
                })?;
                found.push(own_crate.clone());
            }
            found.remove(0)
        };
        // Verify + unpack into own src
        let bytes = std::fs::read(&crate_file).map_err(|e| format!("crate read failed: {e}"))?;
        verify_cksum(&bytes, cksum, &dir_name)?;
        unpack_crate(&bytes, &own, &dir_name)?;
        std::fs::write(own.join(".cargo-ok"), "ok\n")
            .map_err(|e| format!("cargo-ok write failed: {e}"))?;
        Ok(own)
    }

    fn ensure_download_config(&self, endpoint: &mut Endpoint) -> Result<(), RErr> {
        if endpoint.download.is_some() {
            return Ok(());
        }
        let text = match &endpoint.backend {
            Backend::Sparse { base } => {
                let url = format!("{}config.json", ensure_trailing_slash(base));
                let cache = self
                    .root
                    .join("index")
                    .join(source_key(&endpoint.index_url))
                    .join("config.json");
                if cache.is_file() {
                    std::fs::read_to_string(&cache)
                        .map_err(|e| format!("registry config read failed: {e}"))?
                } else {
                    if self.offline {
                        return Err(crate::options::offline_error(format!(
                            "registry {} missing config.json cache,",
                            endpoint.index_url
                        )));
                    }
                    let text = self.http_text(&url, endpoint_token(endpoint, &self.config)?)?;
                    write_cache(&cache, text.as_bytes(), "registry config")?;
                    text
                }
            }
            Backend::GitIndex { url, checkout } => {
                ensure_git_index(checkout, url, self.offline)?;
                std::fs::read_to_string(checkout.join("config.json")).map_err(|e| {
                    format!(
                        "Git registry {} missing config.json: {e}",
                        endpoint.index_url
                    )
                })?
            }
            Backend::LocalRegistry { path } => {
                std::fs::read_to_string(path.join("index/config.json")).map_err(|e| {
                    format!(
                        "local registry {} missing index/config.json: {e}",
                        path.display()
                    )
                })?
            }
            Backend::Directory { .. } => return Ok(()),
        };
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("registry {} config.json invalid: {e}", endpoint.index_url))?;
        let template = value
            .get("dl")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "registry {} config.json missing string dl",
                    endpoint.index_url
                )
            })?
            .to_string();
        endpoint.download = Some(DownloadConfig {
            template,
            auth_required: value
                .get("auth-required")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        });
        self.endpoints
            .borrow_mut()
            .values_mut()
            .filter(|saved| saved.index_url == endpoint.index_url)
            .for_each(|saved| saved.download = endpoint.download.clone());
        Ok(())
    }

    fn http_text(&self, url: &str, token: Option<String>) -> Result<String, RErr> {
        let mut request = self.agent.get(url);
        if let Some(token) = token {
            request = request.header("Authorization", token);
        }
        let mut response = request
            .call()
            .map_err(|e| format!("HTTP fetch failed {url}: {e}"))?;
        response
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("HTTP response read failed {url}: {e}"))
    }

    fn download_crate(
        &self,
        endpoint: &Endpoint,
        name: &str,
        version: &Version,
        cksum: Option<&str>,
    ) -> Result<Vec<u8>, RErr> {
        let download = endpoint
            .download
            .as_ref()
            .ok_or("registry download config not loaded")?;
        let url = download_url(&download.template, name, version, cksum)?;
        let mut request = self.agent.get(&url);
        if download.auth_required {
            let token = endpoint_token(endpoint, &self.config)?.ok_or_else(|| {
                format!(
                    "registry {} requires authentication, but Cargo config/credentials has no usable credentials,",
                    endpoint.index_url
                )
            })?;
            request = request.header("Authorization", token);
        }
        let mut resp = request
            .call()
            .map_err(|e| format!("crate download failed {url}: {e}"))?;
        let mut buf = Vec::new();
        resp.body_mut()
            .as_reader()
            .read_to_end(&mut buf)
            .map_err(|e| format!("crate download read failed {url}: {e}"))?;
        Ok(buf)
    }
}

fn endpoint_from_config(
    root: &Path,
    source: Option<&SourceConfig>,
    registry: RegistryConfig,
    endpoint_name: Option<String>,
) -> Result<Endpoint, RErr> {
    let (index_url, backend) = match source {
        Some(source) if source.directory.is_some() => {
            let path = source.directory.clone().unwrap();
            (
                format!("directory+{}", path.display()),
                Backend::Directory { path },
            )
        }
        Some(source) if source.local_registry.is_some() => {
            let path = source.local_registry.clone().unwrap();
            (
                format!("local-registry+{}", path.display()),
                Backend::LocalRegistry { path },
            )
        }
        Some(source) if source.registry.is_some() => {
            let index = source.registry.clone().unwrap();
            let backend = backend_for_index(root, &index);
            (index, backend)
        }
        Some(source) if source.replace_with.is_none() => {
            return Err(format!(
                "Cargo source `{}` has no registry/local-registry/directory,",
                endpoint_name.as_deref().unwrap_or("unknown")
            ));
        }
        _ => {
            let index = registry.index.clone();
            let backend = backend_for_index(root, &index);
            (index, backend)
        }
    };
    Ok(Endpoint {
        index_url,
        name: endpoint_name,
        token: registry.token,
        credential_provider: registry.credential_provider,
        backend,
        download: None,
    })
}

fn backend_for_index(root: &Path, index: &str) -> Backend {
    if let Some(base) = index.strip_prefix("sparse+") {
        Backend::Sparse {
            base: ensure_trailing_slash(base),
        }
    } else {
        Backend::GitIndex {
            url: index.to_string(),
            checkout: root.join("index-git").join(source_key(index)),
        }
    }
}

fn lock_source(index: &str) -> String {
    if index.starts_with("sparse+") || index.starts_with("registry+") {
        index.to_string()
    } else {
        format!("registry+{index}")
    }
}

fn source_key(source: &str) -> String {
    format!("{:016x}", crate::utils::content::fnv1a(source.as_bytes()))
}

fn ensure_trailing_slash(value: &str) -> String {
    if value.ends_with('/') {
        value.to_string()
    } else {
        format!("{value}/")
    }
}

fn write_cache(path: &Path, bytes: &[u8], what: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "{what} cache directory creation failed {}: {error}",
                parent.display()
            )
        })?;
    }
    std::fs::write(path, bytes)
        .map_err(|error| format!("{what} cache write failed {}: {error}", path.display()))
}

fn ensure_git_index(checkout: &Path, url: &str, offline: bool) -> Result<(), String> {
    if checkout.join(".git").is_dir() {
        let actual = command_output(
            Command::new("git").arg("-C").arg(checkout).args([
                "config",
                "--get",
                "remote.origin.url",
            ]),
            "read registry Git index origin",
        )?;
        if actual != url {
            return Err(format!(
                "registry Git index cache identity mismatch {}: expected {url}, got {actual}",
                checkout.display()
            ));
        }
        if !offline {
            command_ok(
                Command::new("git")
                    .arg("-C")
                    .arg(checkout)
                    .args(["fetch", "--force", "origin"]),
                "update registry Git index",
            )?;
            command_ok(
                Command::new("git")
                    .arg("-C")
                    .arg(checkout)
                    .args(["reset", "--hard", "FETCH_HEAD"]),
                "switch registry Git index",
            )?;
        }
        return Ok(());
    }
    if offline {
        return Err(crate::options::offline_error(format!(
            "registry Git index {url} has no local cache"
        )));
    }
    if let Some(parent) = checkout.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("registry Git index directory creation failed: {error}"))?;
    }
    command_ok(
        Command::new("git")
            .args(["clone", "--no-tags", "--depth", "1", url])
            .arg(checkout),
        &format!("clone registry Git index {url}"),
    )
}

fn command_ok(command: &mut Command, what: &str) -> Result<(), String> {
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("{what} launch failed: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{what} failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn command_output(command: &mut Command, what: &str) -> Result<String, String> {
    let output = command
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("{what} launch failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{what} failed (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn directory_index_entry(root: &Path, name: &str) -> Result<IndexEntry, String> {
    let mut entries = Vec::new();
    for item in std::fs::read_dir(root).map_err(|error| {
        format!(
            "read Cargo directory source {} failed: {error}",
            root.display()
        )
    })? {
        let item = item.map_err(|error| format!("read directory source entry failed: {error}"))?;
        if !item.path().is_dir() || !item.path().join("Cargo.toml").is_file() {
            continue;
        }
        let mut entry =
            VendorDir::entry_from_dir(&item.path(), &format!("directory source package {name}"))?;
        if entry.name != name {
            continue;
        }
        let checksum_file = item.path().join(".cargo-checksum.json");
        let checksum_text = std::fs::read_to_string(&checksum_file)
            .map_err(|error| format!("read {} failed: {error}", checksum_file.display()))?;
        let checksum: serde_json::Value = serde_json::from_str(&checksum_text)
            .map_err(|error| format!("{} invalid: {error}", checksum_file.display()))?;
        entry.cksum = checksum
            .get("package")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        entries.push(entry);
    }
    entries.sort_by(|left, right| left.version.cmp(&right.version));
    if entries.is_empty() {
        return Err(format!(
            "Cargo directory source {} has no package {name}",
            root.display()
        ));
    }
    Ok(entries.into())
}

fn ensure_directory_source(
    root: &Path,
    name: &str,
    version: &Version,
    expected_package_checksum: Option<&str>,
) -> Result<PathBuf, String> {
    let directory = find_directory_package(root, name, version)?;
    let checksum_file = directory.join(".cargo-checksum.json");
    let text = std::fs::read_to_string(&checksum_file)
        .map_err(|error| format!("read {} failed: {error}", checksum_file.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("{} is invalid: {error}", checksum_file.display()))?;
    if let (Some(expected), Some(actual)) = (
        expected_package_checksum,
        value.get("package").and_then(serde_json::Value::as_str),
    ) && expected != actual
    {
        return Err(format!(
            "directory source {name}-{version} package checksum mismatch: lock={expected} vendor={actual}"
        ));
    }
    let files = value
        .get("files")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{} missing files checksum table", checksum_file.display()))?;
    for (relative, checksum) in files {
        let checksum = checksum.as_str().ok_or_else(|| {
            format!(
                "{} files.{relative} checksum is not a string,",
                checksum_file.display()
            )
        })?;
        let bytes = std::fs::read(directory.join(relative))
            .map_err(|error| format!("directory source file missing {relative}: {error}"))?;
        let actual = sha256_hex(&bytes);
        if actual != checksum.to_ascii_lowercase() {
            return Err(format!(
                "directory source file {relative} sha256 verification failed (want {checksum} got {actual})"
            ));
        }
    }
    Ok(directory)
}

fn find_directory_package(root: &Path, name: &str, version: &Version) -> Result<PathBuf, String> {
    for item in std::fs::read_dir(root).map_err(|error| {
        format!(
            "read Cargo directory source {} failed: {error}",
            root.display()
        )
    })? {
        let item = item.map_err(|error| format!("read directory source entry failed: {error}"))?;
        if !item.path().is_dir() || !item.path().join("Cargo.toml").is_file() {
            continue;
        }
        let manifest = PackageManifest::read_dir(&item.path()).map_err(|error| {
            format!(
                "parse directory source {} failed: {error}",
                item.path().display()
            )
        })?;
        if manifest.name == name && manifest.version == *version {
            return Ok(item.path());
        }
    }
    Err(format!(
        "Cargo directory source {} is missing package {name} {version}",
        root.display()
    ))
}

fn endpoint_token(endpoint: &Endpoint, config: &CargoConfig) -> Result<Option<String>, String> {
    let mut providers = if let Some(provider) = &endpoint.credential_provider {
        vec![provider.clone()]
    } else {
        config.global_credential_providers()?
    };
    providers.reverse();
    for provider in providers {
        let provider = config.credential_provider(&provider)?;
        let parts = command_parts(&provider)?;
        let Some((program, configured_args)) = parts.split_first() else {
            return Err("Cargo credential provider command is empty".into());
        };
        if program == "cargo:token" {
            if endpoint.token.is_some() {
                return Ok(endpoint.token.clone());
            }
            continue;
        }
        if program == "cargo:token-from-stdout" {
            let Some((command, args)) = configured_args.split_first() else {
                return Err("cargo:token-from-stdout missing command".into());
            };
            let token = command_output(
                Command::new(command).args(args),
                "execute Cargo token provider",
            )?;
            if !token.is_empty() {
                return Ok(Some(token));
            }
            continue;
        }
        if program.starts_with("cargo:") {
            return Err(format!(
                "Cargo built-in credential provider `{provider}` cannot be called outside the mirvm process; configure the corresponding cargo-credential-* executable for this provider"
            ));
        }
        if let Some(token) = external_credential(program, configured_args, endpoint)? {
            return Ok(Some(token));
        }
    }
    Ok(None)
}

fn external_credential(
    program: &str,
    configured_args: &[String],
    endpoint: &Endpoint,
) -> Result<Option<String>, String> {
    let mut child = Command::new(program)
        .arg("--cargo-plugin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("launch Cargo credential provider `{program}` failed: {error}"))?;
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut hello = String::new();
    stdout
        .read_line(&mut hello)
        .map_err(|error| format!("read credential provider hello failed: {error}"))?;
    let hello_value: serde_json::Value = serde_json::from_str(hello.trim())
        .map_err(|error| format!("credential provider hello invalid: {error}"))?;
    if !hello_value
        .get("v")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|versions| versions.iter().any(|version| version.as_u64() == Some(1)))
    {
        return Err("credential provider does not support protocol v1".into());
    }
    let request = serde_json::json!({
        "v": 1,
        "kind": "get",
        "operation": "read",
        "registry": {
            "index-url": endpoint.index_url,
            "name": endpoint.name,
        },
        "args": configured_args,
    });
    writeln!(child.stdin.take().unwrap(), "{request}")
        .map_err(|error| format!("write credential provider request failed: {error}"))?;
    let mut response = String::new();
    stdout
        .read_line(&mut response)
        .map_err(|error| format!("read credential provider response failed: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("wait for credential provider failed: {error}"))?;
    if !status.success() {
        return Err(format!(
            "credential provider `{program}` failed (exit {:?})",
            status.code()
        ));
    }
    let value: serde_json::Value = serde_json::from_str(response.trim())
        .map_err(|error| format!("credential provider response invalid: {error}"))?;
    let ok = value.get("Ok").unwrap_or(&value);
    if let Some(token) = ok.get("token").and_then(serde_json::Value::as_str) {
        return Ok(Some(token.to_string()));
    }
    if value.get("Err").is_some() {
        return Ok(None);
    }
    Err("credential provider response has neither token nor Err".into())
}

fn command_parts(command: &str) -> Result<Vec<String>, String> {
    if command.contains('\u{1f}') {
        return Ok(command.split('\u{1f}').map(str::to_string).collect());
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if quote == Some(character) {
            quote = None;
        } else if quote.is_none() && matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if quote.is_none() && character.is_whitespace() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if escaped || quote.is_some() {
        return Err(format!(
            "Cargo credential provider command quoting/escaping incomplete: {command}"
        ));
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

fn download_url(
    template: &str,
    name: &str,
    version: &Version,
    checksum: Option<&str>,
) -> Result<String, String> {
    let lower = name.to_ascii_lowercase();
    let prefix = match lower.len() {
        1 => "1".to_string(),
        2 => "2".to_string(),
        3 => format!("3/{}", &lower[..1]),
        _ => format!("{}/{}", &lower[..2], &lower[2..4]),
    };
    let has_marker = [
        "{crate}",
        "{version}",
        "{prefix}",
        "{lowerprefix}",
        "{sha256-checksum}",
    ]
    .iter()
    .any(|marker| template.contains(marker));
    let checksum = checksum.unwrap_or("");
    if template.contains("{sha256-checksum}") && checksum.is_empty() {
        return Err(
            "registry dl template requires {sha256-checksum} but index has no checksum".into(),
        );
    }
    let mut url = template
        .replace("{crate}", name)
        .replace("{version}", &version.to_string())
        .replace("{prefix}", &prefix)
        .replace("{lowerprefix}", &prefix.to_ascii_lowercase())
        .replace("{sha256-checksum}", checksum);
    if !has_marker {
        url = format!("{}/{name}/{version}/download", url.trim_end_matches('/'));
    }
    Ok(url)
}

// ---------- index JSON line parsing ----------

fn parse_index_lines(text: &str) -> Result<Vec<IndexVersion>, RErr> {
    let mut out = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("index JSON line {} parse failed: {e}", ln + 1))?;
        out.push(parse_index_version(&v).map_err(|e| format!("index line {}: {e}", ln + 1))?);
    }
    Ok(out)
}

fn parse_index_version(v: &serde_json::Value) -> Result<IndexVersion, RErr> {
    let get_str = |k: &str| v.get(k).and_then(|x| x.as_str());
    let name = get_str("name").ok_or("missing name")?.to_string();
    let version = Version::parse(get_str("vers").ok_or("missing vers")?)
        .map_err(|e| format!("vers invalid: {e}"))?;
    let cksum = get_str("cksum").ok_or("missing cksum")?.to_string();
    let yanked = v.get("yanked").and_then(|x| x.as_bool()).unwrap_or(false);
    let links = get_str("links").map(str::to_string);
    let rust_version = get_str("rust_version")
        .or_else(|| get_str("rust_version2"))
        .map(|version| parse_rust_version(version, "registry rust_version"))
        .transpose()?;
    let mut features = std::collections::BTreeMap::new();
    for (fk, fv) in v
        .get("features")
        .and_then(|x| x.as_object())
        .into_iter()
        .flatten()
    {
        let vals = fv
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        features.insert(fk.clone(), vals);
    }
    // features2 (weakly activated "?/" extension table) merges into the same table (shape judged by manifest::parse_feature_value)
    for (fk, fv) in v
        .get("features2")
        .and_then(|x| x.as_object())
        .into_iter()
        .flatten()
    {
        let vals = fv
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        features.entry(fk.clone()).or_default().extend(vals);
    }
    let mut deps = Vec::new();
    for d in v
        .get("deps")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        let dname = d
            .get("name")
            .and_then(|x| x.as_str())
            .ok_or("dep missing name")?;
        let req = VersionReq::parse(
            d.get("req")
                .and_then(|x| x.as_str())
                .ok_or("dep missing req")?,
        )
        .map_err(|e| format!("dep {dname} req invalid: {e}"))?;
        let features = d
            .get("features")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        deps.push(IndexDep {
            name: dname.to_string(),
            req,
            features,
            optional: d.get("optional").and_then(|x| x.as_bool()).unwrap_or(false),
            default_features: d
                .get("default_features")
                .and_then(|x| x.as_bool())
                .unwrap_or(true),
            target: d.get("target").and_then(|x| x.as_str()).map(str::to_string),
            kind: d.get("kind").and_then(|x| x.as_str()).map(str::to_string),
            package: d
                .get("package")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            registry: d
                .get("registry")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        });
    }
    Ok(IndexVersion {
        name,
        version,
        cksum,
        yanked,
        deps,
        features,
        links,
        rust_version,
    })
}

// ---------- cksum and unpack ----------

/// Registry protocol only accepts sha256 (64 hex) — an absent cksum skips verification, which only
/// synthetic tests rely on; real registry paths always carry one.
fn verify_cksum(bytes: &[u8], cksum: Option<&str>, dir_name: &str) -> Result<(), RErr> {
    let Some(want) = cksum else { return Ok(()) };
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{dir_name} cksum format invalid (not 64 hex sha256)"
        ));
    }
    let got = sha256_hex(bytes);
    if got != want.to_ascii_lowercase() {
        return Err(format!(
            "{dir_name} sha256 verification failed (want {want} got {got}) — refusing to unpack"
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    // Minimal SHA-256 implementation (avoid pulling in a dependency tree just for this verification; registry protocol only needs this)
    struct Sha256 {
        state: [u32; 8],
        buf: [u8; 64],
        buf_len: usize,
        total: u64,
    }
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    impl Sha256 {
        fn new() -> Self {
            Self {
                state: [
                    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                    0x1f83d9ab, 0x5be0cd19,
                ],
                buf: [0; 64],
                buf_len: 0,
                total: 0,
            }
        }
        fn block(&mut self, b: &[u8]) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ (!e & g);
                let t1 = h
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                h = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            self.state = [
                a.wrapping_add(self.state[0]),
                b.wrapping_add(self.state[1]),
                c.wrapping_add(self.state[2]),
                d.wrapping_add(self.state[3]),
                e.wrapping_add(self.state[4]),
                f.wrapping_add(self.state[5]),
                g.wrapping_add(self.state[6]),
                h.wrapping_add(self.state[7]),
            ];
        }
        fn update(&mut self, mut data: &[u8]) {
            self.total = self.total.wrapping_add(data.len() as u64);
            if self.buf_len > 0 {
                let need = 64 - self.buf_len;
                let take = need.min(data.len());
                self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
                self.buf_len += take;
                data = &data[take..];
                if self.buf_len == 64 {
                    let b = self.buf;
                    self.block(&b);
                    self.buf_len = 0;
                }
            }
            while data.len() >= 64 {
                let (head, tail) = data.split_at(64);
                self.block(head);
                data = tail;
            }
            if !data.is_empty() {
                self.buf[..data.len()].copy_from_slice(data);
                self.buf_len = data.len();
            }
        }
        fn finish(mut self) -> [u8; 32] {
            let bit_len = self.total.wrapping_mul(8);
            self.update(&[0x80]);
            while self.buf_len != 56 {
                self.update(&[0]);
            }
            self.update(&bit_len.to_be_bytes());
            let mut out = [0u8; 32];
            for (i, s) in self.state.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&s.to_be_bytes());
            }
            out
        }
    }
    let mut h = Sha256::new();
    h.update(bytes);
    h.finish().iter().map(|b| format!("{b:02x}")).collect()
}

/// .crate = tar.gz with a single inner top-level directory `{name}-{version}/` (verified and stripped).
fn unpack_crate(bytes: &[u8], dest: &Path, dir_name: &str) -> Result<(), RErr> {
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut ar = tar::Archive::new(gz);
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(|e| format!("unpack target cleanup failed: {e}"))?;
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("unpack target creation failed: {e}"))?;
    for entry in ar
        .entries()
        .map_err(|e| format!("crate tar read failed: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("crate tar entry read failed: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("crate tar path read failed: {e}"))?
            .into_owned();
        let mut comps = path.components();
        let top = comps
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        if top != dir_name {
            return Err(format!(
                "crate top directory {top} does not match {dir_name} — refusing to unpack"
            ));
        }
        let rel: PathBuf = comps.collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        // Path traversal guard (.. and absolute paths are rejected)
        if rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            return Err(format!(
                "crate contains traversal path {} — refusing to unpack",
                rel.display()
            ));
        }
        let target = dest.join(&rel);
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| format!("unpack mkdir failed: {e}"))?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("unpack mkdir failed: {e}"))?;
            }
            entry
                .unpack(&target)
                .map_err(|e| format!("unpack write file failed {}: {e}", target.display()))?;
        }
    }
    Ok(())
}

// ---------- read-through (cargo cache read-only) ----------

fn cargo_registry_sub(kind: &str) -> PathBuf {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .unwrap_or_default();
    home.join("registry").join(kind)
}

/// Find an entry named file_name under cargo's {kind}/index.crates.io-*/ (discovered by glob,
/// not hard-coding the hash suffix).
fn glob_dirs(base: &Path, file_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(base) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let Some(dirname) = p.file_name().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if !dirname.starts_with("index.crates.io-") {
            continue;
        }
        let cand = p.join(file_name);
        if cand.is_file() || cand.is_dir() {
            out.push(cand);
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests;
